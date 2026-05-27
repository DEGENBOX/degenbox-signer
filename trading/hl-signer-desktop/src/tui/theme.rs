//! Color tokens.
//!
//! Respects `NO_COLOR=1` (https://no-color.org). When set, every
//! style returned here is `Style::default()` — the layout still
//! draws but with the terminal's default fg/bg.

use ratatui::style::{Color, Modifier, Style};

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
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        }
    }

    pub fn warn(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        }
    }

    pub fn err(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        }
    }

    pub fn neutral(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Gray)
        } else {
            Style::default()
        }
    }

    pub fn muted(self) -> Style {
        if self.colored {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn accent(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn tab_active(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    pub fn tab_inactive(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Gray)
        } else {
            Style::default()
        }
    }

    pub fn header_bg(self) -> Style {
        if self.colored {
            Style::default().bg(Color::Black)
        } else {
            Style::default()
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
}
