//! Neon-on-glass look of the overlay.

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle};

pub const CYAN: Color32 = Color32::from_rgb(0, 229, 255);
pub const MAGENTA: Color32 = Color32::from_rgb(255, 46, 170);
pub const GREEN: Color32 = Color32::from_rgb(57, 255, 136);
pub const AMBER: Color32 = Color32::from_rgb(255, 184, 0);
pub const DANGER: Color32 = Color32::from_rgb(255, 59, 64);
pub const TEXT: Color32 = Color32::from_rgb(222, 228, 240);
pub const DIM: Color32 = Color32::from_rgb(128, 140, 165);

/// Background of the panel for an opacity in `0.0..=1.0`.
pub fn panel_fill(opacity: f32) -> Color32 {
    let a = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(10, 12, 20, a)
}

/// Background of the inner cards.
pub fn card_fill(opacity: f32) -> Color32 {
    let a = (opacity.clamp(0.0, 1.0) * 200.0).round() as u8;
    Color32::from_rgba_unmultiplied(22, 26, 40, a)
}

/// Frame of the whole overlay.
pub fn panel_frame(opacity: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(panel_fill(opacity))
        .stroke(Stroke::new(1.0_f32, CYAN.gamma_multiply(0.55)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(8))
}

/// Frame of a section.
pub fn card_frame(opacity: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(card_fill(opacity))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(0, 229, 255, 40)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(6, 4))
}

/// Compact dark style with neon accents.
pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(10.0, FontFamily::Proportional)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(6.0, 2.0);
    style.spacing.interact_size.y = 18.0;
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = panel_fill(0.95);
    v.extreme_bg_color = Color32::from_rgba_unmultiplied(0, 0, 0, 140);
    v.selection.bg_fill = CYAN.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, CYAN);
    v.hyperlink_color = CYAN;
    v.widgets.inactive.weak_bg_fill = Color32::from_rgba_unmultiplied(30, 36, 56, 200);
    v.widgets.inactive.bg_fill = Color32::from_rgba_unmultiplied(30, 36, 56, 200);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(0, 120, 140, 200);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, CYAN);
    v.widgets.active.weak_bg_fill = Color32::from_rgba_unmultiplied(0, 160, 190, 220);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, CYAN);
    style.visuals = v;
    ctx.set_style(style);
}
