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

pub struct Policy {
    project_dir: PathBuf,
    allowed_commands: Vec<String>,
}

impl Policy {
    pub fn new(project_dir: PathBuf, allowed_commands: Vec<String>) -> Self {
        Self {
            project_dir,
            allowed_commands,
        }
    }

    pub fn check(&self, action: &Action) -> Decision {
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
        normalize(&base).starts_with(&self.project_dir)
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

    #[test]
    fn reads_and_writes_inside_project_are_allowed() {
        let p = policy();
        assert_eq!(
            p.check(&Action::Read("src/main.rs".into())),
            Decision::Allow
        );
        assert_eq!(
            p.check(&Action::Write("src/new.rs".into())),
            Decision::Allow
        );
        assert_eq!(
            p.check(&Action::Write("/home/me/project/a/b.txt".into())),
            Decision::Allow
        );
    }

    #[test]
    fn access_outside_project_asks() {
        let p = policy();
        assert_eq!(p.check(&Action::Write("/etc/passwd".into())), Decision::Ask);
        // path traversal escaping the project must not be auto-allowed
        assert_eq!(
            p.check(&Action::Write("../../etc/passwd".into())),
            Decision::Ask
        );
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
