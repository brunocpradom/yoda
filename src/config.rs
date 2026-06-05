//! Harness configuration. Phase 2 keeps this in code with environment-variable
//! overrides; later phases move it to a TOML file (model routing, fuller
//! permission config, skills dir — see DESIGN.md §5).

use std::path::PathBuf;

use anyhow::{Context, Result};

pub struct Config {
    pub base_url: String,
    pub model: String,
    pub system_prompt: String,
    /// Canonical project directory; the permission gate auto-allows file access
    /// inside it.
    pub project_dir: PathBuf,
    /// Programs the model may run without asking (subject to no shell chaining).
    pub allowed_commands: Vec<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let project_dir = std::env::current_dir()
            .context("could not read current directory")?
            .canonicalize()
            .context("could not canonicalize current directory")?;

        Ok(Self {
            base_url: std::env::var("YODA_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:11434".into()),
            model: std::env::var("YODA_MODEL").unwrap_or_else(|_| "qwen2.5-coder:7b".into()),
            system_prompt: SYSTEM_PROMPT.into(),
            project_dir,
            allowed_commands: default_commands(),
        })
    }
}

const SYSTEM_PROMPT: &str = "You are Yoda, a concise local coding assistant with tools. \
You can read_file, write_file, edit_file, and run_bash. Paths are relative to the project \
directory. Read a file before editing it. Use tools only when they help; otherwise answer \
directly. When the task is done, reply with a short final message and no tool call. Be brief.";

fn default_commands() -> Vec<String> {
    [
        "ls", "pwd", "echo", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "file",
        "which", "date", "whoami", "git", "cargo", "rustc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
