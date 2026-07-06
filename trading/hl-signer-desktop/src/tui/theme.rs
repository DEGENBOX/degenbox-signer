//! Color tokens — wired to the DegenBox web palette.
//!
//! Mapping (web CSS var  →  ratatui `Color::Rgb`):
//!
//!   --accent     #7bebc4   rgb(123, 235, 196)   brand green, action / focus
//!   --up         #7bebc4   rgb(123, 235, 196)   ok / ready / long / filled
//!   --down       #f43f5e   rgb(244,  63,  94)   err / offline / short
//!   --warn       #f4a261   rgb(244, 162,  97)   amber for connecting / pending
//!   --ink-1      #e8eaee   rgb(232, 234, 238)   primary text (neutral)
//!   --ink-3      #8e929e   rgb(142, 146, 158)   muted labels
//!   --canvas     #16171b   rgb( 22,  23,  27)   page background
//!
//! Respects `NO_COLOR=1` (https://no-color.org). When set, every style
//! returned here is `Style::default()` — the layout still draws but
//! with the terminal's default fg/bg.

use ratatui::style::{Color, Modifier, Style};

/// DegenBox accent green — matches `--accent` / `--up` in the web app.
pub const BRAND_ACCENT: Color = Color::Rgb(123, 235, 196);
/// DegenBox down red — matches `--down` in the web app.
pub const BRAND_DOWN: Color = Color::Rgb(244, 63, 94);
/// DegenBox amber — used for "connecting" / "pending" status. The web
/// app doesn't expose a CSS var for this; we picked an orange that
/// reads well against `--canvas` in both dark and light terminals.
pub const BRAND_WARN: Color = Color::Rgb(244, 162, 97);
/// Primary ink — matches `--ink-1` (near-white on dark canvas).
pub const BRAND_INK_1: Color = Color::Rgb(232, 234, 238);
/// Muted ink — matches `--ink-3`.
pub const BRAND_INK_3: Color = Color::Rgb(142, 146, 158);
/// Page canvas — matches `--canvas`.
pub const BRAND_CANVAS: Color = Color::Rgb(22, 23, 27);
/// Accent ink — matches `--accent-ink` (very dark, used as fg on the
/// filled accent tab pill).
pub const BRAND_ACCENT_INK: Color = Color::Rgb(11, 12, 14);
/// Paused — a calm slate-blue, deliberately DISTINCT from the amber
/// "connecting" warn so a paused client can't be mistaken for one
/// that's mid-handshake. (Web app has no var; chosen to read on canvas.)
pub const BRAND_PAUSED: Color = Color::Rgb(125, 168, 232);
/// Strong emphasis ink for headline values (account value, big labels).
pub const BRAND_EMPHASIS: Color = Color::Rgb(247, 248, 250);

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub colored: bool,
}

impl Theme {
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self { colored: !no_color }
    }

    /// Used by snapshot tests so output is stable across hosts that
    /// might have NO_COLOR set globally.
    #[cfg(test)]
    pub fn plain() -> Self {
        Self { colored: false }
    }

    pub fn ok(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_ACCENT)
        } else {
            Style::default()
        }
    }

    pub fn warn(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_WARN)
        } else {
            Style::default()
        }
    }

    pub fn err(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_DOWN)
        } else {
            Style::default()
        }
    }

    pub fn neutral(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_INK_1)
        } else {
            Style::default()
        }
    }

    pub fn muted(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_INK_3)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn accent(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_ACCENT)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn tab_active(self) -> Style {
        if self.colored {
            Style::default()
                .fg(BRAND_ACCENT_INK)
                .bg(BRAND_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    pub fn tab_inactive(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_INK_3)
        } else {
            Style::default()
        }
    }

    pub fn header_bg(self) -> Style {
        if self.colored {
            Style::default().bg(BRAND_CANVAS)
        } else {
            Style::default()
        }
    }

    /// Paused state — slate-blue, distinct from connecting-amber so the
    /// two read differently at a glance.
    pub fn paused(self) -> Style {
        if self.colored {
            Style::default().fg(BRAND_PAUSED)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Headline emphasis (account value, big numbers).
    pub fn emphasis(self) -> Style {
        if self.colored {
            Style::default()
                .fg(BRAND_EMPHASIS)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A solid status pill: dark ink on a colored background, used for
    /// the connection state so it reads as a badge, not just text.
    pub fn pill(self, color: Color) -> Style {
        if self.colored {
            Style::default()
                .fg(BRAND_ACCENT_INK)
                .bg(color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    /// Resolve the brand color for a connection-state pill background.
    pub fn conn_color(self, ready: bool, paused: bool, error: bool) -> Color {
        if error {
            BRAND_DOWN
        } else if paused {
            BRAND_PAUSED
        } else if ready {
            BRAND_ACCENT
        } else {
            BRAND_WARN
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_strips_styles() {
        let t = Theme { colored: false };
        assert_eq!(t.ok(), Style::default());
        assert_eq!(t.err(), Style::default());
        assert_eq!(t.warn(), Style::default());
        assert_eq!(t.neutral(), Style::default());
    }

    #[test]
    fn colored_paints_status_distinctly() {
        let t = Theme { colored: true };
        assert_ne!(t.ok(), t.warn());
        assert_ne!(t.warn(), t.err());
    }

    #[test]
    fn brand_palette_matches_web_tokens() {
        // Sanity: lock the RGB triples so a future refactor can't
        // silently drift the brand colors away from the web palette.
        assert_eq!(BRAND_ACCENT, Color::Rgb(123, 235, 196));
        assert_eq!(BRAND_DOWN, Color::Rgb(244, 63, 94));
        assert_eq!(BRAND_INK_1, Color::Rgb(232, 234, 238));
        assert_eq!(BRAND_INK_3, Color::Rgb(142, 146, 158));
        assert_eq!(BRAND_CANVAS, Color::Rgb(22, 23, 27));
    }
}
