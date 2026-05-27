//! Small, reusable widgets shared across screens.
//!
//! The general rule: anything that draws into more than one screen
//! lives here. Anything screen-local lives in `screens::<name>`.

use std::time::Duration;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{ConnState, Modal, Tab};
use super::theme::Theme;
use super::TAB_ORDER;

pub struct HeaderProps {
    pub conn: ConnState,
    pub uptime: Duration,
    pub version: &'static str,
    pub update_available: Option<String>,
}

pub fn header(frame: &mut Frame<'_>, area: Rect, props: HeaderProps, theme: Theme) {
    let pill_style = match props.conn {
        ConnState::Ready => theme.ok().add_modifier(Modifier::BOLD),
        ConnState::Connecting => theme.warn().add_modifier(Modifier::BOLD),
        ConnState::Offline => theme.muted().add_modifier(Modifier::BOLD),
        ConnState::Paused => theme.warn().add_modifier(Modifier::BOLD),
        ConnState::Error => theme.err().add_modifier(Modifier::BOLD),
    };
    let dot_style = match props.conn {
        ConnState::Ready => theme.ok(),
        ConnState::Connecting => theme.warn(),
        ConnState::Offline => theme.muted(),
        ConnState::Paused => theme.warn(),
        ConnState::Error => theme.err(),
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("\u{25CF} ", dot_style),
        Span::styled(props.conn.label(), pill_style),
        Span::raw("   "),
        Span::styled("uptime ", theme.muted()),
        Span::styled(format_uptime(props.uptime), theme.neutral()),
        Span::raw("   "),
        Span::styled("v", theme.muted()),
        Span::styled(props.version, theme.neutral()),
    ];
    if let Some(latest) = props.update_available {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("\u{2191} update ", theme.warn()));
        spans.push(Span::styled(
            latest,
            theme.warn().add_modifier(Modifier::BOLD),
        ));
    }
    let title = Line::from(vec![Span::styled(
        "  DegenBox HL Signer",
        theme.accent().add_modifier(Modifier::BOLD),
    )]);
    let body = Line::from(spans);
    let lines = vec![title, body];
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_type(BorderType::Plain)
        .border_style(theme.muted());
    let p = Paragraph::new(lines).block(block).style(theme.header_bg());
    frame.render_widget(p, area);
}

/// Render the tab strip and return click hit-boxes plus the row they
/// live on so the parent can route mouse events.
pub fn tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    active: Tab,
    theme: Theme,
) -> (Vec<(u16, u16)>, u16) {
    let mut spans: Vec<Span> = Vec::new();
    let mut hits: Vec<(u16, u16)> = Vec::new();
    let mut col = area.x;
    spans.push(Span::raw(" "));
    col += 1;
    for (idx, tab) in TAB_ORDER.iter().enumerate() {
        let label = format!(" {} {} ", idx + 1, tab.label());
        let style = if *tab == active {
            theme.tab_active()
        } else {
            theme.tab_inactive()
        };
        let len = label.chars().count() as u16;
        hits.push((col, col + len.saturating_sub(1)));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        col += len + 1;
    }
    let p = Paragraph::new(Line::from(spans));
    frame.render_widget(p, area);
    (hits, area.y)
}

pub fn footer(frame: &mut Frame<'_>, area: Rect, hint: &str, theme: Theme) {
    let p = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(hint, theme.muted()),
    ]));
    frame.render_widget(p, area);
}

/// Two-column key-value row used by Status and Wallet.
pub fn kv_lines<'a>(rows: &'a [(&'a str, String)], theme: Theme) -> Vec<Line<'a>> {
    let max_key = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:width$}", k, width = max_key), theme.muted()),
                Span::raw("  "),
                Span::styled(v.clone(), theme.neutral()),
            ])
        })
        .collect()
}

pub fn panel<'a>(title: &'a str, theme: Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_set(border::ROUNDED)
        .border_style(theme.muted())
        .title(Span::styled(
            format!(" {} ", title),
            theme.accent().add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Left)
}

pub fn modal(frame: &mut Frame<'_>, modal: &Modal, theme: Theme) {
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(Clear, area);
    let (title, body) = match modal {
        Modal::Unlock { input, error } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    "Enter keystore passphrase to unlock.",
                    theme.neutral(),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("> ", theme.accent()),
                    Span::styled(
                        "*".repeat(input.chars().count()),
                        theme.neutral().add_modifier(Modifier::BOLD),
                    ),
                ]),
            ];
            if let Some(err) = error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(err.clone(), theme.err())));
            }
            ("Unlock keystore", lines)
        }
        Modal::RevealPhrase {
            input,
            revealed,
            error,
        } => {
            let mut lines = vec![Line::from(Span::styled(
                "Re-enter passphrase to reveal the secret key.",
                theme.neutral(),
            ))];
            if let Some(secret) = revealed {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(secret.clone(), theme.warn())));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Press Esc to dismiss. Do NOT screenshot this.",
                    theme.muted(),
                )));
            } else {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("> ", theme.accent()),
                    Span::styled(
                        "*".repeat(input.chars().count()),
                        theme.neutral().add_modifier(Modifier::BOLD),
                    ),
                ]));
                if let Some(err) = error {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(err.clone(), theme.err())));
                }
            }
            ("Reveal secret key", lines)
        }
        Modal::Confirm { title, body, .. } => (
            title.as_str(),
            vec![
                Line::from(Span::styled(body.clone(), theme.neutral())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("[y] yes", theme.warn()),
                    Span::raw("   "),
                    Span::styled("[n] no", theme.neutral()),
                ]),
            ],
        ),
        Modal::Message { title, body } => (
            title.as_str(),
            vec![
                Line::from(Span::styled(body.clone(), theme.neutral())),
                Line::from(""),
                Line::from(Span::styled("[Enter] dismiss", theme.muted())),
            ],
        ),
    };
    let block = panel(title, theme);
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    frame.render_widget(block, area);
    let p = Paragraph::new(body).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

pub fn format_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let (h, rem) = (s / 3600, s % 3600);
    let (m, sec) = (rem / 60, rem % 60);
    if h > 0 {
        format!("{h}h{m:02}m{sec:02}s")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Render a faint message inside a panel — used by tabs that have
/// nothing to show yet.
pub fn empty_panel<'a>(title: &'a str, msg: &'a str, theme: Theme) -> Paragraph<'a> {
    let body = vec![Line::from(""), Line::from(Span::styled(msg, theme.muted()))];
    Paragraph::new(body)
        .alignment(Alignment::Center)
        .block(panel(title, theme))
        .style(Style::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_formats_at_breakpoints() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "0s");
        assert_eq!(format_uptime(Duration::from_secs(45)), "45s");
        assert_eq!(format_uptime(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_uptime(Duration::from_secs(3661)), "1h01m01s");
    }

    #[test]
    fn kv_lines_pad_keys_to_max_width() {
        let theme = Theme::plain();
        let rows = vec![
            ("Short", "v1".to_string()),
            ("MuchLongerKey", "v2".to_string()),
        ];
        let lines = kv_lines(&rows, theme);
        // First line's key span has padding so visible width matches
        // the longest key.
        let first_key_span = &lines[0].spans[1];
        assert_eq!(first_key_span.content.len(), "MuchLongerKey".len());
    }

    #[test]
    fn centered_rect_shrinks_into_parent() {
        let r = Rect::new(0, 0, 100, 40);
        let inner = centered_rect(60, 30, r);
        assert!(inner.width <= r.width);
        assert!(inner.height <= r.height);
        assert!(inner.x >= r.x);
    }
}
