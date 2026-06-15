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
    /// Context window (in tokens) requested from the model server per call.
    /// Ollama defaults to 4096, which is too small for agentic work — the
    /// system prompt + tool specs + a couple of file reads overflow it and the
    /// model "forgets" earlier turns (including that it has tools).
    pub num_ctx: u32,
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
            model: std::env::var("YODA_MODEL").unwrap_or_else(|_| "qwen3:14b".into()),
            system_prompt: SYSTEM_PROMPT.into(),
            project_dir,
            allowed_commands: load_allowed_commands(),
            num_ctx: load_num_ctx()?,
        })
    }
}

/// Context window per request, overridable with `YODA_NUM_CTX`. 16k is a
/// deliberate default for agentic use on 16 GB machines: roughly +1.5 GB of
/// KV cache on a 7–8B model, with enough room that tool specs and file reads
/// don't push earlier turns out of the window. A malformed override is an
/// error, not a silent fallback — the user asked for a specific value.
fn load_num_ctx() -> Result<u32> {
    match std::env::var("YODA_NUM_CTX") {
        Ok(v) => v
            .trim()
            .parse()
            .with_context(|| format!("YODA_NUM_CTX must be a positive integer, got {v:?}")),
        Err(_) => Ok(16384),
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
DO have filesystem access through your tools. When the user asks you to look at, summarize, \
explore, or review a repository, directory, or file, CALL the tools RIGHT AWAY — start with \
glob_files or run_bash with ls to see the structure, then read_file the relevant files. \
Never reply that you cannot access files or that the user must paste file contents. \
IMPORTANT: you also DO have internet access. When the user asks you to search the web, find or look something up, \
or asks about current/online information, CALL web_search RIGHT AWAY — do not ask what they \
want and do not answer from memory. Use web_fetch to read a specific page. Never reply that you \
cannot access the internet. Only call ask_user when the request is genuinely ambiguous; a clear \
instruction like 'search the web for X' you must simply carry out. Paths are \
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
