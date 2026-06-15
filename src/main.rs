//! Yoda — a local-first agentic harness. Phase 5: tool-using agent loop with
//! search tools, sessions, MCP, skills, and runtime model switching.
//! See DESIGN.md for the full plan.

use std::io::{self, Write};

use anyhow::{Context, Result};

use yoda::config::Config;
use yoda::permission::{Mode, Policy};
use yoda::provider::{Message, OllamaProvider, Usage};
use yoda::skill::Skill;
use yoda::tools::ToolRegistry;
use yoda::{agent, mcp, session, skill, tools, ui};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load()?;
    let mut provider = OllamaProvider::new(&cfg.base_url, &cfg.model, cfg.num_ctx);
    let mut tools = tools::default_registry(cfg.project_dir.clone());
    let mcp_servers = mcp::setup(&cfg.project_dir, &mut tools);
    let mut policy = Policy::new(cfg.project_dir.clone(), cfg.allowed_commands.clone());
    let sessions_dir = session::sessions_dir();
    let skills = skill::load_all(&skill::skills_dir());

    // Reserve the bottom row before anything else is printed: install scrolls
    // the screen into the scrollback, so output from here on starts clean.
    let mut status_bar = ui::StatusBar::install();

    let skill_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    ui::banner(
        &cfg.model,
        &cfg.base_url,
        &cfg.project_dir.display().to_string(),
        &tools.names(),
        &mcp_servers,
        &skill_names,
    );

    let mut history = vec![Message::system(&cfg.system_prompt)];
    // Token usage of the most recent model call — what /status, /context and
    // the status bar report. None until the first request (nothing measured
    // yet).
    let mut last_usage: Option<Usage> = None;
    let workdir = ui::tilde(&cfg.project_dir.display().to_string());

    loop {
        let context = last_usage.map(|u| (u.total(), cfg.num_ctx as u64));
        status_bar.draw(&workdir, context);
        print!("{}", ui::mode_prompt(policy.mode().label()));
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break; // EOF (Ctrl-D)
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if let Some(rest) = input.strip_prefix('/') {
            let ctx = CommandCtx {
                history: &mut history,
                sessions_dir: &sessions_dir,
                provider: &mut provider,
                policy: &mut policy,
                skills: &skills,
                cfg: &cfg,
                tools: &tools,
                last_usage,
            };
            if handle_command(rest, ctx) {
                break;
            }
            continue;
        }

        history.push(Message::user(input));
        let turn_usage = agent::run_turn(&provider, &tools, &policy, &mut history).await?;
        last_usage = turn_usage.or(last_usage);
    }

    status_bar.remove();
    println!("{}", ui::green("May the Force be with you."));
    Ok(())
}

/// Everything a `/command` may need to read or mutate, grouped so the handler
/// doesn't take nine parameters. The lifetime `'a` says: this struct only
/// borrows — it lives no longer than the things in `main` it points at.
struct CommandCtx<'a> {
    history: &'a mut Vec<Message>,
    sessions_dir: &'a std::path::Path,
    provider: &'a mut OllamaProvider,
    policy: &'a mut Policy,
    skills: &'a [Skill],
    cfg: &'a Config,
    tools: &'a ToolRegistry,
    last_usage: Option<Usage>,
}

/// Handle a `/command`. Returns true if the program should exit. Takes the
/// context by value: it's only a bundle of borrows, so building one per
/// command is free, and destructuring it recovers the plain `&`/`&mut`
/// references (no `&mut &mut` double-indirection a `&mut CommandCtx` would
/// introduce).
fn handle_command(input: &str, ctx: CommandCtx<'_>) -> bool {
    let CommandCtx {
        history,
        sessions_dir,
        provider,
        policy,
        skills,
        cfg,
        tools,
        last_usage,
    } = ctx;
    let mut parts = input.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let session_name = if arg.is_empty() { "default" } else { arg };

    match cmd {
        "quit" | "exit" => return true,
        "help" => print_help(),
        "reset" => {
            history.truncate(1); // keep the base system prompt
            println!("(history cleared)\n");
        }
        "save" => match session::save_in(sessions_dir, session_name, history) {
            Ok(path) => println!("saved → {}\n", path.display()),
            Err(e) => println!("save failed: {e}\n"),
        },
        "load" => match session::load_in(sessions_dir, session_name) {
            Ok(loaded) => {
                let n = loaded.len();
                *history = loaded;
                println!("loaded '{session_name}' ({n} messages)\n");
            }
            Err(e) => println!("load failed: {e}\n"),
        },
        "sessions" => {
            let names = session::list_in(sessions_dir);
            if names.is_empty() {
                println!("(no saved sessions)\n");
            } else {
                println!("sessions: {}\n", names.join(", "));
            }
        }
        "skills" => {
            if skills.is_empty() {
                println!("(no skills in {})\n", skill::skills_dir().display());
            } else {
                for s in skills {
                    println!("  {} — {}", s.name, s.description);
                }
                println!();
            }
        }
        "skill" => {
            if arg.is_empty() {
                println!("usage: /skill <name>\n");
            } else {
                match skill::find(skills, arg) {
                    Some(s) => {
                        history.push(Message::system(&s.body));
                        println!("(activated skill '{}')\n", s.name);
                    }
                    None => println!("unknown skill '{arg}'. Try /skills\n"),
                }
            }
        }
        "model" => {
            if arg.is_empty() {
                println!("model: {}\n", provider.model());
            } else {
                provider.set_model(arg);
                println!("(model → {arg})\n");
            }
        }
        "mode" => {
            if arg.is_empty() {
                println!(
                    "mode: {} (normal | auto | read-only)\n",
                    policy.mode().label()
                );
            } else {
                match Mode::parse(arg) {
                    Some(Mode::Auto) => {
                        policy.set_mode(Mode::Auto);
                        println!(
                            "{}\n",
                            ui::bold_red(
                                "(mode → auto) EVERYTHING is now auto-approved, including destructive commands. /mode normal to undo."
                            )
                        );
                    }
                    Some(m) => {
                        policy.set_mode(m);
                        println!("(mode → {})\n", m.label());
                    }
                    None => println!("unknown mode '{arg}'. Try: normal, auto, read-only\n"),
                }
            }
        }
        "think" => match arg {
            "" => println!(
                "thinking: {} (usage: /think on|off)\n",
                if provider.think() { "on" } else { "off" }
            ),
            "on" => {
                provider.set_think(true);
                println!("(thinking → on — reasoning models will show their work)\n");
            }
            "off" => {
                provider.set_think(false);
                println!("(thinking → off)\n");
            }
            _ => println!("usage: /think on|off\n"),
        },
        "status" => print_status(cfg, provider, policy, history, last_usage),
        "context" => print_context(cfg, provider, tools, history, last_usage),
        "copy" => match last_assistant_reply(history) {
            Some(text) => match clipboard_copy(text) {
                Ok(()) => println!("(copied {} chars to clipboard)\n", text.chars().count()),
                Err(e) => println!("copy failed: {e}\n"),
            },
            None => println!("(nothing to copy — no assistant reply yet)\n"),
        },
        other => println!("unknown command /{other}. Try /help\n"),
    }
    false
}

/// `/status` — the session at a glance, Claude Code style: model, endpoint,
/// working directory, permission mode, history size, and context fill.
fn print_status(
    cfg: &Config,
    provider: &OllamaProvider,
    policy: &Policy,
    history: &[Message],
    last_usage: Option<Usage>,
) {
    let key = |k: &str| ui::dim(k);
    println!();
    println!("  {} {}", key("model:    "), provider.model());
    println!("  {} {}", key("endpoint: "), cfg.base_url);
    println!("  {} {}", key("work dir: "), cfg.project_dir.display());
    println!("  {} {}", key("mode:     "), policy.mode().label());
    println!("  {} {} messages", key("history:  "), history.len());
    let window = cfg.num_ctx as u64;
    match last_usage {
        Some(usage) => println!(
            "  {} {}/{} tokens ({}%)",
            key("context:  "),
            ui::fmt_tokens(usage.total()),
            ui::fmt_tokens(window),
            ui::context_percent(usage.total(), window),
        ),
        None => println!(
            "  {} {}-token window (no requests yet)",
            key("context:  "),
            ui::fmt_tokens(window),
        ),
    }
    println!();
}

/// `/context` — visualize context-window usage like Claude Code's /context:
/// a fill bar plus a composition breakdown. The headline number is the model
/// server's own token count from the last request when we have one; the
/// breakdown is always a chars/4 estimate (no local tokenizer).
fn print_context(
    cfg: &Config,
    provider: &OllamaProvider,
    tools: &ToolRegistry,
    history: &[Message],
    last_usage: Option<Usage>,
) {
    let window = cfg.num_ctx as u64;

    // Estimate each category by its serialized size — tool-call arguments and
    // results count too, not just plain text content.
    let json_len = |m: &Message| serde_json::to_string(m).map_or(0, |s| s.chars().count());
    let (system_chars, convo_chars) = history.iter().fold((0, 0), |(sys, convo), m| {
        if m.role == "system" {
            (sys + json_len(m), convo)
        } else {
            (sys, convo + json_len(m))
        }
    });
    let spec_chars = serde_json::to_string(&tools.specs()).map_or(0, |s| s.chars().count());
    let estimate = |chars: usize| (chars / 4) as u64;

    let (used, source) = match last_usage {
        Some(usage) => (usage.total(), "measured by the model server, last request"),
        None => (
            estimate(system_chars + spec_chars + convo_chars),
            "estimated — no requests sent yet",
        ),
    };

    println!();
    println!(
        "  {} · {}-token window",
        provider.model(),
        ui::fmt_tokens(window)
    );
    println!();
    println!(
        "  {} {}/{} tokens ({}%) · {} free",
        ui::context_bar(used, window, 30),
        ui::fmt_tokens(used),
        ui::fmt_tokens(window),
        ui::context_percent(used, window),
        ui::fmt_tokens(window.saturating_sub(used)),
    );
    println!("  {}", ui::dim(source));
    println!();
    println!("  {}", ui::dim("estimated composition (~4 chars/token):"));
    println!(
        "    {} ~{} tokens",
        ui::dim("system prompt:"),
        ui::fmt_tokens(estimate(system_chars))
    );
    println!(
        "    {} ~{} tokens",
        ui::dim("tool specs:   "),
        ui::fmt_tokens(estimate(spec_chars))
    );
    println!(
        "    {} ~{} tokens",
        ui::dim("conversation: "),
        ui::fmt_tokens(estimate(convo_chars))
    );
    println!();
}

/// The text of the most recent assistant reply, straight from history. This is
/// the un-wrapped original — copying from here (rather than selecting terminal
/// output) is what keeps the terminal's visual line breaks out of the paste.
/// Skips assistant turns that carried only tool calls (no text).
fn last_assistant_reply(history: &[Message]) -> Option<&str> {
    history
        .iter()
        .rev()
        .filter(|m| m.role == "assistant")
        .find_map(|m| {
            m.content
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
        })
}

/// Put `text` on the macOS clipboard by piping it to `pbcopy`.
fn clipboard_copy(text: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("could not run pbcopy (is this macOS?)")?;
    child
        .stdin
        .take()
        .expect("stdin was requested as piped above")
        .write_all(text.as_bytes())
        .context("could not write to pbcopy")?;
    let status = child.wait().context("pbcopy did not exit")?;
    anyhow::ensure!(status.success(), "pbcopy exited with {status}");
    Ok(())
}

fn print_help() {
    println!(
        "commands:\n  \
         /help              show this help\n  \
         /model [name]      show or switch the active model\n  \
         /mode [name]       permission mode: normal | auto | read-only\n  \
         /think [on|off]    show or toggle the model's reasoning (on by default)\n  \
         /status            session at a glance: model, mode, work dir, context\n  \
         /context           visualize context-window usage\n  \
         /copy              copy the last reply to the clipboard (no wrap artifacts)\n  \
         /skills            list available skills\n  \
         /skill <name>      activate a skill (inject its instructions)\n  \
         /save [name]       save the conversation (default: 'default')\n  \
         /load [name]       load a saved conversation\n  \
         /sessions          list saved sessions\n  \
         /reset             clear history (keep system prompt)\n  \
         /quit, /exit       leave\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use yoda::provider::{FunctionCall, ToolCall};

    fn call() -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn copy_finds_the_last_textual_reply() {
        let history = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant(Some("first answer".into()), vec![]),
            Message::user("again"),
            Message::assistant(Some("  second answer\n".into()), vec![]),
        ];
        assert_eq!(last_assistant_reply(&history), Some("second answer"));
    }

    #[test]
    fn copy_skips_tool_call_only_turns() {
        let history = vec![
            Message::assistant(Some("real text".into()), vec![]),
            Message::assistant(None, vec![call()]),
            Message::assistant(Some("   ".into()), vec![]),
        ];
        assert_eq!(last_assistant_reply(&history), Some("real text"));
    }

    #[test]
    fn copy_handles_an_empty_session() {
        assert_eq!(last_assistant_reply(&[Message::system("sys")]), None);
        assert_eq!(last_assistant_reply(&[]), None);
    }
}
