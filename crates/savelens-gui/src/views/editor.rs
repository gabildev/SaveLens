//! AST editor of the WORKING file, respecting the Safe Edit Gate.

use egui::RichText;
use savelens_core::{EditMode, Endianness};

use crate::app::{Intent, SaveLensApp};
use crate::scanner::ALL_TYPES;
use crate::theme;
use crate::views::tree::{show_tree, TreeCtx, TreeMode};

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.heading("AST Editor");
        ui.add_space(theme::SPACING);
        if let Some(f) = &app.selected_file {
            theme::chip(ui, &format!("{f} @ WORKING"), theme::CYAN);
        }
        if ui.add(theme::ghost_button("↻ Reload", theme::CYAN)).clicked() {
            app.intents.push(Intent::LoadEditor);
        }
        if app.editor.loading {
            ui.add(egui::Spinner::new().color(theme::CYAN));
        }
        ui.label(RichText::new("🔍").color(theme::TEXT_DIM));
        ui.add(egui::TextEdit::singleline(&mut app.editor.search).hint_text("fuzzy search").desired_width(220.0));
    });
    if app.editor.search != app.editor.applied_search {
        app.refilter_editor();
    }
    if let Some(e) = &app.editor.error {
        ui.colored_label(theme::RED, format!("✖ {e}"));
    }
    let Some(d) = &app.editor.data else {
        if !app.editor.loading && app.editor.error.is_none() {
            super::empty_state(ui, "No file loaded", "Select a tracked file in the top bar");
        }
        return;
    };

    let structural = d.edit_mode == EditMode::Structural;
    let gate = app.editor.gate_error.clone().or_else(|| d.gate_reason.clone());
    if let Some(reason) = gate {
        egui::Frame::new()
            .fill(theme::RED.gamma_multiply(0.12))
            .stroke(egui::Stroke::new(1.0_f32, theme::RED))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_CARD))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("⛔ SAFE EDIT GATE").strong().color(theme::RED));
                ui.label(RichText::new(reason).color(theme::TEXT));
                if d.edit_mode == EditMode::RawInPlace || !structural {
                    ui.checkbox(
                        &mut app.editor.raw_mode,
                        "Raw in-place mode (fixed-width writes + checksum recalculation)",
                    );
                }
            });
        ui.add_space(theme::SPACING);
    } else {
        ui.horizontal(|ui| {
            theme::chip(ui, "Lossless round-trip: structural edits enabled", theme::LIME);
            ui.checkbox(&mut app.editor.raw_mode, "show raw in-place writer");
        });
    }

    if app.editor.raw_mode && d.edit_mode != EditMode::BackupRestoreOnly {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, "Raw in-place write (payload offsets)");
            ui.horizontal_wrapped(|ui| {
                ui.label("offset");
                ui.add(
                    egui::TextEdit::singleline(&mut app.editor.raw_offset)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(90.0),
                );
                egui::ComboBox::from_id_salt("raw-type").selected_text(app.editor.raw_type.name()).width(80.0).show_ui(
                    ui,
                    |ui| {
                        for t in ALL_TYPES {
                            ui.selectable_value(&mut app.editor.raw_type, t, t.name());
                        }
                    },
                );
                ui.selectable_value(&mut app.editor.raw_endian, Endianness::Little, "LE");
                ui.selectable_value(&mut app.editor.raw_endian, Endianness::Big, "BE");
                ui.label("value");
                ui.add(
                    egui::TextEdit::singleline(&mut app.editor.raw_value)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(120.0),
                );
                if ui.add(theme::accent_button("Write & re-sign", theme::AMBER)).clicked() {
                    app.intents.push(Intent::RawWrite);
                }
            });
        });
        ui.add_space(theme::SPACING);
    }

    let visible = app.editor.visible.as_ref();
    let edits = &mut app.editor.edits;
    let intents = &mut app.intents;
    theme::card().show(ui, |ui| {
        egui::ScrollArea::both().id_salt("editor-tree").auto_shrink([false, false]).show(ui, |ui| {
            let mut ctx = TreeCtx {
                id_prefix: "editor",
                diff: None,
                visible,
                mode: TreeMode::Edit { edits, intents, enabled: structural },
            };
            show_tree(ui, &d.root, &mut ctx);
        });
    });
}
