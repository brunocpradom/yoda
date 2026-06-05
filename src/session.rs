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
    let path = dir.join(format!("{}.json", sanitize(name)));
    let json = serde_json::to_string_pretty(history)?;
    std::fs::write(&path, json).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

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
