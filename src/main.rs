//! Yoda — a local-first agentic harness. Phase 5: tool-using agent loop with
//! search tools, sessions, MCP, skills, and runtime model switching.
//! See DESIGN.md for the full plan.

use std::io::{self, Write};

use anyhow::Result;

use yoda::config::Config;
use yoda::permission::{Mode, Policy};
use yoda::provider::{Message, OllamaProvider};
use yoda::skill::Skill;
use yoda::{agent, mcp, session, skill, tools, ui};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load()?;
    let mut provider = OllamaProvider::new(&cfg.base_url, &cfg.model);
    let mut tools = tools::default_registry(cfg.project_dir.clone());
    let mcp_servers = mcp::setup(&cfg.project_dir, &mut tools);
    let mut policy = Policy::new(cfg.project_dir.clone(), cfg.allowed_commands.clone());
    let sessions_dir = session::sessions_dir();
    let skills = skill::load_all(&skill::skills_dir());

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

    loop {
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
            if handle_command(
                rest,
                &mut history,
                &sessions_dir,
                &mut provider,
                &mut policy,
                &skills,
            ) {
                break;
            }
            continue;
        }

        history.push(Message::user(input));
        agent::run_turn(&provider, &tools, &policy, &mut history).await?;
    }

    println!("{}", ui::green("May the Force be with you."));
    Ok(())
}

/// Handle a `/command`. Returns true if the program should exit.
fn handle_command(
    input: &str,
    history: &mut Vec<Message>,
    sessions_dir: &std::path::Path,
    provider: &mut OllamaProvider,
    policy: &mut Policy,
    skills: &[Skill],
) -> bool {
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
        other => println!("unknown command /{other}. Try /help\n"),
    }
    false
}

fn print_help() {
    println!(
        "commands:\n  \
         /help              show this help\n  \
         /model [name]      show or switch the active model\n  \
         /mode [name]       permission mode: normal | auto | read-only\n  \
         /skills            list available skills\n  \
         /skill <name>      activate a skill (inject its instructions)\n  \
         /save [name]       save the conversation (default: 'default')\n  \
         /load [name]       load a saved conversation\n  \
         /sessions          list saved sessions\n  \
         /reset             clear history (keep system prompt)\n  \
         /quit, /exit       leave\n"
    );
}
