//! Small reusable UI pieces: text input, toasts, layout helpers, formatting.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

/// A single-line text input with a cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    buf: String,
    /// Cursor position in characters.
    cursor: usize,
}

/// Outcome of a key press in a text input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Still editing.
    Editing,
    /// Enter pressed.
    Submit,
    /// Esc pressed.
    Cancel,
}

impl TextInput {
    pub fn new(initial: &str) -> Self {
        TextInput { buf: initial.to_string(), cursor: initial.chars().count() }
    }

    pub fn value(&self) -> &str {
        &self.buf
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.buf.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.buf.len())
    }

    /// Applies a key press.
    pub fn handle(&mut self, key: KeyEvent) -> InputEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return InputEvent::Submit,
            KeyCode::Esc => return InputEvent::Cancel,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte_index(self.cursor);
                self.buf.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let end = self.byte_index(self.cursor);
                let head = &self.buf[..end];
                let trimmed = head.trim_end();
                let start = trimmed.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                let removed = self.buf[start..end].chars().count();
                self.buf.replace_range(start..end, "");
                self.cursor -= removed;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.buf.chars().count(),
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte_index(self.cursor);
                self.buf.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let at = self.byte_index(self.cursor - 1);
                    self.buf.remove(at);
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.buf.chars().count() {
                    let at = self.byte_index(self.cursor);
                    self.buf.remove(at);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.buf.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.buf.chars().count(),
            _ => {}
        }
        InputEvent::Editing
    }

    /// Rendered spans with a block cursor.
    pub fn spans(&self, focused: bool) -> Vec<Span<'static>> {
        let text_style = Style::default().fg(theme::TEXT);
        if !focused {
            return vec![Span::styled(self.buf.clone(), text_style)];
        }
        let at = self.byte_index(self.cursor);
        let (head, tail) = self.buf.split_at(at);
        let mut chars = tail.chars();
        let cur = chars.next().map(|c| c.to_string()).unwrap_or_else(|| " ".to_string());
        vec![
            Span::styled(head.to_string(), text_style),
            Span::styled(cur, Style::default().fg(theme::BG).bg(theme::ACCENT)),
            Span::styled(chars.collect::<String>(), text_style),
        ]
    }
}

/// Severity of a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

impl Level {
    pub fn color(self) -> Color {
        match self {
            Level::Info => theme::ACCENT,
            Level::Success => theme::GREEN,
            Level::Warning => theme::YELLOW,
            Level::Error => theme::RED,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Level::Info => "●",
            Level::Success => "✔",
            Level::Warning => "▲",
            Level::Error => "✖",
        }
    }
}

/// A notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

/// Notification area: recent toasts are shown as an overlay, the full
/// history is kept (bounded) for the status line.
#[derive(Debug, Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    /// How long a toast stays on screen.
    pub const TTL: Duration = Duration::from_secs(6);
    const MAX: usize = 200;

    pub fn push(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        tracing::debug!(?level, %text, "toast");
        self.items.push(Toast { level, text, at: Instant::now() });
        if self.items.len() > Self::MAX {
            let extra = self.items.len() - Self::MAX;
            self.items.drain(..extra);
        }
    }

    /// Toasts still visible (newest last).
    pub fn visible(&self, max: usize) -> Vec<&Toast> {
        let now = Instant::now();
        let live: Vec<&Toast> = self.items.iter().filter(|t| now.duration_since(t.at) < Self::TTL).collect();
        let skip = live.len().saturating_sub(max);
        live.into_iter().skip(skip).collect()
    }

    /// Every toast (oldest first).
    pub fn all(&self) -> &[Toast] {
        &self.items
    }
}

/// A rectangle of `w` x `h` cells centred in `area` (clamped to it).
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// Human byte size.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Compact timestamp: time only for today, date + time otherwise.
pub fn short_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) => {
            if t.date_naive() == Local::now().date_naive() {
                t.format("%H:%M:%S").to_string()
            } else {
                t.format("%m-%d %H:%M").to_string()
            }
        }
        _ => ms.to_string(),
    }
}

/// Human age (`3s ago`, `5m ago`).
pub fn age(ms: i64) -> String {
    let d = (savelens_cas::now_ms() - ms).max(0) / 1000;
    if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// Truncates to `max` characters with an ellipsis.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// A `label: value` line.
pub fn kv(label: &str, value: impl Into<String>, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme::muted()),
        Span::styled(value.into(), Style::default().fg(color)),
    ])
}

/// A coloured `[badge]`.
pub fn badge(text: &str, color: Color) -> Span<'static> {
    Span::styled(format!("[{text}]"), Style::default().fg(color).add_modifier(Modifier::BOLD))
}

/// Spinner frame.
pub fn spinner(frame: u64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(frame % FRAMES.len() as u64) as usize]
}

/// Moves a selection index by `delta` within `len` items (clamped).
pub fn step(sel: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    if delta < 0 {
        sel.saturating_sub(delta.unsigned_abs()).min(max)
    } else {
        sel.saturating_add(delta as usize).min(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn text_input_editing() {
        let mut t = TextInput::new("ab");
        assert_eq!(t.handle(key(KeyCode::Char('c'))), InputEvent::Editing);
        assert_eq!(t.value(), "abc");
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Char('é')));
        assert_eq!(t.value(), "aébc");
        t.handle(key(KeyCode::Backspace));
        assert_eq!(t.value(), "abc");
        t.handle(key(KeyCode::End));
        t.handle(key(KeyCode::Char(' ')));
        t.handle(key(KeyCode::Char('x')));
        t.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.value(), "abc ");
        t.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(t.value(), "");
        assert_eq!(t.handle(key(KeyCode::Enter)), InputEvent::Submit);
        assert_eq!(t.handle(key(KeyCode::Esc)), InputEvent::Cancel);
    }

    #[test]
    fn helpers() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
        assert_eq!(step(0, 3, -1), 0);
        assert_eq!(step(1, 3, 5), 2);
        assert_eq!(step(0, 0, 1), 0);
        let r = centered(Rect::new(0, 0, 10, 4), 20, 2);
        assert_eq!((r.width, r.height, r.y), (10, 2, 1));
    }

    #[test]
    fn toasts_are_bounded() {
        let mut t = Toasts::default();
        for i in 0..250 {
            t.push(Level::Info, format!("n{i}"));
        }
        assert_eq!(t.all().len(), 200);
        assert_eq!(t.visible(3).len(), 3);
        assert_eq!(t.all().last().map(|x| x.text.as_str()), Some("n249"));
    }
}
