//! Explicit and natural-language control of the local Kibitzer launcher.
//!
//! The launcher remains the source of truth for starting Chainlit, switching
//! audio, and managing its PID. Yoda only validates the request and invokes it
//! directly — no shell is involved, and user text is never interpolated into a
//! command string.

use anyhow::{Result, anyhow, bail};
use tokio::process::Command;

use crate::config::Config;

const USAGE: &str = "kibitzer:
  /kibitzer help
  /kibitzer reuniao --briefing <arquivo|-> [--no-audio] [--no-open]
  /kibitzer reuniao --briefing-text \"texto\" [--no-audio] [--no-open]
  /kibitzer entrevista --stacks \"Django, FastAPI\" --briefing <arquivo|-> [--no-audio] [--no-open]
  /kibitzer entrevista --stacks \"Django, FastAPI\" --briefing-text \"texto\" [--no-audio] [--no-open]
  /kibitzer status
  /kibitzer stop

Obrigatórios:
  reunião      --briefing ou --briefing-text
  entrevista   --stacks e --briefing ou --briefing-text

Se o texto tiver espaços, coloque-o entre aspas.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Help,
    Start {
        mode: &'static str,
        args: Vec<String>,
    },
    Status,
    Stop,
}

impl Request {
    pub fn mutates(&self) -> bool {
        matches!(self, Self::Start { .. } | Self::Stop)
    }
}

pub fn usage() -> &'static str {
    USAGE
}

/// Parse `/kibitzer ...` or one of the deliberately small natural aliases.
/// Natural aliases become the same validated request as explicit commands.
pub fn parse_input(input: &str) -> Result<Option<Request>> {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();
    let natural = if lower.contains("kibitzer") {
        if lower.contains("para") || lower.contains("desliga") || lower.contains("deslig") {
            Some("/kibitzer stop".to_string())
        } else if lower.contains("status") || lower.contains("rodando") {
            Some("/kibitzer status".to_string())
        } else if lower.contains("entrevista") {
            // The stacks are intentionally left for the help/validation path
            // when the phrase is ambiguous or incomplete.
            Some("/kibitzer entrevista".to_string())
        } else if lower.contains("sobe")
            || lower.contains("liga")
            || lower.contains("inicia")
            || lower.contains("iniciar")
        {
            Some("/kibitzer reuniao".to_string())
        } else {
            None
        }
    } else {
        None
    };

    let command = if let Some(rest) = trimmed.strip_prefix("/kibitzer") {
        format!("/kibitzer{rest}")
    } else if let Some(natural) = natural {
        natural
    } else {
        return Ok(None);
    };

    parse_explicit(&command)
}

fn parse_explicit(input: &str) -> Result<Option<Request>> {
    let Some(rest) = input.strip_prefix("/kibitzer") else {
        return Ok(None);
    };
    let tokens = tokenize(rest.trim())?;
    let Some(command) = tokens.first().map(String::as_str) else {
        return Ok(Some(Request::Help));
    };
    if matches!(command, "help" | "-h" | "--help") {
        return Ok(Some(Request::Help));
    }
    if command == "status" && tokens.len() == 1 {
        return Ok(Some(Request::Status));
    }
    if command == "stop" && tokens.len() == 1 {
        return Ok(Some(Request::Stop));
    }
    if !matches!(command, "reuniao" | "reunião" | "entrevista") {
        bail!("opção desconhecida: {command}\n\n{USAGE}");
    }

    let is_interview = command == "entrevista";
    let mut args = vec![
        if is_interview {
            "entrevista"
        } else {
            "reuniao"
        }
        .to_string(),
    ];
    let mut stacks = false;
    let mut briefing = false;
    let mut i = 1;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "--stacks" if is_interview => {
                let value = tokens.get(i + 1).filter(|v| !v.is_empty()).ok_or_else(|| {
                    anyhow!("faltou o valor de --stacks. Exemplo: --stacks \"Django, FastAPI\"")
                })?;
                args.extend(["--stacks".into(), value.clone()]);
                stacks = true;
                i += 2;
            }
            "--briefing" | "--briefing-text" => {
                let value = tokens
                    .get(i + 1)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| anyhow!("faltou o valor de {}", tokens[i]))?;
                args.extend([tokens[i].clone(), value.clone()]);
                briefing = true;
                i += 2;
            }
            "--no-audio" | "--no-open" => {
                args.push(tokens[i].clone());
                i += 1;
            }
            unknown => bail!("opção desconhecida: {unknown}\n\n{USAGE}"),
        }
    }

    let mut missing = Vec::new();
    if is_interview && !stacks {
        missing.push("--stacks \"Django, FastAPI\"");
    }
    if !briefing {
        missing.push("--briefing <arquivo> ou --briefing-text \"texto\"");
    }
    if !missing.is_empty() {
        bail!("faltou informar {}.\n\n{USAGE}", missing.join(" e "));
    }

    Ok(Some(Request::Start {
        mode: if is_interview {
            "entrevista"
        } else {
            "reuniao"
        },
        args,
    }))
}

/// Invoke the existing launcher directly. The launcher itself handles the
/// background Chainlit process and returns after it is ready (or reports an
/// actionable error).
pub async fn execute(cfg: &Config, request: &Request) -> Result<String> {
    let (args, label) = match request {
        Request::Help => return Ok(USAGE.to_string()),
        Request::Status => (vec!["status".to_string()], "status"),
        Request::Stop => (vec!["stop".to_string()], "stop"),
        Request::Start { args, mode } => (args.clone(), *mode),
    };

    let bin = &cfg.kibitzer_bin;
    let output = Command::new(bin)
        .args(&args)
        .output()
        .await
        .map_err(|e| anyhow!("não consegui executar o Kibitzer em {}: {e}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        bail!(
            "Kibitzer ({label}) falhou:\n{}",
            non_empty(&stderr, &stdout)
        );
    }
    Ok(non_empty(&stdout, &stderr).to_string())
}

fn non_empty<'a>(first: &'a str, fallback: &'a str) -> &'a str {
    if first.is_empty() { fallback } else { first }
}

/// Small shell-like tokenizer: supports single/double quotes and backslash
/// escapes, enough for briefing text and stack lists in the REPL.
fn tokenize(input: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(q) = quote {
            if ch == q {
                quote = None
            } else {
                current.push(ch)
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        bail!("aspas não fechadas no comando do Kibitzer")
    }
    if !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_meeting_requires_briefing() {
        let err = parse_input("/kibitzer reuniao").unwrap_err().to_string();
        assert!(err.contains("--briefing"));
    }

    #[test]
    fn explicit_interview_requires_stacks_and_briefing() {
        let err = parse_input("/kibitzer entrevista").unwrap_err().to_string();
        assert!(err.contains("--stacks"));
        assert!(err.contains("--briefing"));
    }

    #[test]
    fn quoted_arguments_are_preserved() {
        let request = parse_input("/kibitzer reuniao --briefing-text \"roadmap do Q3\"")
            .unwrap()
            .unwrap();
        assert_eq!(
            request,
            Request::Start {
                mode: "reuniao",
                args: vec![
                    "reuniao".into(),
                    "--briefing-text".into(),
                    "roadmap do Q3".into(),
                ],
            }
        );
    }

    #[test]
    fn natural_aliases_are_supported() {
        let err = parse_input("ei yoda, sobe o kibitzer")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--briefing"));
        assert_eq!(
            parse_input("yoda, para o kibitzer").unwrap(),
            Some(Request::Stop)
        );
    }
}
