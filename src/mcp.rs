//! Minimal MCP (Model Context Protocol) client over the stdio transport.
//!
//! Scope kept deliberately small (simpler-is-better): we spawn each configured
//! MCP server as a subprocess and speak newline-delimited JSON-RPC 2.0 over its
//! stdin/stdout. Because the agent calls one tool at a time, request/response
//! correlation is sequential and blocking — no async machinery needed. Each
//! remote tool is wrapped as a `Tool` and added to the registry; calls are
//! always routed through the permission gate (see `Action::External`).
//!
//! Config: a `mcp.json` in the project dir or `~/.yoda/mcp.json`, shaped like
//! `{ "servers": { "name": { "command": "...", "args": ["..."] } } }`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::permission::Action;
use crate::tools::{Tool, ToolRegistry};

/// Protocol version we advertise. `2024-11-05` is broadly accepted by servers;
/// bump if a server requires a newer revision.
const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A live connection to one MCP server subprocess.
pub struct McpClient {
    name: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    pub fn connect(
        name: &str,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<McpClient> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start MCP server '{name}' ({command})"))?;

        let stdin = child.stdin.take().context("MCP child has no stdin")?;
        let stdout = child.stdout.take().context("MCP child has no stdout")?;

        let mut client = McpClient {
            name: name.to_string(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "yoda", "version": "0.1.0" }
        });
        self.request("initialize", params)?;
        // The spec requires this notification after a successful initialize.
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(())
    }

    pub fn list_tools(&mut self) -> Result<Vec<McpToolDef>> {
        let result = self.request("tools/list", Value::Null)?;
        Ok(parse_tools(&result))
    }

    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        let result = self.request("tools/call", json!({ "name": name, "arguments": args }))?;
        Ok(extract_text(&result))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send(&msg)?;
        self.read_result(id)
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let line = serde_json::to_string(msg)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Read JSON-RPC messages until the response matching `id` arrives, skipping
    /// any interleaved notifications.
    fn read_result(&mut self, id: i64) -> Result<Value> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(anyhow!("MCP server '{}' closed the connection", self.name));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                continue; // ignore non-JSON log noise
            };
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(anyhow!("MCP server '{}' error: {err}", self.name));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // otherwise a notification or unrelated message — keep reading
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_tools(result: &Value) -> Vec<McpToolDef> {
    result
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?.to_string();
                    let description = t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input_schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" }));
                    Some(McpToolDef {
                        name,
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Flatten an MCP `tools/call` result into plain text for the model.
fn extract_text(result: &Value) -> String {
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut text = String::new();
    if let Some(blocks) = result.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
    }
    if text.is_empty() {
        text = result.to_string();
    }
    if is_error {
        format!("[tool error] {text}")
    } else {
        text
    }
}

// --- tool wrapper ---

struct McpTool {
    client: Arc<Mutex<McpClient>>,
    server: String,
    def: McpToolDef,
    qualified: String,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn parameters(&self) -> Value {
        self.def.input_schema.clone()
    }
    fn actions(&self, _args: &Value) -> Vec<Action> {
        // We can't classify an external tool's effects, so always ask.
        vec![Action::External {
            server: self.server.clone(),
            tool: self.def.name.clone(),
        }]
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| anyhow!("MCP client lock poisoned"))?;
        client.call_tool(&self.def.name, args)
    }
}

// --- config & setup ---

#[derive(Debug, Deserialize)]
struct McpConfig {
    #[serde(default)]
    servers: BTreeMap<String, ServerSpec>,
}

#[derive(Debug, Deserialize)]
struct ServerSpec {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// Extra environment variables for the server process (e.g. per-server
    /// credentials). Inherited on top of Yoda's own environment.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// First existing config among `<project>/mcp.json` and `~/.yoda/mcp.json`.
fn config_path(project_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        project_dir.join("mcp.json"),
        crate::session::sessions_dir()
            .parent()
            .map(|p| p.join("mcp.json"))
            .unwrap_or_default(),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// Connect every configured MCP server and register its tools. Returns a short
/// status line per server for the banner. Failures are reported but non-fatal.
pub fn setup(project_dir: &Path, registry: &mut ToolRegistry) -> Vec<String> {
    let Some(path) = config_path(project_dir) else {
        return vec![];
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let config: McpConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("MCP: ignoring {} (parse error: {e})", path.display());
            return vec![];
        }
    };

    let mut status = Vec::new();
    for (name, spec) in config.servers {
        match connect_and_register(&name, &spec, registry) {
            Ok(count) => status.push(format!("{name} ({count} tools)")),
            Err(e) => eprintln!("MCP '{name}': {e}"),
        }
    }
    status
}

fn connect_and_register(
    name: &str,
    spec: &ServerSpec,
    registry: &mut ToolRegistry,
) -> Result<usize> {
    let mut client = McpClient::connect(name, &spec.command, &spec.args, &spec.env)?;
    let defs = client.list_tools()?;
    let shared = Arc::new(Mutex::new(client));
    let count = defs.len();
    for def in defs {
        let qualified = format!("{name}__{}", def.name);
        registry.push(Box::new(McpTool {
            client: Arc::clone(&shared),
            server: name.to_string(),
            def,
            qualified,
        }));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_list() {
        let result = json!({
            "tools": [
                { "name": "echo", "description": "Echo text",
                  "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } },
                { "name": "noschema" }
            ]
        });
        let tools = parse_tools(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo text");
        // missing schema defaults to an empty object schema
        assert_eq!(tools[1].input_schema, json!({ "type": "object" }));
    }

    #[test]
    fn extracts_and_joins_text_content() {
        let result = json!({ "content": [
            { "type": "text", "text": "hello" },
            { "type": "text", "text": "world" }
        ]});
        assert_eq!(extract_text(&result), "hello\nworld");
    }

    #[test]
    fn flags_error_results() {
        let result = json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] });
        assert!(extract_text(&result).starts_with("[tool error]"));
    }
}
