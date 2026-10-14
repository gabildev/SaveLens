//! Dark colour theme and shared styles.
//!
//! The palette is a muted dark scheme with a blue primary accent and a mauve
//! secondary accent; every widget takes its colours from here so the UI stays
//! consistent.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};
use savelens_cas::CommitKind;

pub const BG: Color = Color::Rgb(17, 17, 27);
pub const PANEL: Color = Color::Rgb(24, 24, 37);
pub const SURFACE: Color = Color::Rgb(49, 50, 68);
pub const OVERLAY: Color = Color::Rgb(88, 91, 112);
pub const TEXT: Color = Color::Rgb(205, 214, 244);
pub const SUBTEXT: Color = Color::Rgb(166, 173, 200);
pub const MUTED: Color = Color::Rgb(108, 112, 134);
pub const ACCENT: Color = Color::Rgb(137, 180, 250);
pub const ACCENT2: Color = Color::Rgb(203, 166, 247);
pub const GREEN: Color = Color::Rgb(166, 227, 161);
pub const YELLOW: Color = Color::Rgb(249, 226, 175);
pub const RED: Color = Color::Rgb(243, 139, 168);
pub const PEACH: Color = Color::Rgb(250, 179, 135);
pub const TEAL: Color = Color::Rgb(148, 226, 213);
pub const PINK: Color = Color::Rgb(245, 194, 231);

/// Default text on the background.
pub fn base() -> Style {
    Style::default().fg(TEXT).bg(BG)
}

/// Default text inside panels.
pub fn panel() -> Style {
    Style::default().fg(TEXT).bg(PANEL)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn subtle() -> Style {
    Style::default().fg(SUBTEXT)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn bold_accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn title() -> Style {
    Style::default().fg(ACCENT2).add_modifier(Modifier::BOLD)
}

/// Highlighted row of a focused list or table.
pub fn selected() -> Style {
    Style::default().bg(SURFACE).fg(TEXT).add_modifier(Modifier::BOLD)
}

/// Highlighted row of an unfocused list or table.
pub fn selected_dim() -> Style {
    Style::default().bg(Color::Rgb(36, 37, 54))
}

pub fn header_row() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// A rounded panel block; the border is accented when focused.
pub fn block(title: &str, focused: bool) -> Block<'static> {
    let border = if focused { ACCENT } else { OVERLAY };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(format!(" {title} "), if focused { title_focused() } else { subtle() }))
        .style(panel())
}

fn title_focused() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Block of a modal dialog.
pub fn modal(title: &str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(color))
        .title(Span::styled(format!(" {title} "), Style::default().fg(color).add_modifier(Modifier::BOLD)))
        .style(Style::default().fg(TEXT).bg(PANEL))
}

/// Colour of a commit-kind badge.
pub fn kind_color(kind: CommitKind) -> Color {
    match kind {
        CommitKind::Auto => TEAL,
        CommitKind::Manual => ACCENT,
        CommitKind::Trainer => PINK,
        CommitKind::Edit => PEACH,
        CommitKind::CherryPick => ACCENT2,
        CommitKind::Restore => YELLOW,
        CommitKind::Import => GREEN,
    }
}

/// Colour of an integrity badge.
pub fn badge_color(badge: &str) -> Color {
    if badge.starts_with("Lossless") {
        GREEN
    } else if badge.starts_with("Checksum") {
        TEAL
    } else if badge.starts_with("Compression") || badge.starts_with("Base64") {
        ACCENT
    } else if badge.starts_with("Crypto") {
        ACCENT2
    } else if badge.starts_with("Lossy") {
        YELLOW
    } else if badge.starts_with("Round-trip") {
        RED
    } else {
        SUBTEXT
    }
}

/// A key hint (`key` highlighted, `label` muted).
pub fn hint<'a>(key: &'a str, label: &'a str) -> [Span<'a>; 3] {
    [
        Span::styled(format!(" {key} "), Style::default().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {label}"), Style::default().fg(SUBTEXT)),
        Span::raw("  "),
    ]
}
