//! LLM integration for command mode.
//!
//! Sends selected text + a voice instruction to an LLM chat API and returns
//! the rewritten text. Supports any OpenAI-compatible `/chat/completions`
//! endpoint — OpenAI, Groq, or a local server (LM Studio, Ollama, llama.cpp
//! server, ...). Point `[llm] api_url` at the local server (e.g.
//! `http://localhost:1234/v1/chat/completions` for LM Studio) and set
//! `model` to whatever the server has loaded; local servers don't validate
//! `api_key`, so any placeholder string works.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

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

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
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

/// Send selected text and a voice instruction to the LLM, returning the rewritten text.
pub async fn rewrite_text(
    config: &LlmConfig,
    selected_text: &str,
    instruction: &str,
) -> anyhow::Result<String> {
    if config.api_key.is_empty() {
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
        return rewrite_with_key(config, &key, selected_text, instruction).await;
    }

    rewrite_with_key(config, &config.api_key, selected_text, instruction).await
}

async fn rewrite_with_key(
    config: &LlmConfig,
    api_key: &str,
    selected_text: &str,
    instruction: &str,
) -> anyhow::Result<String> {
    info!(
        "command mode: sending to LLM (model={}, instruction={:?})",
        config.model, instruction
    );
    debug!("selected text: {:?}", selected_text);

    let system_prompt = "You are a text editing assistant. The user will give you some selected text and a voice instruction. \
        Apply the instruction to the text and return ONLY the modified text. \
        Do not add explanations, markdown formatting, or quotes — just return the raw result text.";

    let user_message = format!("Selected text:\n{selected_text}\n\nInstruction: {instruction}");

    let request = ChatRequest {
        model: config.model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_message,
            },
        ],
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

    info!("command mode: LLM returned {} chars", result.len());
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
                ChatMessage {
                    role: "system".to_string(),
                    content: "You are helpful.".to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                },
            ],
            temperature: 0.3,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("gpt-4o-mini"));
        assert!(json.contains("system"));
        assert!(json.contains("Hello"));
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
