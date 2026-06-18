//! Terminal presentation: ANSI colors, the startup banner, a live "thinking"
//! spinner with an elapsed-time clock, and Yoda-speak status phrases.
//!
//! Everything degrades gracefully: when stdout is not a TTY (e.g. piped) or
//! `NO_COLOR` is set, colors are dropped and the spinner doesn't animate.

use std::collections::VecDeque;
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

/// Plain (uncolored) input prompt for the current permission mode. It must
/// carry no ANSI escapes: the line editor (rustyline) measures the prompt's
/// display width to place the cursor, and counting escape bytes would push it
/// off. Color is applied separately by [`color_prompt`] via the editor's
/// highlighter, so width stays correct while the prompt still shows in color.
pub fn prompt_text(mode: &str) -> &'static str {
    match mode {
        "auto" => "you (auto) ▸ ",
        "read-only" => "you (read-only) ▸ ",
        _ => "you ▸ ",
    }
}

/// Colorize a plain prompt from [`prompt_text`]: cyan normally, bold red in
/// `auto` (everything is auto-approved — make it loud), yellow in `read-only`.
/// Kept beside `prompt_text` so the text and its color never drift apart.
/// Honors `NO_COLOR`/non-TTY through [`paint`].
pub fn color_prompt(plain: &str) -> String {
    let code = if plain.contains("(auto)") {
        "1;31"
    } else if plain.contains("(read-only)") {
        "33"
    } else {
        "1;36"
    };
    paint(plain, code)
}

/// Whether ANSI color is on (stdout is a TTY and `NO_COLOR` is unset). Public so
/// the line editor can match its color mode to the rest of the UI.
pub fn colors_enabled() -> bool {
    color_enabled()
}

/// The `yoda ▸` reply label (green, bold).
pub fn yoda_label() -> String {
    paint("yoda ▸", "1;32")
}

/// The `thinking ▸` label for a model's reasoning trace (dim, italic).
pub fn thinking_label() -> String {
    paint("thinking ▸", "2;3")
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

// --- bottom status bar ----------------------------------------------------------

/// Terminal size as (rows, cols), or `None` when stdout is not a real terminal.
#[cfg(unix)]
fn term_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (ok == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

#[cfg(not(unix))]
fn term_size() -> Option<(u16, u16)> {
    None
}

/// A status line pinned to the terminal's bottom row. Works by shrinking the
/// scroll region (DECSTBM, `ESC[1;N r`) to every row but the last: the
/// conversation scrolls inside the region, while the reserved row below it is
/// redrawn in place with cursor save/restore. A no-op when stdout is not a TTY
/// (piped output stays clean), `NO_COLOR` is set, or the terminal is tiny.
pub struct StatusBar {
    /// Last seen terminal height — compared on each draw to catch resizes,
    /// since a resize moves the bottom row out from under the scroll region.
    rows: u16,
    active: bool,
}

impl StatusBar {
    /// Reserve the bottom row. DECSTBM needs a known cursor position to avoid
    /// clobbering whatever is mid-screen, and a REPL can't know where the
    /// shell left the cursor — so this first scrolls the visible screen into
    /// the scrollback and starts clean from the top row.
    pub fn install() -> Self {
        let inactive = Self {
            rows: 0,
            active: false,
        };
        if !color_enabled() {
            return inactive;
        }
        let Some((rows, _)) = term_size() else {
            return inactive;
        };
        if rows < 4 {
            return inactive; // no room for a reserved row plus a conversation
        }
        print!("\x1b[{rows};1H{}", "\n".repeat(rows as usize));
        print!("\x1b[1;{}r\x1b[H", rows - 1);
        let _ = std::io::stdout().flush();
        Self { rows, active: true }
    }

    /// Redraw the bar: work dir on the left, context usage on the right.
    /// `context` is (used tokens, window tokens) from the last request, or
    /// `None` before anything has been measured. The whole row turns yellow at
    /// 70% fill and red at 90% — same thresholds as [`context_bar`].
    pub fn draw(&mut self, workdir: &str, context: Option<(u64, u64)>) {
        if !self.active {
            return;
        }
        let Some((rows, cols)) = term_size() else {
            return;
        };
        if rows != self.rows {
            self.rows = rows;
            print!("\x1b7\x1b[1;{}r\x1b8", rows.max(2) - 1);
        }
        let style = match context.map(|(used, window)| context_percent(used, window)) {
            Some(p) if p >= 90 => "7;31",
            Some(p) if p >= 70 => "7;33",
            _ => "7;2",
        };
        let line = status_line(workdir, context, cols as usize);
        print!("\x1b7\x1b[{rows};1H\x1b[2K{}\x1b8", paint(&line, style));
        let _ = std::io::stdout().flush();
    }

    /// Clear the bar and give the bottom row back to the terminal (`ESC[r`
    /// resets the scroll region to the full screen).
    pub fn remove(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        print!("\x1b7\x1b[{};1H\x1b[2K\x1b[r\x1b8", self.rows);
        let _ = std::io::stdout().flush();
    }
}

impl Drop for StatusBar {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The bar's text — ` <workdir>    context: 8.1k/16.4k (49%) ` — padded to
/// exactly `cols` characters so reverse video colors the full row. When space
/// is short the work dir is truncated from the left: for a path, the tail is
/// the informative part.
fn status_line(workdir: &str, context: Option<(u64, u64)>, cols: usize) -> String {
    let right = match context {
        Some((used, window)) => format!(
            "context: {}/{} ({}%)",
            fmt_tokens(used),
            fmt_tokens(window),
            context_percent(used, window)
        ),
        None => "context: —".to_string(),
    };
    let dir = truncate_left(workdir, cols.saturating_sub(right.chars().count() + 3));
    let pad = cols.saturating_sub(2 + dir.chars().count() + right.chars().count());
    let line = format!(" {dir}{}{right} ", " ".repeat(pad));
    line.chars().take(cols).collect()
}

/// Truncate to at most `max` characters by dropping the front; an `…` marks
/// the cut.
fn truncate_left(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = s.chars().skip(len - (max - 1)).collect();
    format!("…{tail}")
}

/// `$HOME`-prefixed paths shortened to `~` for display.
pub fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) => tilde_with(path, &home),
        Err(_) => path.to_string(),
    }
}

fn tilde_with(path: &str, home: &str) -> String {
    if home.is_empty() || !path.starts_with(home) {
        return path.to_string();
    }
    match &path[home.len()..] {
        rest if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(), // e.g. /Users/bruno2 is not under /Users/bruno
    }
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

// --- thinking pane ------------------------------------------------------------

/// How many lines of reasoning the live pane shows at once.
const THINKING_WINDOW: usize = 7;

/// A bounded, self-erasing viewport for the model's reasoning. As thinking
/// streams in, it shows the most recent [`THINKING_WINDOW`] lines under a
/// `thinking ▸` header, scrolling older lines off the top; on [`finish`] the
/// whole block is deleted so only the final answer remains in the scrollback.
///
/// On a TTY it redraws in place with cursor-up + clear-line and tears down with
/// DL (`ESC[nM`, delete-line) — which respects the status bar's scroll region,
/// so the reserved bottom row is never touched. With no TTY (piped/`NO_COLOR`)
/// it degrades to plain inline printing, which stays readable in logs.
///
/// [`finish`]: ThinkingPane::finish
pub struct ThinkingPane {
    active: bool,
    /// Max printable chars per row; lines longer than this are hard-wrapped so
    /// each logical row occupies exactly one physical terminal row (keeping the
    /// redraw line-count exact).
    width: usize,
    /// Completed rows, capped at the window size — older rows are dropped.
    rows: VecDeque<String>,
    /// The in-progress row (no newline seen yet); shown as the last visible line.
    current: String,
    /// Physical rows the pane currently occupies on screen (header + body).
    drawn: usize,
    /// Inactive-mode only: whether the `thinking ▸` label was already printed.
    label_printed: bool,
}

impl ThinkingPane {
    pub fn new() -> Self {
        let width = term_size()
            .map(|(_, cols)| (cols as usize).saturating_sub(1).max(20))
            .unwrap_or(80);
        Self {
            active: color_enabled(),
            width,
            rows: VecDeque::new(),
            current: String::new(),
            drawn: 0,
            label_printed: false,
        }
    }

    /// Feed a chunk of reasoning text; updates the live view.
    pub fn push(&mut self, delta: &str) {
        if !self.active {
            if !self.label_printed {
                print!("{} ", thinking_label());
                self.label_printed = true;
            }
            print!("{}", dim(delta));
            let _ = std::io::stdout().flush();
            return;
        }
        for ch in delta.chars() {
            match ch {
                '\r' => {}
                '\n' => self.commit_row(),
                _ => {
                    self.current.push(ch);
                    if self.current.chars().count() >= self.width {
                        self.commit_row();
                    }
                }
            }
        }
        self.render();
    }

    /// Erase the pane entirely, leaving the cursor where the block began so the
    /// answer prints in its place.
    pub fn finish(&mut self) {
        if !self.active {
            if self.label_printed {
                println!("\n");
            }
            return;
        }
        if self.drawn == 0 {
            return;
        }
        let mut out = String::new();
        if self.drawn > 1 {
            out.push_str(&format!("\x1b[{}A", self.drawn - 1)); // up to the first block row
        }
        out.push('\r');
        out.push_str(&format!("\x1b[{}M", self.drawn)); // delete the block's rows (scrolls region up)
        print!("{out}");
        let _ = std::io::stdout().flush();
        self.drawn = 0;
    }

    fn commit_row(&mut self) {
        self.rows.push_back(std::mem::take(&mut self.current));
        while self.rows.len() > THINKING_WINDOW {
            self.rows.pop_front();
        }
    }

    /// The styled lines to show: the `thinking ▸` header plus the last
    /// `THINKING_WINDOW` body rows (completed rows, then the in-progress one).
    fn visible(&self) -> Vec<String> {
        let mut body: Vec<&str> = self.rows.iter().map(String::as_str).collect();
        if !self.current.is_empty() || body.is_empty() {
            body.push(&self.current);
        }
        let start = body.len().saturating_sub(THINKING_WINDOW);
        let mut out = Vec::with_capacity(body.len() - start + 1);
        out.push(thinking_label());
        out.extend(body[start..].iter().map(|line| dim(line)));
        out
    }

    /// Redraw the block in place: jump to its first row, then clear-and-rewrite
    /// each line. Lines are separated by `\r\n` but the block ends with no
    /// trailing newline, so a stable redraw never scrolls the screen; only
    /// growth (a newly added row) advances the cursor and scrolls if needed.
    fn render(&mut self) {
        let lines = self.visible();
        let mut out = String::new();
        if self.drawn > 1 {
            out.push_str(&format!("\x1b[{}A", self.drawn - 1));
        }
        out.push('\r');
        for (i, line) in lines.iter().enumerate() {
            out.push_str("\x1b[2K"); // clear the whole row before rewriting
            out.push_str(line);
            if i + 1 < lines.len() {
                out.push_str("\r\n");
            }
        }
        self.drawn = lines.len();
        print!("{out}");
        let _ = std::io::stdout().flush();
    }
}

impl Default for ThinkingPane {
    fn default() -> Self {
        Self::new()
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
    fn status_line_is_exactly_terminal_width() {
        let line = status_line("~/code/yoda", Some((8_132, 16_384)), 60);
        assert_eq!(line.chars().count(), 60);
        assert!(line.starts_with(" ~/code/yoda"));
        assert!(line.contains("context: 8.1k/16.4k (49%)"));
    }

    #[test]
    fn status_line_shows_a_dash_before_any_measurement() {
        let line = status_line("~/p", None, 40);
        assert_eq!(line.chars().count(), 40);
        assert!(line.contains("context: —"));
    }

    #[test]
    fn status_line_truncates_the_work_dir_keeping_the_tail() {
        let line = status_line("/very/long/path/to/some/project", None, 30);
        assert_eq!(line.chars().count(), 30);
        assert!(line.contains('…'));
        assert!(line.contains("project"));
    }

    #[test]
    fn status_line_survives_tiny_terminals() {
        for cols in 0..15 {
            assert!(status_line("/p", Some((1, 2)), cols).chars().count() <= cols);
        }
    }

    #[test]
    fn truncate_left_keeps_the_tail() {
        assert_eq!(truncate_left("abcdef", 6), "abcdef");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate_left("abcdef", 1), "…");
        assert_eq!(truncate_left("abcdef", 0), "");
    }

    #[test]
    fn tilde_shortens_only_real_home_prefixes() {
        assert_eq!(tilde_with("/Users/bruno/code", "/Users/bruno"), "~/code");
        assert_eq!(tilde_with("/Users/bruno", "/Users/bruno"), "~");
        assert_eq!(
            tilde_with("/Users/bruno2/code", "/Users/bruno"),
            "/Users/bruno2/code"
        );
        assert_eq!(tilde_with("/etc", ""), "/etc");
    }

    #[test]
    fn context_percent_is_safe_and_exact() {
        assert_eq!(context_percent(8_192, 16_384), 50);
        assert_eq!(context_percent(0, 16_384), 0);
        assert_eq!(context_percent(0, 0), 0); // zero window must not panic
    }

    /// A pane wired for logic tests: active (so it buffers rows) with a fixed
    /// wrap width, independent of the real terminal. Color is off under `cargo
    /// test`, so `visible()` returns plain text we can assert on directly.
    fn test_pane(width: usize) -> ThinkingPane {
        ThinkingPane {
            active: true,
            width,
            rows: VecDeque::new(),
            current: String::new(),
            drawn: 0,
            label_printed: false,
        }
    }

    #[test]
    fn thinking_pane_keeps_only_the_last_window_of_lines() {
        let mut p = test_pane(100);
        for i in 1..=10 {
            p.push(&format!("line {i}\n"));
        }
        let visible = p.visible();
        // Header + exactly THINKING_WINDOW body rows, showing the most recent.
        assert_eq!(visible[0], thinking_label());
        assert_eq!(visible.len(), THINKING_WINDOW + 1);
        assert_eq!(visible[1], "line 4");
        assert_eq!(visible.last().unwrap(), "line 10");
    }

    #[test]
    fn thinking_pane_shows_the_in_progress_line_last() {
        let mut p = test_pane(100);
        p.push("done line\n");
        p.push("partial");
        let visible = p.visible();
        assert_eq!(visible.last().unwrap(), "partial");
    }

    #[test]
    fn thinking_pane_hard_wraps_overlong_lines_to_width() {
        let mut p = test_pane(10);
        p.push("0123456789ABCDE"); // 15 chars, wraps at 10
        let visible = p.visible();
        // First 10 chars become a committed row; the rest is the live tail.
        assert!(visible.iter().any(|l| l == "0123456789"));
        assert_eq!(visible.last().unwrap(), "ABCDE");
    }
}
