//! Yoda — a local-first agentic harness. Phase 2: a tool-using agent loop over
//! a local open-source model, with an allowlist-based permission gate.
//! See DESIGN.md for the full plan.

mod agent;
mod config;
mod permission;
mod provider;
mod tools;

use std::io::{self, Write};

use anyhow::Result;

use config::Config;
use permission::Policy;
use provider::{Message, OllamaProvider};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load()?;
    let provider = OllamaProvider::new(&cfg.base_url, &cfg.model);
    let tools = tools::default_registry();
    let policy = Policy::new(cfg.project_dir.clone(), cfg.allowed_commands.clone());

    println!("Yoda — local agent harness (Phase 2: tools)");
    println!("  model:    {}", cfg.model);
    println!("  endpoint: {}", cfg.base_url);
    println!("  project:  {}", cfg.project_dir.display());
    println!("  tools:    {}", tools.names().join(", "));
    println!("Type a message, or /quit to exit.\n");

    let mut history = vec![Message::system(&cfg.system_prompt)];

    loop {
        print!("you ▸ ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break; // EOF (Ctrl-D)
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            break;
        }

        history.push(Message::user(input));
        agent::run_turn(&provider, &tools, &policy, &mut history).await?;
    }

    println!("May the Force be with you.");
    Ok(())
}
