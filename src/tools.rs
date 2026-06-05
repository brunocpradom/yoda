//! The tool set the agent can call. Kept deliberately small — local 7B models
//! choose poorly when given many tools. Each tool declares the `Action`s it
//! would take so the permission gate can vet it before `run`. Current tools:
//! read_file, write_file, edit_file, run_bash, glob_files, grep_files,
//! web_fetch, web_search, ask_user.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::net::ToSocketAddrs;
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
            Box::new(WebSearch),
            Box::new(AskUser),
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

/// SSRF guard for `web_fetch`: refuse loopback/link-local/private targets
/// (e.g. `localhost`, `127.0.0.1`, cloud metadata `169.254.169.254`, `10/8`).
/// Set `YODA_ALLOW_LOCAL_FETCH=1` to permit them (e.g. a local dev server).
fn guard_fetch_target(u: &reqwest::Url) -> Result<()> {
    if std::env::var_os("YODA_ALLOW_LOCAL_FETCH").is_some() {
        return Ok(());
    }
    let host = u.host_str().ok_or_else(|| anyhow!("URL has no host"))?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err(anyhow!(
            "refusing to fetch localhost (set YODA_ALLOW_LOCAL_FETCH=1 to allow)"
        ));
    }
    let port = u.port_or_known_default().unwrap_or(80);
    let mut resolved_any = false;
    for addr in (host, port)
        .to_socket_addrs()
        .map_err(|e| anyhow!("could not resolve host '{host}': {e}"))?
    {
        resolved_any = true;
        if is_blocked_ip(addr.ip()) {
            return Err(anyhow!(
                "refusing to fetch a private/loopback/link-local address ({}); set YODA_ALLOW_LOCAL_FETCH=1 to override",
                addr.ip()
            ));
        }
    }
    if !resolved_any {
        return Err(anyhow!("could not resolve host '{host}'"));
    }
    Ok(())
}

fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || v6
                    .to_ipv4_mapped()
                    .map(|m| m.is_loopback() || m.is_private() || m.is_link_local())
                    .unwrap_or(false)
        }
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
        let parsed = reqwest::Url::parse(&url).map_err(|e| anyhow!("invalid URL: {e}"))?;
        guard_fetch_target(&parsed)?;

        let client = reqwest::Client::builder()
            .user_agent("yoda/0.1 (+web_fetch)")
            .timeout(std::time::Duration::from_secs(20))
            // Re-check every redirect hop so a public URL can't bounce us to an
            // internal address (SSRF via redirect).
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 || guard_fetch_target(attempt.url()).is_err() {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| anyhow!("could not build HTTP client: {e}"))?;

        let response = client
            .get(parsed)
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

// --- ask the user ---

struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the user a question when you are missing information or unsure how to proceed. Returns the user's typed answer. Prefer this over guessing."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to ask the user." }
            },
            "required": ["question"]
        })
    }
    fn actions(&self, _args: &Value) -> Vec<Action> {
        vec![] // asking a question is not a side effect — no permission needed
    }
    async fn run(&self, args: &Value) -> Result<String> {
        use std::io::Write;
        let question = get_str(args, "question")?;
        println!("\n{} {question}", crate::ui::ask_label());
        print!("{}", crate::ui::answer_prompt());
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| anyhow!("could not read answer: {e}"))?;
        println!();
        let answer = line.trim();
        if answer.is_empty() {
            Ok("The user gave no answer.".to_string())
        } else {
            Ok(format!("The user answered: {answer}"))
        }
    }
}

// --- web search (DuckDuckGo) ---

const SEARCH_RESULTS: usize = 8;

struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web and return the top results as 'title — url'. Use it to find pages on a topic, then read one with web_fetch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." }
            },
            "required": ["query"]
        })
    }
    fn actions(&self, args: &Value) -> Vec<Action> {
        // Network egress (the query goes to DuckDuckGo) — gate like a fetch.
        match args.get("query").and_then(|v| v.as_str()).map(ddg_url) {
            Some(Ok(u)) => vec![Action::Fetch(u.to_string())],
            _ => vec![],
        }
    }
    async fn run(&self, args: &Value) -> Result<String> {
        let query = get_str(args, "query")?;
        let url = ddg_url(&query)?;
        guard_fetch_target(&url)?;

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; yoda/0.1; +web_search)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| anyhow!("could not build HTTP client: {e}"))?;

        let html = client
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow!("search request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("search returned an error: {e}"))?
            .text()
            .await
            .map_err(|e| anyhow!("could not read search results: {e}"))?;

        let results = parse_ddg(&html);
        if results.is_empty() {
            return Ok(format!(
                "No results parsed for '{query}'. The search page may have changed; try web_fetch on a specific URL instead."
            ));
        }
        let mut out = format!("Search results for '{query}':\n");
        for (i, (title, link, snippet)) in results.iter().enumerate() {
            out.push_str(&format!("{}. {title} — {link}\n", i + 1));
            if !snippet.is_empty() {
                let s: String = snippet.chars().take(220).collect();
                out.push_str(&format!("   {s}\n"));
            }
        }
        Ok(truncate(out))
    }
}

/// Build the DuckDuckGo HTML-results URL for a query.
fn ddg_url(query: &str) -> Result<reqwest::Url> {
    let mut u = reqwest::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|e| anyhow!("bad search URL: {e}"))?;
    u.query_pairs_mut().append_pair("q", query);
    Ok(u)
}

/// Extract `(title, url)` pairs from a DuckDuckGo HTML results page. Best-effort
/// scraping: DuckDuckGo wraps each result link in a `result__a` anchor whose
/// `href` is a `/l/?uddg=<real-url>` redirect.
fn parse_ddg(html: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for seg in html.split("class=\"result__a\"").skip(1) {
        let Some(href) = slice_between(seg, "href=\"", "\"") else {
            continue;
        };
        let title = slice_between(seg, ">", "</a>")
            .map(|t| decode_entities(&strip_tags(&t)))
            .unwrap_or_default();
        // The snippet follows the title anchor, in the same result block.
        let snippet = seg
            .split_once("result__snippet")
            .and_then(|(_, after)| slice_between(after, ">", "</a>"))
            .map(|t| decode_entities(&strip_tags(&t)))
            .unwrap_or_default();
        if let Some(link) = decode_ddg_redirect(&href)
            && !title.is_empty()
        {
            out.push((title, link, snippet));
        }
        if out.len() >= SEARCH_RESULTS {
            break;
        }
    }
    out
}

fn slice_between(s: &str, start: &str, end: &str) -> Option<String> {
    let from = s.find(start)? + start.len();
    let rest = &s[from..];
    let to = rest.find(end)?;
    Some(rest[..to].to_string())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
}

/// Turn a DuckDuckGo `//duckduckgo.com/l/?uddg=<encoded>` redirect into the real
/// target URL; pass through plain http(s) links unchanged.
fn decode_ddg_redirect(href: &str) -> Option<String> {
    let full = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => href.to_string(),
    };
    let u = reqwest::Url::parse(&full).ok()?;
    if let Some((_, v)) = u.query_pairs().find(|(k, _)| k == "uddg") {
        Some(v.into_owned())
    } else if full.starts_with("http") {
        Some(full)
    } else {
        None
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

    #[test]
    fn web_fetch_blocks_private_and_loopback_targets() {
        let blocked = |u: &str| guard_fetch_target(&reqwest::Url::parse(u).unwrap()).is_err();
        assert!(blocked("http://127.0.0.1/"));
        assert!(blocked("http://169.254.169.254/latest/meta-data/")); // cloud metadata
        assert!(blocked("http://10.0.0.5/"));
        assert!(blocked("http://192.168.1.1/"));
        assert!(blocked("http://172.16.9.9/"));
        assert!(blocked("http://localhost/"));
        assert!(blocked("http://[::1]/"));
        // A public IP literal is allowed (resolves without DNS, not in a blocked range).
        assert!(!blocked("http://93.184.216.34/"));
    }

    #[test]
    fn parses_duckduckgo_results() {
        let html = concat!(
            r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&amp;rut=x">Example &amp; <b>A</b></a>"#,
            r#"<a class="result__snippet" href="x">A <b>snippet</b> about example.</a>"#,
            r#" junk <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">Rust</a>"#,
        );
        let r = parse_ddg(html);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, "Example & A");
        assert_eq!(r[0].1, "https://example.com/a");
        assert_eq!(r[0].2, "A snippet about example.");
        assert_eq!(r[1].1, "https://www.rust-lang.org/");
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn web_search_returns_results() {
        let out = WebSearch
            .run(&json!({ "query": "rust programming language" }))
            .await
            .unwrap();
        assert!(out.contains("http"), "got: {out}");
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
