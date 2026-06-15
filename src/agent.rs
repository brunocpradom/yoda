//! The agentic loop. Given the conversation so far, ask the model what to do;
//! if it requests tools, vet each through the permission gate, run the allowed
//! ones, feed the results back, and repeat until the model produces a final
//! text answer (or we hit the step cap that stops runaway local-model loops).

use std::io::{self, Write};

use anyhow::Result;
use serde_json::{Value, json};

use crate::permission::{Action, Decision, Policy};
use crate::provider::{FunctionCall, Message, Provider, ToolCall, Usage};
use crate::tools::ToolRegistry;

/// Upper bound on tool round-trips per user turn. Small models sometimes loop;
/// this guarantees the turn terminates.
const MAX_STEPS: usize = 8;

/// Run one user turn. Returns the token usage of the turn's last model call
/// (when the backend reports it) so the caller can show context-window fill.
pub async fn run_turn(
    provider: &dyn Provider,
    tools: &ToolRegistry,
    policy: &Policy,
    history: &mut Vec<Message>,
) -> Result<Option<Usage>> {
    let specs = tools.specs();
    let turn_start = std::time::Instant::now();
    let mut last_usage = None;

    for step in 0..MAX_STEPS {
        // Animate a Yoda-speak spinner with an elapsed clock until the model's
        // first token arrives (which may be many seconds locally). When thinking
        // is on, the model's reasoning then streams in live — dimmed, under a
        // `thinking ▸` label — so the user watches it form instead of waiting for
        // the whole block. The reasoning is shown but never stored in history.
        let mut spinner = Some(crate::ui::Spinner::start(crate::ui::thinking_phrase()));
        let mut pane: Option<crate::ui::ThinkingPane> = None;
        // Scope the sink so its borrows on `spinner`/`pane` end before we touch
        // them again below.
        let result = {
            let mut on_thinking = |delta: &str| {
                // First reasoning token: tear down the spinner, open the live pane.
                pane.get_or_insert_with(|| {
                    spinner.take(); // drop → stop the spinner thread and clear its line
                    crate::ui::ThinkingPane::new()
                })
                .push(delta);
            };
            provider.complete(history, &specs, &mut on_thinking).await
        };
        spinner.take(); // stop the spinner if no reasoning streamed (thinking off / non-reasoning model)
        if let Some(mut pane) = pane.take() {
            pane.finish(); // erase the live thinking window before the answer prints
        }

        let mut completion = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}", crate::ui::red(&format!("[error: {e:#}]")));
                return Ok(last_usage);
            }
        };
        last_usage = completion.usage.or(last_usage);

        // Fallback for models that print tool calls as text instead of using
        // the structured `tool_calls` field (e.g. qwen2.5-coder). If the reply
        // has no native tool calls but its text contains tool-call JSON for a
        // known tool, treat that as the tool call and don't echo the raw JSON.
        if completion.tool_calls.is_empty()
            && let Some(text) = &completion.content
        {
            let recovered = extract_tool_calls(text, tools);
            if !recovered.is_empty() {
                completion.tool_calls = recovered;
                completion.content = None;
            }
        }

        // Some models omit tool-call ids; synthesize stable ones so the tool
        // result messages can reference them.
        for (i, call) in completion.tool_calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{step}_{i}");
            }
        }

        if let Some(text) = &completion.content {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                println!("{} {trimmed}\n", crate::ui::yoda_label());
            }
        }

        history.push(Message::assistant(
            completion.content.clone(),
            completion.tool_calls.clone(),
        ));

        if completion.tool_calls.is_empty() {
            println!(
                "{}",
                crate::ui::elapsed_line(turn_start.elapsed(), context_fill(provider, last_usage))
            );
            return Ok(last_usage); // model gave a final answer
        }

        for call in &completion.tool_calls {
            let result = execute_call(tools, policy, call).await;
            history.push(Message::tool_result(call.id.clone(), result));
        }
    }

    println!(
        "{} {}",
        crate::ui::yoda_label(),
        crate::ui::dim(&format!("[stopped after {MAX_STEPS} tool steps]"))
    );
    println!(
        "{}",
        crate::ui::elapsed_line(turn_start.elapsed(), context_fill(provider, last_usage))
    );
    Ok(last_usage)
}

/// `(used, window)` for the elapsed-line context readout — `None` unless both
/// the backend reported usage and the provider knows its window size.
fn context_fill(provider: &dyn Provider, usage: Option<Usage>) -> Option<(u64, u64)> {
    Some((usage?.total(), provider.context_window()? as u64))
}

/// Run one tool call after vetting it. Returns the text fed back to the model —
/// including error/denied strings, so the model can recover rather than crash.
async fn execute_call(tools: &ToolRegistry, policy: &Policy, call: &ToolCall) -> String {
    let name = &call.function.name;
    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);

    let Some(tool) = tools.get(name) else {
        println!("  {} {name} — unknown tool", crate::ui::yellow("🔧"));
        return format!("Error: unknown tool '{name}'");
    };

    println!(
        "  {} {}",
        crate::ui::cyan("🔧"),
        crate::ui::dim(&format!("{name}({})", compact(&args)))
    );

    for action in tool.actions(&args) {
        match policy.check(&action) {
            Decision::Allow => {}
            Decision::Deny => {
                println!(
                    "  {}",
                    crate::ui::red(&format!("⛔ denied by policy: {}", action.describe()))
                );
                return format!("Permission denied by policy: {}", action.describe());
            }
            Decision::Ask => {
                if !ask_user(&action) {
                    println!(
                        "  {}",
                        crate::ui::yellow(&format!("⛔ you denied: {}", action.describe()))
                    );
                    return format!("User denied permission: {}", action.describe());
                }
            }
        }
    }

    let started = std::time::Instant::now();
    match tool.run(&args).await {
        Ok(output) => {
            println!(
                "  {}\n",
                crate::ui::green(&format!(
                    "✓ done ({})",
                    crate::ui::fmt_duration(started.elapsed())
                ))
            );
            output
        }
        Err(e) => {
            println!("  {}\n", crate::ui::red(&format!("✗ {e}")));
            format!("Error running {name}: {e}")
        }
    }
}

/// Recover tool calls that a model emitted as plain text. Only objects whose
/// `name` matches a registered tool are accepted, so ordinary JSON answers
/// aren't mistaken for tool calls.
fn extract_tool_calls(text: &str, tools: &ToolRegistry) -> Vec<ToolCall> {
    let known = tools.names();
    let mut calls = Vec::new();
    for obj in scan_json_objects(text) {
        let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if !known.contains(&name) {
            continue;
        }
        let args = obj
            .get("arguments")
            .or_else(|| obj.get("parameters"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let arguments = match args {
            Value::String(s) => s,
            other => other.to_string(),
        };
        calls.push(ToolCall {
            id: String::new(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.to_string(),
                arguments,
            },
        });
    }
    calls
}

/// Extract every balanced top-level `{...}` JSON object from arbitrary text
/// (ignoring code fences, `<tool_call>` tags, and surrounding prose).
fn scan_json_objects(text: &str) -> Vec<Value> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Ok(value) = serde_json::from_str::<Value>(&text[start..=i])
                    && value.is_object()
                {
                    out.push(value);
                }
            }
            _ => {}
        }
    }
    out
}

fn ask_user(action: &Action) -> bool {
    print!(
        "  {} ",
        crate::ui::yellow(&format!("⚠ allow {}? [y/N]", action.describe()))
    );
    io::stdout().flush().ok();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// A short one-line rendering of tool arguments for display.
fn compact(args: &Value) -> String {
    let s = args.to_string();
    if s.chars().count() <= 120 {
        s
    } else {
        let head: String = s.chars().take(117).collect();
        format!("{head}...")
    }
}
