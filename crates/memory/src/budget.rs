//! One derivation of how a router prompt is allowed to spend the model's
//! context (`D-125`, `T23-04`).
//!
//! Every number below was measured on 2026-08-31 with the shipped model's own
//! tokenizer (`gemma-4-e2b-it`, `context_length = 32_768`) against the owner's
//! live store, and the measurement is reproducible:
//! `crates/local-rag/tests/prompt_budget_live.rs`. It is quoted here rather
//! than summarised because the constants are only defensible with it.
//!
//! **What was wrong.** The window was bounded by a row count and by nothing
//! else (`consolidation_batch_size = 20`), on a comment's assumption that a
//! window is "20 short excerpts". Measured on six windows a real daemon failed
//! on, the window cost **17 599 – 23 127 tokens** of a 32 768-token context —
//! it was the largest term in the prompt, not the smallest. The conflict set,
//! which `D-095` already bounds, cost 8 791 – 14 159, and the system prompt
//! 1 213. Reassembling those windows here reproduces the daemon's own
//! `request needs N tokens` to within 3 tokens, and twice exactly, so this is
//! a model of the real prompt rather than an approximation of one.
//!
//! **Why estimating was not enough.** `recall::pipeline::estimate_tokens`
//! prices text at four characters per token. Measured, that holds for prose
//! and for memory entries (3.66 chars/token aggregate, so the estimate is
//! within 9 %) and fails for observation excerpts, which are tool output, JSON
//! and code: **2.93 chars/token aggregate, 2.02 at the worst real window,
//! 1.87 at the worst single excerpt.** A window priced at four characters per
//! token is therefore under-counted by up to a factor of two — which is the
//! whole defect, since the window is the term that cannot be cut afterwards.
//!
//! **The rule this module applies.** Whatever cannot be tokenized where it is
//! decided is priced at [`CONSERVATIVE_CHARS_PER_TOKEN`], the measured floor
//! for a whole window (that constant's own doc states what it does not
//! bound, and what absorbs the difference).
//! Whatever can be tokenized is tokenized: `router::route` counts the real
//! assembled prompt and cuts the conflict set to what actually fits.
//!
//! **Why the window is the one that gets a static budget.** A window is a
//! promise to the cursor: `apply_run` advances `processing_cursor` to
//! `to_received_seq` whatever the router read, so an observation dropped at
//! prompt-assembly time is an observation that never becomes memory and is
//! never retried. The conflict set carries no such promise — `D-095` settled
//! that it may be cut ("the router can route with no conflict set, but not
//! with a prompt that does not fit"). So the window is bounded early, in the
//! store, conservatively; the conflict set absorbs whatever is left, exactly.

use crate::prompt;
use crate::router::MAX_GENERATION_TOKENS;

/// Characters per token for text that must be priced where no tokenizer is
/// available — a measured floor, not the average.
///
/// Measured over 400 real excerpts: aggregate 2.926, p5 2.377, minimum 1.870;
/// and over six real windows as assembled: 2.019 at the worst.
///
/// Two is below every measured *window* ratio and below the p5 of single
/// excerpts. It is **not** below the worst single excerpt, and this comment
/// says so rather than rounding the inconvenient figure away: a window made
/// entirely of 1.87-chars-per-token text would cost about 8 % more than its
/// budget. That residual is deliberate, and what absorbs it is named rather
/// than hoped for: `router::route` counts the real assembled prompt and cuts
/// the conflict set to the true remainder, so the overspend is paid in
/// entries the model is shown, not in a failed window. The refusal path
/// underneath that — and then `D-058`'s ladder — is reached only if the
/// system prompt and the window alone exceed the ceiling, which at this
/// budget would take a window denser than one character per token. Pricing at
/// one character per token here instead would remove the residual and halve a
/// normal window, buying certainty for a case no measurement has produced.
pub const CONSERVATIVE_CHARS_PER_TOKEN: u32 = 2;

/// Real tokens the conflict set is promised before the window may claim the
/// rest.
///
/// Measured: `router_conflict_token_budget`'s 12 000 *estimated* tokens cost
/// 8 791 – 14 159 real ones, the spread being how long the scope's entries
/// happen to be. Reserving the top of that range keeps `D-080`/`D-095`'s set
/// as large as it is today instead of quietly trading memory quality for
/// window width. It is a floor and not a cap: when the window spends less,
/// `router::route` measures the true remainder and the conflict set gets it.
pub const CONFLICT_SET_FLOOR_TOKENS: u32 = 14_000;

/// Price `text` where it cannot be tokenized (see
/// [`CONSERVATIVE_CHARS_PER_TOKEN`]).
pub fn conservative_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(CONSERVATIVE_CHARS_PER_TOKEN)
}

/// How one router call may spend `context_tokens`, derived once so that
/// `T23-06`'s answer budget and this window budget cannot drift apart:
/// both are fields of this struct, and changing one changes the other here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptBudget {
    /// The model's context window (`GeneratorCatalogEntry::context_length`).
    /// Never hard-coded: `32_768` appears in the catalog and nowhere else.
    pub context_tokens: u32,
    /// What the answer needs. `llama.cpp` counts this against `n_ctx` itself
    /// (`generate_greedy`'s `requested = tokens.len() + max_tokens`), so it is
    /// unavailable to the prompt even though no prompt contains it.
    pub answer_reserve_tokens: u32,
    /// What the one corrective re-prompt adds on top: the model's own first
    /// response is resent as an assistant turn, plus the correction itself
    /// (`prompt::correction_prompt`). `ADR-0006`'s `n_batch` finding is the
    /// same observation from the other side — a legal single submission is
    /// "the full window prompt, or that prompt plus the model's own first
    /// response". Nobody had reserved for it, so a first call that only just
    /// fit guaranteed a second one that did not.
    pub retry_reserve_tokens: u32,
    /// The system prompt, priced conservatively: it is fixed text, but it is
    /// spent before the store can ask anyone to tokenize it.
    pub system_tokens: u32,
    /// [`CONFLICT_SET_FLOOR_TOKENS`], reduced only if the context is too small
    /// to honour it.
    pub conflict_floor_tokens: u32,
    /// What is left for the window, in real tokens.
    pub window_tokens: u32,
}

impl PromptBudget {
    /// The single subtraction chain. Every term above is named, and their sum
    /// is exactly `context_tokens` — asserted in this module's tests, so no
    /// caller can restate a piece of it.
    pub fn derive(context_tokens: u32) -> Self {
        let answer_reserve_tokens = MAX_GENERATION_TOKENS;
        // The re-prompt resends the first answer (up to `MAX_GENERATION_TOKENS`)
        // plus `correction_prompt`, whose length varies with the parser's own
        // message. Priced against a deliberately long one — 200 characters is
        // several times any `serde_json` error seen in this store's failures —
        // so a verbose error cannot be what pushes the second call over.
        let retry_reserve_tokens = MAX_GENERATION_TOKENS
            + conservative_tokens(&prompt::correction_prompt(&"e".repeat(200)));
        let system_tokens = conservative_tokens(&prompt::system_prompt());

        let available = context_tokens
            .saturating_sub(answer_reserve_tokens)
            .saturating_sub(retry_reserve_tokens)
            .saturating_sub(system_tokens);
        // Halving rather than reserving outright is what makes a small-context
        // model degrade instead of collapsing: at `context_length = 4096`
        // (the catalog's Phi-3 entry) the fixed parts already consume
        // everything, and both budgets go to zero together rather than the
        // window going negative while the conflict set keeps its 14 000.
        let conflict_floor_tokens = CONFLICT_SET_FLOOR_TOKENS.min(available / 2);
        let window_tokens = available - conflict_floor_tokens;

        Self {
            context_tokens,
            answer_reserve_tokens,
            retry_reserve_tokens,
            system_tokens,
            conflict_floor_tokens,
            window_tokens,
        }
    }

    /// The budget for a store whose model reports no context at all: bound
    /// the window by rows alone, the shape every call had before `T23-04`.
    ///
    /// Reached only when the default model has no catalog entry, which also
    /// means no local provider was built, so nothing will consolidate. It
    /// exists so that case degrades to the old behaviour instead of to a
    /// one-observation window derived from a context of zero.
    pub fn unbounded() -> Self {
        // Every term is unbounded, not just the window: a `context_tokens` of
        // zero would give `prompt_ceiling_tokens` of zero, and a provider that
        // *could* count would then find that nothing fits. Unreachable today
        // — no catalog entry means no local provider and therefore no counter
        // — but "unbounded" has to mean unbounded on both sides, or it is a
        // trap for whoever adds the third provider.
        Self {
            context_tokens: u32::MAX,
            answer_reserve_tokens: 0,
            retry_reserve_tokens: 0,
            system_tokens: 0,
            conflict_floor_tokens: 0,
            window_tokens: u32::MAX,
        }
    }

    /// The largest prompt a first call may assemble, leaving room for the
    /// answer and for the one corrective re-prompt.
    pub fn prompt_ceiling_tokens(&self) -> u32 {
        self.context_tokens
            .saturating_sub(self.answer_reserve_tokens)
            .saturating_sub(self.retry_reserve_tokens)
    }

    /// The window budget as the store must see it: characters of
    /// `short_evidence_excerpt`, because `open_window` decides the window in
    /// SQL, where no tokenizer exists.
    pub fn window_chars(&self) -> i64 {
        i64::from(self.window_tokens).saturating_mul(i64::from(CONSERVATIVE_CHARS_PER_TOKEN))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the module: the terms add up to the context exactly, so
    /// nothing can be restated somewhere else and drift.
    #[test]
    fn the_terms_sum_to_the_whole_context() {
        let b = PromptBudget::derive(32_768);
        assert_eq!(
            b.answer_reserve_tokens
                + b.retry_reserve_tokens
                + b.system_tokens
                + b.conflict_floor_tokens
                + b.window_tokens,
            b.context_tokens,
            "every token of the context is accounted to exactly one term"
        );
        assert_eq!(
            b.prompt_ceiling_tokens(),
            b.system_tokens + b.conflict_floor_tokens + b.window_tokens,
            "the ceiling is what the prompt's own three terms may spend"
        );
    }

    /// `D-122`/`T23-06` will change the answer reserve; this asserts the
    /// window budget is a consequence of it rather than a second constant.
    #[test]
    fn the_window_budget_follows_the_answer_reserve() {
        let b = PromptBudget::derive(32_768);
        assert_eq!(b.answer_reserve_tokens, MAX_GENERATION_TOKENS);
        assert!(
            b.retry_reserve_tokens > MAX_GENERATION_TOKENS,
            "the corrective re-prompt resends the first answer plus the correction"
        );
        assert!(
            b.window_tokens > 0 && b.conflict_floor_tokens == CONFLICT_SET_FLOOR_TOKENS,
            "a 32k context honours the conflict set's floor and still leaves a window: {b:?}"
        );
    }

    /// The measurement this module is built on, frozen as an assertion: a
    /// window budgeted at these characters cannot cost more real tokens than
    /// it was given, at the worst ratio ever observed (2.019 chars/token).
    #[test]
    fn the_window_budget_survives_the_worst_measured_ratio() {
        let b = PromptBudget::derive(32_768);
        // The worst ratio ever measured on a whole assembled *window*, which
        // is the unit this budget is spent in. The worst single excerpt is
        // denser still (1.870) — see `CONSERVATIVE_CHARS_PER_TOKEN` for what
        // catches a window made only of those, and why it is not this.
        let worst_measured_chars_per_token = 2.019_f64;
        let real = b.window_chars() as f64 / worst_measured_chars_per_token;
        assert!(
            real <= f64::from(b.window_tokens),
            "{} chars cost {real:.0} real tokens at the worst measured ratio, \
             budgeted {}",
            b.window_chars(),
            b.window_tokens
        );
    }

    /// A context too small for the fixed parts must not produce a negative
    /// window or a conflict set that eats a window that does not exist.
    #[test]
    fn a_context_smaller_than_the_fixed_parts_collapses_both_budgets() {
        let b = PromptBudget::derive(4_096);
        assert_eq!(b.window_tokens, 0);
        assert_eq!(b.conflict_floor_tokens, 0);
        assert_eq!(b.window_chars(), 0);
    }

    /// The escape hatch has to be an escape on both sides.
    #[test]
    fn an_unbounded_budget_bounds_neither_the_window_nor_the_prompt() {
        let b = PromptBudget::unbounded();
        assert_eq!(b.window_tokens, u32::MAX);
        assert!(
            b.prompt_ceiling_tokens() > 0,
            "a zero ceiling would make a counting provider reject every prompt"
        );
        assert!(b.window_chars() > i64::from(u32::MAX));
    }

    #[test]
    fn conservative_pricing_is_the_measured_floor_not_the_average() {
        assert_eq!(conservative_tokens(""), 0);
        assert_eq!(conservative_tokens("ab"), 1);
        assert_eq!(
            conservative_tokens("abc"),
            2,
            "a remainder is a whole token"
        );
        // Characters, not bytes: a two-byte character is one character, the
        // same unit `estimate_tokens` and SQLite's `length()` both use.
        assert_eq!(conservative_tokens("ыы"), 1);
    }
}
