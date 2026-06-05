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
            allowed_commands: load_allowed_commands(),
        })
    }
}

/// The commands `run_bash` may execute without prompting. Overridable with
/// `YODA_ALLOWED_COMMANDS` (comma-separated) for users who knowingly want to
/// auto-allow more (e.g. `git,cargo`).
fn load_allowed_commands() -> Vec<String> {
    match std::env::var("YODA_ALLOWED_COMMANDS") {
        Ok(v) => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => default_commands(),
    }
}

const SYSTEM_PROMPT: &str = "You are Yoda, a concise local coding assistant with tools. \
Available tools: read_file, write_file, edit_file, run_bash, glob_files and grep_files \
(find/search files in the project), web_search (search the web), web_fetch (fetch an \
http/https URL and read its text), and ask_user (ask the user a question). IMPORTANT: you \
DO have internet access — to research, CALL web_search to find pages and web_fetch to read \
them; never reply that you cannot access the internet or browse the web. If you are missing \
information or unsure what the user wants, CALL ask_user instead of guessing. Paths are \
relative to the project directory. Read a file before editing it. Use tools only when they \
help; otherwise answer directly. When the task is done, reply with a short final message and \
no tool call. Be brief.";

/// Default auto-allow set: only commands that neither execute arbitrary code
/// nor dump arbitrary file contents to the model. Notably EXCLUDED (so they
/// prompt): `cat`/`head`/`tail`/`grep`/`rg`/`wc` (could read any file, bypassing
/// the project-scoped read gate) and `find`/`git`/`cargo`/`rustc` (can execute
/// arbitrary code — e.g. `find -exec`, git hooks, cargo build scripts). For
/// in-project reading the model has the path-scoped read_file/grep_files tools.
fn default_commands() -> Vec<String> {
    [
        "ls", "pwd", "echo", "tree", "file", "which", "date", "whoami",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_auto_allow_excludes_dangerous_commands() {
        let cmds = default_commands();
        for dangerous in [
            "find", "git", "cargo", "rustc", "cat", "grep", "rg", "head", "tail",
        ] {
            assert!(
                !cmds.iter().any(|c| c == dangerous),
                "{dangerous} must not be auto-allowed"
            );
        }
    }
}
