//! Auto-expiring toast notifications rendered in the top-right corner.

use std::time::{Duration, Instant};

use egui::{Align2, CornerRadius, Margin, RichText, Stroke};

use crate::theme;

/// Severity of a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    fn color(self) -> egui::Color32 {
        match self {
            ToastKind::Info => theme::CYAN,
            ToastKind::Success => theme::LIME,
            ToastKind::Warning => theme::AMBER,
            ToastKind::Error => theme::RED,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            ToastKind::Info => "ℹ",
            ToastKind::Success => "✔",
            ToastKind::Warning => "⚠",
            ToastKind::Error => "✖",
        }
    }

    fn ttl(self) -> Duration {
        match self {
            ToastKind::Info | ToastKind::Success => Duration::from_secs(4),
            ToastKind::Warning => Duration::from_secs(7),
            ToastKind::Error => Duration::from_secs(10),
        }
    }
}

/// One notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub created: Instant,
    pub ttl: Duration,
}

/// The toast stack.
#[derive(Debug, Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

/// Maximum number of toasts kept on screen.
const MAX_TOASTS: usize = 6;

impl Toasts {
    /// Adds a toast.
    pub fn push(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        match kind {
            ToastKind::Error => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        self.items.push(Toast { kind, text, created: Instant::now(), ttl: kind.ttl() });
        if self.items.len() > MAX_TOASTS {
            let extra = self.items.len() - MAX_TOASTS;
            self.items.drain(..extra);
        }
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.push(ToastKind::Info, text);
    }

    pub fn success(&mut self, text: impl Into<String>) {
        self.push(ToastKind::Success, text);
    }

    pub fn warning(&mut self, text: impl Into<String>) {
        self.push(ToastKind::Warning, text);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(ToastKind::Error, text);
    }

    /// Current toasts (oldest first).
    pub fn items(&self) -> &[Toast] {
        &self.items
    }

    /// Removes expired toasts.
    pub fn expire(&mut self, now: Instant) {
        self.items.retain(|t| now.duration_since(t.created) < t.ttl);
    }

    /// Draws the stack and drops expired entries.
    pub fn show(&mut self, ctx: &egui::Context) {
        self.expire(Instant::now());
        if self.items.is_empty() {
            return;
        }
        let mut dismiss = None;
        egui::Area::new(egui::Id::new("savelens-toasts"))
            .anchor(Align2::RIGHT_TOP, egui::vec2(-16.0, 64.0))
            .order(egui::Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                for (i, t) in self.items.iter().enumerate().rev() {
                    let color = t.kind.color();
                    let frame = egui::Frame::new()
                        .fill(theme::BG_CARD)
                        .stroke(Stroke::new(1.0_f32, color))
                        .corner_radius(CornerRadius::same(theme::RADIUS_CARD))
                        .inner_margin(Margin::symmetric(12, 8));
                    let resp = frame
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(t.kind.icon()).color(color).strong());
                                ui.add(egui::Label::new(RichText::new(&t.text).color(theme::TEXT)).wrap());
                            });
                        })
                        .response
                        .interact(egui::Sense::click());
                    if resp.clicked() {
                        dismiss = Some(i);
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(i) = dismiss {
            self.items.remove(i);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_expire_and_are_capped() {
        let mut t = Toasts::default();
        for i in 0..10 {
            t.info(format!("n{i}"));
        }
        assert_eq!(t.items().len(), MAX_TOASTS);
        assert_eq!(t.items()[0].text, "n4");
        t.error("boom");
        let later = Instant::now() + Duration::from_secs(5);
        t.expire(later);
        assert_eq!(t.items().len(), 1);
        assert_eq!(t.items()[0].kind, ToastKind::Error);
        t.expire(Instant::now() + Duration::from_secs(11));
        assert!(t.items().is_empty());
    }
}
