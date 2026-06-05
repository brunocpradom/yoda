//! Conversation persistence. Sessions are saved as JSON arrays of `Message`
//! under `~/.yoda/sessions/`. The core functions take the directory explicitly
//! so they can be unit-tested without touching the real home directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::provider::Message;

/// Default directory for saved sessions: `$HOME/.yoda/sessions`.
pub fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".yoda").join("sessions")
}

pub fn save_in(dir: &Path, name: &str, history: &[Message]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    // Saved conversations can contain anything the model saw (file contents, MCP
    // results, secrets), so keep them owner-only.
    restrict_dir(dir);
    if let Some(parent) = dir.parent() {
        restrict_dir(parent);
    }
    let path = dir.join(format!("{}.json", sanitize(name)));
    let json = serde_json::to_string_pretty(history)?;
    write_private(&path, json.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// Write bytes creating the file `0600` atomically, so it's never briefly
/// world-readable. Re-asserts `0600` in case the file already existed.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

pub fn load_in(dir: &Path, name: &str) -> Result<Vec<Message>> {
    let path = dir.join(format!("{}.json", sanitize(name)));
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let history = serde_json::from_str(&json).context("could not parse session file")?;
    Ok(history)
}

pub fn list_in(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) == Some("json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// Keep session names to safe filename characters.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("yoda_sess_test_{}", std::process::id()))
    }

    #[cfg(unix)]
    #[test]
    fn saved_session_and_dir_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("yoda_sess_perm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = save_in(&dir, "s", &[Message::system("x")]).unwrap();
        let fmode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "session file must be 0600, got {fmode:o}");
        assert_eq!(dmode, 0o700, "session dir must be 0700, got {dmode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_list_round_trip() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);

        let history = vec![Message::system("sys"), Message::user("hi there")];
        let path = save_in(&dir, "my session!", &history).unwrap();
        assert!(path.exists());
        // unsafe characters in the name are sanitized
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "my_session_.json"
        );

        let loaded = load_in(&dir, "my session!").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].content.as_deref(), Some("hi there"));

        assert!(list_in(&dir).contains(&"my_session_".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
