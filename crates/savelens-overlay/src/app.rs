//! egui rendering of the overlay (thin layer over [`OverlayController`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align, Color32, Layout, RichText, Sense, Stroke, StrokeKind, Ui, ViewportCommand};
use savelens_watcher::Engine;

use crate::controller::{
    CheatRow, DaemonMode, NoticeLevel, OverlayCommand, OverlayController, Trend, LATENCY_WARN_MS,
};
use crate::theme;
use crate::OverlayOptions;

/// The overlay application.
pub struct OverlayApp {
    controller: OverlayController,
    opacity: f32,
    /// Frozen-value text buffers, by alias.
    edits: HashMap<String, String>,
    preset_name: String,
    selected_preset: String,
    styled: bool,
}

impl std::fmt::Debug for OverlayApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayApp").field("controller", &self.controller).finish()
    }
}

/// Converts the result of an action into an inline notice.
fn report<T>(c: &mut OverlayController, what: &str, r: anyhow::Result<T>) {
    if let Err(e) = r {
        c.notify(NoticeLevel::Error, format!("{what}: {e:#}"));
    }
}

impl OverlayApp {
    /// Builds the app and starts the daemon / hotkeys requested by `opts`.
    pub fn new(engine: Engine, opts: OverlayOptions) -> anyhow::Result<Self> {
        let mut controller = OverlayController::new(engine, &opts)?;
        controller.start_services();
        Ok(Self::with_controller(controller, opts.clamped_opacity()))
    }

    /// Wraps an existing controller (services already started or not).
    pub fn with_controller(controller: OverlayController, opacity: f32) -> Self {
        OverlayApp {
            controller,
            opacity: opacity.clamp(0.2, 1.0),
            edits: HashMap::new(),
            preset_name: String::new(),
            selected_preset: String::new(),
            styled: false,
        }
    }

    /// Hooks the app to the window: theme + UI wake-ups from hotkeys and
    /// daemon events.
    pub fn attach_context(&mut self, ctx: &egui::Context) {
        theme::apply(ctx);
        self.styled = true;
        let ctx = ctx.clone();
        self.controller.set_waker(Arc::new(move || ctx.request_repaint()));
    }

    pub fn controller(&self) -> &OverlayController {
        &self.controller
    }

    pub fn controller_mut(&mut self) -> &mut OverlayController {
        &mut self.controller
    }

    /// Draws one frame (and runs one controller step).
    pub fn ui(&mut self, ctx: &egui::Context) {
        if !self.styled {
            theme::apply(ctx);
            self.styled = true;
        }
        for cmd in self.controller.tick(Instant::now()) {
            match cmd {
                OverlayCommand::SetVisible(v) => {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(v));
                    if v {
                        ctx.send_viewport_cmd(ViewportCommand::Focus);
                    }
                }
            }
        }
        egui::CentralPanel::default()
            .frame(theme::panel_frame(self.opacity))
            .show(ctx, |ui| {
                self.header(ui, ctx);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.body(ui));
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn body(&mut self, ui: &mut Ui) {
        if self.controller.games().is_empty() {
            empty_state(
                ui,
                "No game tracked yet",
                "Track a game first (`savelens discover` + `savelens track`, or the desktop GUI), \
                 then pin variables such as Oro, Vida or Nivel with the scanner.",
            );
            self.footer(ui);
            return;
        }
        self.trainer_bar(ui);
        ui.add_space(4.0);
        self.cheat_table(ui);
        ui.add_space(4.0);
        if !self.controller.rows().is_empty() {
            self.presets_bar(ui);
            ui.add_space(4.0);
        }
        self.actions(ui);
        ui.add_space(4.0);
        self.footer(ui);
    }

    fn header(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let resp = ui
            .horizontal(|ui| {
                ui.label(RichText::new("◆ SAVELENS").color(theme::CYAN).strong().heading());
                let games: Vec<(i64, String)> = self.controller.games().iter().map(|g| (g.id, g.name.clone())).collect();
                if !games.is_empty() {
                    let current = self.controller.selected_game().map(|g| g.name.clone()).unwrap_or_default();
                    let mut chosen = None;
                    egui::ComboBox::from_id_salt("overlay-game")
                        .selected_text(RichText::new(current).color(theme::TEXT))
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for (id, name) in &games {
                                let sel = self.controller.selected_game().is_some_and(|g| g.id == *id);
                                if ui.selectable_label(sel, name).clicked() {
                                    chosen = Some(*id);
                                }
                            }
                        });
                    if let Some(id) = chosen {
                        self.edits.clear();
                        let r = self.controller.select_game(id);
                        report(&mut self.controller, "select game", r);
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("✕").on_hover_text("Close the overlay").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    if ui.small_button("—").on_hover_text("Minimize").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                    let (color, tip) = match self.controller.daemon_mode() {
                        DaemonMode::Embedded => (theme::GREEN, "embedded daemon running".to_string()),
                        DaemonMode::External { pid } => (theme::GREEN, format!("`savelens watch` running (pid {pid})")),
                        DaemonMode::Absent => (theme::AMBER, "no daemon: auto-freeze inactive".to_string()),
                    };
                    ui.label(RichText::new("●").color(color)).on_hover_text(tip);
                });
            })
            .response;
        // Dragging the header moves the borderless window.
        let drag = ui.interact(resp.rect, ui.id().with("overlay-drag"), Sense::drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        ui.painter().hline(
            resp.rect.x_range(),
            resp.rect.bottom() + 3.0,
            Stroke::new(1.0_f32, theme::CYAN.gamma_multiply(0.4)),
        );
        ui.add_space(6.0);
    }

    fn trainer_bar(&mut self, ui: &mut Ui) {
        let opacity = self.opacity;
        theme::card_frame(opacity).show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut on = self.controller.trainer_enabled();
                let label = if on { "TRAINER ON" } else { "TRAINER OFF" };
                if toggle(ui, &mut on).on_hover_text("Freeze-on-save: re-apply frozen values on every autosave").changed() {
                    let r = self.controller.set_trainer_enabled(on);
                    report(&mut self.controller, "trainer", r);
                }
                ui.label(RichText::new(label).strong().color(if on { theme::GREEN } else { theme::DIM }));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.controller.last_latency() {
                    Some(l) => {
                        let color = if l.ms > LATENCY_WARN_MS { theme::AMBER } else { theme::GREEN };
                        let tip = if l.applied.is_empty() {
                            format!("{:?}", l.source)
                        } else {
                            format!("{} → {} ({:?})", l.applied.join(", "), l.rel_path, l.source)
                        };
                        let mut text = format!("⚡ {:.1} ms", l.ms);
                        if l.is_slow() {
                            text.push_str(" ⚠");
                        }
                        ui.label(RichText::new(text).monospace().color(color)).on_hover_text(tip);
                    }
                    None => {
                        ui.label(RichText::new("⚡ — ms").monospace().color(theme::DIM))
                            .on_hover_text("no injection yet");
                    }
                });
            });
        });
    }

    fn cheat_table(&mut self, ui: &mut Ui) {
        let rows: Vec<CheatRow> = self.controller.rows().to_vec();
        if rows.is_empty() {
            empty_state(
                ui,
                "No pinned variables",
                "Use the Scanner (GUI / TUI) or `savelens pin` to pin Oro, Vida, Nivel… — they will appear here with live values.",
            );
            return;
        }
        let opacity = self.opacity;
        theme::card_frame(opacity).show(ui, |ui| {
            egui::Grid::new("overlay-cheats")
                .num_columns(5)
                .spacing([6.0, 4.0])
                .striped(false)
                .show(ui, |ui| {
                    ui.label(RichText::new("VAR").small().color(theme::DIM));
                    ui.label(RichText::new("ON DISK").small().color(theme::DIM));
                    ui.label(RichText::new("TREND").small().color(theme::DIM));
                    ui.label(RichText::new("❄").small().color(theme::DIM));
                    ui.label(RichText::new("FROZEN").small().color(theme::DIM));
                    ui.end_row();
                    for row in &rows {
                        self.cheat_row(ui, row);
                        ui.end_row();
                    }
                });
        });
    }

    fn cheat_row(&mut self, ui: &mut Ui, row: &CheatRow) {
        ui.label(RichText::new(&row.alias).strong().color(theme::TEXT))
            .on_hover_text(format!("{} · {}", row.rel_path, row.location));
        match (&row.current, &row.error) {
            (Some(v), _) => {
                let color = if row.drifted() { theme::AMBER } else { theme::CYAN };
                ui.label(RichText::new(v.to_string()).monospace().strong().color(color));
            }
            (None, Some(e)) => {
                ui.label(RichText::new("error").color(theme::DANGER)).on_hover_text(e);
            }
            (None, None) => {
                ui.label(RichText::new("—").color(theme::DIM));
            }
        }
        ui.horizontal(|ui| {
            let (arrow, color) = match row.trend() {
                Trend::Up => ("▲", theme::GREEN),
                Trend::Down => ("▼", theme::MAGENTA),
                Trend::Flat => ("=", theme::DIM),
                Trend::Unknown => ("·", theme::DIM),
            };
            ui.label(RichText::new(arrow).color(color));
            sparkline(ui, row);
        });
        let mut frozen = row.freeze_enabled;
        if ui.checkbox(&mut frozen, "").on_hover_text("Freeze on save").changed() {
            let r = self.controller.set_freeze_enabled(&row.alias, frozen);
            report(&mut self.controller, &row.alias, r);
        }
        let id = ui.make_persistent_id(("overlay-frozen", &row.alias));
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        let stored = row.frozen_value.map(|v| v.to_string()).unwrap_or_default();
        let buf = self.edits.entry(row.alias.clone()).or_default();
        if !focused {
            buf.clone_from(&stored);
        }
        let hint = row.current.map(|v| v.to_string()).unwrap_or_default();
        let resp = ui.add(
            egui::TextEdit::singleline(buf)
                .id(id)
                .desired_width(70.0)
                .font(egui::TextStyle::Monospace)
                .hint_text(hint),
        );
        if resp.lost_focus() && *buf != stored {
            let input = buf.clone();
            let r = self.controller.set_frozen_value(&row.alias, &input);
            report(&mut self.controller, &row.alias, r);
        }
    }

    fn presets_bar(&mut self, ui: &mut Ui) {
        let presets = self.controller.presets();
        let opacity = self.opacity;
        theme::card_frame(opacity).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("TEMPLATE").small().color(theme::DIM));
                if !presets.iter().any(|p| p.name == self.selected_preset) {
                    self.selected_preset = presets.first().map(|p| p.name.clone()).unwrap_or_default();
                }
                let shown = if self.selected_preset.is_empty() {
                    "(none)".to_string()
                } else {
                    self.selected_preset.clone()
                };
                egui::ComboBox::from_id_salt("overlay-preset")
                    .selected_text(shown)
                    .width(96.0)
                    .show_ui(ui, |ui| {
                        for p in &presets {
                            let label = if p.builtin { format!("★ {}", p.name) } else { p.name.clone() };
                            ui.selectable_value(&mut self.selected_preset, p.name.clone(), label);
                        }
                    });
                let has = !self.selected_preset.is_empty();
                if ui.add_enabled(has, egui::Button::new("Load")).on_hover_text("Set the frozen values of this template").clicked() {
                    let name = self.selected_preset.clone();
                    let r = self.controller.apply_preset(&name);
                    report(&mut self.controller, "load preset", r);
                }
                let custom = presets.iter().any(|p| p.name == self.selected_preset && !p.builtin);
                if ui.add_enabled(custom, egui::Button::new("🗑")).on_hover_text("Delete this template").clicked() {
                    let name = self.selected_preset.clone();
                    let r = self.controller.delete_preset(&name);
                    report(&mut self.controller, "delete preset", r);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.preset_name)
                        .desired_width(80.0)
                        .hint_text("name…"),
                );
                let can_save = !self.preset_name.trim().is_empty();
                if ui.add_enabled(can_save, egui::Button::new("Save")).on_hover_text("Save the current frozen values as a template").clicked() {
                    let name = self.preset_name.trim().to_string();
                    match self.controller.save_preset(&name) {
                        Ok(p) => {
                            self.selected_preset = p.name;
                            self.preset_name.clear();
                        }
                        Err(e) => self.controller.notify(NoticeLevel::Error, format!("save preset: {e:#}")),
                    }
                }
            });
        });
    }

    fn actions(&mut self, ui: &mut Ui) {
        let has_game = self.controller.selected_game().is_some();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(has_game, neon_button("⚡ Apply now", theme::CYAN))
                .on_hover_text("Inject the frozen values into the save on disk now")
                .clicked()
            {
                let r = self.controller.apply_now();
                report(&mut self.controller, "apply now", r);
            }
            if ui
                .add_enabled(has_game, neon_button("📷 Snap + template", theme::CYAN))
                .on_hover_text("Snapshot every tracked file, then apply the template")
                .clicked()
            {
                let r = self.controller.quick_snapshot();
                report(&mut self.controller, "quick snapshot", r);
            }
            if ui
                .add_enabled(has_game, neon_button("⟲ Rollback", theme::MAGENTA))
                .on_hover_text("Restore the previous commit of the current branch")
                .clicked()
            {
                let r = self.controller.quick_rollback();
                report(&mut self.controller, "quick rollback", r);
            }
        });
        let panic = egui::Button::new(RichText::new("⛔ PANIC RESTORE").strong().color(Color32::WHITE))
            .fill(theme::DANGER.gamma_multiply(0.85))
            .stroke(Stroke::new(1.0_f32, theme::DANGER));
        let width = ui.available_width().max(1.0);
        if ui
            .add_sized([width, 24.0], panic)
            .on_hover_text("Revert the last mutation to its pre-mutation bytes (bit for bit)")
            .clicked()
        {
            let r = self.controller.panic_restore();
            report(&mut self.controller, "panic restore", r);
        }
    }

    fn footer(&mut self, ui: &mut Ui) {
        let status = self.controller.hotkey_status().clone();
        let color = if status.is_warning() { theme::AMBER } else { theme::DIM };
        ui.label(RichText::new(format!("⌨ {}", status.describe())).small().color(color));
        let notices: Vec<_> = self.controller.notices().cloned().collect();
        if !notices.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("NOTICES").small().color(theme::DIM));
                if ui.small_button("clear").clicked() {
                    self.controller.clear_notices();
                }
            });
            for n in notices.iter().rev() {
                let color = match n.level {
                    NoticeLevel::Info => theme::TEXT,
                    NoticeLevel::Warn => theme::AMBER,
                    NoticeLevel::Error => theme::DANGER,
                };
                ui.label(RichText::new(&n.text).small().color(color));
            }
        }
        let activity: Vec<String> = self.controller.activity().cloned().collect();
        if !activity.is_empty() {
            ui.label(RichText::new("ACTIVITY").small().color(theme::DIM));
            for line in activity.iter().rev().take(4) {
                ui.label(RichText::new(line).small().color(theme::DIM));
            }
        }
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn empty_state(ui: &mut Ui, title: &str, hint: &str) {
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).heading().color(theme::CYAN));
        ui.add_space(4.0);
        ui.label(RichText::new(hint).color(theme::DIM));
    });
    ui.add_space(12.0);
}

fn neon_button(text: &str, accent: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(accent)).stroke(Stroke::new(1.0_f32, accent.gamma_multiply(0.7)))
}

/// iOS-style switch.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(32.0, 16.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let radius = rect.height() / 2.0;
        let bg = if *on { theme::GREEN.gamma_multiply(0.45) } else { Color32::from_rgb(45, 50, 70) };
        let painter = ui.painter();
        painter.rect_filled(rect, radius, bg);
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0_f32, if *on { theme::GREEN } else { theme::DIM }),
            StrokeKind::Inside,
        );
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        painter.circle_filled(egui::pos2(x, rect.center().y), radius - 2.5, theme::TEXT);
    }
    resp
}

/// Small line chart of the last values of a variable.
fn sparkline(ui: &mut Ui, row: &CheatRow) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(52.0, 14.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let values: Vec<f64> = row.history.iter().copied().filter(|v| v.is_finite()).collect();
    let painter = ui.painter_at(rect);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(0.5_f32, theme::DIM.gamma_multiply(0.4)));
    if values.len() < 2 {
        return;
    }
    let (min, max) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
    let span = if max > min { max - min } else { 1.0 };
    let n = values.len();
    let points: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * i as f32 / (n - 1) as f32;
            let norm = if max > min { ((v - min) / span) as f32 } else { 0.5 };
            let y = rect.bottom() - 1.0 - norm * (rect.height() - 2.0);
            egui::pos2(x, y)
        })
        .collect();
    let last = points[points.len() - 1];
    painter.line(points, Stroke::new(1.2_f32, theme::CYAN));
    painter.circle_filled(last, 1.8, theme::MAGENTA);
    resp.on_hover_text(format!(
        "last {n} values: min {} · max {}",
        savelens_core::node::format_float(min),
        savelens_core::node::format_float(max)
    ));
}
