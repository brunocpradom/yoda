//! `Provider` implementation backed by an OpenAI-compatible HTTP endpoint —
//! served locally by Ollama at `http://localhost:11434/v1`. Using the
//! OpenAI-compatible shape means the same code also works against llama.cpp's
//! `llama-server`, vLLM, or LM Studio.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use super::{Completion, Message, Provider, ToolSpec};

pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }
}

#[derive(Deserialize)]
struct CompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[async_trait::async_trait]
impl Provider for OllamaProvider {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), serde_json::json!(self.model));
        body.insert("messages".into(), serde_json::to_value(messages)?);
        body.insert("stream".into(), serde_json::json!(false));
        if !tools.is_empty() {
            body.insert("tools".into(), serde_json::to_value(tools)?);
        }

        let response = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .context("could not reach the model server — is `ollama serve` running?")?
            .error_for_status()
            .context("model server returned an error (is the model pulled?)")?;

        let parsed: CompletionResponse = response
            .json()
            .await
            .context("could not parse model response")?;

        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow!("model returned no choices"))?;

        Ok(Completion {
            content: message.content,
            tool_calls: message.tool_calls.unwrap_or_default(),
        })
    }
}
