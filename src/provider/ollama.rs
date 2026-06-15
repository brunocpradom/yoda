//! `Provider` implementation backed by Ollama's native chat API
//! (`/api/chat`). We previously used Ollama's OpenAI-compatible endpoint
//! (`/v1/chat/completions`), but it silently ignores `options.num_ctx` —
//! leaving the context window at Ollama's 4096-token default, which is too
//! small for agentic work (verified empirically via `ollama ps`). Portability
//! to other backends (llama.cpp, vLLM, LM Studio) is the `Provider` trait's
//! job: an OpenAI-compatible backend gets its own impl in this module.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use super::{Completion, Message, Provider, ToolSpec, Usage};

pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    num_ctx: u32,
    /// User toggle (`/think on|off`): whether to ask the model to expose its
    /// reasoning. On by default.
    think: bool,
    /// Cache of whether the *current* model accepts the `think` option. Reasoning
    /// models (qwen3, deepseek-r1) do; others return 400 "does not support
    /// thinking". We start optimistic, flip to false on the first such 400, and
    /// reset on `set_model` — so default-on thinking degrades cleanly instead of
    /// erroring a turn. `Atomic` because `complete` takes `&self`.
    think_supported: AtomicBool,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, num_ctx: u32) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            model: model.into(),
            num_ctx,
            think: true,
            think_supported: AtomicBool::new(true),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Whether the user has thinking enabled (`/think`).
    pub fn think(&self) -> bool {
        self.think
    }

    /// Toggle exposing the model's reasoning (`/think on|off`).
    pub fn set_think(&mut self, on: bool) {
        self.think = on;
    }

    /// Switch the active model at runtime (manual routing — see DESIGN.md §5b).
    /// Re-arms thinking support: the new model may be reasoning-capable even if
    /// the previous one was not.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        self.think_supported.store(true, Ordering::Relaxed);
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    message: Message,
    /// Prompt / generated token counts, reported by Ollama on the final
    /// (non-streamed) response. `Option` because error shapes omit them — and
    /// note `prompt_eval_count` counts tokens *evaluated*, so a warm prompt
    /// cache can make it undercount the true context occupancy.
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

/// Convert our internal (OpenAI-shaped) messages to the native API's shape.
/// The only difference: tool-call `arguments` must be a JSON *object*; the
/// OpenAI shape we store internally is a JSON-encoded *string*, and the native
/// endpoint rejects that form outright. An arguments string that doesn't parse
/// (the model emitted malformed JSON earlier in the session) degrades to `{}`
/// rather than erroring — the tool already reported its own failure to the
/// model at execution time, and bricking every later turn over it helps no one.
fn to_native_messages(messages: &[Message]) -> Result<serde_json::Value> {
    let mut out = serde_json::to_value(messages).context("could not serialize messages")?;
    for message in out.as_array_mut().expect("messages serialize to an array") {
        let Some(calls) = message.get_mut("tool_calls").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for call in calls {
            let Some(arguments) = call.pointer_mut("/function/arguments") else {
                continue;
            };
            if let Some(s) = arguments.as_str() {
                *arguments = serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}));
            }
        }
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Provider for OllamaProvider {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let native = to_native_messages(messages)?;
        let tools_value = if tools.is_empty() {
            None
        } else {
            Some(serde_json::to_value(tools)?)
        };

        // Send the `think` flag whenever the model supports it, with the user's
        // actual on/off value. We must send it *explicitly*: omitting it lets a
        // reasoning model (e.g. qwen3) use its default of thinking ON, so
        // `/think off` would be silently ignored — sending `false` truly disables
        // it. On a "does not support thinking" 400 we cache that and retry once
        // without the field, so non-reasoning models still work. Loop runs ≤ 2×.
        let mut send_think = self.think_supported.load(Ordering::Relaxed);
        loop {
            let mut body = serde_json::Map::new();
            body.insert("model".into(), serde_json::json!(self.model));
            body.insert("messages".into(), native.clone());
            body.insert("stream".into(), serde_json::json!(false));
            body.insert(
                "options".into(),
                serde_json::json!({ "num_ctx": self.num_ctx }),
            );
            if let Some(t) = &tools_value {
                body.insert("tools".into(), t.clone());
            }
            if send_think {
                body.insert("think".into(), serde_json::json!(self.think));
            }

            let response = self
                .client
                .post(format!("{}/api/chat", self.base_url))
                .json(&serde_json::Value::Object(body))
                .send()
                .await
                .context("could not reach the model server — is `ollama serve` running?")?;

            // The native API returns useful JSON errors ({"error": "..."}); read
            // the body instead of discarding it with error_for_status.
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                // Graceful degradation: this model has no reasoning mode. Cache
                // it so later turns skip the probe, and retry without `think`.
                if send_think && detail.contains("does not support thinking") {
                    self.think_supported.store(false, Ordering::Relaxed);
                    send_think = false;
                    continue;
                }
                return Err(anyhow!("model server returned {status}: {detail}")
                    .context("is the model pulled?"));
            }

            let parsed: ChatResponse = response
                .json()
                .await
                .context("could not parse model response")?;

            // No prompt count → no usage at all: a total built from only one of
            // the two numbers would be silently wrong, worse than absent.
            let usage = parsed.prompt_eval_count.map(|prompt_tokens| Usage {
                prompt_tokens,
                completion_tokens: parsed.eval_count.unwrap_or(0),
            });

            return Ok(Completion {
                content: parsed.message.content,
                thinking: parsed.message.thinking,
                tool_calls: parsed.message.tool_calls.unwrap_or_default(),
                usage,
            });
        }
    }

    fn context_window(&self) -> Option<u32> {
        Some(self.num_ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCall};

    #[test]
    fn tool_call_arguments_become_objects() {
        let messages = [Message::assistant(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "calc".into(),
                    arguments: r#"{"expr":"2+2"}"#.into(),
                },
            }],
        )];
        let native = to_native_messages(&messages).unwrap();
        let arguments = native
            .pointer("/0/tool_calls/0/function/arguments")
            .unwrap();
        assert_eq!(arguments, &serde_json::json!({"expr": "2+2"}));
    }

    #[test]
    fn malformed_arguments_degrade_to_empty_object() {
        let messages = [Message::assistant(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "calc".into(),
                    arguments: "not json {".into(),
                },
            }],
        )];
        let native = to_native_messages(&messages).unwrap();
        let arguments = native
            .pointer("/0/tool_calls/0/function/arguments")
            .unwrap();
        assert_eq!(arguments, &serde_json::json!({}));
    }

    #[test]
    fn parses_token_counts_from_chat_response() {
        let json = r#"{
            "message": {"role": "assistant", "content": "hi"},
            "prompt_eval_count": 26,
            "eval_count": 298
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.prompt_eval_count, Some(26));
        assert_eq!(parsed.eval_count, Some(298));
    }

    #[test]
    fn token_counts_are_optional() {
        let json = r#"{"message": {"role": "assistant", "content": "hi"}}"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.prompt_eval_count, None);
        assert_eq!(parsed.eval_count, None);
    }

    #[test]
    fn plain_messages_pass_through_unchanged() {
        let messages = [Message::user("hi"), Message::system("be brief")];
        let native = to_native_messages(&messages).unwrap();
        assert_eq!(native.pointer("/0/content").unwrap(), "hi");
        assert!(native.pointer("/0/tool_calls").is_none());
    }

    #[test]
    fn thinking_defaults_on_and_toggles() {
        let mut p = OllamaProvider::new("http://x", "qwen3:8b", 8192);
        assert!(p.think(), "thinking is on by default");
        p.set_think(false);
        assert!(!p.think());
        // Switching models must not silently re-enable the user's toggle.
        p.set_model("qwen2.5-coder:7b");
        assert!(!p.think());
    }

    #[test]
    fn thinking_parsed_from_response() {
        let json = r#"{
            "message": {"role": "assistant", "content": "391", "thinking": "17*23 = 340+51"},
            "prompt_eval_count": 10,
            "eval_count": 5
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message.thinking.as_deref(), Some("17*23 = 340+51"));
        assert_eq!(parsed.message.content.as_deref(), Some("391"));
    }
}
