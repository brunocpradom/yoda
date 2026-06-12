//! Terminal presentation: ANSI colors, the startup banner, a live "thinking"
//! spinner with an elapsed-time clock, and Yoda-speak status phrases.
//!
//! Everything degrades gracefully: when stdout is not a TTY (e.g. piped) or
//! `NO_COLOR` is set, colors are dropped and the spinner doesn't animate.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Whether to emit ANSI codes at all (cached — `is_terminal` is a syscall).
fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(s: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn green(s: &str) -> String {
    paint(s, "32")
}
pub fn bold_green(s: &str) -> String {
    paint(s, "1;32")
}
pub fn dim(s: &str) -> String {
    paint(s, "2")
}
pub fn cyan(s: &str) -> String {
    paint(s, "36")
}
pub fn yellow(s: &str) -> String {
    paint(s, "33")
}
pub fn red(s: &str) -> String {
    paint(s, "31")
}

/// The `you ▸ ` input prompt (cyan, bold).
pub fn user_prompt() -> String {
    paint("you ▸ ", "1;36")
}

/// The `yoda ▸` reply label (green, bold).
pub fn yoda_label() -> String {
    paint("yoda ▸", "1;32")
}

pub fn bold_red(s: &str) -> String {
    paint(s, "1;31")
}

/// Label for a question the model is asking the user (bold yellow).
pub fn ask_label() -> String {
    paint("yoda asks ▸", "1;33")
}

/// The `↳ ` prompt where the user types an answer to `ask_user` (bold cyan).
pub fn answer_prompt() -> String {
    paint("  ↳ ", "1;36")
}

/// The input prompt for the current permission mode. `normal` shows the plain
/// cyan prompt; `auto`/`read-only` are flagged so you always know when risky
/// actions are being auto-approved (or all mutations blocked).
pub fn mode_prompt(mode: &str) -> String {
    match mode {
        "auto" => bold_red("you (auto) ▸ "),
        "read-only" => yellow("you (read-only) ▸ "),
        _ => user_prompt(),
    }
}

// --- banner -------------------------------------------------------------------

const LOGO: [&str; 5] = [
    "█   █   ███  ███    ███ ",
    " █ █   █   █ █  █  █   █ ",
    "  █    █   █ █  █  █████ ",
    "  █    █   █ █  █  █   █ ",
    "  █     ███  ███   █   █ ",
];

/// Print the startup banner: the green YODA logo, a Yoda tagline, and the
/// session's configuration.
pub fn banner(
    model: &str,
    endpoint: &str,
    project: &str,
    tools: &[&str],
    mcp: &[String],
    skills: &[String],
) {
    println!();
    for line in LOGO {
        println!("  {}", bold_green(line));
    }
    println!();
    println!("  {}", dim("\"Do or do not. There is no try.\""));
    println!();
    let key = |k: &str| dim(k);
    println!("  {} {}", key("model:   "), model);
    println!("  {} {}", key("endpoint:"), endpoint);
    println!("  {} {}", key("project: "), project);
    println!("  {} {}", key("tools:   "), tools.join(", "));
    if !mcp.is_empty() {
        println!("  {} {}", key("mcp:     "), mcp.join(", "));
    }
    if !skills.is_empty() {
        println!("  {} {}", key("skills:  "), skills.join(", "));
    }
    println!();
    println!(
        "  {}",
        green("Ready, I am. Speak your wish — /help for commands, /quit to leave.")
    );
    println!();
}

// --- thinking spinner ---------------------------------------------------------

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const PHRASES: [&str; 7] = [
    "Meditating, I am",
    "Consult the Force, I must",
    "Patience you must have",
    "Hmmm. Thinking, I am",
    "Reach out with feelings, I do",
    "Ponder your words, I do",
    "Forming, the answer is",
];

/// The next Yoda-speak status phrase (rotates through the list).
pub fn thinking_phrase() -> &'static str {
    static N: AtomicUsize = AtomicUsize::new(0);
    PHRASES[N.fetch_add(1, Ordering::Relaxed) % PHRASES.len()]
}

const DONE_VERBS: [&str; 4] = ["Pondered", "Meditated", "Reflected", "Mulled"];

/// Format a duration compactly: `3.4s`, `42s`, `5m 12s`, `1h 3m`.
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Format a token count compactly: `742`, `8.1k`, `16.4k`.
pub fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Context-window fill as a whole percentage, safe against a zero window.
pub fn context_percent(used: u64, window: u64) -> u64 {
    used * 100 / window.max(1)
}

/// A `[████████░░░░]` meter showing `used` of `window` tokens across `width`
/// cells. The fill turns yellow at 70% and red at 90% — the same "window is
/// getting tight" warning Claude Code gives, since on a small local window the
/// model starts forgetting earlier turns once this overflows.
pub fn context_bar(used: u64, window: u64, width: usize) -> String {
    let window = window.max(1);
    let filled = ((used.min(window) as usize) * width) / window as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
    let code = match context_percent(used, window) {
        0..=69 => "32",
        70..=89 => "33",
        _ => "31",
    };
    format!("[{}]", paint(&bar, code))
}

/// A dim-green summary printed after a turn, e.g.
/// `✦ Pondered for 5m 12s · context 8.1k/16.4k (49%)`. The context part is
/// omitted when the backend didn't report token counts.
pub fn elapsed_line(d: Duration, context: Option<(u64, u64)>) -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let verb = DONE_VERBS[N.fetch_add(1, Ordering::Relaxed) % DONE_VERBS.len()];
    let mut line = format!("✦ {verb} for {}", fmt_duration(d));
    if let Some((used, window)) = context {
        line.push_str(&format!(
            " · context {}/{} ({}%)",
            fmt_tokens(used),
            fmt_tokens(window),
            context_percent(used, window)
        ));
    }
    paint(&line, "2;32")
}

/// An animated single-line spinner showing a Yoda phrase and elapsed seconds,
/// e.g. `⠹ Meditating, I am (3s)`. Runs on its own thread; stops and clears the
/// line when dropped (or via [`Spinner::stop`]).
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(label: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        // No TTY → no animation (avoids spewing control codes into pipes/logs).
        if !color_enabled() {
            return Self { stop, handle: None };
        }
        let label = label.to_string();
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let start = Instant::now();
            let mut i = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let secs = start.elapsed().as_secs();
                print!(
                    "\r\x1b[32m{}\x1b[0m \x1b[2m{} ({}s)\x1b[0m\x1b[K",
                    FRAMES[i % FRAMES.len()],
                    label,
                    secs
                );
                let _ = std::io::stdout().flush();
                i += 1;
                thread::sleep(Duration::from_millis(90));
            }
            print!("\r\x1b[K"); // erase the spinner line
            let _ = std::io::stdout().flush();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and clear its line (consumes `self`; same as dropping).
    pub fn stop(self) {}
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_like_a_clock() {
        assert_eq!(fmt_duration(Duration::from_secs(5 * 60 + 12)), "5m 12s");
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_duration(Duration::from_secs(3600 + 3 * 60)), "1h 3m");
        assert_eq!(fmt_duration(Duration::from_millis(3400)), "3.4s");
    }

    #[test]
    fn formats_token_counts_compactly() {
        assert_eq!(fmt_tokens(742), "742");
        assert_eq!(fmt_tokens(8_132), "8.1k");
        assert_eq!(fmt_tokens(16_384), "16.4k");
    }

    /// Count fill cells instead of matching the whole string, so the test
    /// holds with or without ANSI color codes around the bar.
    fn fill_of(bar: &str) -> (usize, usize) {
        (bar.matches('█').count(), bar.matches('░').count())
    }

    #[test]
    fn context_bar_fills_proportionally() {
        assert_eq!(fill_of(&context_bar(0, 100, 10)), (0, 10));
        assert_eq!(fill_of(&context_bar(50, 100, 10)), (5, 5));
        assert_eq!(fill_of(&context_bar(100, 100, 10)), (10, 0));
        // Overflow (cache quirks, model overrun) clamps instead of panicking.
        assert_eq!(fill_of(&context_bar(150, 100, 10)), (10, 0));
        // A zero window must not divide by zero.
        assert_eq!(fill_of(&context_bar(0, 0, 10)), (0, 10));
    }

    #[test]
    fn context_percent_is_safe_and_exact() {
        assert_eq!(context_percent(8_192, 16_384), 50);
        assert_eq!(context_percent(0, 16_384), 0);
        assert_eq!(context_percent(0, 0), 0); // zero window must not panic
    }
}
