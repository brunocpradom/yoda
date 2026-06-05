//! Skills — reusable instruction snippets loaded from `~/.yoda/skills/*.md` and
//! injected into the conversation on demand (`/skill <name>`). Activation is
//! manual rather than automatic: a local 7B model's context is precious, so we
//! only add a skill's text when the user asks for it.
//!
//! File format: an optional YAML-ish frontmatter block for a one-line
//! `description:`, followed by the instruction body.
//!
//! ```text
//! ---
//! description: Fix grammar and phrasing in English text.
//! ---
//! You are an English editor. Correct the user's text and briefly explain changes.
//! ```

use std::path::{Path, PathBuf};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Default skills directory: `$HOME/.yoda/skills`.
pub fn skills_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".yoda").join("skills")
}

pub fn load_all(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) == Some("md")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("skill")
                    .to_string();
                skills.push(parse(name, &text));
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub fn find<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

/// Split optional `--- ... ---` frontmatter (for `description:`) from the body.
fn parse(name: String, text: &str) -> Skill {
    let mut description = String::new();
    let mut body = text.trim().to_string();

    if let Some(after_open) = text.strip_prefix("---")
        && let Some(end) = after_open.find("\n---")
    {
        let front = &after_open[..end];
        body = after_open[end + 4..].trim_start().to_string();
        description = front
            .lines()
            .find_map(|l| l.trim().strip_prefix("description:"))
            .map(|d| d.trim().to_string())
            .unwrap_or_default();
    }

    Skill {
        name,
        description,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let text = "---\ndescription: Fix English text.\n---\nYou are an editor.";
        let skill = parse("english".into(), text);
        assert_eq!(skill.name, "english");
        assert_eq!(skill.description, "Fix English text.");
        assert_eq!(skill.body, "You are an editor.");
    }

    #[test]
    fn handles_missing_frontmatter() {
        let skill = parse("plain".into(), "Just instructions, no frontmatter.");
        assert_eq!(skill.description, "");
        assert_eq!(skill.body, "Just instructions, no frontmatter.");
    }

    #[test]
    fn find_by_name() {
        let skills = vec![
            Skill {
                name: "a".into(),
                description: String::new(),
                body: "x".into(),
            },
            Skill {
                name: "b".into(),
                description: String::new(),
                body: "y".into(),
            },
        ];
        assert_eq!(find(&skills, "b").unwrap().body, "y");
        assert!(find(&skills, "z").is_none());
    }
}
