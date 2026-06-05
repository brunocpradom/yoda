//! The tool set the agent can call. Kept deliberately small — local 7B models
//! choose poorly when given many tools. Each tool declares the `Action`s it
//! would take so the permission gate can vet it before `run`. Current tools:
//! read_file, write_file, edit_file, run_bash, glob_files, grep_files, web_fetch.

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

    /// Add a tool at runtime (used to register MCP-provided tools).
    pub fn push(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }
}

pub fn default_registry(project_dir: PathBuf) -> ToolRegistry {
    ToolRegistry {
        tools: vec![
            Box::new(ReadFile),
            Box::new(WriteFile),
            Box::new(EditFile),
            Box::new(RunBash),
            Box::new(GlobFiles {
                root: project_dir.clone(),
            }),
            Box::new(GrepFiles { root: project_dir }),
            Box::new(WebFetch),
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

/// Normalize a user/model-supplied URL for `web_fetch`: accept http(s) as-is,
/// reject other explicit schemes (e.g. `file:`), and assume `https://` for a
/// bare host like `www.example.com` so the model doesn't have to remember it.
fn normalize_url(input: &str) -> Result<String> {
    let u = input.trim();
    if u.is_empty() {
        Err(anyhow!("empty URL"))
    } else if u.starts_with("http://") || u.starts_with("https://") {
        Ok(u.to_string())
    } else if u.contains("://") {
        Err(anyhow!("only http and https URLs are supported"))
    } else {
        Ok(format!("https://{u}"))
    }
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

// --- search tools (read-only, scoped to the project directory) ---

const MAX_MATCHES: usize = 200;
const MAX_GREP_FILE_BYTES: u64 = 1_000_000;

/// True if any path component is a directory we never want to search.
fn is_ignored(p: &Path) -> bool {
    p.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git") | Some("target") | Some("node_modules")
        )
    })
}

/// Expand a glob pattern under `root` (relative patterns are anchored to it),
/// keeping only results inside `root` and outside ignored directories.
fn glob_in_root(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let full = if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        format!("{}/{}", root.display(), pattern)
    };
    let Ok(paths) = glob::glob(&full) else {
        return vec![];
    };
    paths
        .flatten()
        .filter(|p| p.starts_with(root) && !is_ignored(p))
        .collect()
}

struct GlobFiles {
    root: PathBuf,
}

#[async_trait]
impl Tool for GlobFiles {
    fn name(&self) -> &str {
        "glob_files"
    }
    fn description(&self) -> &str {
        "List files in the project matching a glob pattern (e.g. 'src/**/*.rs'). Read-only."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. '**/*.rs'." }
            },
            "required": ["pattern"]
        })
    }
    fn actions(&self, _args: &Value) -> Vec<Action> {
        vec![] // listing filenames inside the project is safe
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let pattern = get_str(args, "pattern")?;
        let mut files: Vec<String> = glob_in_root(&self.root, &pattern)
            .into_iter()
            .filter(|p| p.is_file())
            .map(|p| {
                p.strip_prefix(&self.root)
                    .unwrap_or(&p)
                    .display()
                    .to_string()
            })
            .collect();
        files.sort();
        files.truncate(MAX_MATCHES);
        if files.is_empty() {
            Ok(format!("No files match '{pattern}'."))
        } else {
            Ok(truncate(files.join("\n")))
        }
    }
}

struct GrepFiles {
    root: PathBuf,
}

#[async_trait]
impl Tool for GrepFiles {
    fn name(&self) -> &str {
        "grep_files"
    }
    fn description(&self) -> &str {
        "Search project file contents for a literal substring. Returns 'path:line: text' matches. Read-only."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Literal text to search for." },
                "path_glob": { "type": "string", "description": "Optional glob to limit files, default '**/*'." },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive match, default false." }
            },
            "required": ["query"]
        })
    }
    fn actions(&self, _args: &Value) -> Vec<Action> {
        vec![] // reads are confined to the project directory, already auto-allowed
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let query = get_str(args, "query")?;
        let ignore_case = args
            .get("ignore_case")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let path_glob = args
            .get("path_glob")
            .and_then(|v| v.as_str())
            .unwrap_or("**/*");

        let needle = if ignore_case {
            query.to_lowercase()
        } else {
            query.clone()
        };

        let mut results = Vec::new();
        'files: for file in glob_in_root(&self.root, path_glob) {
            if !file.is_file() {
                continue;
            }
            if file.metadata().map(|m| m.len()).unwrap_or(0) > MAX_GREP_FILE_BYTES {
                continue;
            }
            // read_to_string fails on non-UTF-8 (binary) files — we skip those.
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let rel = file.strip_prefix(&self.root).unwrap_or(&file);
            for (n, line) in content.lines().enumerate() {
                let hay = if ignore_case {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if hay.contains(&needle) {
                    results.push(format!("{}:{}: {}", rel.display(), n + 1, line.trim()));
                    if results.len() >= MAX_MATCHES {
                        break 'files;
                    }
                }
            }
        }

        if results.is_empty() {
            Ok(format!("No matches for '{query}'."))
        } else {
            Ok(truncate(results.join("\n")))
        }
    }
}

// --- web fetch (network egress, gated by the permission policy) ---

/// Column width html2text wraps to. Wide enough to avoid chopping sentences,
/// narrow enough to stay readable.
const FETCH_WRAP_WIDTH: usize = 100;

struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch an http/https URL and return its main text content with HTML markup stripped. Use it to read documentation or research a topic on the web."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch." }
            },
            "required": ["url"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        // Show the normalized URL in the permission prompt when we can.
        match args.get("url").and_then(|v| v.as_str()) {
            Some(u) => vec![Action::Fetch(
                normalize_url(u).unwrap_or_else(|_| u.to_string()),
            )],
            None => vec![],
        }
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let url = normalize_url(&get_str(args, "url")?)?;

        let client = reqwest::Client::builder()
            .user_agent("yoda/0.1 (+web_fetch)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| anyhow!("could not build HTTP client: {e}"))?;

        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("could not fetch {url}: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("{url} returned an error: {e}"))?;

        let is_html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("html"))
            .unwrap_or(false);

        let body = response
            .text()
            .await
            .map_err(|e| anyhow!("could not read body of {url}: {e}"))?;

        let text = if is_html {
            html2text::from_read(body.as_bytes(), FETCH_WRAP_WIDTH)
                .map_err(|e| anyhow!("could not parse HTML from {url}: {e}"))?
        } else {
            body
        };

        let trimmed = text.trim();
        if trimmed.is_empty() {
            Ok(format!("(fetched {url} but it had no readable text)"))
        } else {
            Ok(truncate(format!("Source: {url}\n\n{trimmed}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_keeps_http_and_https() {
        assert_eq!(normalize_url("https://x.com/y").unwrap(), "https://x.com/y");
        assert_eq!(normalize_url("http://x.com").unwrap(), "http://x.com");
    }

    #[test]
    fn normalize_url_prepends_https_for_bare_host() {
        assert_eq!(
            normalize_url("www.example.com").unwrap(),
            "https://www.example.com"
        );
        assert_eq!(
            normalize_url("example.com/path").unwrap(),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_rejects_other_schemes_and_empty() {
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("   ").is_err());
    }

    // Network-dependent; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn web_fetch_strips_html_to_text() {
        let out = WebFetch
            .run(&json!({ "url": "https://example.com" }))
            .await
            .unwrap();
        assert!(out.contains("Example Domain"), "got: {out}");
        assert!(!out.contains("<html"), "markup not stripped: {out}");
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_scheme() {
        let err = WebFetch
            .run(&json!({ "url": "file:///etc/passwd" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http"));
    }
}
