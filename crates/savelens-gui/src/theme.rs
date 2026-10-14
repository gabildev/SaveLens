//! Visual identity of the SaveLens desktop app: a near-black "cyberpunk"
//! dark theme with neon accents. Every color, radius and spacing used by the
//! views is defined here and applied once to the egui [`egui::Style`].

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Vec2};

/// Window background.
pub const BG: Color32 = Color32::from_rgb(0x0B, 0x0E, 0x14);
/// Panels (sidebar, top bar, bottom panel).
pub const BG_PANEL: Color32 = Color32::from_rgb(0x11, 0x15, 0x1E);
/// Cards and raised surfaces.
pub const BG_CARD: Color32 = Color32::from_rgb(0x17, 0x1C, 0x28);
/// Hovered cards / inputs.
pub const BG_RAISED: Color32 = Color32::from_rgb(0x1F, 0x26, 0x35);
/// Text fields, scroll bars.
pub const BG_INPUT: Color32 = Color32::from_rgb(0x07, 0x09, 0x0E);
/// Hairline borders.
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x32, 0x45);

/// Primary accent.
pub const CYAN: Color32 = Color32::from_rgb(0x00, 0xE5, 0xFF);
/// Secondary accent.
pub const MAGENTA: Color32 = Color32::from_rgb(0xFF, 0x2B, 0xD6);
/// Success.
pub const LIME: Color32 = Color32::from_rgb(0x9C, 0xFF, 0x3A);
/// Warnings.
pub const AMBER: Color32 = Color32::from_rgb(0xFF, 0xB0, 0x20);
/// Danger.
pub const RED: Color32 = Color32::from_rgb(0xFF, 0x3B, 0x3B);
/// Informational violet (tags, misc badges).
pub const VIOLET: Color32 = Color32::from_rgb(0x9D, 0x7B, 0xFF);

/// Main text.
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xEA, 0xF2);
/// Secondary text.
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8A, 0x94, 0xA8);
/// Disabled / faint text.
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x55, 0x5E, 0x70);

/// Corner radius of buttons and inputs.
pub const RADIUS: u8 = 6;
/// Corner radius of cards and windows.
pub const RADIUS_CARD: u8 = 10;
/// Base spacing unit.
pub const SPACING: f32 = 8.0;

/// Colors cycled through for the lanes of the commit graph.
pub const LANE_COLORS: [Color32; 6] = [CYAN, MAGENTA, LIME, AMBER, VIOLET, Color32::from_rgb(0x3A, 0xA0, 0xFF)];

/// Color of a lane of the commit graph.
pub fn lane_color(lane: usize) -> Color32 {
    LANE_COLORS[lane % LANE_COLORS.len()]
}

/// Builds the complete style from the egui defaults.
pub fn style_from(base: &egui::Style) -> egui::Style {
    let mut style = base.clone();
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = None;
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_CARD;
    v.extreme_bg_color = BG_INPUT;
    v.faint_bg_color = BG_CARD;
    v.code_bg_color = BG_INPUT;
    v.hyperlink_color = CYAN;
    v.warn_fg_color = AMBER;
    v.error_fg_color = RED;
    v.window_corner_radius = CornerRadius::same(RADIUS_CARD);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.selection.bg_fill = CYAN.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, CYAN);
    v.striped = true;
    v.slider_trailing_fill = true;
    v.indent_has_left_vline = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG_PANEL;
    w.noninteractive.weak_bg_fill = BG_PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = CornerRadius::same(RADIUS);

    w.inactive.bg_fill = BG_RAISED;
    w.inactive.weak_bg_fill = BG_RAISED;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = CornerRadius::same(RADIUS);

    w.hovered.bg_fill = Color32::from_rgb(0x25, 0x2E, 0x40);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x25, 0x2E, 0x40);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, CYAN.gamma_multiply(0.7));
    w.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.hovered.corner_radius = CornerRadius::same(RADIUS);

    w.active.bg_fill = CYAN.gamma_multiply(0.30);
    w.active.weak_bg_fill = CYAN.gamma_multiply(0.30);
    w.active.bg_stroke = Stroke::new(1.0_f32, CYAN);
    w.active.fg_stroke = Stroke::new(2.0_f32, Color32::WHITE);
    w.active.corner_radius = CornerRadius::same(RADIUS);

    w.open.bg_fill = BG_RAISED;
    w.open.weak_bg_fill = BG_RAISED;
    w.open.bg_stroke = Stroke::new(1.0_f32, CYAN);
    w.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.open.corner_radius = CornerRadius::same(RADIUS);

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(SPACING, SPACING * 0.75);
    s.button_padding = Vec2::new(SPACING * 1.25, SPACING * 0.6);
    s.window_margin = Margin::same(14);
    s.menu_margin = Margin::same(8);
    s.indent = 18.0;
    s.interact_size = Vec2::new(40.0, 24.0);
    s.slider_width = 160.0;
    s.combo_width = 140.0;

    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    style
}

/// Applies the theme to every egui style (dark and light) and forces dark mode.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        *style = style_from(style);
    });
}

/// Frame of a card.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(12))
}

/// Frame of a highlighted card (selected / current).
pub fn card_highlight(accent: Color32) -> egui::Frame {
    card().stroke(Stroke::new(1.5_f32, accent)).fill(BG_RAISED)
}

/// Frame of a side / top panel.
pub fn panel_frame() -> egui::Frame {
    egui::Frame::new().fill(BG_PANEL).inner_margin(Margin::same(12))
}

/// Frame of the central area.
pub fn central_frame() -> egui::Frame {
    egui::Frame::new().fill(BG).inner_margin(Margin::same(16))
}

/// Small rounded "chip" with a colored outline.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.8)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).size(11.5).color(color)))
        .response
}

/// Section title in the accent color.
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(12.0).strong().color(CYAN).extra_letter_spacing(1.2));
}

/// Monospace text in the primary text color.
pub fn mono(text: impl Into<String>) -> RichText {
    RichText::new(text).monospace().color(TEXT)
}

/// Filled accent button.
pub fn accent_button(text: &str, color: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).strong().color(BG))
        .fill(color)
        .corner_radius(CornerRadius::same(RADIUS))
}

/// Outlined button.
pub fn ghost_button(text: &str, color: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(color))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.8)))
        .corner_radius(CornerRadius::same(RADIUS))
}

/// Chip color for an integrity badge.
pub fn badge_color(badge: &str) -> Color32 {
    let lower = badge.to_ascii_lowercase();
    if lower.contains("lossless") {
        LIME
    } else if lower.contains("failed") || lower.contains("lossy") {
        RED
    } else if lower.contains("not verified") {
        AMBER
    } else if lower.contains("checksum") {
        MAGENTA
    } else if lower.contains("compression") {
        VIOLET
    } else if lower.starts_with("codec") {
        CYAN
    } else {
        AMBER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_uses_palette() {
        let s = style_from(&egui::Style::default());
        assert_eq!(s.visuals.panel_fill, BG_PANEL);
        assert_eq!(s.visuals.extreme_bg_color, BG_INPUT);
        assert_eq!(s.visuals.hyperlink_color, CYAN);
        assert_eq!(s.visuals.widgets.inactive.corner_radius, CornerRadius::same(RADIUS));
        assert!(s.visuals.dark_mode);
    }

    #[test]
    fn badge_colors() {
        assert_eq!(badge_color("Lossless Verified"), LIME);
        assert_eq!(badge_color("Checksum Auto-Matched: CRC32"), MAGENTA);
        assert_eq!(badge_color("Compression: Zlib"), VIOLET);
        assert_eq!(badge_color("Codec: json"), CYAN);
        assert_eq!(badge_color("Round-trip failed: boom"), RED);
        assert_eq!(lane_color(0), CYAN);
        assert_eq!(lane_color(LANE_COLORS.len()), CYAN);
    }
}
