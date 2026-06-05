//! The safety boundary. Every side-effecting tool call declares the `Action`s it
//! would perform; the `Policy` decides allow / ask / deny before anything runs.
//!
//! Phase 2 policy (allowlist by path/command, per the user's choice):
//!   - Reads & writes inside the project directory  → Allow
//!   - Reads & writes outside the project directory  → Ask
//!   - Shell commands whose program is on the allowlist
//!     AND that contain no shell chaining/redirection → Allow
//!   - Any other shell command                        → Ask

use std::path::{Component, Path, PathBuf};

/// A side effect a tool wants to perform.
#[derive(Debug, Clone)]
pub enum Action {
    Read(PathBuf),
    Write(PathBuf),
    Run(String),
    /// An outbound HTTP(S) request to a URL (network egress).
    Fetch(String),
    /// An opaque call to an external MCP tool, whose effects we can't classify.
    External {
        server: String,
        tool: String,
    },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Read(p) => format!("read {}", p.display()),
            Action::Write(p) => format!("write {}", p.display()),
            Action::Run(c) => format!("run `{c}`"),
            Action::Fetch(url) => format!("fetch {url}"),
            Action::External { server, tool } => format!("call MCP tool {server}/{tool}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// How aggressively the policy approves actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Ask before risky actions (the safe default).
    #[default]
    Normal,
    /// Auto-approve EVERYTHING, including otherwise-catastrophic commands.
    Auto,
    /// Allow reads; block all writes, commands, fetches, and external calls.
    ReadOnly,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_lowercase().as_str() {
            "normal" | "ask" => Some(Mode::Normal),
            "auto" | "yolo" => Some(Mode::Auto),
            "read-only" | "readonly" | "read" | "ro" => Some(Mode::ReadOnly),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Auto => "auto",
            Mode::ReadOnly => "read-only",
        }
    }
}

pub struct Policy {
    project_dir: PathBuf,
    allowed_commands: Vec<String>,
    mode: Mode,
}

impl Policy {
    pub fn new(project_dir: PathBuf, allowed_commands: Vec<String>) -> Self {
        Self {
            project_dir,
            allowed_commands,
            mode: Mode::Normal,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn check(&self, action: &Action) -> Decision {
        // Mode overrides come first.
        match self.mode {
            // The user explicitly chose "truly allow everything" for auto mode —
            // no exceptions, not even the catastrophic-command seatbelt.
            Mode::Auto => return Decision::Allow,
            Mode::ReadOnly => {
                return match action {
                    Action::Read(p) => {
                        if self.within_project(p) {
                            Decision::Allow
                        } else {
                            Decision::Ask
                        }
                    }
                    // No mutations, commands, network, or external calls.
                    _ => Decision::Deny,
                };
            }
            Mode::Normal => {}
        }

        match action {
            Action::Read(p) | Action::Write(p) => {
                if self.within_project(p) {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
            Action::Run(cmd) => {
                if is_catastrophic(cmd) {
                    Decision::Deny
                } else if self.command_allowed(cmd) {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
            // Network egress: show the user the URL and ask before fetching.
            Action::Fetch(_) => Decision::Ask,
            // External (MCP) tools can do anything; always ask before running.
            Action::External { .. } => Decision::Ask,
        }
    }

    /// True if `p` resolves to a location inside the project directory. Relative
    /// paths are resolved against the project dir; `..` is collapsed lexically so
    /// traversal attempts fall outside the prefix and require asking.
    fn within_project(&self, p: &Path) -> bool {
        let base = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_dir.join(p)
        };
        // Collapse `..`/`.` lexically, THEN resolve symlinks: an in-project
        // symlink pointing outside must not let access slip past this boundary.
        let resolved = resolve_symlinks(&normalize(&base));
        resolved.starts_with(&self.project_dir)
    }

    fn command_allowed(&self, cmd: &str) -> bool {
        // Refuse to auto-allow anything that chains or redirects: an allowlisted
        // `ls` in `ls; rm -rf ~` must NOT slip through on the `ls`.
        const UNSAFE: &[&str] = &[";", "&&", "||", "|", ">", "<", "`", "$(", "&", "\n"];
        if UNSAFE.iter().any(|m| cmd.contains(m)) {
            return false;
        }
        let Some(first) = cmd.split_whitespace().next() else {
            return false;
        };
        let program = first.rsplit('/').next().unwrap_or(first);
        self.allowed_commands.iter().any(|a| a == program)
    }
}

/// Best-effort hard block for unambiguously destructive commands — a backstop so
/// a distracted "y" at the prompt can't approve a system-wrecking command. This
/// is NOT a security boundary (it's pattern-based and bypassable); the real
/// boundary is that the model runs locally and you review what it asks to do.
fn is_catastrophic(cmd: &str) -> bool {
    let c = cmd.replace(' ', "");
    const PATTERNS: &[&str] = &[
        "rm-rf/",
        "rm-fr/",
        "rm-rf~",
        "rm-fr~",
        "rm-rf*",
        "mkfs",
        "dd if=",
        "ddif=",
        ":(){:|:&};:",
        ">/dev/sda",
        "chmod-R000",
        "rm-rf--no-preserve-root",
    ];
    PATTERNS.iter().any(|p| c.contains(&p.replace(' ', "")))
}

/// Resolve symlinks as far as the path exists: canonicalize the deepest
/// existing ancestor (resolving any symlinked directories) and re-append the
/// not-yet-existing tail. This detects a symlink escaping the project even for a
/// target file that doesn't exist yet (e.g. a write). Input is assumed lexically
/// normalized (no `..`/`.`), so the walked-off tail components are plain names.
fn resolve_symlinks(p: &Path) -> PathBuf {
    if let Ok(canonical) = p.canonicalize() {
        return canonical;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while let Some(parent) = cur.parent().map(Path::to_path_buf) {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(mut resolved) = parent.canonicalize() {
            while let Some(name) = tail.pop() {
                resolved.push(name);
            }
            return resolved;
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// Collapse `.` and `..` components without touching the filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(
            PathBuf::from("/home/me/project"),
            ["ls", "cargo", "git"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }

    /// A real, canonicalized temp directory to stand in for the project dir
    /// (the check now resolves symlinks, so the dir must actually exist — which
    /// it always does in production, where project_dir is canonicalized).
    fn real_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yoda_pol_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn reads_and_writes_inside_project_are_allowed() {
        let dir = real_project("in");
        let p = Policy::new(dir.clone(), vec![]);
        assert_eq!(
            p.check(&Action::Read("src/main.rs".into())),
            Decision::Allow
        );
        assert_eq!(
            p.check(&Action::Write("src/new.rs".into())),
            Decision::Allow
        );
        assert_eq!(
            p.check(&Action::Write(dir.join("a/b.txt"))),
            Decision::Allow
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn access_outside_project_asks() {
        let dir = real_project("out");
        let p = Policy::new(dir.clone(), vec![]);
        assert_eq!(p.check(&Action::Write("/etc/passwd".into())), Decision::Ask);
        // path traversal escaping the project must not be auto-allowed
        assert_eq!(
            p.check(&Action::Write("../../etc/passwd".into())),
            Decision::Ask
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowlisted_commands_run_chained_ones_ask() {
        let p = policy();
        assert_eq!(p.check(&Action::Run("ls -la".into())), Decision::Allow);
        assert_eq!(p.check(&Action::Run("cargo test".into())), Decision::Allow);
        // not on the allowlist
        assert_eq!(
            p.check(&Action::Run("curl example.com".into())),
            Decision::Ask
        );
        // allowlisted program but chained with something else: must not slip through
        assert_eq!(p.check(&Action::Run("ls; echo hi".into())), Decision::Ask);
        assert_eq!(p.check(&Action::Run("ls && curl x".into())), Decision::Ask);
        // and if the chained tail is catastrophic, it's denied outright (stronger)
        assert_eq!(p.check(&Action::Run("ls; rm -rf ~".into())), Decision::Deny);
    }

    #[test]
    fn auto_mode_allows_everything_including_catastrophic() {
        let mut p = policy();
        p.set_mode(Mode::Auto);
        assert_eq!(p.check(&Action::Run("rm -rf /".into())), Decision::Allow);
        assert_eq!(
            p.check(&Action::Write("/etc/passwd".into())),
            Decision::Allow
        );
        assert_eq!(
            p.check(&Action::Fetch("http://127.0.0.1".into())),
            Decision::Allow
        );
    }

    #[test]
    fn read_only_mode_blocks_mutations() {
        let dir = real_project("ro");
        let mut p = Policy::new(dir.clone(), vec!["ls".into()]);
        p.set_mode(Mode::ReadOnly);
        assert_eq!(p.check(&Action::Read("src/x.rs".into())), Decision::Allow);
        assert_eq!(p.check(&Action::Write("src/x.rs".into())), Decision::Deny);
        assert_eq!(p.check(&Action::Run("ls".into())), Decision::Deny);
        assert_eq!(
            p.check(&Action::Fetch("https://example.com".into())),
            Decision::Deny
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_parses_aliases() {
        assert_eq!(Mode::parse("yolo"), Some(Mode::Auto));
        assert_eq!(Mode::parse("read"), Some(Mode::ReadOnly));
        assert_eq!(Mode::parse("normal"), Some(Mode::Normal));
        assert_eq!(Mode::parse("nope"), None);
    }

    #[test]
    fn external_mcp_calls_always_ask() {
        let p = policy();
        let action = Action::External {
            server: "fs".into(),
            tool: "read".into(),
        };
        assert_eq!(p.check(&action), Decision::Ask);
    }

    #[test]
    fn web_fetch_asks() {
        let p = policy();
        assert_eq!(
            p.check(&Action::Fetch("https://example.com".into())),
            Decision::Ask
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_escaping_project_is_not_auto_allowed() {
        use std::os::unix::fs::symlink;
        let tmp = std::env::temp_dir().join(format!("yoda_perm_{}", std::process::id()));
        let project = tmp.join("project");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = project.join("escape");
        let _ = std::fs::remove_file(&link);
        symlink(&outside, &link).unwrap();

        let canonical_project = project.canonicalize().unwrap();
        let p = Policy::new(canonical_project.clone(), vec![]);
        // Reading through the symlink resolves outside the project → must ask.
        assert_eq!(
            p.check(&Action::Read(link.join("secret.txt"))),
            Decision::Ask
        );
        // A genuine in-project path is still auto-allowed.
        assert_eq!(
            p.check(&Action::Write(canonical_project.join("ok.txt"))),
            Decision::Allow
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn catastrophic_commands_are_hard_denied() {
        let p = policy();
        assert_eq!(p.check(&Action::Run("rm -rf /".into())), Decision::Deny);
        assert_eq!(
            p.check(&Action::Run("rm -rf --no-preserve-root /".into())),
            Decision::Deny
        );
        assert_eq!(
            p.check(&Action::Run(":(){ :|:& };:".into())),
            Decision::Deny
        );
    }
}
