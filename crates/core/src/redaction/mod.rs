//! Shared, versioned secret scanner (spec 12 §2 `[FIXED]` redaction, `[SPEC]`
//! rule set).
//!
//! A single scanner serves three flows so their verdicts stay consistent and
//! reproducible against one `redaction_version`:
//!
//! - **file classification** (spec 06 §2.2): a file whose content the scanner
//!   flags is `skipped_file(reason='secret')` — no `source_blob`, no occurrences
//!   (spec 12 §5). The classifier calls [`Scanner::has_secret`].
//! - **spool ingestion** (spec 07 §2, authored in group 13): redaction runs before
//!   anything touches disk.
//! - **remote transmission** (spec 12 §1/§2, authored in group 16): redaction runs
//!   again before any payload leaves the machine.
//!
//! The latter two consume [`Scanner::scan`], which returns byte spans so a payload
//! can be rewritten; T03-02 only wires the file-classification verdict — the
//! payload-rewriting transform is deferred to those later groups.
//!
//! # Versioning
//!
//! [`REDACTION_VERSION`] identifies the rule set that produced a verdict; consumers
//! record it in envelopes (spec 12 §2 `[SPEC]`) so a verdict is auditable against
//! the exact rules that produced it. Bumping the rule set MUST bump the version.
//!
//! # Rule set v0 (`[SPEC]`, conservative and dependency-free)
//!
//! Detection is deterministic byte/line scanning — no regex engine — so the crate
//! takes on no new dependency and every verdict is stable across platforms:
//!
//! 1. **PEM private keys** — a line bearing both `-----BEGIN` and `PRIVATE KEY`.
//! 2. **Known credential formats** — tokens with a recognized prefix and a
//!    plausible minimum length (AWS `AKIA`/`ASIA`, GitHub `ghp_`/`gho_`/`ghs_`/
//!    `github_pat_`, Slack `xox[bpas]-`, OpenAI-style `sk-`).
//! 3. **Assigned secrets** — a `password`/`secret`/`api_key`/`token`/… key
//!    assigned a **quoted** literal of non-trivial length (an unquoted value such
//!    as `let token = get()` is not flagged, keeping ordinary code quiet).
//! 4. **High-entropy strings** — a long token (≥ [`ENTROPY_MIN_LEN`]) whose Shannon
//!    entropy per character is ≥ [`ENTROPY_MIN_BITS`]. The threshold sits above a
//!    hex digest's ceiling (log2 16 = 4.0) so git SHAs and hex checksums do **not**
//!    trip it, while base64-encoded key material (≈5.5–6.0 bits/char) does.
//!
//! The set is intentionally conservative: it aims to catch obvious committed
//! secrets with a low false-positive rate on real source, and is expected to grow
//! (each growth bumps [`REDACTION_VERSION`]).

/// The rule-set version stamped on every verdict (spec 12 §2 `[SPEC]`).
///
/// Recorded by spool/remote flows in their envelopes so a redaction verdict is
/// reproducible against the rules that produced it. Any change to the rule set
/// below MUST increment this.
pub const REDACTION_VERSION: u32 = 1;

/// Minimum token length considered by the high-entropy rule.
pub const ENTROPY_MIN_LEN: usize = 40;

/// Minimum Shannon entropy (bits per character) for the high-entropy rule.
///
/// Chosen above a hex digest's maximum (`log2 16 = 4.0`) so 40-char git SHAs and
/// hex checksums are not flagged, while base64 key material (`≈5.5–6.0`) is.
pub const ENTROPY_MIN_BITS: f64 = 4.5;

/// The fixed marker substituted for every detected secret span by
/// [`Scanner::redact`] (spec 07 §2, 12 §2). A single generic marker rather than
/// one per [`FindingKind`]: the consumer already has `redaction_version` to
/// audit which rule set produced a verdict, and a kind-specific marker would
/// leak a hint about what was removed for no operational benefit.
pub const REDACTION_MARKER: &str = "[REDACTED]";

/// The result of applying [`Scanner::redact`] to a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    /// `text` with every detected secret span replaced by [`REDACTION_MARKER`].
    pub text: String,
    /// How many (merged) secret spans were replaced. Two rules matching the
    /// same span (e.g. `AssignedSecret` and `HighEntropy` on one value) count as
    /// one finding here, since exactly one marker was inserted.
    pub findings: usize,
}

/// What kind of secret a [`Finding`] identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// A PEM private-key header line.
    PrivateKey,
    /// A token matching a known credential format.
    CredentialToken,
    /// A secret-like key assigned a quoted literal value.
    AssignedSecret,
    /// A long, high-entropy string.
    HighEntropy,
}

/// One secret located in scanned text, as a byte span `[start, end)`.
///
/// The span indexes the exact bytes passed to [`Scanner::scan`], so a consumer can
/// redact the payload in place (spool/remote flows, later groups).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finding {
    /// Which rule matched.
    pub kind: FindingKind,
    /// Byte offset of the first matched byte.
    pub start: usize,
    /// Byte offset one past the last matched byte.
    pub end: usize,
}

/// The versioned secret scanner (spec 12 §2).
///
/// Construct with [`Scanner::new`]; the rule set is fixed per [`REDACTION_VERSION`].
///
/// ```
/// let scanner = local_rag_core::redaction::Scanner::new();
/// assert!(scanner.has_secret("aws_key = \"AKIAIOSFODNN7EXAMPLE\""));
/// assert!(!scanner.has_secret("fn add(a: i32, b: i32) -> i32 { a + b }"));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Scanner {
    version: u32,
}

impl Scanner {
    /// A scanner using rule set [`REDACTION_VERSION`].
    pub fn new() -> Self {
        Scanner {
            version: REDACTION_VERSION,
        }
    }

    /// The rule-set version this scanner applies.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether `text` contains at least one secret.
    ///
    /// The file classifier's verdict: `true` ⇒ `skipped_file(reason='secret')`.
    /// Short-circuits on the first match.
    pub fn has_secret(&self, text: &str) -> bool {
        !self.scan_inner(text, true).is_empty()
    }

    /// Every secret located in `text`, in ascending start order.
    ///
    /// Spans reference `text`'s bytes so a consumer can rewrite the payload
    /// (spool/remote redaction, later groups).
    pub fn scan(&self, text: &str) -> Vec<Finding> {
        self.scan_inner(text, false)
    }

    /// Shared scan; `first_only` lets [`has_secret`](Scanner::has_secret) stop
    /// early. Findings are returned in ascending `start`.
    fn scan_inner(&self, text: &str, first_only: bool) -> Vec<Finding> {
        let mut findings = Vec::new();
        scan_lines(text, &mut findings, first_only);
        if first_only && !findings.is_empty() {
            return findings;
        }
        scan_tokens(text, &mut findings, first_only);
        findings.sort_by_key(|f| f.start);
        findings
    }

    /// Scan `text` and replace every detected secret span with
    /// [`REDACTION_MARKER`] (spec 07 §2 "REDACTION" step; 12 §2).
    ///
    /// Overlapping or touching findings — e.g. a long assigned quoted value that
    /// is *also* high-entropy (`token = "<48-char base64>"` matches both
    /// [`FindingKind::AssignedSecret`] and [`FindingKind::HighEntropy`] on the
    /// identical span) — are merged into one replaced range first, so the marker
    /// is inserted exactly once per secret rather than doubled or corrupting
    /// neighboring bytes. Merged ranges are then replaced in descending `start`
    /// order so replacing one range never invalidates the (still-ascending, still
    /// byte-valid) offsets of the ranges before it.
    pub fn redact(&self, text: &str) -> Redacted {
        let findings = self.scan(text);
        let merged = merge_spans(&findings);
        let mut out = text.to_string();
        for &(start, end) in merged.iter().rev() {
            out.replace_range(start..end, REDACTION_MARKER);
        }
        Redacted {
            text: out,
            findings: merged.len(),
        }
    }
}

/// Merge overlapping or touching ascending `[start, end)` spans (as returned by
/// [`Scanner::scan`]) into disjoint ranges, so a caller replacing them never
/// double-processes the same bytes.
fn merge_spans(findings: &[Finding]) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for f in findings {
        match merged.last_mut() {
            Some((_, last_end)) if f.start <= *last_end => {
                *last_end = (*last_end).max(f.end);
            }
            _ => merged.push((f.start, f.end)),
        }
    }
    merged
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner::new()
    }
}

/// Line-oriented rules: PEM private-key headers and assigned quoted secrets.
fn scan_lines(text: &str, out: &mut Vec<Finding>, first_only: bool) {
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let base = offset;
        offset += line.len();
        let trimmed = line.trim_end_matches(['\n', '\r']);

        if is_pem_private_key_line(trimmed) {
            out.push(Finding {
                kind: FindingKind::PrivateKey,
                start: base,
                end: base + trimmed.len(),
            });
            if first_only {
                return;
            }
            continue;
        }

        if let Some((start, end)) = assigned_secret_span(trimmed) {
            out.push(Finding {
                kind: FindingKind::AssignedSecret,
                start: base + start,
                end: base + end,
            });
            if first_only {
                return;
            }
        }
    }
}

/// A PEM private-key header, e.g. `-----BEGIN OPENSSH PRIVATE KEY-----`.
fn is_pem_private_key_line(line: &str) -> bool {
    line.contains("-----BEGIN") && line.contains("PRIVATE KEY")
}

/// Known credential prefixes and the minimum total token length that makes a match
/// plausible (short bare prefixes like `sk-` alone must not trip).
const CREDENTIAL_RULES: &[(&str, usize)] = &[
    ("AKIA", 20),
    ("ASIA", 20),
    ("ghp_", 40),
    ("gho_", 40),
    ("ghs_", 40),
    ("github_pat_", 40),
    ("xoxb-", 20),
    ("xoxp-", 20),
    ("xoxa-", 20),
    ("xoxs-", 20),
    ("sk-", 20),
];

/// Keys whose quoted assignment is treated as a hardcoded secret (matched
/// case-insensitively on a word boundary).
const ASSIGNMENT_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "secret_key",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "client_secret",
    "auth_token",
    "token",
    "private_key",
];

/// Minimum inner length of a quoted assigned value to be considered secret-like.
const ASSIGNED_VALUE_MIN: usize = 8;

/// If `line` assigns one of [`ASSIGNMENT_KEYS`] a quoted literal of at least
/// [`ASSIGNED_VALUE_MIN`] inner characters, return the value span (offsets into
/// `line`, excluding the quotes).
fn assigned_secret_span(line: &str) -> Option<(usize, usize)> {
    let lower = line.to_ascii_lowercase();
    let mut best: Option<usize> = None;
    for key in ASSIGNMENT_KEYS {
        if let Some(pos) = find_key_on_boundary(&lower, key) {
            // Prefer the earliest key so the reported span is stable.
            best = Some(best.map_or(pos + key.len(), |b| b.min(pos + key.len())));
        }
    }
    let after_key = best?;

    // Between the key and the value there must be an assignment operator.
    let bytes = line.as_bytes();
    let mut i = after_key;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || (bytes[i] != b'=' && bytes[i] != b':') {
        return None;
    }
    i += 1;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let quote = bytes[i];
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let value_start = i + 1;
    let mut j = value_start;
    while j < bytes.len() && bytes[j] != quote {
        j += 1;
    }
    if j >= bytes.len() {
        return None; // unterminated quote
    }
    if j - value_start >= ASSIGNED_VALUE_MIN {
        Some((value_start, j))
    } else {
        None
    }
}

/// Find `key` in `haystack` (already lowercased) not immediately flanked by
/// identifier characters, so `password` matches but `mypasswordx` does not.
fn find_key_on_boundary(haystack: &str, key: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(key) {
        let pos = from + rel;
        let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
        let after = pos + key.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(pos);
        }
        from = pos + key.len();
    }
    None
}

/// Token-oriented rules: known credential prefixes and high-entropy strings.
fn scan_tokens(text: &str, out: &mut Vec<Finding>, first_only: bool) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if !is_token_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_token_byte(bytes[i]) {
            i += 1;
        }
        let token = &text[start..i];

        if let Some(kind) = classify_token(token) {
            out.push(Finding {
                kind,
                start,
                end: i,
            });
            if first_only {
                return;
            }
        }
    }
}

/// Classify a single token as a credential or high-entropy secret, if either rule
/// matches (credential formats take precedence).
fn classify_token(token: &str) -> Option<FindingKind> {
    for (prefix, min_len) in CREDENTIAL_RULES {
        if token.len() >= *min_len && token.starts_with(prefix) {
            return Some(FindingKind::CredentialToken);
        }
    }
    if token.len() >= ENTROPY_MIN_LEN
        && looks_base64ish(token)
        && shannon_entropy_bits(token) >= ENTROPY_MIN_BITS
    {
        return Some(FindingKind::HighEntropy);
    }
    None
}

/// Byte that may appear inside a scanned token.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'=')
}

/// Byte that is part of a programming identifier (for key word-boundary checks).
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether every byte of `token` is a base64/base64url character (so hex-only and
/// mixed-punctuation tokens are excluded from the entropy rule's scope).
fn looks_base64ish(token: &str) -> bool {
    token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-'))
}

/// Shannon entropy of `token` in bits per character.
fn shannon_entropy_bits(token: &str) -> f64 {
    let mut counts = [0u32; 256];
    let len = token.len();
    for &b in token.as_bytes() {
        counts[b as usize] += 1;
    }
    let len_f = len as f64;
    let mut entropy = 0.0f64;
    for &c in &counts {
        if c == 0 {
            continue;
        }
        let p = f64::from(c) / len_f;
        entropy -= p * p.log2();
    }
    entropy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_stable_and_exposed() {
        assert_eq!(REDACTION_VERSION, 1);
        assert_eq!(Scanner::new().version(), REDACTION_VERSION);
        assert_eq!(Scanner::default().version(), REDACTION_VERSION);
    }

    #[test]
    fn detects_pem_private_key() {
        let s = Scanner::new();
        let text = "prefix\n-----BEGIN OPENSSH PRIVATE KEY-----\nbody\n";
        assert!(s.has_secret(text));
        let f = s.scan(text);
        assert!(f.iter().any(|f| f.kind == FindingKind::PrivateKey));
    }

    #[test]
    fn detects_known_credential_formats() {
        let s = Scanner::new();
        for cred in [
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_012345678901234567890123456789012345",
            "xoxb-0000000000-abcdefghijklmno",
        ] {
            assert!(s.has_secret(&format!("k = {cred}")), "missed {cred}");
        }
        // A bare short prefix is not a credential.
        assert!(!s.has_secret("let sk = 3;"));
        assert!(!s.has_secret("akia is a lake"));
    }

    #[test]
    fn detects_assigned_quoted_secret_but_not_unquoted_code() {
        let s = Scanner::new();
        assert!(s.has_secret("password = \"hunter2please\""));
        assert!(s.has_secret("api_key: 'abcdef123456'"));
        // Ordinary code: unquoted, and short quoted values are quiet.
        assert!(!s.has_secret("let password = user.password;"));
        assert!(!s.has_secret("token = \"abc\""));
        // Word boundary: a substring of a longer identifier is not a key.
        assert!(!s.has_secret("mypasswordfield = \"somevaluehere\""));
    }

    #[test]
    fn high_entropy_base64_flagged_hex_sha_not() {
        let s = Scanner::new();
        // 44-char base64 of 32 random-looking bytes: high entropy.
        let b64 = "aGVsbG9Xb3JsZERlYWRCZWVmQ2FmZUJhYmVMMzM3SHVudGVy";
        assert!(s.has_secret(&format!("key={b64}")));
        // 40-char hex git SHA: entropy ceiling 4.0 < threshold, so not flagged.
        let sha = "da39a3ee5e6b4b0d3255bfef95601890afd80709aa";
        assert!(!s.has_secret(&format!("rev = {sha}")));
    }

    #[test]
    fn clean_code_has_no_secret() {
        let s = Scanner::new();
        let code = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        assert!(!s.has_secret(code));
        assert!(s.scan(code).is_empty());
    }

    #[test]
    fn scan_reports_ascending_spans_into_input() {
        let s = Scanner::new();
        let text = "password = \"hunter2please\"\nAKIAIOSFODNN7EXAMPLE\n";
        let f = s.scan(text);
        assert!(f.len() >= 2, "expected two findings, got {f:?}");
        assert!(f.windows(2).all(|w| w[0].start <= w[1].start));
        // Spans index the input bytes.
        for finding in &f {
            assert!(finding.end <= text.len() && finding.start < finding.end);
        }
    }

    #[test]
    fn entropy_of_uniform_alphabet_is_maximal() {
        // Two symbols, equal counts → 1 bit/char.
        assert!((shannon_entropy_bits("abab") - 1.0).abs() < 1e-9);
    }

    // ---- `Scanner::redact` (T13-01) -----------------------------------------

    #[test]
    fn redact_replaces_every_finding_with_the_marker() {
        let s = Scanner::new();
        let text = "line one\npassword = \"hunter2please\"\nAKIAIOSFODNN7EXAMPLE\nline four";
        let out = s.redact(text);
        assert_eq!(out.findings, 2);
        assert!(!out.text.contains("hunter2please"));
        assert!(!out.text.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(out.text.matches(REDACTION_MARKER).count(), 2);
        // Unrelated surrounding text is untouched.
        assert!(out.text.contains("line one"));
        assert!(out.text.contains("line four"));
        assert!(out.text.contains("password = \""));
    }

    #[test]
    fn redact_leaves_clean_text_byte_identical() {
        let s = Scanner::new();
        let code = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let out = s.redact(code);
        assert_eq!(out.findings, 0);
        assert_eq!(out.text, code);
    }

    #[test]
    fn redact_output_never_contains_the_original_secret_substrings() {
        let s = Scanner::new();
        let b64 = "aGVsbG9Xb3JsZERlYWRCZWVmQ2FmZUJhYmVMMzM3SHVudGVy";
        let text = format!("key={b64}\nghp_012345678901234567890123456789012345\n");
        let out = s.redact(&text);
        assert!(!out.text.contains(b64));
        assert!(
            !out.text
                .contains("ghp_012345678901234567890123456789012345")
        );
        assert!(out.findings >= 2);
    }

    #[test]
    fn redact_merges_overlapping_findings_without_corruption() {
        // A long, base64-looking assigned value matches BOTH the line-based
        // `AssignedSecret` rule (quoted value after `token =`) and the
        // token-based `HighEntropy` rule (>=40 chars, high entropy) on the
        // identical span — a real, reachable overlap given the two rules run
        // as separate passes over the same bytes.
        let s = Scanner::new();
        let secret = "aGVsbG9Xb3JsZERlYWRCZWVmQ2FmZUJhYmVMMzM3SHVudGVy";
        assert!(secret.len() >= ENTROPY_MIN_LEN);
        let text = format!("token = \"{secret}\"\nafter");
        let out = s.redact(&text);
        // Exactly one marker for the overlapping pair, not two, and no corruption.
        assert_eq!(out.findings, 1);
        assert_eq!(out.text.matches(REDACTION_MARKER).count(), 1);
        assert!(!out.text.contains(secret));
        assert!(out.text.starts_with("token = \""));
        assert!(out.text.ends_with("\"\nafter"));
    }
}
