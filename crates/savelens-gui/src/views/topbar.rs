//! Top bar: title, game / branch / file selectors, integrity chips, watcher
//! status and the panic button.

use egui::{RichText, Stroke};

use crate::app::{Intent, SaveLensApp};
use crate::theme;

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("🔸 SAVELENS").size(22.0).strong().color(theme::CYAN).extra_letter_spacing(2.0));
        ui.add_space(theme::SPACING * 2.0);

        // Game selector.
        let game_label = app.selected_game_record().map(|g| g.name.clone()).unwrap_or_else(|| "No game".to_string());
        egui::ComboBox::from_id_salt("topbar-game")
            .selected_text(RichText::new(game_label).strong())
            .width(200.0)
            .show_ui(ui, |ui| {
                for g in &app.games {
                    if ui.selectable_label(app.selected_game == Some(g.id), &g.name).clicked() {
                        app.intents.push(Intent::SelectGame(g.id));
                    }
                }
            });

        if let Some(g) = app.selected_game_record() {
            theme::chip(ui, &format!("🔀 {}", g.current_branch), theme::MAGENTA);
        }

        // File selector.
        let file_label = app.selected_file.clone().unwrap_or_else(|| "No file".to_string());
        egui::ComboBox::from_id_salt("topbar-file").selected_text(theme::mono(file_label)).width(220.0).show_ui(
            ui,
            |ui| {
                for f in &app.files {
                    let sel = app.selected_file.as_deref() == Some(f.rel_path.as_str());
                    if ui.selectable_label(sel, theme::mono(format!("{}  ({} B)", f.rel_path, f.size))).clicked() {
                        app.intents.push(Intent::SelectFile(f.rel_path.clone()));
                    }
                }
            },
        );

        // Integrity chips.
        if let Some(info) = &app.integrity {
            for b in &info.badges {
                theme::chip(ui, b, theme::badge_color(b));
            }
            let (mode, color) = match info.edit_mode {
                savelens_core::EditMode::Structural => ("Edit: structural", theme::LIME),
                savelens_core::EditMode::RawInPlace => ("Edit: raw in-place", theme::AMBER),
                savelens_core::EditMode::BackupRestoreOnly => ("Edit: backup/restore only", theme::RED),
            };
            theme::chip(ui, mode, color);
        } else if let Some(e) = &app.integrity_error {
            theme::chip(ui, "Unreadable", theme::RED).on_hover_text(e);
        } else if app.selected_file.is_some() {
            ui.add(egui::Spinner::new().size(14.0).color(theme::CYAN));
        }
    });

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        // Watcher status.
        let (status, color) = if app.daemon_running() {
            ("⏺ Watcher running (embedded)", theme::LIME)
        } else if let Some(d) = &app.external_daemon {
            if d.pid == std::process::id() {
                ("⏺ Watcher running", theme::LIME)
            } else {
                ("⏺ External watcher running", theme::CYAN)
            }
        } else {
            ("○ Watcher stopped", theme::TEXT_DIM)
        };
        theme::chip(ui, status, color);
        let label = if app.daemon_running() { "Stop watcher" } else { "Start watcher" };
        if ui.add(theme::ghost_button(label, theme::CYAN)).clicked() {
            app.intents.push(Intent::ToggleDaemon);
        }
        if let Some(s) = &app.stats {
            ui.label(
                RichText::new(format!(
                    "{} commits · {} backups · dedup ×{:.1}",
                    s.commits,
                    s.safety_backups,
                    s.dedup_ratio()
                ))
                .small()
                .color(theme::TEXT_DIM),
            );
        }
        if let Some(t) = &app.last_safety_tag {
            ui.label(RichText::new(format!("last backup: {t}")).small().monospace().color(theme::AMBER));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let panic = egui::Button::new(
                RichText::new("⚠  PANIC RESTORE ORIGINAL").size(15.0).strong().color(egui::Color32::WHITE),
            )
            .fill(theme::RED)
            .stroke(Stroke::new(2.0_f32, egui::Color32::from_rgb(0xFF, 0x8A, 0x8A)))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS))
            .min_size(egui::vec2(250.0, 34.0));
            if ui
                .add(panic)
                .on_hover_text(
                    "Revert the most recent SaveLens mutation to its safety/pre-mutation backup (Ctrl+Shift+F12)",
                )
                .clicked()
            {
                app.intents.push(Intent::RequestPanic);
            }
            ui.checkbox(&mut app.panic_game_scope, "only this game");
        });
    });
}
