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

/// A dim-green summary printed after a turn, e.g. `✦ Pondered for 5m 12s`.
pub fn elapsed_line(d: Duration) -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let verb = DONE_VERBS[N.fetch_add(1, Ordering::Relaxed) % DONE_VERBS.len()];
    paint(&format!("✦ {verb} for {}", fmt_duration(d)), "2;32")
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
}
