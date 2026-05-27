//! Branded stdout helpers for one-shot CLI commands.
//!
//! The TUI uses the `tui::theme` module + ratatui's `Style` API. For
//! the non-TUI commands (`setup`, `register`, `address`, `migrate`,
//! `check-update`, `self-update`, and the daemon boot banner before
//! tracing kicks in) we print directly to stdout/stderr — so we need
//! ANSI escape sequences instead of ratatui's `Style`.
//!
//! Color choices mirror `tui::theme`:
//!
//!   ACCENT (brand green) #7bebc4   rgb(123, 235, 196)   prompts / wordmark
//!   OK     (also green)  #7bebc4                        success ticks
//!   WARN   (amber)       #f4a261   rgb(244, 162,  97)   "connecting"
//!   ERR    (down red)    #f43f5e   rgb(244,  63,  94)   failures
//!   MUTED  (ink-3)       #8e929e   rgb(142, 146, 158)   labels / hints
//!   INK    (ink-1)       #e8eaee   rgb(232, 234, 238)   primary text
//!
//! The whole module is no-op when `NO_COLOR=1` is set in the
//! environment (https://no-color.org) or when stdout isn't a TTY — so
//! piping into `less` / journald stays clean.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// 24-bit truecolor SGR helpers. Terminals that don't support truecolor
/// (very rare in 2026 — modern iTerm2 / Terminal.app / kitty / wezterm
/// / Windows Terminal all do) will still render the text; they just
/// fall back to whatever the OS does with unsupported sequences.
const ACCENT: &str = "\x1b[38;2;123;235;196m";
const WARN: &str = "\x1b[38;2;244;162;97m";
const ERR: &str = "\x1b[38;2;244;63;94m";
const MUTED: &str = "\x1b[38;2;142;146;158m";
const INK: &str = "\x1b[38;2;232;234;238m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn colored_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        // Force-on lets users opt in even when piping (e.g. `tee`-ing
        // the output to a file while watching live).
        if std::env::var_os("FORCE_COLOR").is_some() {
            return true;
        }
        std::io::stdout().is_terminal()
    })
}

fn paint(prefix: &str, body: &str) -> String {
    if colored_enabled() {
        format!("{prefix}{body}{RESET}")
    } else {
        body.to_string()
    }
}

/// Brand-green accent text.
pub fn accent(body: &str) -> String {
    paint(ACCENT, body)
}

/// Brand-green + bold — used for the wordmark + section headings.
pub fn accent_bold(body: &str) -> String {
    if colored_enabled() {
        format!("{ACCENT}{BOLD}{body}{RESET}")
    } else {
        body.to_string()
    }
}

/// Amber warning.
pub fn warn(body: &str) -> String {
    paint(WARN, body)
}

/// Down-red error. Reserved for future error paths that print to
/// stdout instead of going through `tracing` (e.g. a future `doctor`
/// subcommand).
#[allow(dead_code)]
pub fn err(body: &str) -> String {
    paint(ERR, body)
}

/// Muted ink — labels, hints, secondary lines.
pub fn muted(body: &str) -> String {
    paint(MUTED, body)
}

/// Primary text ink — the default body color.
pub fn ink(body: &str) -> String {
    paint(INK, body)
}

/// Standard prompt prefix `›` in brand green. Used at the start of
/// every step / instruction line.
pub fn prefix() -> String {
    accent_bold("›")
}

/// Success tick `✓` in brand green.
pub fn tick() -> String {
    accent_bold("\u{2713}")
}

/// Failure cross `✗` in down-red. Pairs with [`tick`] — kept available
/// for future paths that need to render an inline failure marker.
#[allow(dead_code)]
pub fn cross() -> String {
    err("\u{2717}")
}

/// Status pill — mirrors the web app's `StatusBadge` treatment.
///
///   "ready"      → green dot + green label
///   "connecting" → amber dot + amber label
///   "offline"    → muted dot + muted label
///   anything else → red (treat unknowns as a problem worth surfacing)
pub fn status_pill(state: &str) -> String {
    let dot = "\u{25CF}";
    let (color, label) = match state.to_ascii_lowercase().as_str() {
        "ready" | "ok" | "online" => (ACCENT, "ready"),
        "connecting" | "pending" | "registering" => (WARN, "connecting"),
        "offline" | "idle" => (MUTED, "offline"),
        other => {
            // Echo the unknown verbatim so support output is honest.
            if !colored_enabled() {
                return format!("{dot} {other}");
            }
            return format!("{ERR}{dot} {other}{RESET}");
        }
    };
    if !colored_enabled() {
        return format!("{dot} {label}");
    }
    format!("{color}{dot} {label}{RESET}")
}

/// The CLI wordmark printed at the top of every non-TUI command.
///
/// Plain ASCII so it renders identically in iTerm, macOS Terminal,
/// Windows Terminal, journald and `script(1)` recordings. The accent
/// color is the only thing that needs truecolor support.
pub fn wordmark() -> String {
    let version = env!("CARGO_PKG_VERSION");
    if !colored_enabled() {
        return format!(
            "\n\
             ::    ::  DegenBox HL Signer  v{version}\n\
             ::    ::  keys stay local · self-custody\n",
        );
    }
    format!(
        "\n\
         {ACCENT}{BOLD}::    ::{RESET}  {ACCENT}{BOLD}DegenBox HL Signer{RESET}  {MUTED}v{version}{RESET}\n\
         {ACCENT}{BOLD}::    ::{RESET}  {MUTED}keys stay local · self-custody{RESET}\n",
    )
}

/// Section heading printed before each scripted step of a multi-step
/// command (e.g. setup). Visually distinct from log lines.
pub fn heading(title: &str) -> String {
    if !colored_enabled() {
        return format!("\n  {title}\n  {}", "-".repeat(title.len()));
    }
    format!(
        "\n  {ACCENT}{BOLD}{title}{RESET}\n  {MUTED}{}{RESET}",
        "-".repeat(title.len())
    )
}

/// Stable bracketed prefix for log-ish output the daemon's tracing
/// layer doesn't own (the lines we `eprintln!` before tracing is
/// initialised, plus the self-update notice). Mirrors the web app's
/// "DegenBox" wordmark.
pub fn brand_tag() -> String {
    accent_bold("[DegenBox]")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When colour is off (NO_COLOR=1, non-TTY, or test runner) the
    /// helpers must return ASCII-only strings with no ANSI escape
    /// sequences — otherwise log files / CI scrapes break.
    #[test]
    fn no_color_paths_are_plain_ascii() {
        // The OnceLock is cached per-process, so we can't flip the env
        // and re-test. Instead exercise `paint` directly with
        // colored_enabled=false via the cached `colored_enabled()`
        // helper at test time: cargo test sets stdout to a pipe, so
        // `IsTerminal` returns false and we go down the plain path.
        // (CI also sets NO_COLOR=1 most of the time.)
        let s = ink("hello");
        assert!(!s.contains('\x1b'), "expected no ANSI escapes, got {s:?}");
    }

    #[test]
    fn status_pill_renders_known_states() {
        let r = status_pill("ready");
        let c = status_pill("connecting");
        let o = status_pill("offline");
        // The labels are always present, with or without color.
        assert!(r.contains("ready"));
        assert!(c.contains("connecting"));
        assert!(o.contains("offline"));
    }

    #[test]
    fn wordmark_carries_version_and_tagline() {
        let w = wordmark();
        assert!(w.contains("DegenBox HL Signer"));
        assert!(w.contains(env!("CARGO_PKG_VERSION")));
        assert!(w.contains("keys stay local"));
    }
}
