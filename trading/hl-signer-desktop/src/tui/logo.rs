//! DegenBox brand marks for the TUI: a figlet logo for the first-run
//! wizard + empty splash, a compact wordmark for the header, and small
//! animated glyphs (spinner + online pulse) that advance on each tick.
//!
//! Everything here is plain ASCII / box-drawing so it aligns on every
//! terminal font. Colors are applied by the caller via `theme`.

/// Big logo — drawn on the wizard welcome and the "daemon stopped"
/// splash. ANSI-Shadow "DEGENBOX" (~68 cols). Block glyphs render in
/// brand green + bold; the Windows console init flips the codepage to
/// UTF-8 + enables VT so these don't mojibake in conhost.
pub const LOGO: &[&str] = &[
    "██████╗ ███████╗ ██████╗ ███████╗███╗   ██╗██████╗  ██████╗ ██╗  ██╗",
    "██╔══██╗██╔════╝██╔════╝ ██╔════╝████╗  ██║██╔══██╗██╔═══██╗╚██╗██╔╝",
    "██║  ██║█████╗  ██║  ███╗█████╗  ██╔██╗ ██║██████╔╝██║   ██║ ╚███╔╝ ",
    "██║  ██║██╔══╝  ██║   ██║██╔══╝  ██║╚██╗██║██╔══██╗██║   ██║ ██╔██╗ ",
    "██████╔╝███████╗╚██████╔╝███████╗██║ ╚████║██████╔╝╚██████╔╝██╔╝ ██╗",
    "╚═════╝ ╚══════╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝╚═════╝  ╚═════╝ ╚═╝  ╚═╝",
];

/// Tagline shown under the logo.
pub const TAGLINE: &str = "Hyperliquid Signer";

/// Braille spinner frames — advance one per tick for a smooth spin.
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spinner frame for a tick counter.
pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

/// Online "heartbeat" — a gentle pulse for the ONLINE pill. Slowed to
/// half tick-rate so it breathes rather than flickers.
pub const PULSE: &[&str] = &["●", "◉", "●", "○"];
pub fn pulse(tick: u64) -> &'static str {
    PULSE[((tick / 2) as usize) % PULSE.len()]
}

/// Horizontal "scanner" used as a thin animated accent under the
/// header while the daemon is connecting. Kept (and unit-tested) for the
/// header accent the connecting state may render; not wired on every
/// build, hence the allow.
#[allow(dead_code)]
pub fn scanner(width: usize, tick: u64) -> String {
    if width == 0 {
        return String::new();
    }
    let pos = (tick as usize) % width;
    let mut s = String::with_capacity(width * 3);
    for i in 0..width {
        s.push(if i == pos { '━' } else { '┈' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_cycles() {
        assert_eq!(spinner(0), SPINNER[0]);
        assert_eq!(spinner(SPINNER.len() as u64), SPINNER[0]);
        assert_ne!(spinner(0), spinner(1));
    }

    #[test]
    fn logo_is_nonempty() {
        // `LOGO` is a const slice so its emptiness is known at compile
        // time; assert on the rows instead (the meaningful invariant: no
        // row is blank, so the figlet renders intact).
        assert!(LOGO.iter().all(|l| !l.is_empty()));
    }

    #[test]
    fn scanner_has_exactly_one_marker() {
        let s = scanner(10, 3);
        assert_eq!(s.chars().filter(|&c| c == '━').count(), 1);
    }
}
