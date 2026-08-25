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
//! # Rule set v3 (`[SPEC]`, conservative and dependency-free)
//!
//! Detection is deterministic byte/line scanning — no regex engine — so the crate
//! takes on no new dependency and every verdict is stable across platforms:
//!
//! 1. **PEM private keys** — a line that **begins** with `-----BEGIN` (after
//!    leading whitespace) and also bears `PRIVATE KEY`.
//! 2. **Known credential formats** — a token, **or any of its `= + /`-separated
//!    parts** ([`CREDENTIAL_GLUE`], D-099), with a recognized prefix and a
//!    plausible minimum length (AWS `AKIA`/`ASIA`, GitHub `ghp_`/`gho_`/`ghs_`/
//!    `github_pat_`, Slack `xox[bpas]-`, OpenAI-style `sk-`).
//! 3. **Assigned secrets** — a `password`/`secret`/`api_key`/`token`/… key
//!    assigned a **quoted** literal of non-trivial length (an unquoted value such
//!    as `let token = get()` is not flagged, keeping ordinary code quiet) whose
//!    value is not self-evidently a label rather than a secret (see
//!    [`assigned_value_is_secret`]).
//! 4. **High-entropy strings** — a long token (≥ [`ENTROPY_MIN_LEN`]) whose Shannon
//!    entropy per character is ≥ [`ENTROPY_MIN_BITS`], which mixes character
//!    classes, is not inside a URL, and is not a subresource-integrity digest. The
//!    threshold sits above a hex digest's ceiling (log2 16 = 4.0) so git SHAs and
//!    hex checksums do **not** trip it, while base64-encoded key material
//!    (≈5.5–6.0 bits/char) does.
//!
//! # What D-097 changed, and why it had to
//!
//! Rule set v1 promised "a low false-positive rate on real source" and did not
//! deliver it. Measured on the owner's store, of the 43 files skipped
//! `reason='secret'` in one real repository **32 were false**, and because a
//! flagged file is dropped whole (spec 12 §5 `[FIXED]`) each one silently deleted
//! working source from the code index — a support-article URL in a comment cost
//! the entire iManage integration client. The four mechanisms were:
//!
//! - a **URL** in a comment: [`is_token_byte`] counts `/ - + =` as token bytes, so
//!   any URL path ≥ [`ENTROPY_MIN_LEN`] cleared the entropy bar (13 files);
//! - a long **camelCase identifier**, for the same reason
//!   ([`looks_base64ish`] accepts any alphanumeric run) (3 files);
//! - [`assigned_secret_span`] firing on test fixtures and enum values —
//!   `const CLIENT_SECRET = 'test-client-secret'`, `PASSWORD: 'password'`, even
//!   `errors.password = 'Password is required'` (16 files);
//! - a file holding `'-----BEGIN PRIVATE KEY-----'` as a **detector constant**,
//!   because the PEM rule matched the substring anywhere on the line (1 file).
//!
//! The narrowings below are each measured against that corpus rather than
//! reasoned about: they take it from 43 flagged to 11, and every one of the 11 is
//! genuine base64 blob or key-shaped material. No true positive in the adversarial
//! corpus is lost — `adversarial.redaction.*` in `fixtures/adversarial/index.json`
//! holds both halves, and `crates/core/tests/redaction_corpus.rs` gates on them.
//!
//! The set stays intentionally conservative: it aims to catch obvious committed
//! secrets with a low false-positive rate on real source, and is expected to grow
//! (each growth bumps [`REDACTION_VERSION`]).

/// The rule-set version stamped on every verdict (spec 12 §2 `[SPEC]`).
///
/// Recorded by spool/remote flows in their envelopes so a redaction verdict is
/// reproducible against the rules that produced it. Any change to the rule set
/// below MUST increment this.
pub const REDACTION_VERSION: u32 = 3;

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
        // D-097: computed once for the whole text, not per token — the entropy
        // rule needs to know whether a token sits inside a URL, and a URL's own
        // path segments are the single largest source of false positives.
        let urls = url_spans(text);
        scan_tokens(text, &urls, &mut findings, first_only);
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
///
/// The header must **begin** the line (leading whitespace allowed, so an indented
/// key inside YAML or a heredoc still matches). D-097: matching the substring
/// anywhere on the line made a file that merely *names* the header — a detector
/// constant such as `const PRIVATE_KEY_START = '-----BEGIN PRIVATE KEY-----'` —
/// indistinguishable from a file that contains a key. A key actually pasted into
/// a string literal is still caught: its base64 body trips the entropy rule, which
/// is where that detection belongs.
fn is_pem_private_key_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("-----BEGIN") && trimmed.contains("PRIVATE KEY")
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

/// Substrings that mark a value as a stand-in rather than a credential (D-097).
///
/// Matched case-insensitively anywhere in the value: a placeholder is a
/// placeholder wherever it appears (`test-client-secret`, `password = 'CHANGEME'`,
/// `integration-test-password`).
const PLACEHOLDER_MARKERS: &[&str] = &[
    "test",
    "example",
    "dummy",
    "fake",
    "sample",
    "placeholder",
    "changeme",
    "xxx",
];

/// Words that make a value a **label for** a secret rather than a secret (D-097).
///
/// This is the rule that does the most work, and it is not a heuristic about test
/// files — it is a statement about what secrets look like: real credential
/// material does not contain the word "secret". A value that names itself
/// (`'a-completely-different-secret'`, `'top-secret-1234567890'`,
/// `'session-token-1'`) is documentation, a fixture, or an enum member.
///
/// Deliberately excludes `key`: it is a substring of ordinary words (`monkey`),
/// and a rule that rejects `password = "monkey123"` would lose a real secret to
/// save a rare false positive. Every entry here is long enough not to collide.
const SECRET_VOCABULARY: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "credential",
];

/// Whether an assigned quoted `value` for `key` is plausibly a real secret
/// (D-097).
///
/// Four ways a value proves it is not, each measured against the live corpus:
///
/// 1. it **is** the key, modulo case and separators — `CLIENT_SECRET =
///    'clientSecret'`, `PASSWORD: 'password'`: an enum member or a field name;
/// 2. it carries a [`PLACEHOLDER_MARKERS`] substring — `'test-client-secret'`;
/// 3. it carries a [`SECRET_VOCABULARY`] word — see that constant for why a value
///    that names itself is a label;
/// 4. it reads as prose — a space plus an initial capital, which is an error
///    message (`errors.password = 'Password is required'`), not a credential.
///
/// A strong password (`"Xk7#mQ2vLp9!zR4t"`), a hex key
/// (`"9f3b1c7e5a2d8046b1e4c9f2a7d3e1b5"`) and a passphrase
/// (`"correct-horse-battery-staple"`) pass all four and stay flagged.
fn assigned_value_is_secret(value: &str, key: &str) -> bool {
    if squash_identifier(value) == squash_identifier(key) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if PLACEHOLDER_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }
    if SECRET_VOCABULARY.iter().any(|w| lower.contains(w)) {
        return false;
    }
    if value.contains(' ') && value.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return false;
    }
    true
}

/// `value` reduced to its lowercase alphanumerics, so `CLIENT_SECRET`,
/// `clientSecret` and `client-secret` compare equal.
fn squash_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// If `line` assigns one of [`ASSIGNMENT_KEYS`] a quoted literal of at least
/// [`ASSIGNED_VALUE_MIN`] inner characters that [`assigned_value_is_secret`]
/// accepts, return the value span (offsets into `line`, excluding the quotes).
fn assigned_secret_span(line: &str) -> Option<(usize, usize)> {
    let lower = line.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for key in ASSIGNMENT_KEYS {
        if let Some(pos) = find_key_on_boundary(&lower, key) {
            // Prefer the earliest key so the reported span is stable.
            let end = pos + key.len();
            if best.is_none_or(|(b, _)| end < b) {
                best = Some((end, key));
            }
        }
    }
    let (after_key, matched_key) = best?;

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
    if j - value_start < ASSIGNED_VALUE_MIN {
        return None;
    }
    if !assigned_value_is_secret(&line[value_start..j], matched_key) {
        return None;
    }
    Some((value_start, j))
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
///
/// `urls` are the byte spans of URLs found in `text` ([`url_spans`]); a token
/// wholly inside one is exempt from the entropy rule but **not** from the
/// credential-format rule (D-097).
fn scan_tokens(text: &str, urls: &[(usize, usize)], out: &mut Vec<Finding>, first_only: bool) {
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
        let inside_url = urls.iter().any(|&(s, e)| start >= s && i <= e);

        if let Some(kind) = classify_token(token, inside_url) {
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

/// Subresource-integrity digest prefixes (D-097).
///
/// `sha512-<base64>` in a lockfile or an `integrity` attribute is a **public**
/// content digest whose whole purpose is to be published. It is base64 of a hash,
/// so it is indistinguishable from key material by shape alone — the prefix is
/// the only signal, and it is exact.
const INTEGRITY_PREFIXES: &[&str] = &["sha512-", "sha384-", "sha256-", "sha1-"];

/// Classify a single token as a credential or high-entropy secret, if either rule
/// matches (credential formats take precedence).
///
/// `inside_url` exempts the token from the **entropy** rule only. That asymmetry
/// is deliberate: a URL's path segments are opaque high-entropy strings by design
/// (a Google Docs id, a support-article slug, a commit sha) and were the single
/// largest false-positive source measured in D-097, while a `ghp_…` token pasted
/// into a URL is exactly as leaked as one pasted anywhere else.
fn classify_token(token: &str, inside_url: bool) -> Option<FindingKind> {
    if has_credential_prefix(token) {
        return Some(FindingKind::CredentialToken);
    }
    if inside_url {
        return None;
    }
    if INTEGRITY_PREFIXES.iter().any(|p| token.starts_with(p)) {
        return None;
    }
    if token.len() >= ENTROPY_MIN_LEN
        && looks_base64ish(token)
        && has_mixed_character_classes(token)
        && shannon_entropy_bits(token) >= ENTROPY_MIN_BITS
    {
        return Some(FindingKind::HighEntropy);
    }
    None
}

/// Bytes that may glue a credential to something else inside one scanned token
/// (D-099).
///
/// [`is_token_byte`] accepts `= + /`, so `?k=ghp_…` in a URL query string is a
/// **single** token whose prefix check fails — the most ordinary shape of a
/// leaked token went undetected by every rule. Splitting on exactly these three
/// is safe by construction: every format in [`CREDENTIAL_RULES`] is drawn from
/// `[A-Za-z0-9_-]`, so none of them can be torn apart by the split.
///
/// Deliberately applied to the credential rule **only**. Feeding the parts to the
/// entropy rule would split base64 on its own `/` and `=` and manufacture exactly
/// the false positives D-097 had to measure away.
const CREDENTIAL_GLUE: &[char] = &['=', '+', '/'];

/// Whether `token`, or any of its [`CREDENTIAL_GLUE`]-separated parts, is a known
/// credential format at a plausible length (D-099).
fn has_credential_prefix(token: &str) -> bool {
    token.split(CREDENTIAL_GLUE).any(|part| {
        CREDENTIAL_RULES
            .iter()
            .any(|(prefix, min_len)| part.len() >= *min_len && part.starts_with(prefix))
    })
}

/// Whether `token` mixes digits, lowercase and uppercase (D-097).
///
/// Base64 of random bytes contains all three with overwhelming probability at
/// [`ENTROPY_MIN_LEN`] characters; a 40+ character run that is missing an entire
/// class is a name or a path, not key material. This is what stops
/// `selectRowIdsOfUploadingFilesForLinkedCheckboxColumns` (no digits) and
/// `migrations/v18/2026-05-27-l2-7509-signing-document-revisions` (no uppercase)
/// from being read as secrets — both were measured, both cost a whole file.
fn has_mixed_character_classes(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.iter().any(u8::is_ascii_digit)
        && bytes.iter().any(u8::is_ascii_lowercase)
        && bytes.iter().any(u8::is_ascii_uppercase)
}

/// Byte spans of every URL in `text`, located by its `://` (D-097).
///
/// A span runs from the start of the scheme back through `[A-Za-z0-9+.-]` to the
/// first byte that cannot terminate a URL — whitespace, a quote, a backtick, or a
/// bracket. Offsets are only ever **compared**, never used to slice, so a
/// multi-byte character inside a URL cannot produce an invalid boundary.
fn url_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find("://") {
        let marker = from + rel;
        let mut start = marker;
        while start > 0 && is_scheme_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = marker + 3;
        while end < bytes.len() && !is_url_terminator(bytes[end]) {
            end += 1;
        }
        spans.push((start, end));
        from = end.max(marker + 3);
    }
    spans
}

/// Byte that may appear in a URL scheme (`https`, `git+ssh`, `x-custom.1`).
fn is_scheme_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')
}

/// Byte that ends a URL in source text.
fn is_url_terminator(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'`' | b'<' | b'>' | b')')
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

    // -----------------------------------------------------------------
    // D-097: one unit case per narrowing, against the helper it narrowed
    // -----------------------------------------------------------------

    #[test]
    fn a_credential_is_found_however_it_is_glued_to_its_neighbour() {
        let s = Scanner::new();
        let token = "ghp_012345678901234567890123456789012345";

        // The shape that had no detection at all: a query-string parameter.
        assert!(s.has_secret(&format!("const u = \"https://x.example/cb?k={token}\";")));
        assert!(s.has_secret(&format!("fetch(`/api?token={token}&page=2`)")));
        // The other two glue bytes `is_token_byte` accepts.
        assert!(s.has_secret(&format!("const u = \"https://x.example/{token}\";")));
        assert!(s.has_secret(&format!("const u = \"a+{token}\";")));
        // And still, unglued.
        assert!(s.has_secret(&format!("token = \"{token}\"")));

        // Splitting is for the credential rule only: a base64 blob must not be
        // chopped on its own `/` and `=` and re-judged part by part, which is how
        // the entropy false positives D-097 measured away would come back.
        assert!(!has_credential_prefix(
            "w+YQ0eVUHNzQ2zp/u8Ip1aRsaJQ2sgWzAS5umnXA7JA="
        ));
        // A prefix below its plausible length is still not a credential.
        assert!(!has_credential_prefix("?k=ghp_short"));
        assert!(!has_credential_prefix("sk-"));
    }

    #[test]
    fn the_pem_rule_needs_the_header_to_begin_the_line() {
        assert!(is_pem_private_key_line(
            "-----BEGIN OPENSSH PRIVATE KEY-----"
        ));
        // Indented is still a header: keys are indented in YAML and heredocs.
        assert!(is_pem_private_key_line(
            "\t  -----BEGIN RSA PRIVATE KEY-----"
        ));
        // Naming the header is not holding a key.
        assert!(!is_pem_private_key_line(
            "const PRIVATE_KEY_START = '-----BEGIN PRIVATE KEY-----'"
        ));
        // Beginning with the marker is not enough on its own.
        assert!(!is_pem_private_key_line("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn the_entropy_rule_needs_all_three_character_classes() {
        assert!(has_mixed_character_classes("aB3"));
        // A long identifier: no digits.
        assert!(!has_mixed_character_classes(
            "selectRowIdsOfUploadingFilesForLinkedCheckboxColumns"
        ));
        // A dated path literal: no uppercase.
        assert!(!has_mixed_character_classes(
            "migrations/v18/2026-05-27-l2-7509-signing-document-revisions"
        ));
        // A digit-only run: no letters at all.
        assert!(!has_mixed_character_classes(
            "012345678901234567890123456789"
        ));

        let s = Scanner::new();
        assert!(!s.has_secret(
            "export const selectRowIdsOfUploadingFilesForLinkedCheckboxColumns = () => {}"
        ));
        assert!(s.has_secret(
            "const k = \"TWFuIGlzIGRpc3Rpbmd1aXNoZWQsIG5vdCBvbmx5IGJ5IGhpcyByZWFzb24sIDk5\";"
        ));
    }

    #[test]
    fn url_spans_cover_the_whole_url_and_stop_at_its_delimiters() {
        let text = "// see https://example.com/a/b?c=d and more";
        let spans = url_spans(text);
        assert_eq!(spans.len(), 1);
        let (s, e) = spans[0];
        assert_eq!(&text[s..e], "https://example.com/a/b?c=d");

        // A quoted URL ends at the quote, not at the end of the literal.
        let quoted = "const u = \"https://x.example/p\"; // tail";
        let (s, e) = url_spans(quoted)[0];
        assert_eq!(&quoted[s..e], "https://x.example/p");

        // Two URLs on one line are two spans.
        assert_eq!(url_spans("a http://x.dev/1 b https://y.dev/2").len(), 2);
        // No scheme, no span — a bare path is left to the other guards.
        assert!(url_spans("migrations/v18/a-b-c").is_empty());
        // A multi-byte character inside a URL must not produce a bad boundary.
        let unicode = "// см. https://пример.рф/путь/страница\n";
        let (s, e) = url_spans(unicode)[0];
        assert!(unicode.is_char_boundary(s) && unicode.is_char_boundary(e));
    }

    #[test]
    fn a_url_exempts_entropy_but_never_a_credential_format() {
        let s = Scanner::new();
        // The opaque id is high-entropy and mixed-class; only the URL tells it
        // apart from key material.
        assert!(!s.has_secret(
            "// https://docs.example.com/d/1ntEj7HYd9kfQ8ePut6_7koE_AQwnn9GGOX0DDWSY/edit"
        ));
        // The same id outside a URL is still a secret.
        assert!(s.has_secret("const k = \"1ntEj7HYd9kfQ8ePut6_7koE_AQwnn9GGOX0DDWSYq\";"));
        // A recognized credential format is never exempted by its surroundings.
        assert!(s.has_secret(
            "const u = \"https://x.example/cb#ghp_012345678901234567890123456789012345\";"
        ));
    }

    #[test]
    fn subresource_integrity_digests_are_not_secrets() {
        let s = Scanner::new();
        assert!(!s.has_secret(
            "\"integrity\": \"sha512-oLDq3jw7AcLqKWH2AhCpVTZl8mf6X2YReP+Neh0SJUzV/BdZYjth9\""
        ));
        // The same bytes without the integrity prefix are key-shaped again.
        assert!(s.has_secret("\"k\": \"oLDq3jw7AcLqKWH2AhCpVTZl8mf6X2YReP+Neh0SJUzV/BdZYjth9\""));
    }

    #[test]
    fn an_assigned_value_that_describes_itself_is_a_label_not_a_secret() {
        // 1. the value is the key.
        assert!(!assigned_value_is_secret("clientSecret", "client_secret"));
        assert!(!assigned_value_is_secret("password", "password"));
        // 2. a placeholder marker anywhere in it.
        assert!(!assigned_value_is_secret(
            "test-client-secret",
            "client_secret"
        ));
        assert!(!assigned_value_is_secret("CHANGEME-please", "password"));
        // 3. it names itself.
        assert!(!assigned_value_is_secret(
            "a-completely-different-secret",
            "secret"
        ));
        assert!(!assigned_value_is_secret("session-token-1", "token"));
        // 4. it reads as prose — a space AND an initial capital. The capital is
        // load-bearing: a lowercase multi-word value is a passphrase, and
        // rejecting on the space alone would lose it.
        assert!(!assigned_value_is_secret(
            "Password is required",
            "password"
        ));
        assert!(!assigned_value_is_secret("Enter a valid value", "token"));
        assert!(assigned_value_is_secret("open sesame now", "password"));

        // And the ones that must survive all four.
        assert!(assigned_value_is_secret("Xk7#mQ2vLp9!zR4t", "password"));
        assert!(assigned_value_is_secret(
            "9f3b1c7e5a2d8046b1e4c9f2a7d3e1b5",
            "api_key"
        ));
        assert!(
            assigned_value_is_secret("correct-horse-battery-staple", "password"),
            "a word-shaped passphrase is a real secret; word shape alone must not reject",
        );
        // `key` is deliberately absent from the vocabulary: it is a substring of
        // ordinary words, and rejecting this would lose a real password.
        assert!(assigned_value_is_secret("monkey123", "password"));
    }

    #[test]
    fn the_assigned_rule_still_fires_on_real_hardcoded_credentials() {
        let s = Scanner::new();
        assert!(s.has_secret("const password = \"Xk7#mQ2vLp9!zR4t\";"));
        assert!(s.has_secret("api_key: \"9f3b1c7e5a2d8046b1e4c9f2a7d3e1b5\""));
        assert!(s.has_secret("password = \"correct-horse-battery-staple\""));
        // And stays quiet on the shapes D-097 measured as false.
        assert!(!s.has_secret("  CLIENT_SECRET = 'clientSecret',"));
        assert!(!s.has_secret("  PASSWORD: 'password',"));
        assert!(!s.has_secret("errors.password = 'Password is required'"));
    }

    #[test]
    fn version_is_stable_and_exposed() {
        // D-097 narrowed four rules and D-099 widened one, so the stamp on every
        // verdict must move —
        // consumers record it precisely so an old verdict stays auditable against
        // the rules that produced it (spec 12 §2 `[SPEC]`).
        assert_eq!(REDACTION_VERSION, 3);
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
