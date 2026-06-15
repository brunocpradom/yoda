//! The model-backend abstraction. Everything above this layer (the agent loop,
//! tools, UI) is written against the `Provider` trait, never against Ollama
//! directly — so swapping models or backends is a change in this module only.
//! See DESIGN.md §4–5.

mod ollama;
pub use ollama::OllamaProvider;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

/// One turn in the conversation. Shapes match the OpenAI chat format; the
/// Ollama provider converts to the native `/api/chat` shape on the way out
/// (see `to_native_messages`). The same struct is used both to send history
/// and to parse the model's reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The model's reasoning trace, when it is a thinking-capable model and
    /// thinking is requested. Only populated when *parsing* a response — our
    /// outgoing history never sets it (skipped when None), so reasoning is shown
    /// to the user but not fed back into the next prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            thinking: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            thinking: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }
    /// An assistant turn, which may carry text, tool calls, or both.
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            thinking: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        }
    }
    /// The result of running a tool, fed back to the model.
    pub fn tool_result(tool_call_id: String, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content),
            thinking: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
        }
    }
}

/// A tool invocation requested by the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_kind() -> String {
    "function".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments. The OpenAI shape sends this as a string, but some
    /// backends emit an object — normalize both to a string on the way in.
    #[serde(default = "empty_args", deserialize_with = "de_arguments")]
    pub arguments: String,
}

fn empty_args() -> String {
    "{}".into()
}

fn de_arguments<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(match value {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

/// A tool schema advertised to the model (OpenAI function-calling format).
#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Token accounting for one exchange, as reported by the backend.
/// `prompt_tokens` covers everything sent (system prompt, tool specs, the whole
/// history); `completion_tokens` is what the model generated. Their sum is how
/// many context-window tokens this conversation occupies right now.
#[derive(Clone, Copy, Debug)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    /// Context-window tokens occupied after this exchange.
    pub fn total(self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// A fully-assembled model response: final text and/or tool calls. The provider
/// streams the reply internally (pushing reasoning deltas to `on_thinking` as
/// they arrive) and returns this once the stream completes.
pub struct Completion {
    pub content: Option<String>,
    /// The full reasoning trace, when thinking is on and supported. Streamed to
    /// the user live during generation, then returned here whole; not stored in
    /// history.
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Token usage for this exchange, when the backend reports it.
    pub usage: Option<Usage>,
}

/// A source of model completions.
///
/// `?Send`: the returned future is not required to be `Send`. The agent loop
/// awaits it directly on the main task (never `tokio::spawn`s it), and this lets
/// `complete` take a non-`Send` `on_thinking` sink that borrows the caller's
/// terminal/spinner state for live streaming.
#[async_trait::async_trait(?Send)]
pub trait Provider: Send + Sync {
    /// Send the conversation plus available tools; get back text and/or tool
    /// calls. `on_thinking` is invoked with each reasoning-trace delta as it
    /// streams in, so the caller can render the model's thinking live; it is
    /// never called for non-reasoning models or when thinking is off.
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_thinking: &mut dyn for<'a> FnMut(&'a str),
    ) -> Result<Completion>;

    /// The context window (in tokens) this provider requests per call, if
    /// known. Lets the UI turn a raw token count into a "percent full" readout
    /// without knowing which backend is behind the trait.
    fn context_window(&self) -> Option<u32> {
        None
    }
}
