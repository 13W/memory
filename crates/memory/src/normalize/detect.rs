//! Which script a memory entry is written in — a pure, model-free decision
//! (ADR-0010 Decision 8) — T21-03.
//!
//! Translating every entry would spend ≈800 ms of local inference on each one,
//! including the ones with nothing to translate — which, in a typical store, is
//! most of them. This module is the short-circuit: given an entry's text it
//! answers whether that text is written in a non-Latin script at all, and it
//! does so with no model, no network, and no state.
//!
//! # Declared limitation: this distinguishes scripts, not languages
//!
//! **Latin-script non-English text (German, French, Spanish, Polish, Turkish,
//! …) is classified as English and therefore never normalized.** Closing that
//! would require either an n-gram language model (a new dependency and weights
//! in the distribution) or an LLM call on every entry and every query (which
//! destroys ADR-0010 Decision 8 — that already-English text costs zero
//! inference). Such text keeps today's behavior — it does not get worse.
//!
//! This is a decision recorded in advance, not a defect found later. It is
//! stated here, in ADR-0010's own Consequences section, and — from T21-08 — in
//! `local-rag doctor`, so a user reading any of the three finds the same answer.
//!
//! # How the count is taken
//!
//! 1. **Neutral noise is removed first.** Fenced and inline code, URLs, emails,
//!    paths, uuid/hex/base64-shaped tokens, and identifiers (`snake_case`,
//!    `Foo::bar`, `camelCase`, `call()`) look identical in every language; left
//!    in, they only dilute the ratio, and an entry that is *mostly* an
//!    identifier dump would read as English no matter what prose surrounds it.
//! 2. **NFD, combining marks dropped.** So a precomposed `é` and a decomposed
//!    `e`+`U+0301` are counted the same way, and marks — which belong to no
//!    script on their own — never enter the count.
//! 3. **Alphabetic characters are counted Latin vs non-Latin** against an
//!    explicit table of Unicode Latin blocks ([`is_latin`]). The table, rather
//!    than an ASCII test, is what makes `é`, `ü`, and `ł` Latin — including the
//!    letters NFD does not decompose at all (`ł`, `ø`, `đ`).
//!
//! Everything here is a pure function of its input: same text, same answer,
//! always.

use unicode_normalization::UnicodeNormalization;

/// Below this many alphabetic characters (after noise removal) the ratio is
/// decided by one or two words, which is not evidence about how an entry is
/// written — [`ScriptClass::Undetermined`] rather than a guess.
///
/// `[SPEC]`-chosen, not measured: eight is about two short words, low enough to
/// still judge a terse note ("починил баг в парсере") and high enough that a
/// bare `TODO` or a lone identifier never decides anything.
pub const MIN_ALPHABETIC_CHARS: usize = 8;

/// The share of non-Latin alphabetic characters at which an entry counts as
/// written in another script.
///
/// `[SPEC]`-chosen, not measured. A threshold rather than "contains at least
/// one": a Russian note quoting English terms and an English note quoting one
/// Russian word must land in different classes, and both are ordinary in this
/// product's own store. One third is comfortably above the incidental-quote
/// case and comfortably below any genuinely non-Latin prose, which in practice
/// scores far higher once identifiers are removed.
pub const NON_LATIN_RATIO: f64 = 0.30;

/// What script a text is written in, as far as a model-free detector can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptClass {
    /// Latin script — treated as English and never normalized. See the module
    /// doc's declared limitation: this covers Latin-script languages that are
    /// not English at all.
    English,
    /// Predominantly non-Latin: the only class worth spending inference on.
    NonLatin,
    /// Not enough alphabetic evidence to judge — an empty entry, a bare URL, a
    /// code fragment, a two-word note. Treated exactly like [`Self::English`]
    /// by every caller (nothing is translated), but kept distinct so a
    /// diagnostic surface can say "could not tell" instead of claiming
    /// "English".
    Undetermined,
}

/// The raw counts [`script_class`] decides on — exposed so a caller (or a
/// human reading `doctor`) can see *why* a text was classified the way it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScriptStats {
    /// Alphabetic characters in a Unicode Latin block.
    pub latin: usize,
    /// Alphabetic characters outside every Latin block.
    pub non_latin: usize,
}

impl ScriptStats {
    /// Alphabetic characters that entered the count at all.
    pub fn considered(&self) -> usize {
        self.latin + self.non_latin
    }

    /// The non-Latin share, `0.0` when nothing was counted.
    pub fn non_latin_ratio(&self) -> f64 {
        let considered = self.considered();
        if considered == 0 {
            return 0.0;
        }
        self.non_latin as f64 / considered as f64
    }
}

/// Count `text`'s alphabetic characters by script, after removing the neutral
/// noise described in the module doc.
pub fn script_stats(text: &str) -> ScriptStats {
    let prose = strip_neutral_noise(text);
    let mut stats = ScriptStats::default();
    for ch in prose.nfd() {
        if !ch.is_alphabetic() {
            continue;
        }
        if is_latin(ch) {
            stats.latin += 1;
        } else {
            stats.non_latin += 1;
        }
    }
    stats
}

/// Classify `text` (see [`ScriptClass`]).
pub fn script_class(text: &str) -> ScriptClass {
    let stats = script_stats(text);
    if stats.considered() < MIN_ALPHABETIC_CHARS {
        return ScriptClass::Undetermined;
    }
    if stats.non_latin_ratio() >= NON_LATIN_RATIO {
        ScriptClass::NonLatin
    } else {
        ScriptClass::English
    }
}

/// Whether `ch` belongs to a Unicode Latin block.
///
/// An explicit range table rather than a script database: it costs no
/// dependency, it is auditable in one screen, and it covers every Latin-script
/// language this detector will ever be asked about. Anything alphabetic outside
/// these ranges — Cyrillic, Greek, Han, Kana, Hangul, Arabic, Hebrew,
/// Devanagari, … — is non-Latin by construction, which is exactly the question
/// being asked.
fn is_latin(ch: char) -> bool {
    matches!(ch,
        'A'..='Z'                       // Basic Latin
        | 'a'..='z'
        | '\u{00C0}'..='\u{00FF}'       // Latin-1 Supplement (letters)
        | '\u{0100}'..='\u{017F}'       // Latin Extended-A  (ł, ő, š, ž, …)
        | '\u{0180}'..='\u{024F}'       // Latin Extended-B  (ƀ, ǝ, ș, ț, …)
        | '\u{1E00}'..='\u{1EFF}'       // Latin Extended Additional (ạ, ế, ỹ, …)
    )
}

/// Replace neutral, language-independent spans with a space.
///
/// Order matters: fenced blocks first (they may contain anything at all),
/// then inline code, then whitespace-delimited tokens that are structurally
/// identifiers rather than words. Replacing with a space — never deleting —
/// keeps neighbouring words from fusing into one.
fn strip_neutral_noise(text: &str) -> String {
    let without_fences = strip_fenced_code(text);
    let without_inline = strip_inline_code(&without_fences);
    without_inline
        .split_whitespace()
        .filter(|token| !is_neutral_token(token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drop ```` ``` ````-fenced spans. An unterminated fence swallows the rest of
/// the text, which is the same thing every Markdown renderer does and the safe
/// reading here: the tail really is code.
fn strip_fenced_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(' ');
            continue;
        }
        if !in_fence {
            out.push_str(line);
        }
        out.push(' ');
    }
    out
}

/// Drop `` `inline` `` spans. An unmatched backtick is left alone — treating
/// the remainder of an entry as code because of one stray tick would throw
/// away real prose.
fn strip_inline_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push(' ');
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether a whitespace-delimited token carries no language signal.
fn is_neutral_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '/');
    if trimmed.is_empty() {
        return true;
    }
    is_url_or_email(trimmed)
        || is_path(trimmed)
        || is_machine_token(trimmed)
        || is_identifier(token)
        || is_code_shaped(token)
}

/// Structurally a fragment of code rather than a word: it carries a statement
/// terminator, a brace, or a complete call's parentheses.
///
/// Both parentheses are required, so prose that merely opens one — `(см.`,
/// `(see` — keeps its word; a token holding both is `println!("{x}")`, not a
/// sentence. This is what makes a pure-code entry `Undetermined` instead of
/// English: `fn`, `let` and a variable name are the only word-shaped tokens
/// left, and three of them are not evidence about any language.
fn is_code_shaped(token: &str) -> bool {
    token.contains(';')
        || token.contains('{')
        || token.contains('}')
        || (token.contains('(') && token.contains(')'))
}

fn is_url_or_email(token: &str) -> bool {
    token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("www.")
        || token.contains("://")
        || (token.contains('@') && token.contains('.'))
}

/// A filesystem-ish path: a slash between two non-empty segments, or a leading
/// `/`/`./`/`~/`. A bare `and/or` is deliberately *not* a path — it has no
/// second slash and no leading marker — because it is a word.
fn is_path(token: &str) -> bool {
    if token.starts_with('/') || token.starts_with("./") || token.starts_with("~/") {
        return true;
    }
    token.matches('/').count() >= 2
}

/// uuid/hex/base64-shaped: a long run of characters with no vowel-and-consonant
/// structure to speak of. Judged structurally, never by dictionary: at least
/// twelve characters, all alphanumeric-or-`-`, and either mostly digits or a
/// hex/uuid layout.
fn is_machine_token(token: &str) -> bool {
    const MIN_LEN: usize = 12;
    if token.chars().count() < MIN_LEN {
        return false;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '+' || c == '=' || c == '_')
    {
        return false;
    }
    let digits = token.chars().filter(char::is_ascii_digit).count();
    let hexish = token
        .chars()
        .filter(|c| c.is_ascii_hexdigit() || *c == '-')
        .count();
    let len = token.chars().count();
    // A uuid is all hex+dashes; a base64 blob is long, mixed-case and
    // digit-heavy; a long English word is neither.
    hexish == len || digits * 4 >= len
}

/// Structurally an identifier rather than a word: `snake_case`, `Foo::bar`,
/// `camelCase`, `call()`, `a.b.c`.
fn is_identifier(token: &str) -> bool {
    let core = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if core.is_empty() {
        return false;
    }
    if token.contains("::") || token.contains("()") {
        return true;
    }
    if core.contains('_') {
        return true;
    }
    // `a.b.c` — dotted, and not a sentence-ending word like `конец.`
    if core.contains('.') && !core.ends_with('.') {
        return true;
    }
    has_internal_capital(core)
}

/// A capital letter after a lowercase one — `camelCase`, `RunOnce`, but not
/// `Начало` or `The`.
fn has_internal_capital(token: &str) -> bool {
    let mut seen_lower = false;
    for ch in token.chars() {
        if ch.is_lowercase() {
            seen_lower = true;
        } else if ch.is_uppercase() && seen_lower {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expectation table, reproduced one-to-one by the test below: every
    /// row is `(name, input, expected)`, and the name says what the row is
    /// there to pin.
    const CASES: &[(&str, &str, ScriptClass)] = &[
        // --- ordinary prose, both scripts ---------------------------------
        (
            "russian prose",
            "Для фьюжна поиска остановились на RRF вместо линейной комбинации весов",
            ScriptClass::NonLatin,
        ),
        (
            "english prose",
            "We settled on reciprocal rank fusion instead of a weighted linear combination",
            ScriptClass::English,
        ),
        (
            "russian prose with english terms quoted",
            "Флейковость в CI объяснилась не гонкой в тестах, а тем, что SQLite держал WAL",
            ScriptClass::NonLatin,
        ),
        (
            "english prose with one russian word",
            "The consolidation runner keeps retrying the same window, a настоящий retry storm, \
             until the fingerprint dead-letter fires and the run is parked for good",
            ScriptClass::English,
        ),
        // --- code-switching: prose plus machinery -------------------------
        (
            "russian prose with identifiers and a path",
            "Правил apply_run в crates/store/src/memory/runner.rs, там дедуп цитат",
            ScriptClass::NonLatin,
        ),
        (
            "english prose with identifiers and a path",
            "Fixed apply_run in crates/store/src/memory/runner.rs by deduplicating citations",
            ScriptClass::English,
        ),
        (
            "russian prose around a fenced code block",
            "Вот итоговый запрос, он же и есть исправление:\n```sql\nSELECT run_id, state FROM \
             consolidation_run WHERE state = 'failed'\n```\nбольше ничего не менялось",
            ScriptClass::NonLatin,
        ),
        (
            "english prose around inline code",
            "The `stale_runs` reader excludes a `mechanical` failure whose fingerprint matches",
            ScriptClass::English,
        ),
        // --- no language signal at all ------------------------------------
        ("empty", "", ScriptClass::Undetermined),
        ("whitespace only", "   \n\t  ", ScriptClass::Undetermined),
        (
            "pure code, no prose",
            "fn main() { let x = compute_value(42); println!(\"{x}\"); }",
            ScriptClass::Undetermined,
        ),
        (
            "fenced code only",
            "```rust\nlet mut stats = ScriptStats::default();\n```",
            ScriptClass::Undetermined,
        ),
        (
            "url only",
            "https://example.com/some/deep/path?query=1",
            ScriptClass::Undetermined,
        ),
        (
            "uuid only",
            "01a01648-42b3-797d-bebb-3e6cba2bf7a5",
            ScriptClass::Undetermined,
        ),
        (
            "hex digest only",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            ScriptClass::Undetermined,
        ),
        (
            "path only",
            "crates/store/src/memory/normalization.rs",
            ScriptClass::Undetermined,
        ),
        (
            "email only",
            "somebody@example.com",
            ScriptClass::Undetermined,
        ),
        (
            "digits and punctuation",
            "42 — 17 = 25!",
            ScriptClass::Undetermined,
        ),
        ("emoji only", "🎉🚀✅", ScriptClass::Undetermined),
        // --- shorter than the alphabetic floor ----------------------------
        ("one short russian word", "баг", ScriptClass::Undetermined),
        ("one short english word", "bug", ScriptClass::Undetermined),
        (
            "two short russian words",
            "починил баг",
            ScriptClass::NonLatin,
        ),
        // --- the declared limitation: scripts, not languages --------------
        (
            "german is Latin, so English (declared limitation)",
            "Die Konsolidierung wiederholte denselben Fehler stundenlang ohne Fortschritt",
            ScriptClass::English,
        ),
        (
            "french is Latin, so English (declared limitation)",
            "La consolidation répétait la même erreur pendant des heures sans progresser",
            ScriptClass::English,
        ),
        (
            "polish is Latin, so English (declared limitation)",
            "Konsolidacja powtarzała ten sam błąd godzinami bez żadnego postępu",
            ScriptClass::English,
        ),
        (
            "turkish is Latin, so English (declared limitation)",
            "Birleştirme aynı hatayı saatlerce hiçbir ilerleme olmadan tekrarladı",
            ScriptClass::English,
        ),
        (
            "vietnamese is Latin, so English (declared limitation)",
            "Việc hợp nhất lặp lại cùng một lỗi trong nhiều giờ mà không có tiến triển",
            ScriptClass::English,
        ),
        // --- other scripts -------------------------------------------------
        (
            "chinese",
            "合并操作连续数小时重复同一个错误，没有任何进展",
            ScriptClass::NonLatin,
        ),
        (
            "japanese",
            "統合処理は何時間も同じエラーを繰り返し、まったく進展がありませんでした",
            ScriptClass::NonLatin,
        ),
        (
            "korean",
            "통합 작업이 몇 시간 동안 같은 오류를 반복했고 아무런 진전이 없었습니다",
            ScriptClass::NonLatin,
        ),
        (
            "arabic",
            "كرر الدمج نفس الخطأ لساعات دون أي تقدم يذكر في المعالجة",
            ScriptClass::NonLatin,
        ),
        (
            "hebrew",
            "האיחוד חזר על אותה שגיאה במשך שעות בלי שום התקדמות בעיבוד",
            ScriptClass::NonLatin,
        ),
        (
            "greek",
            "Η ενοποίηση επαναλάμβανε το ίδιο σφάλμα για ώρες χωρίς καμία πρόοδο",
            ScriptClass::NonLatin,
        ),
        (
            "mixed scripts, russian dominant",
            "Проверил на живом сторе: run 01a01648 applied с первой попытки, дублей нет",
            ScriptClass::NonLatin,
        ),
    ];

    #[test]
    fn the_expectation_table_holds_row_by_row() {
        assert!(
            CASES.len() >= 30,
            "the card asks for at least 30 rows, found {}",
            CASES.len()
        );
        for (name, input, expected) in CASES {
            assert_eq!(
                script_class(input),
                *expected,
                "case: {name} (stats: {:?})",
                script_stats(input),
            );
        }
    }

    #[test]
    fn classification_is_a_pure_function_of_its_input() {
        for (name, input, _) in CASES {
            assert_eq!(
                script_class(input),
                script_class(input),
                "case: {name} — same input, same answer",
            );
        }
    }

    /// `MIN_ALPHABETIC_CHARS` is a real boundary, tested from both sides with
    /// text that differs by exactly one letter.
    #[test]
    fn min_alphabetic_chars_is_the_floor_for_judging_at_all() {
        let just_under: String = "я".repeat(MIN_ALPHABETIC_CHARS - 1);
        let exactly: String = "я".repeat(MIN_ALPHABETIC_CHARS);
        assert_eq!(
            script_stats(&just_under).considered(),
            MIN_ALPHABETIC_CHARS - 1
        );
        assert_eq!(script_class(&just_under), ScriptClass::Undetermined);
        assert_eq!(script_stats(&exactly).considered(), MIN_ALPHABETIC_CHARS);
        assert_eq!(script_class(&exactly), ScriptClass::NonLatin);
    }

    /// `NON_LATIN_RATIO` likewise: 30 % non-Latin is non-Latin, 29 % is not.
    #[test]
    fn non_latin_ratio_is_the_threshold_between_the_two_scripts() {
        // 30 of 100 alphabetic characters non-Latin — exactly at the threshold.
        let at_threshold = format!("{}{}", "я".repeat(30), "a".repeat(70));
        let stats = script_stats(&at_threshold);
        assert_eq!((stats.considered(), stats.non_latin), (100, 30));
        assert!((stats.non_latin_ratio() - NON_LATIN_RATIO).abs() < 1e-9);
        assert_eq!(script_class(&at_threshold), ScriptClass::NonLatin);

        let just_under = format!("{}{}", "я".repeat(29), "a".repeat(71));
        assert_eq!(script_class(&just_under), ScriptClass::English);
    }

    /// Latin diacritics are Latin — including the letters NFD leaves alone.
    #[test]
    fn latin_diacritics_count_as_latin() {
        for text in [
            "café société française naïve",
            "Grüße über größere Änderungen",
            "źle złożone łańcuchy słów",
            "đường ø forsøg ærlig",
        ] {
            let stats = script_stats(text);
            assert_eq!(
                stats.non_latin, 0,
                "{text:?} must be all-Latin, got {stats:?}",
            );
            assert_eq!(script_class(text), ScriptClass::English, "{text:?}");
        }
    }

    /// Precomposed and decomposed spellings of the same word must classify
    /// identically — that is what the NFD step buys.
    #[test]
    fn precomposed_and_decomposed_forms_agree() {
        let precomposed = "café société naïve über";
        let decomposed: String = precomposed.nfd().collect();
        assert_ne!(precomposed, decomposed.as_str(), "the fixture must differ");
        assert_eq!(script_stats(precomposed), script_stats(&decomposed));
        assert_eq!(script_class(precomposed), script_class(&decomposed));
    }

    /// The noise removal is what makes a code-switched note readable: without
    /// it, an identifier dump would drag Russian prose toward English.
    #[test]
    fn identifiers_and_paths_do_not_vote() {
        let with_machinery = "Правил apply_run и upsert_normalization в \
             crates/store/src/memory/normalization.rs";
        let prose_only = "Правил и в";
        assert_eq!(
            script_stats(with_machinery).latin,
            script_stats(prose_only).latin,
            "no Latin character may survive noise removal here",
        );
    }
}
