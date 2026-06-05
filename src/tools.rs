//! The tool set the agent can call. Kept deliberately small (4 tools) — local
//! 7B models choose poorly when given many tools. Each tool declares the
//! `Action`s it would take so the permission gate can vet it before `run`.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::permission::Action;
use crate::provider::{ToolFunction, ToolSpec};

const MAX_OUTPUT_CHARS: usize = 20_000;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the tool's arguments.
    fn parameters(&self) -> Value;
    /// Side effects this call would cause, for the permission gate.
    fn actions(&self, args: &Value) -> Vec<Action>;
    async fn run(&self, args: &Value) -> Result<String>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                kind: "function".into(),
                function: ToolFunction {
                    name: t.name().into(),
                    description: t.description().into(),
                    parameters: t.parameters(),
                },
            })
            .collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }
}

pub fn default_registry() -> ToolRegistry {
    ToolRegistry {
        tools: vec![
            Box::new(ReadFile),
            Box::new(WriteFile),
            Box::new(EditFile),
            Box::new(RunBash),
        ],
    }
}

// --- helpers ---

fn get_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing string argument '{key}'"))
}

fn opt_path(args: &Value, key: &str) -> Vec<Action> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(p) => vec![Action::Write(PathBuf::from(p))],
        None => vec![],
    }
}

fn truncate(s: String) -> String {
    if s.chars().count() <= MAX_OUTPUT_CHARS {
        return s;
    }
    let cut: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
    format!("{cut}\n…[output truncated]")
}

// --- tools ---

struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file and return its contents. Path is relative to the project directory."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read." }
            },
            "required": ["path"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => vec![Action::Read(PathBuf::from(p))],
            None => vec![],
        }
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let path = get_str(args, "path")?;
        let content =
            std::fs::read_to_string(&path).map_err(|e| anyhow!("could not read {path}: {e}"))?;
        Ok(truncate(content))
    }
}

struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Path is relative to the project directory."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write." },
                "content": { "type": "string", "description": "Full file content." }
            },
            "required": ["path", "content"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        opt_path(args, "path")
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let path = get_str(args, "path")?;
        let content = get_str(args, "content")?;
        if let Some(parent) = Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, &content).map_err(|e| anyhow!("could not write {path}: {e}"))?;
        Ok(format!("Wrote {} bytes to {path}", content.len()))
    }
}

struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace the first occurrence of old_string with new_string in a file. old_string must match exactly."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit." },
                "old_string": { "type": "string", "description": "Exact text to find." },
                "new_string": { "type": "string", "description": "Replacement text." }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        opt_path(args, "path")
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let path = get_str(args, "path")?;
        let old = get_str(args, "old_string")?;
        let new = get_str(args, "new_string")?;
        let content =
            std::fs::read_to_string(&path).map_err(|e| anyhow!("could not read {path}: {e}"))?;
        let count = content.matches(&old).count();
        if count == 0 {
            return Err(anyhow!("old_string not found in {path}"));
        }
        let updated = content.replacen(&old, &new, 1);
        std::fs::write(&path, &updated).map_err(|e| anyhow!("could not write {path}: {e}"))?;
        Ok(format!(
            "Edited {path} (replaced 1 of {count} occurrence(s))"
        ))
    }
}

struct RunBash;

#[async_trait]
impl Tool for RunBash {
    fn name(&self) -> &str {
        "run_bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the project directory and return its combined stdout/stderr and exit code."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run." }
            },
            "required": ["command"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => vec![Action::Run(c.to_string())],
            None => vec![],
        }
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let command = get_str(args, "command")?;
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .output()
            .map_err(|e| anyhow!("could not run command: {e}"))?;
        let mut out = String::new();
        out.push_str(&String::from_utf8_lossy(&output.stdout));
        if !output.stderr.is_empty() {
            out.push_str("\n[stderr]\n");
            out.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        let code = output.status.code().unwrap_or(-1);
        Ok(truncate(format!("(exit {code})\n{}", out.trim_end())))
    }
}
