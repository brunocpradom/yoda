//! The `gmail_*` tools the agent (and artoo's sensors) can call. Each wraps a
//! [`Gmail`] client call and renders its result as compact, model-readable text.
//! They are only registered when Gmail is configured (see [`gmail_tools`]).

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::client::Gmail;
use crate::permission::Action;
use crate::tools::Tool;

/// Cap tool output so a huge thread can't blow the context window.
const MAX_OUTPUT_CHARS: usize = 12_000;
const DEFAULT_SEARCH_MAX: u32 = 10;
const MAX_SEARCH_MAX: u32 = 25;

/// The Gmail tools, or an empty list when Gmail isn't configured (no token and
/// no client credentials) — keeping the model's tool set small in that case.
pub fn gmail_tools() -> Vec<Box<dyn Tool>> {
    if !super::auth::is_configured() {
        return Vec::new();
    }
    vec![
        Box::new(GmailSearch),
        Box::new(GmailRead),
        Box::new(GmailSend),
        Box::new(GmailDelete),
    ]
}

fn get_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing string argument '{key}'"))
}

fn truncate(mut s: String) -> String {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        s = s.chars().take(MAX_OUTPUT_CHARS).collect::<String>() + "\n…[truncated]";
    }
    s
}

// --- gmail_search -------------------------------------------------------------

struct GmailSearch;

#[async_trait]
impl Tool for GmailSearch {
    fn name(&self) -> &str {
        "gmail_search"
    }
    fn description(&self) -> &str {
        "Search the user's Gmail with a Gmail query (e.g. 'is:unread', 'from:alice newer_than:2d') and return matching threads as 'id — From — Subject (date): snippet'. Use the returned id with gmail_read, gmail_delete."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Gmail search query, e.g. 'is:unread'." },
                "max_results": { "type": "integer", "description": "How many threads to return (default 10, max 25)." }
            },
            "required": ["query"]
        })
    }
    fn actions(&self, _args: &Value) -> Vec<Action> {
        // Network egress carrying private mail data — gate like a fetch.
        vec![Action::Fetch(
            "https://gmail.googleapis.com (search)".into(),
        )]
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let query = get_str(args, "query")?;
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|n| (n as u32).clamp(1, MAX_SEARCH_MAX))
            .unwrap_or(DEFAULT_SEARCH_MAX);

        let threads = Gmail::new()?.search(&query, max).await?;
        if threads.is_empty() {
            return Ok(format!("No threads match '{query}'."));
        }
        let mut out = format!("{} thread(s) for '{query}':\n", threads.len());
        for t in &threads {
            out.push_str(&format!(
                "- {} — {} — {} ({}): {}\n",
                t.id,
                t.from,
                t.subject,
                t.date,
                t.snippet.chars().take(120).collect::<String>(),
            ));
        }
        Ok(truncate(out))
    }
}

// --- gmail_read ---------------------------------------------------------------

struct GmailRead;

#[async_trait]
impl Tool for GmailRead {
    fn name(&self) -> &str {
        "gmail_read"
    }
    fn description(&self) -> &str {
        "Read a full Gmail thread by its id (from gmail_search): every message's From/Date/Subject and plain-text body."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "Thread id from gmail_search." }
            },
            "required": ["thread_id"]
        })
    }
    fn actions(&self, _args: &Value) -> Vec<Action> {
        vec![Action::Fetch("https://gmail.googleapis.com (read)".into())]
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let thread_id = get_str(args, "thread_id")?;
        Ok(truncate(Gmail::new()?.read_thread(&thread_id).await?))
    }
}

// --- gmail_send ---------------------------------------------------------------

struct GmailSend;

#[async_trait]
impl Tool for GmailSend {
    fn name(&self) -> &str {
        "gmail_send"
    }
    fn description(&self) -> &str {
        "Send a plain-text email from the user's Gmail account. Requires explicit confirmation."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Recipient address(es), comma-separated." },
                "subject": { "type": "string", "description": "Subject line." },
                "body": { "type": "string", "description": "Plain-text body." },
                "cc": { "type": "string", "description": "Optional Cc address(es)." },
                "bcc": { "type": "string", "description": "Optional Bcc address(es)." }
            },
            "required": ["to", "subject", "body"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        // Sending is a consequential, outbound side effect — always ask.
        let to = args.get("to").and_then(Value::as_str).unwrap_or("?");
        vec![Action::External {
            server: "gmail".into(),
            tool: format!("send → {to}"),
        }]
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let to = get_str(args, "to")?;
        let subject = get_str(args, "subject")?;
        let body = get_str(args, "body")?;
        let cc = args.get("cc").and_then(Value::as_str);
        let bcc = args.get("bcc").and_then(Value::as_str);
        let id = Gmail::new()?.send(&to, &subject, &body, cc, bcc).await?;
        Ok(format!("Sent to {to} (message id {id})."))
    }
}

// --- gmail_delete -------------------------------------------------------------

struct GmailDelete;

#[async_trait]
impl Tool for GmailDelete {
    fn name(&self) -> &str {
        "gmail_delete"
    }
    fn description(&self) -> &str {
        "Delete a Gmail thread by id. By default moves it to Trash (reversible); pass permanent=true to delete it forever. Requires explicit confirmation."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "Thread id from gmail_search." },
                "permanent": { "type": "boolean", "description": "If true, delete forever instead of trashing. Default false." }
            },
            "required": ["thread_id"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        let permanent = args
            .get("permanent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let id = args.get("thread_id").and_then(Value::as_str).unwrap_or("?");
        vec![Action::External {
            server: "gmail".into(),
            tool: if permanent {
                format!("PERMANENTLY DELETE thread {id}")
            } else {
                format!("trash thread {id}")
            },
        }]
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let thread_id = get_str(args, "thread_id")?;
        let permanent = args
            .get("permanent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let gmail = Gmail::new()?;
        if permanent {
            gmail.delete_thread(&thread_id).await?;
            Ok(format!("Permanently deleted thread {thread_id}."))
        } else {
            gmail.trash_thread(&thread_id).await?;
            Ok(format!("Moved thread {thread_id} to Trash."))
        }
    }
}
