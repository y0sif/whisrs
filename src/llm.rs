//! LLM integration for command mode and `[[llm_commands]]`.
//!
//! Sends selected text + a voice instruction to an LLM chat API and returns
//! the rewritten text. Supports any OpenAI-compatible `/chat/completions`
//! endpoint — OpenAI, Groq, or a local server (LM Studio, Ollama, llama.cpp
//! server, ...). Point `[llm] api_url` at the local server (e.g.
//! `http://localhost:1234/v1/chat/completions` for LM Studio) and set
//! `model` to whatever the server has loaded; local servers don't validate
//! `api_key`, so any placeholder string works.
//!
//! One entry point, [`rewrite_text`], shared by both callers: `whisrs command`
//! applies a spoken instruction to the text the user selected, and an
//! `[[llm_commands]]` entry does the same with the roles swapped (dictated
//! text + a preset instruction). A generic instruction — "treat the following
//! text as a request and output only what is asked" — turns an entry into
//! request-in / text-out generation, which is how issue #91's use case is
//! served.
//!
//! Whatever comes back is typed at the user's cursor, so this module also owns
//! the *text* side of making a reply safe to type: [`clean_llm_output`] strips
//! the wrappers models add, and [`contains_line_break`] answers the one
//! question the injection site needs in order to decide whether a reply may be
//! sent to a terminal. The decision itself is not here — see
//! `daemon/injection.rs`, which knows what is focused.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Configuration for the LLM backend used in command mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// API key for the LLM provider.
    #[serde(default)]
    pub api_key: String,
    /// Chat model to use (e.g. "gpt-4o-mini", "llama-3.3-70b-versatile").
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// API base URL. Defaults to OpenAI. Set to Groq or other provider URL.
    #[serde(default = "default_llm_url")]
    pub api_url: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: default_llm_model(),
            api_url: default_llm_url(),
        }
    }
}

fn default_llm_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_llm_url() -> String {
    "https://api.openai.com/v1/chat/completions".to_string()
}

/// A named custom LLM command: dictate → the configured instruction is
/// applied to the transcribed text by the LLM → the result is typed at the
/// cursor. Unlike command mode (`[hotkeys] command`), this doesn't touch the
/// selection or clipboard — it's a toggle-recording flavor of plain
/// dictation, just with an LLM post-processing step and a fixed instruction
/// instead of a spoken one.
///
/// Each entry gets its own global hotkey, registered the same way as the
/// built-in `[hotkeys]` entries. Uses the shared `[llm]` config — there's no
/// per-command model/key override (keep the config surface small; add one
/// only if real usage shows it's needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCommandConfig {
    /// Unique identifier — used in logs and by `whisrs llm-command <name>`
    /// for compositor keybind integration (same pattern as `whisrs toggle`).
    pub name: String,
    /// Key combo string (e.g. "Super+Shift+T"), same format as `[hotkeys]`.
    /// Runs the command: dictate → LLM applies `instruction` → type at cursor.
    pub hotkey: String,
    /// Optional second key combo that *reprograms* this command: press it with
    /// text selected and the highlighted text becomes the new `instruction`
    /// (saved to config, applied live). Same format as `hotkey`. Absent = the
    /// instruction can only be changed by editing the config.
    #[serde(default)]
    pub set_hotkey: Option<String>,
    /// Instruction applied to the dictated text (e.g. "Translate the following
    /// text into German. Return only the translated text."). Can be replaced at
    /// runtime via `set_hotkey`.
    pub instruction: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

impl ChatMessage {
    fn system(content: &str) -> Self {
        Self {
            role: "system".to_string(),
            content: content.to_string(),
        }
    }

    fn user(content: String) -> Self {
        Self {
            role: "user".to_string(),
            content,
        }
    }

    fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

/// System prompt shared by command mode and every `[[llm_commands]]` entry.
///
/// Whatever comes back is typed at the cursor, so a preamble ("Sure! Here's
/// the command:"), a code fence, or wrapping quotes are not cosmetic — they
/// are corrupt output. Saying so in prose is necessary but demonstrably not
/// sufficient: this wording already forbade explanations, and gpt-4o-mini
/// still answered "To install Steam on Arch Linux, you can use the following
/// command: sudo pacman -S steam". [`REWRITE_EXAMPLES`] is the part that
/// actually moves the model; the prose is kept short so it does not compete
/// with the demonstrations.
const REWRITE_SYSTEM_PROMPT: &str = "You are a text editing assistant. The user will give you some selected text and an instruction. \
        Apply the instruction to the text and return ONLY the resulting text. \
        Your reply is typed straight into whatever the user has focused, so it must contain the artifact and nothing else: \
        no preamble, no explanation, no commentary, no surrounding quotes, no markdown formatting and no code fences.";

/// One demonstration turn, in the same frame a real turn uses.
struct RewriteExample {
    /// Plays the role of the selection (command mode) or the dictated text
    /// (`[[llm_commands]]`).
    selected_text: &'static str,
    instruction: &'static str,
    /// The whole assistant reply. Bare artifact, no preamble.
    reply: &'static str,
}

/// Few-shot turns prepended to every request.
///
/// The maintainer's report is the first one verbatim: asking for the Arch
/// command to install Steam came back as a sentence with the command buried in
/// it. Prose in the system prompt did not fix that, so the model is shown the
/// shape instead of told it.
///
/// Chosen to span what a turn can produce, because a single shell-command
/// example teaches "answer tersely" rather than "answer with only the
/// artifact":
///
/// 1. a shell command — one line, no sentence around it;
/// 2. a translation — the rewrite case, no "Translation:" label;
/// 3. an email — **multi-line on purpose**, so terseness is not overgeneralized
///    into "always answer in one line". Multi-line replies are legitimate and
///    are injected normally everywhere except a terminal (see
///    `daemon/injection.rs`).
/// 4. a spelling fix on question-shaped input — the counterweight to 1 and 3.
///
/// The instruction on examples 1 and 3 is the generic "treat the following
/// text as a request" wording from the documented generate recipe, so the
/// examples double as the demonstration for that configuration. Left at that,
/// two thirds of the set would demonstrate *request-fulfilment*, and a tuned
/// `[[llm_commands]]` entry — "Fix the spelling. Return only the corrected
/// text." — fed question-shaped dictation could have its question answered
/// instead of its text corrected. Example 4 is the same question, under a
/// transformation instruction, answered by transforming it: the instruction,
/// not the shape of the text, decides which job is done. It is deliberately
/// last, so it is the demonstration nearest the real request.
const REWRITE_EXAMPLES: &[RewriteExample] = &[
    RewriteExample {
        selected_text: "what is the command to install steam on arch linux",
        instruction: "Treat the following text as a request and output only what is asked.",
        reply: "sudo pacman -S steam",
    },
    RewriteExample {
        selected_text: "Where is the train station?",
        instruction: "Translate the following text into German. Return only the translation.",
        reply: "Wo ist der Bahnhof?",
    },
    RewriteExample {
        selected_text: "write my landlord an email about the leaking kitchen tap and ask for a plumber this week",
        instruction: "Treat the following text as a request and output only what is asked.",
        reply: "Hi Sam,\n\nThe kitchen tap has started leaking and it is getting worse through the day. Could you arrange for a plumber to come round this week?\n\nThanks,\nAlex",
    },
    RewriteExample {
        selected_text: "wht is teh comand to instal steam",
        instruction: "Fix the spelling and grammar. Return only the corrected text.",
        reply: "What is the command to install steam",
    },
];

/// Frame a (text, instruction) pair as one user turn. Used for the real turn
/// *and* for every example, so a demonstration is always shaped exactly like
/// the request it is demonstrating.
fn rewrite_user_message(selected_text: &str, instruction: &str) -> String {
    format!("Selected text:\n{selected_text}\n\nInstruction: {instruction}")
}

/// The full messages array for a rewrite: system prompt, the [`REWRITE_EXAMPLES`]
/// as alternating user/assistant turns, then the real request last.
fn rewrite_messages(selected_text: &str, instruction: &str) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(2 + REWRITE_EXAMPLES.len() * 2);
    messages.push(ChatMessage::system(REWRITE_SYSTEM_PROMPT));
    for example in REWRITE_EXAMPLES {
        messages.push(ChatMessage::user(rewrite_user_message(
            example.selected_text,
            example.instruction,
        )));
        messages.push(ChatMessage::assistant(example.reply));
    }
    messages.push(ChatMessage::user(rewrite_user_message(
        selected_text,
        instruction,
    )));
    messages
}

/// Characters that would end a line if they were injected. `\r` and `\n` are
/// the ones models actually emit; the rest are here because the injector's
/// two paths fail differently and neither fails safely — an unmapped
/// character falls through to a clipboard paste (plain Ctrl+V, which a
/// terminal reads as readline's quoted-insert and which silently splices
/// adjacent lines), while a mapped one is tapped as a real key.
///
/// `\r` is normalized away by [`clean_llm_output`] before this is consulted;
/// it is listed for the property test that pins the set.
const LINE_BREAKS: [char; 7] = [
    '\n',       // line feed
    '\r',       // carriage return — Return in the XKB keymap, i.e. a real Enter
    '\u{000b}', // vertical tab
    '\u{000c}', // form feed
    '\u{0085}', // next line
    '\u{2028}', // line separator
    '\u{2029}', // paragraph separator
];

/// Post-process an LLM reply before it is injected at the cursor. Applied to
/// **every** reply — command mode and `[[llm_commands]]` alike.
///
/// The prompt and the few-shot examples both tell the model not to fence or
/// pad its answer; this is the belt to those suspenders, because the failure
/// is expensive. A ```` ```bash ```` line is typed as if it were part of the
/// command, and a trailing newline at a shell prompt submits a command the
/// user has not read yet.
///
/// Line endings are normalized to `\n` first, so a CRLF reply cannot smuggle a
/// `\r` past [`contains_line_break`] (the keymap maps `\r` to Return, so a
/// surviving one is tapped as a real Enter mid-injection). Then a code fence
/// wrapping the *entire* answer is stripped — always, with no opt-out — and
/// the result is trimmed. A reply containing *several* fenced blocks is prose
/// about code, not a wrapper, and is left exactly as it came.
///
/// "Always" has a cost worth naming: an `[[llm_commands]]` entry whose
/// instruction genuinely asks for a fenced block ("wrap this in a python code
/// fence") cannot produce one — the fence is removed on the way to the cursor.
/// No config key buys it back. That is deliberate: the entries people actually
/// write want the artifact, a stray fence is typed as literal characters into
/// whatever is focused, and one config key per rare case is a worse trade than
/// documenting the limitation (see `docs/configuration.md`).
///
/// Cleaning only. Whether the result may be injected *here, now* depends on
/// what is focused, which this module cannot see: see
/// `daemon/injection.rs::prepare_llm_injection`.
pub fn clean_llm_output(text: &str) -> String {
    let normalized = normalize_line_endings(text);
    strip_wrapping_code_fence(&normalized).trim().to_string()
}

/// Whether `text` still holds a character that would end a line if injected.
///
/// The one fact the injection site needs from this module: at a shell prompt a
/// line break is an Enter that runs a command the user has not read, so a
/// multi-line reply aimed at a terminal is refused rather than typed. Nothing
/// is wrong with a multi-line reply as such — a translated paragraph or a
/// drafted email is exactly what an `[[llm_commands]]` entry is for — so the
/// refusal is the injection site's call, conditional on the target, not this
/// function's.
///
/// Expects [`clean_llm_output`] to have run first, which is what makes `\r`
/// and `\r\n` collapse to the `\n` case.
pub fn contains_line_break(text: &str) -> bool {
    text.contains(LINE_BREAKS)
}

/// Normalize CRLF and bare CR to LF, so exactly one character has to be
/// looked for downstream.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Whether a fence's info string is a language tag (`bash`, `python`, `c++`,
/// ``) rather than content.
///
/// A tag is one token: no whitespace. `` ```bash echo hi `` is not a tag —
/// the model put part of its answer on the fence line, and stripping the
/// fence would delete `echo hi` silently. Such a block is left fenced, which
/// leaves a line break in it, which a terminal target then refuses to inject
/// (see `daemon/injection.rs`): the user is told rather than handed a
/// truncated command.
fn is_fence_language_tag(info: &str) -> bool {
    let info = info.trim();
    info.is_empty()
        || info
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '+' | '-' | '_' | '#' | '.'))
}

/// Strip a code fence that wraps the whole answer, returning `text` unchanged
/// when the answer isn't a single fenced block.
fn strip_wrapping_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return text;
    };
    // The opening fence's info string ("bash", "python", "") runs to the
    // first newline.
    let Some((info, body)) = rest.split_once('\n') else {
        return text;
    };
    if info.contains("```") {
        return text;
    }
    if !is_fence_language_tag(info) {
        // Content on the fence line: stripping would drop it. Leave the reply
        // whole and let the caller decide (see `is_fence_language_tag`).
        warn!(
            "llm: fenced reply carries content on the fence line ({:?}); \
             leaving the fence in place rather than dropping it",
            info.trim()
        );
        return text;
    }
    let Some(body) = body.trim_end().strip_suffix("```") else {
        return text;
    };
    if body.contains("```") {
        // More than one fenced block: not a single wrapper.
        return text;
    }
    body
}

/// Send text and an instruction to the LLM, returning the raw reply.
///
/// Shared by `whisrs command` (selection + spoken instruction) and every
/// `[[llm_commands]]` entry (dictated text + preset instruction). The reply is
/// returned as it arrived; the caller runs it through [`clean_llm_output`] and
/// the injection gate before anything is typed.
pub async fn rewrite_text(
    config: &LlmConfig,
    selected_text: &str,
    instruction: &str,
) -> anyhow::Result<String> {
    let api_key = resolve_api_key(config)?;
    info!(
        "llm: sending to LLM (model={}, instruction={:?})",
        config.model, instruction
    );
    debug!("selected text: {:?}", selected_text);

    chat_with_key(
        config,
        &api_key,
        rewrite_messages(selected_text, instruction),
    )
    .await
}

/// The configured key, or the env-var fallback when `[llm] api_key` is empty.
fn resolve_api_key(config: &LlmConfig) -> anyhow::Result<String> {
    if !config.api_key.is_empty() {
        return Ok(config.api_key.clone());
    }
    // Fall back to env vars.
    let key = std::env::var("WHISRS_OPENAI_API_KEY")
        .or_else(|_| std::env::var("WHISRS_GROQ_API_KEY"))
        .unwrap_or_default();
    if key.is_empty() {
        anyhow::bail!(
            "No LLM API key configured.\n\
             Add [llm] api_key to config.toml, or set WHISRS_OPENAI_API_KEY.\n\
             Run 'whisrs setup' to configure."
        );
    }
    Ok(key)
}

/// POST a prepared messages array to the configured `/chat/completions`
/// endpoint.
///
/// `temperature` stays at 0.3: the preamble problem was a formatting habit,
/// not sampling noise, and it is addressed by the few-shot turns. Lowering it
/// further would also flatten the rewrite path's actual job (rephrasing,
/// translating, drafting), which is not what needed fixing.
async fn chat_with_key(
    config: &LlmConfig,
    api_key: &str,
    messages: Vec<ChatMessage>,
) -> anyhow::Result<String> {
    let request = ChatRequest {
        model: config.model.clone(),
        messages,
        temperature: 0.3,
    };

    let client = reqwest::Client::new();
    let response = client
        .post(&config.api_url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .context("failed to reach LLM API — check your internet connection")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            anyhow::bail!("LLM API: invalid API key — check [llm] api_key in config.toml");
        }
        anyhow::bail!("LLM API error ({status}): {body}");
    }

    let chat_response: ChatResponse = response
        .json()
        .await
        .context("failed to parse LLM response")?;

    let result = chat_response
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    info!("llm: LLM returned {} chars", result.len());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = LlmConfig::default();
        assert_eq!(config.model, "gpt-4o-mini");
        assert!(config.api_url.contains("openai.com"));
        assert!(config.api_key.is_empty());
    }

    #[test]
    fn chat_request_serialization() {
        let request = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![
                ChatMessage::system("You are helpful."),
                ChatMessage::user("Hello".to_string()),
            ],
            temperature: 0.3,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("gpt-4o-mini"));
        assert!(json.contains("system"));
        assert!(json.contains("Hello"));
    }

    /// The framing every turn shares. Both callers depend on it: command mode
    /// puts the selection in `Selected text:` and `[[llm_commands]]` puts the
    /// dictated text there.
    #[test]
    fn a_turn_frames_the_text_and_the_instruction() {
        assert_eq!(
            rewrite_user_message("hello world", "make it shout"),
            "Selected text:\nhello world\n\nInstruction: make it shout"
        );
    }

    /// System prompt, then the examples as alternating user/assistant turns,
    /// then the real request last. Order is the whole mechanism: an example
    /// after the request would be read as a continuation of it.
    #[test]
    fn the_messages_array_is_system_then_examples_then_the_request() {
        let messages = rewrite_messages("hello world", "make it shout");

        assert_eq!(messages.len(), 2 + REWRITE_EXAMPLES.len() * 2);
        assert_eq!(messages[0], ChatMessage::system(REWRITE_SYSTEM_PROMPT));

        for (i, example) in REWRITE_EXAMPLES.iter().enumerate() {
            let user = &messages[1 + i * 2];
            let assistant = &messages[2 + i * 2];
            assert_eq!(
                user,
                &ChatMessage::user(rewrite_user_message(
                    example.selected_text,
                    example.instruction
                )),
                "example {i} must be framed exactly like a real request"
            );
            assert_eq!(assistant, &ChatMessage::assistant(example.reply));
        }

        let last = messages.last().expect("the request is the last turn");
        assert_eq!(
            last,
            &ChatMessage::user(rewrite_user_message("hello world", "make it shout"))
        );
    }

    /// The whole point of the examples: every demonstrated reply is the bare
    /// artifact. A preamble, a label, wrapping quotes or a code fence in one
    /// of these would teach the exact defect they exist to stop —
    /// gpt-4o-mini answering "To install Steam on Arch Linux, you can use the
    /// following command: sudo pacman -S steam".
    #[test]
    fn every_example_reply_is_the_bare_artifact() {
        for example in REWRITE_EXAMPLES {
            let reply = example.reply;
            assert_eq!(reply.trim(), reply, "{reply:?} is padded");
            assert!(!reply.contains("```"), "{reply:?} contains a code fence");
            assert!(
                !reply.starts_with('"') && !reply.starts_with('\''),
                "{reply:?} is quoted"
            );
            for preamble in [
                "Here is",
                "Here's",
                "Sure",
                "Certainly",
                "you can use",
                "The translation",
                "Translation:",
                "Command:",
            ] {
                assert!(
                    !reply.contains(preamble),
                    "{reply:?} demonstrates a preamble ({preamble:?})"
                );
            }
            // An example must survive its own cleaning unchanged, or it is
            // demonstrating something the cleaner would have to undo.
            assert_eq!(clean_llm_output(reply), reply, "{reply:?} is not clean");
        }
    }

    /// The shapes a turn can take. A shell command alone teaches "answer
    /// tersely"; the translation pins the rewrite case; the email keeps a
    /// legitimate multi-line answer in the demonstration set so terseness is
    /// not overgeneralized into "always one line"; and the spelling fix keeps
    /// request-fulfilment from taking over the set (see below).
    #[test]
    fn the_examples_cover_a_command_a_translation_an_email_and_a_transformation() {
        let replies: Vec<&str> = REWRITE_EXAMPLES.iter().map(|e| e.reply).collect();
        assert!(
            replies.contains(&"sudo pacman -S steam"),
            "the reported failure must be demonstrated verbatim: {replies:?}"
        );
        assert!(
            replies.contains(&"Wo ist der Bahnhof?"),
            "a translation example is missing: {replies:?}"
        );
        assert!(
            replies.iter().any(|r| contains_line_break(r)),
            "no example demonstrates a legitimate multi-line reply: {replies:?}"
        );
    }

    /// The instruction decides the job, not the shape of the text. Most of the
    /// set is generic "treat this as a request" fulfilment, so at least one
    /// example must show question-shaped input under a *transformation*
    /// instruction being transformed rather than answered — otherwise a tuned
    /// `[[llm_commands]]` entry ("Fix the spelling. Return only the corrected
    /// text.") fed a dictated question can get the question answered.
    #[test]
    fn an_example_transforms_question_shaped_input_instead_of_answering_it() {
        let example = REWRITE_EXAMPLES
            .iter()
            .find(|e| e.selected_text == "wht is teh comand to instal steam")
            .expect("the question-shaped transformation example is missing");

        assert_eq!(
            example.instruction, "Fix the spelling and grammar. Return only the corrected text.",
            "the instruction must be a transformation, not a request-fulfilment one"
        );
        assert_eq!(
            example.reply, "What is the command to install steam",
            "the reply must be the corrected text, not an answer to the question"
        );
        // The give-away that the question was answered instead of corrected:
        // the answer to it is example 1's reply, which must not appear here.
        assert!(
            !example.reply.contains("pacman"),
            "{:?} answers the question instead of transforming it",
            example.reply
        );
    }

    /// The prose half still has to forbid the wrappers outright — the examples
    /// show the shape, the prompt names the rule.
    #[test]
    fn the_system_prompt_forbids_wrappers() {
        for forbidden in [
            "no preamble",
            "no explanation",
            "no commentary",
            "no surrounding quotes",
            "no markdown formatting and no code fences",
        ] {
            assert!(
                REWRITE_SYSTEM_PROMPT.contains(forbidden),
                "the system prompt must forbid {forbidden:?}"
            );
        }
    }

    #[test]
    fn clean_llm_output_leaves_a_bare_command_alone() {
        assert_eq!(
            clean_llm_output("sudo pacman -S steam"),
            "sudo pacman -S steam"
        );
    }

    /// A trailing newline typed at a shell prompt submits the command before
    /// the user has read it.
    #[test]
    fn clean_llm_output_strips_trailing_and_leading_whitespace() {
        assert_eq!(
            clean_llm_output("\nsudo pacman -S steam\n"),
            "sudo pacman -S steam"
        );
    }

    #[test]
    fn clean_llm_output_strips_a_wrapping_fence_with_a_language() {
        assert_eq!(
            clean_llm_output("```bash\nsudo pacman -S steam\n```"),
            "sudo pacman -S steam"
        );
    }

    #[test]
    fn clean_llm_output_strips_a_bare_wrapping_fence() {
        assert_eq!(
            clean_llm_output("```\nsudo pacman -S steam\n```\n"),
            "sudo pacman -S steam"
        );
    }

    /// A fence wrapping the whole reply is stripped even when the body spans
    /// several lines: a fenced reply is never what the user wants typed, and
    /// where the body may then be injected is the injection site's call.
    /// Internal newlines and indentation survive — only the wrapper goes.
    #[test]
    fn clean_llm_output_strips_the_fence_from_a_multi_line_block() {
        assert_eq!(
            clean_llm_output("```python\ndef f():\n    return 1\n```"),
            "def f():\n    return 1"
        );
    }

    /// Two fenced blocks is prose about code, not a wrapper — stripping the
    /// outer pair would splice unrelated text together.
    #[test]
    fn clean_llm_output_leaves_multiple_fenced_blocks_alone() {
        let text = "```sh\nfirst\n```\nthen run\n```sh\nsecond\n```";
        assert_eq!(clean_llm_output(text), text);
    }

    /// Inline backticks are content, not a fence.
    #[test]
    fn clean_llm_output_leaves_inline_backticks_alone() {
        assert_eq!(
            clean_llm_output("run `pacman -S steam` as root"),
            "run `pacman -S steam` as root"
        );
    }

    /// A language tag is one token, so these fences are wrappers and the tag
    /// carries no content worth keeping.
    #[test]
    fn a_language_tag_fence_is_still_stripped() {
        for fence in ["```", "```bash", "```c++", "```python3", "```objective-c"] {
            assert_eq!(
                clean_llm_output(&format!("{fence}\nsudo pacman -S steam\n```")),
                "sudo pacman -S steam",
                "{fence:?} is a language tag, not content"
            );
        }
    }

    /// Content on the fence line is part of the answer, and a naive stripper
    /// deletes it silently: `"```bash echo hi\nrm -rf /tmp/x\n```"` would come
    /// back as `"rm -rf /tmp/x"`, a different command from the one the model
    /// wrote. The fence is left in place instead, which keeps the reply
    /// multi-line — and a multi-line reply aimed at a terminal is refused out
    /// loud rather than truncated.
    #[test]
    fn a_fence_info_line_with_content_is_never_dropped() {
        let cleaned = clean_llm_output("```bash echo hi\nrm -rf /tmp/x\n```");
        assert!(
            cleaned.contains("echo hi"),
            "content on the fence line must survive cleaning, got {cleaned:?}"
        );
        assert!(contains_line_break(&cleaned));
    }

    /// An unterminated fence: nothing to strip, so the ```` ```bash ```` line
    /// stays and a line break with it. Injecting this into a terminal types a
    /// stray Enter after `sudo` — running a half-typed privileged command.
    #[test]
    fn an_unterminated_fence_stays_multi_line() {
        let cleaned = clean_llm_output("```bash\nsudo rm -rf ~/data");
        assert_eq!(cleaned, "```bash\nsudo rm -rf ~/data");
        assert!(contains_line_break(&cleaned));
    }

    /// A fenced command followed by prose. The fence is not a wrapper, so
    /// nothing is stripped and the whole reply — command, fence, explanation
    /// — stays multi-line.
    #[test]
    fn a_fence_plus_prose_stays_multi_line() {
        let reply = "```\nrm -rf ~/data\n```\nThis deletes your data.";
        assert_eq!(clean_llm_output(reply), reply);
        assert!(contains_line_break(reply));
    }

    /// CRLF is normalized before anything else looks at the text. `\r` is in
    /// the XKB keymap (Return), so a surviving one is tapped as a real Enter
    /// mid-injection.
    #[test]
    fn crlf_is_normalized_away() {
        assert_eq!(
            clean_llm_output("sudo pacman -S steam\r\n"),
            "sudo pacman -S steam",
            "a trailing CRLF is just padding on a single-line answer"
        );

        let cleaned = clean_llm_output("echo one\r\necho two");
        assert_eq!(cleaned, "echo one\necho two");
        assert!(!cleaned.contains('\r'), "no CR may survive cleaning");
        assert!(contains_line_break(&cleaned));
    }

    /// A bare CR (old-Mac line ending, and what a model emits when it botches
    /// a CRLF) is a line break too, and is the exact character the keymap
    /// turns into Enter.
    #[test]
    fn a_bare_cr_is_normalized_to_a_line_feed() {
        let cleaned = clean_llm_output("echo one\recho two");
        assert_eq!(cleaned, "echo one\necho two");
        assert!(!cleaned.contains('\r'));
        assert!(contains_line_break(&cleaned));
    }

    /// Every character the gate treats as a line break is detected, including
    /// the ones the keymap has no entry for — those fall through to a
    /// clipboard paste, whose plain Ctrl+V a terminal reads as readline's
    /// quoted-insert, silently splicing the lines together.
    #[test]
    fn every_line_break_character_is_detected() {
        for break_char in LINE_BREAKS {
            let cleaned = clean_llm_output(&format!("echo one{break_char}echo two"));
            assert!(
                contains_line_break(&cleaned),
                "U+{:04X} must be seen as a line break",
                break_char as u32
            );
        }
    }

    /// A single-line reply has no line break, however it arrived.
    #[test]
    fn a_single_line_reply_has_no_line_break() {
        for reply in [
            "sudo pacman -S steam",
            "```bash\nsudo pacman -S steam\n```",
            "  sudo pacman -S steam  \n",
        ] {
            let cleaned = clean_llm_output(reply);
            assert_eq!(cleaned, "sudo pacman -S steam", "{reply:?}");
            assert!(!contains_line_break(&cleaned));
        }
    }

    /// Nothing usable: an empty reply, whitespace, and the fences models emit
    /// when they have nothing to say. All clean to the empty string, which is
    /// what the injection site refuses.
    #[test]
    fn nothing_usable_cleans_to_empty() {
        for reply in [
            "",
            "   ",
            "\n\t\n",
            "\r\n",
            "```\n```",
            "```bash\n\n```",
            "```\n\n```",
        ] {
            assert_eq!(
                clean_llm_output(reply),
                "",
                "{reply:?} holds no usable text"
            );
        }
    }

    #[test]
    fn chat_response_deserialization() {
        let json = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello back!"
                }
            }]
        }"#;
        let response: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.choices[0].message.content, "Hello back!");
    }
}
