//! Renders a GGUF model's own embedded chat template through a real Jinja
//! interpreter (T14-09), replacing `llama-cpp-2::apply_chat_template`'s call
//! into the vendored `llama.cpp`'s `llm_chat_detect_template` — a
//! fixed-signature heuristic matcher, not a Jinja interpreter, which is why
//! ADR-0006 needed a one-off `chat_template_override` for Gemma 4: its
//! embedded template's text does not match any signature in that matcher's
//! table. `LlamaModel::chat_template`'s own doc comment (`llama-cpp-2`
//! 0.1.152) names [minijinja](https://github.com/mitsuhiko/minijinja) as the
//! intended escape hatch for exactly this case.
//!
//! This module knows nothing about `LlamaModel` or GGUF files — it renders
//! whatever raw Jinja template text and `messages` it is given, which is
//! what makes it independently, offline unit-testable against real template
//! strings without loading any weights (see the `tests` module below).
//!
//! # Why `UndefinedBehavior::Lenient`, not `Strict`
//!
//! An earlier version of this module used `Strict`, reasoning by analogy to
//! HuggingFace's `StrictUndefined`. That assumption did not survive contact
//! with real templates: both Gemma 4's and Qwen2.5's actual embedded
//! templates (captured live, T14-09) reference optional top-level context
//! (`{% if tools %}`, no `default()` guard) and optional per-message
//! attributes (`message.tool_calls`, present only on assistant turns that
//! carry tool calls) that this crate's `messages`/context never supplies —
//! by design, this router never emits tool-calling turns. Under `Strict`,
//! `if tools` on an absent variable and `for x in message.tool_calls` on a
//! message with no such field both hard-fail, breaking rendering for every
//! real template tested, not just an edge case.
//!
//! `Lenient` (`minijinja`'s own default) matches how these templates are
//! actually authored and exercised in practice: printing/iterating an
//! undefined value is allowed (empty string / empty sequence), an undefined
//! value is falsy in an `if`, but **attribute access on an already-undefined
//! value still fails** — so a template that references genuinely
//! out-of-place context (not "this optional field is absent" but "this
//! object I expected to exist does not") still surfaces as a loud
//! [`ChatTemplateError::Render`], not silently blank output. `raise_exception`
//! and template syntax errors are unaffected by this choice — both are
//! reported regardless of undefined-value behavior.
//!
//! # `pycompat`: real templates call Python methods `minijinja` lacks
//!
//! Gemma 4's template also calls plain Python dict/str methods —
//! `message.get('reasoning')`, `text.split('<channel|>')` — that
//! `minijinja`'s own built-in value types do not implement (verified live:
//! without this, rendering fails with `unknown method: map has no method
//! named get`). [`minijinja_contrib::pycompat::unknown_method_callback`],
//! written by `minijinja`'s own author specifically to close this gap,
//! is registered on every [`Environment`] this module builds.
//!
//! # `raise_exception`
//!
//! Real HuggingFace chat templates commonly call a global `raise_exception(message)`
//! function to reject a message sequence their own logic cannot format (for
//! example, a system message anywhere but the first turn). HF's own Jinja
//! environment injects this as a global; this module does the same via
//! [`minijinja::Environment::add_function`]. A call to it is distinguished
//! from every other `ErrorKind::InvalidOperation` minijinja's own internals
//! can raise (arithmetic overflow, a filter given the wrong argument type,
//! …) by a private sentinel prefix only this module ever writes — the error
//! *kind* alone is not a reliable signal, verified directly against
//! `minijinja` 2.21.0's own source (`ErrorKind::InvalidOperation` is reused
//! by more than a dozen unrelated call sites across `filters.rs`/`vm/mod.rs`/
//! `value/ops.rs`).

use minijinja::{Environment, ErrorKind, UndefinedBehavior};
use serde::Serialize;

use local_rag_embed::{GenMessage, GenRole};

/// Why rendering a chat template failed. Every variant is a typed,
/// non-retryable defect (bad template, bad message sequence for that
/// template, or a template bug) — never a transient condition.
#[derive(Debug)]
#[non_exhaustive]
pub enum ChatTemplateError {
    /// The template source itself does not compile (Jinja syntax error).
    Invalid { message: String },
    /// The template's own `raise_exception(...)` call fired — the template
    /// judged the message sequence it was given malformed for its expected
    /// shape (e.g. "System role not supported").
    RaisedByTemplate { message: String },
    /// Any other render-time failure: a strict-undefined variable access, a
    /// filter/function/tag this module's `minijinja::Environment` does not
    /// provide, a type error inside the template's own logic. Not further
    /// categorized — `minijinja`'s own message is the detail.
    Render { message: String },
}

impl std::fmt::Display for ChatTemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatTemplateError::Invalid { message } => {
                write!(f, "chat template does not compile: {message}")
            }
            ChatTemplateError::RaisedByTemplate { message } => {
                write!(f, "chat template rejected the message sequence: {message}")
            }
            ChatTemplateError::Render { message } => {
                write!(f, "chat template render failed: {message}")
            }
        }
    }
}

impl std::error::Error for ChatTemplateError {}

/// Private sentinel prefixed onto the message every [`raise_exception`] call
/// produces, so [`render`] can tell "the template itself rejected this input"
/// apart from every other minijinja-internal `ErrorKind::InvalidOperation`
/// (see the module doc). Contains a byte no real template author would type
/// into a `raise_exception("...")` call, so a collision is not a realistic
/// concern — this is a classification convenience, not a security boundary.
const RAISED_BY_TEMPLATE_MARKER: &str = "\u{0}chat_template::raise_exception\u{0}";

fn raise_exception(message: String) -> Result<String, minijinja::Error> {
    Err(minijinja::Error::new(
        ErrorKind::InvalidOperation,
        format!("{RAISED_BY_TEMPLATE_MARKER}{message}"),
    ))
}

/// The role name string every HuggingFace-authored chat template is written
/// against — the same `"system"`/`"user"`/`"assistant"` vocabulary
/// `llama_chat_apply_template` used before this module replaced it.
fn role_str(role: GenRole) -> &'static str {
    match role {
        GenRole::System => "system",
        GenRole::User => "user",
        GenRole::Assistant => "assistant",
    }
}

#[derive(Serialize)]
struct RenderMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct RenderContext<'a> {
    messages: Vec<RenderMessage<'a>>,
    bos_token: &'a str,
    eos_token: &'a str,
    add_generation_prompt: bool,
}

/// Render `template_source` (a model's raw, embedded Jinja chat template)
/// against `messages`, the same context shape HuggingFace's own
/// `apply_chat_template` exposes: `messages`, `bos_token`, `eos_token`,
/// `add_generation_prompt`.
pub(crate) fn render(
    template_source: &str,
    messages: &[GenMessage],
    bos_token: &str,
    eos_token: &str,
    add_generation_prompt: bool,
) -> Result<String, ChatTemplateError> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    env.add_function("raise_exception", raise_exception);
    // Real HF-authored templates call plain Python dict/str/list methods
    // (`message.get(...)`, `text.split(...)`, ...) minijinja's own value
    // types do not implement natively — verified live against Gemma 4's
    // template, which calls `message.get('reasoning')`. `pycompat` is this
    // same crate's own author's purpose-built shim for exactly this gap.
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);

    let template =
        env.template_from_str(template_source)
            .map_err(|e| ChatTemplateError::Invalid {
                message: e.to_string(),
            })?;

    let ctx = RenderContext {
        messages: messages
            .iter()
            .map(|m| RenderMessage {
                role: role_str(m.role),
                content: &m.content,
            })
            .collect(),
        bos_token,
        eos_token,
        add_generation_prompt,
    };

    template.render(ctx).map_err(classify_render_error)
}

fn classify_render_error(err: minijinja::Error) -> ChatTemplateError {
    if err.kind() == ErrorKind::InvalidOperation
        && let Some(message) = err
            .detail()
            .and_then(|d| d.strip_prefix(RAISED_BY_TEMPLATE_MARKER))
    {
        return ChatTemplateError::RaisedByTemplate {
            message: message.to_string(),
        };
    }
    ChatTemplateError::Render {
        message: err.to_string(),
    }
}

/// Strip `bos_token` from the front of `rendered`, if present — some
/// template families (Gemma's among them) start their rendered text with
/// the literal BOS token string, which would otherwise be tokenized twice:
/// once by the template's own text, once by `LlamaModel::str_to_token`'s
/// `AddBos::Always` (the single source of truth for BOS this crate keeps).
/// A no-op when `bos_token` is empty (some GGUFs define no real BOS) or not
/// a literal prefix of the rendered text.
pub(crate) fn strip_leading_bos<'a>(rendered: &'a str, bos_token: &str) -> &'a str {
    if bos_token.is_empty() {
        return rendered;
    }
    rendered.strip_prefix(bos_token).unwrap_or(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: GenRole, content: &str) -> GenMessage {
        GenMessage {
            role,
            content: content.to_string(),
        }
    }

    /// Gemma 4's real embedded template, captured live (T14-09) from an
    /// installed `gemma-4-e2b-it-gguf-q4-0` GGUF via
    /// `LlamaModel::chat_template(None)`. Exact expected output below was
    /// independently hand-traced against this literal source (not accepted
    /// on `minijinja`'s own say-so) before being pinned here as a
    /// regression fixture.
    const GEMMA4_TEMPLATE: &str = include_str!("chat_template_fixtures/gemma4.jinja");

    #[test]
    fn gemma4_native_template_renders_a_two_message_window() {
        let messages = [msg(GenRole::System, "SYS"), msg(GenRole::User, "USR1")];
        let rendered = render(GEMMA4_TEMPLATE, &messages, "<bos>", "<eos>", true)
            .expect("Gemma 4's real template renders");
        assert_eq!(
            rendered,
            "<bos><|turn>system\nSYS<turn|>\n<|turn>user\nUSR1<turn|>\n<|turn>model\n"
        );
    }

    #[test]
    fn gemma4_native_template_renders_a_true_system_turn_not_merged_into_user() {
        // The regression this task fixes: T14-07's `chat_template_override:
        // Some("gemma")` forced the vendored `llama.cpp`'s legacy
        // `LLM_CHAT_TEMPLATE_GEMMA` formatter, which merges the system turn
        // into the first user turn (ADR-0006's own disclosed cost). Gemma
        // 4's real template renders the system content in its own
        // `<|turn>system...<turn|>` block, closed before `<|turn>user`
        // opens -- proof the native template's system-role support is what
        // actually gets exercised now.
        let messages = [msg(GenRole::System, "SYS"), msg(GenRole::User, "USR1")];
        let rendered = render(GEMMA4_TEMPLATE, &messages, "<bos>", "<eos>", true)
            .expect("Gemma 4's real template renders");
        let system_turn_end = rendered.find("<turn|>\n").expect("a closed system turn");
        let user_turn_start = rendered
            .find("<|turn>user\n")
            .expect("a separate user turn");
        assert!(
            system_turn_end < user_turn_start,
            "system turn must close before the user turn opens: {rendered:?}"
        );
        assert!(
            !rendered["<|turn>user\n".len()..].starts_with("SYS"),
            "system content must not appear inside the user turn: {rendered:?}"
        );
    }

    #[test]
    fn gemma4_native_template_renders_a_four_turn_corrective_reprompt() {
        // Mirrors `local_rag_memory::parse`'s real corrective-retry shape:
        // System, User, Assistant, User.
        let messages = [
            msg(GenRole::System, "SYS"),
            msg(GenRole::User, "USR1"),
            msg(GenRole::Assistant, "ASST"),
            msg(GenRole::User, "USR2"),
        ];
        let rendered = render(GEMMA4_TEMPLATE, &messages, "<bos>", "<eos>", true)
            .expect("Gemma 4's real template renders");
        assert_eq!(
            rendered,
            "<bos><|turn>system\nSYS<turn|>\n\
             <|turn>user\nUSR1<turn|>\n\
             <|turn>model\nASST<turn|>\n\
             <|turn>user\nUSR2<turn|>\n\
             <|turn>model\n"
        );
    }

    /// Qwen2.5's real embedded ChatML template, captured live (T14-09) —
    /// identical across both `qwen2.5-0.5b-instruct-gguf-q4km` and
    /// `qwen2.5-1.5b-instruct-gguf-q4km` (verified: same bytes captured from
    /// both installed GGUFs). Confirms the switch away from
    /// `llama_chat_apply_template` did not change this already-working
    /// family's rendered output.
    const QWEN_CHATML_TEMPLATE: &str = include_str!("chat_template_fixtures/qwen_chatml.jinja");

    #[test]
    fn qwen_chatml_template_renders_the_documented_im_start_format() {
        let messages = [msg(GenRole::System, "SYS"), msg(GenRole::User, "USR1")];
        let rendered = render(
            QWEN_CHATML_TEMPLATE,
            &messages,
            "<|endoftext|>",
            "<|im_end|>",
            true,
        )
        .expect("Qwen's real ChatML template renders");
        assert_eq!(
            rendered,
            "<|im_start|>system\nSYS<|im_end|>\n<|im_start|>user\nUSR1<|im_end|>\n<|im_start|>assistant\n"
        );
    }

    #[test]
    fn qwen_chatml_template_renders_a_four_turn_corrective_reprompt() {
        let messages = [
            msg(GenRole::System, "SYS"),
            msg(GenRole::User, "USR1"),
            msg(GenRole::Assistant, "ASST"),
            msg(GenRole::User, "USR2"),
        ];
        let rendered = render(
            QWEN_CHATML_TEMPLATE,
            &messages,
            "<|endoftext|>",
            "<|im_end|>",
            true,
        )
        .expect("Qwen's real ChatML template renders");
        assert_eq!(
            rendered,
            "<|im_start|>system\nSYS<|im_end|>\n\
             <|im_start|>user\nUSR1<|im_end|>\n\
             <|im_start|>assistant\nASST<|im_end|>\n\
             <|im_start|>user\nUSR2<|im_end|>\n\
             <|im_start|>assistant\n"
        );
    }

    /// Microsoft's real embedded Phi-3-mini-4k-instruct template, captured
    /// live (T14-09) — a third family (neither ChatML nor Gemma's
    /// `<start_of_turn>`/`<|turn>`), proving the general mechanism needs no
    /// per-entry override to render an arbitrary new model's own markup.
    const PHI3_TEMPLATE: &str = include_str!("chat_template_fixtures/phi3_mini_4k.jinja");

    #[test]
    fn third_model_template_renders_its_own_distinct_markup() {
        let messages = [msg(GenRole::System, "SYS"), msg(GenRole::User, "USR1")];
        let rendered = render(PHI3_TEMPLATE, &messages, "<s>", "<|endoftext|>", true)
            .expect("Phi-3-mini-4k's real template renders");
        assert_eq!(rendered, "<s><|user|>\nUSR1<|end|>\n<|assistant|>\n");
    }

    #[test]
    fn third_model_template_silently_drops_system_content_a_disclosed_real_limitation() {
        // Not a defect in this rendering mechanism: this exact GGUF's own
        // embedded template only branches on `role in ('user', 'assistant')`
        // -- a real, verified limitation of Phi-3-mini-4k-instruct's own
        // template (see `crate::catalog::PHI3_MINI_4K_INSTRUCT_Q4`'s doc),
        // not something `chat_template::render` should paper over. This
        // test pins that the renderer is faithful to the template's real
        // logic, dropped system turn included, rather than silently
        // "fixing" it into something the template's own author didn't write.
        let messages = [msg(GenRole::System, "SYS"), msg(GenRole::User, "USR1")];
        let rendered = render(PHI3_TEMPLATE, &messages, "<s>", "<|endoftext|>", true)
            .expect("Phi-3-mini-4k's real template renders");
        assert!(
            !rendered.contains("SYS"),
            "the system content must be absent, matching this template's own real logic: {rendered:?}"
        );
    }

    #[test]
    fn third_model_template_renders_a_four_turn_corrective_reprompt() {
        let messages = [
            msg(GenRole::System, "SYS"),
            msg(GenRole::User, "USR1"),
            msg(GenRole::Assistant, "ASST"),
            msg(GenRole::User, "USR2"),
        ];
        let rendered = render(PHI3_TEMPLATE, &messages, "<s>", "<|endoftext|>", true)
            .expect("Phi-3-mini-4k's real template renders");
        assert_eq!(
            rendered,
            "<s><|user|>\nUSR1<|end|>\n<|assistant|>\n\
             ASST<|end|>\n\
             <|user|>\nUSR2<|end|>\n<|assistant|>\n"
        );
    }

    #[test]
    fn role_str_covers_every_gen_role_with_the_jinja_role_vocabulary() {
        assert_eq!(role_str(GenRole::System), "system");
        assert_eq!(role_str(GenRole::User), "user");
        assert_eq!(role_str(GenRole::Assistant), "assistant");
    }

    #[test]
    fn raise_exception_surfaces_as_a_typed_error_not_a_panic() {
        let template = "{% if messages|length > 1 %}{{ raise_exception(\"boom\") }}{% endif %}";
        let messages = [msg(GenRole::System, "s"), msg(GenRole::User, "u")];

        let err = render(template, &messages, "<bos>", "<eos>", true)
            .expect_err("two messages must trigger raise_exception");

        match err {
            ChatTemplateError::RaisedByTemplate { message } => {
                assert_eq!(message, "boom");
            }
            other => panic!("expected RaisedByTemplate, got {other}"),
        }
    }

    #[test]
    fn raise_exception_is_not_triggered_when_the_templates_own_guard_does_not_fire() {
        let template = "{% if messages|length > 1 %}{{ raise_exception(\"boom\") }}{% endif %}ok";
        let messages = [msg(GenRole::User, "u")];

        let rendered =
            render(template, &messages, "<bos>", "<eos>", true).expect("guard does not fire");
        assert_eq!(rendered, "ok");
    }

    #[test]
    fn invalid_jinja_syntax_is_a_typed_compile_error() {
        let err =
            render("{% if", &[], "<bos>", "<eos>", true).expect_err("malformed Jinja must fail");
        assert!(
            matches!(err, ChatTemplateError::Invalid { .. }),
            "expected Invalid, got {err}"
        );
    }

    #[test]
    fn a_top_level_undefined_variable_renders_as_empty_lenient_text() {
        // Real templates (Gemma 4, Qwen2.5 ChatML -- verified live) reference
        // optional context like `tools` with no `default()` guard, relying
        // on exactly this leniency; printing/if-truthy on an absent
        // top-level variable must not be a hard failure.
        let rendered = render("[{{ nonexistent }}]", &[], "<bos>", "<eos>", true)
            .expect("an absent top-level variable prints as empty text, not an error");
        assert_eq!(rendered, "[]");
    }

    #[test]
    fn chaining_an_attribute_off_an_already_undefined_value_is_a_typed_render_error() {
        // `nonexistent` is itself undefined; `.attr` on top of that is a
        // genuinely broken template reference, not an absent optional field
        // -- this must still be a loud error under `Lenient`.
        let err = render("{{ nonexistent.attr }}", &[], "<bos>", "<eos>", true)
            .expect_err("chaining off an undefined value must fail even under Lenient");
        assert!(
            matches!(err, ChatTemplateError::Render { .. }),
            "expected Render, got {err}"
        );
    }

    #[test]
    fn bos_and_eos_tokens_are_interpolated_not_hardcoded() {
        let messages = [msg(GenRole::User, "hi")];
        let rendered = render(
            "{{ bos_token }}{{ messages[0].content }}{{ eos_token }}",
            &messages,
            "[[BOS]]",
            "[[EOS]]",
            true,
        )
        .expect("trivial template renders");
        assert_eq!(rendered, "[[BOS]]hi[[EOS]]");
    }

    #[test]
    fn add_generation_prompt_is_available_to_the_template() {
        let rendered = render(
            "{% if add_generation_prompt %}open{% else %}closed{% endif %}",
            &[],
            "",
            "",
            true,
        )
        .expect("renders");
        assert_eq!(rendered, "open");

        let rendered = render(
            "{% if add_generation_prompt %}open{% else %}closed{% endif %}",
            &[],
            "",
            "",
            false,
        )
        .expect("renders");
        assert_eq!(rendered, "closed");
    }

    #[test]
    fn strip_leading_bos_removes_only_an_exact_leading_match() {
        assert_eq!(strip_leading_bos("<bos>hello", "<bos>"), "hello");
        assert_eq!(strip_leading_bos("hello", "<bos>"), "hello");
        assert_eq!(strip_leading_bos("hello", ""), "hello");
        assert_eq!(
            strip_leading_bos("hello <bos> world", "<bos>"),
            "hello <bos> world"
        );
    }
}
