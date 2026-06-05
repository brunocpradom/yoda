//! The model-backend abstraction. Everything above this layer (the agent loop,
//! tools, UI) is written against the `Provider` trait, never against Ollama
//! directly — so swapping models or backends is a change in this module only.
//! See DESIGN.md §4–5.

mod ollama;
pub use ollama::OllamaProvider;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

/// One turn in the conversation. Shapes match the OpenAI-compatible chat API
/// that Ollama serves at `/v1/chat/completions`. The same struct is used both
/// to send history and to parse the model's reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }
    /// An assistant turn, which may carry text, tool calls, or both.
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tool_call_id: None,
        }
    }
    /// The result of running a tool, fed back to the model.
    pub fn tool_result(tool_call_id: String, content: String) -> Self {
        Self { role: "tool".into(), content: Some(content), tool_calls: None, tool_call_id: Some(tool_call_id) }
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

/// A single (non-streaming) model response: final text and/or tool calls.
pub struct Completion {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// A source of model completions.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Send the conversation plus available tools; get back text and/or tool calls.
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion>;
}
