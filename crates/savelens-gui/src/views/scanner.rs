//! Scanner Studio (Cheat-Engine-style differential scanner) and the
//! Cheat Table / Pinned Stats bottom panel.

use egui::RichText;
use savelens_core::ScalarValue;
use savelens_watcher::WORKING;

use crate::app::{Intent, SaveLensApp};
use crate::scanner::{slider_range, AlignChoice, FilterKind};
use crate::theme;

const ROW_H: f32 = 26.0;

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.heading("Scanner Studio");
        ui.label(RichText::new("differential on-disk scanner — no RAM access").small().color(theme::TEXT_DIM));
    });
    ui.add_space(4.0);
    controls(app, ui);
    ui.add_space(theme::SPACING);
    results(app, ui);
}

fn controls(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    let running = app.scan_running();
    let has_session = app.scanner.has_session();
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("File").color(theme::TEXT_DIM));
            let file =
                app.scanner.scan_file.clone().or_else(|| app.selected_file.clone()).unwrap_or_else(|| "—".into());
            egui::ComboBox::from_id_salt("scan-file").selected_text(theme::mono(file)).width(180.0).show_ui(ui, |ui| {
                for f in &app.files {
                    if ui
                        .selectable_label(
                            app.selected_file.as_deref() == Some(f.rel_path.as_str()),
                            theme::mono(&f.rel_path),
                        )
                        .clicked()
                    {
                        app.intents.push(Intent::SelectFile(f.rel_path.clone()));
                    }
                }
            });
            ui.label(RichText::new("Snapshot").color(theme::TEXT_DIM));
            let history = app.history.as_ref();
            egui::ComboBox::from_id_salt("scan-source")
                .selected_text(theme::mono(&app.scanner.source_rev))
                .width(150.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut app.scanner.source_rev, WORKING.to_string(), "WORKING (disk)");
                    if let Some(h) = history {
                        for c in h.commits.iter().take(100) {
                            let rev = format!("#{}", c.id);
                            let text = format!("{rev} [{}] {}", c.kind, crate::views::activity::clip(&c.summary, 36));
                            ui.selectable_value(&mut app.scanner.source_rev, rev, text);
                        }
                    }
                });
            ui.label(RichText::new("Filter").color(theme::TEXT_DIM));
            egui::ComboBox::from_id_salt("scan-filter").selected_text(app.scanner.filter.label()).width(170.0).show_ui(
                ui,
                |ui| {
                    for k in FilterKind::ALL {
                        let usable = if has_session { k.allowed_next() } else { k.allowed_first() };
                        ui.add_enabled_ui(usable, |ui| {
                            ui.selectable_value(&mut app.scanner.filter, k, k.label());
                        });
                    }
                },
            );
            let n = app.scanner.filter.value_count();
            if n >= 1 {
                ui.add(
                    egui::TextEdit::singleline(&mut app.scanner.value_a)
                        .hint_text(if n == 2 { "A" } else { "value" })
                        .font(egui::TextStyle::Monospace)
                        .desired_width(90.0),
                );
            }
            if n == 2 {
                ui.label(
                    RichText::new(if app.scanner.filter == FilterKind::Between { "and" } else { "➡" })
                        .color(theme::TEXT_DIM),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut app.scanner.value_b)
                        .hint_text("B")
                        .font(egui::TextStyle::Monospace)
                        .desired_width(90.0),
                );
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Types").color(theme::TEXT_DIM));
            for (t, on) in app.scanner.types.iter_mut() {
                ui.checkbox(on, RichText::new(t.name()).monospace());
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut app.scanner.little_endian, "little endian");
            ui.checkbox(&mut app.scanner.big_endian, "big endian");
            ui.label(RichText::new("Alignment").color(theme::TEXT_DIM));
            egui::ComboBox::from_id_salt("scan-align")
                .selected_text(app.scanner.alignment.label())
                .width(170.0)
                .show_ui(ui, |ui| {
                    for a in AlignChoice::ALL {
                        ui.selectable_value(&mut app.scanner.alignment, a, a.label());
                    }
                });
            if app.scanner.alignment == AlignChoice::Fixed {
                ui.add(egui::DragValue::new(&mut app.scanner.fixed_alignment).range(1..=64).prefix("every "));
            }
            ui.checkbox(&mut app.scanner.scan_ast, "AST leaves");
            ui.checkbox(&mut app.scanner.scan_numeric_strings, "numeric strings");
            ui.checkbox(&mut app.scanner.scan_raw, "raw bytes");
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!running, theme::accent_button("First Scan", theme::CYAN)).clicked() {
                app.intents.push(Intent::FirstScan);
            }
            if ui.add_enabled(!running && has_session, theme::accent_button("Next Scan", theme::MAGENTA)).clicked() {
                app.intents.push(Intent::NextScan);
            }
            if ui.add_enabled(!running, theme::ghost_button("Reset", theme::TEXT_DIM)).clicked() {
                app.intents.push(Intent::ResetScan);
            }
            if running {
                ui.add(egui::Spinner::new().color(theme::CYAN));
                ui.label(RichText::new("scanning…").color(theme::TEXT_DIM));
            }
            if let Some(e) = &app.scanner.error {
                ui.colored_label(theme::RED, format!("✖ {e}"));
            }
        });
    });
}

fn results(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    let history: Vec<String> = app
        .scanner
        .history()
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {} @ {} ➡ {}", i + 1, s.filter, s.snapshot_label, s.remaining))
        .collect();
    ui.horizontal_wrapped(|ui| {
        theme::section_title(ui, "Results");
        let total = app.scanner.total();
        let shown = app.scanner.results().len();
        theme::chip(ui, &format!("{total} candidate(s)"), if total == 1 { theme::LIME } else { theme::CYAN });
        if total > shown {
            ui.label(
                RichText::new(format!("{shown} listed; narrow down with a next scan")).small().color(theme::TEXT_DIM),
            );
        }
    });
    if !history.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("History:").small().color(theme::TEXT_DIM));
            for h in &history {
                theme::chip(ui, h, theme::VIOLET);
            }
        });
    }
    let rows = app.scanner.results();
    if rows.is_empty() {
        ui.label(RichText::new("Run a first scan to list candidates.").color(theme::TEXT_FAINT));
        return;
    }
    let intents = &mut app.intents;
    theme::card().show(ui, |ui| {
        ui.horizontal(|ui| {
            for (w, h) in [(320.0, "LOCATION"), (90.0, "TYPE"), (110.0, "PREVIOUS"), (110.0, "CURRENT")] {
                ui.add_sized([w, 18.0], egui::Label::new(RichText::new(h).small().strong().color(theme::CYAN)));
            }
        });
        egui::ScrollArea::vertical().id_salt("scan-results").auto_shrink([false, false]).show_rows(
            ui,
            ROW_H,
            rows.len(),
            |ui, range| {
                for i in range {
                    let Some(c) = rows.get(i) else { continue };
                    ui.horizontal(|ui| {
                        ui.set_height(ROW_H);
                        ui.add_sized([320.0, ROW_H], egui::Label::new(theme::mono(c.location.to_string())).truncate());
                        ui.add_sized(
                            [90.0, ROW_H],
                            egui::Label::new(RichText::new(c.location.type_label(None)).small().color(theme::VIOLET)),
                        );
                        ui.add_sized(
                            [110.0, ROW_H],
                            egui::Label::new(theme::mono(c.previous.to_string()).color(theme::TEXT_DIM)),
                        );
                        let changed = c.previous != c.current;
                        ui.add_sized(
                            [110.0, ROW_H],
                            egui::Label::new(theme::mono(c.current.to_string()).color(if changed {
                                theme::AMBER
                            } else {
                                theme::TEXT
                            })),
                        );
                        if ui.add(theme::ghost_button("Add to cheat table", theme::LIME).small()).clicked() {
                            intents.push(Intent::AddCandidate(i));
                        }
                    });
                }
            },
        );
    });
}

/// Bottom panel: cheat table with live values and the Apply & Sign button.
pub(super) fn draw_cheat_table(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        theme::section_title(ui, "Cheat Table / Pinned Stats");
        let trainer = app.selected_game_record().map(|g| g.trainer_enabled);
        if let Some(on) = trainer {
            let mut t = on;
            if ui
                .checkbox(&mut t, RichText::new("Trainer (freeze-on-save)").color(theme::MAGENTA))
                .on_hover_text("When the game saves, the watcher re-injects every frozen value and re-signs the file")
                .changed()
            {
                app.intents.push(Intent::SetTrainer(t));
            }
        }
        let pending = app.cheats.pending_edits();
        let n = pending.as_ref().map(Vec::len).unwrap_or(0);
        if ui
            .add_enabled(
                pending.is_ok() && n > 0,
                theme::accent_button(&format!("✔ Apply & Sign Save ({n})"), theme::LIME),
            )
            .on_hover_text("Write all edited values (safety backup, re-encode, re-sign, verify)")
            .clicked()
        {
            app.intents.push(Intent::ApplyCheats);
        }
        if let Err(e) = pending {
            ui.colored_label(theme::RED, e);
        }
        if let Some(t) = &app.cheats.last_safety_tag {
            ui.label(RichText::new(format!("safety tag: {t}")).small().monospace().color(theme::AMBER));
        }
        if let Some(e) = &app.cheats.error {
            ui.colored_label(theme::RED, e);
        }
    });
    if app.cheats.rows.is_empty() {
        ui.label(
            RichText::new("Nothing pinned yet. Use \"Add to cheat table\" on a scan result.").color(theme::TEXT_FAINT),
        );
        return;
    }
    let intents = &mut app.intents;
    let edits = &mut app.cheats.edits;
    let freeze_inputs = &mut app.cheats.freeze_inputs;
    egui::ScrollArea::vertical().id_salt("cheat-table").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("cheat-grid").striped(true).num_columns(7).spacing([14.0, 6.0]).show(ui, |ui| {
            for h in ["ALIAS", "LOCATION", "LIVE", "NEW VALUE", "FREEZE", "FROZEN VALUE", ""] {
                ui.label(RichText::new(h).small().strong().color(theme::CYAN));
            }
            ui.end_row();
            for row in &app.cheats.rows {
                let alias = row.entry.alias.clone();
                ui.label(RichText::new(&alias).strong().color(theme::TEXT));
                ui.label(theme::mono(format!("{}:{}", row.entry.rel_path, row.entry.location)).small())
                    .on_hover_text(row.entry.location.to_string());
                match (&row.current, &row.error) {
                    (Some(v), _) => ui.label(theme::mono(v.to_string()).color(theme::LIME).strong()),
                    (None, Some(e)) => ui.colored_label(theme::RED, "error").on_hover_text(e),
                    (None, None) => ui.label("—"),
                };
                ui.horizontal(|ui| {
                    let live = row.current.map(|v| v.to_string()).unwrap_or_default();
                    let mut text = edits.get(&alias).cloned().unwrap_or_else(|| live.clone());
                    let mut changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut text).font(egui::TextStyle::Monospace).desired_width(110.0),
                        )
                        .changed();
                    match row.current {
                        Some(ScalarValue::Int(cur)) => {
                            if let Ok(cur) = i64::try_from(cur) {
                                let mut v = match ScalarValue::parse(&text) {
                                    Ok(ScalarValue::Int(x)) => i64::try_from(x).unwrap_or(cur),
                                    _ => cur,
                                };
                                let (lo, hi) = slider_range(cur);
                                if ui.add(egui::Slider::new(&mut v, lo..=hi).show_value(false)).changed() {
                                    text = v.to_string();
                                    changed = true;
                                }
                            }
                        }
                        Some(ScalarValue::Float(cur)) => {
                            let mut v = match ScalarValue::parse(&text) {
                                Ok(x) => x.as_f64(),
                                Err(_) => cur,
                            };
                            if ui.add(egui::DragValue::new(&mut v).speed(0.1)).changed() {
                                text = savelens_core::node::format_float(v);
                                changed = true;
                            }
                        }
                        None => {}
                    }
                    if changed {
                        if text == live {
                            edits.remove(&alias);
                        } else {
                            edits.insert(alias.clone(), text);
                        }
                    }
                    if edits.contains_key(&alias) && ui.small_button("↺").on_hover_text("Discard this edit").clicked()
                    {
                        edits.remove(&alias);
                    }
                });
                let mut frozen = row.entry.freeze_enabled;
                if ui.checkbox(&mut frozen, "").changed() {
                    intents.push(Intent::Freeze { alias: alias.clone(), enabled: frozen });
                }
                let fi = freeze_inputs.entry(alias.clone()).or_default();
                ui.add(egui::TextEdit::singleline(fi).font(egui::TextStyle::Monospace).desired_width(90.0))
                    .on_hover_text(match row.entry.frozen_value {
                        Some(v) => format!("frozen at {v}"),
                        None => "value used when freezing (empty = live value)".to_string(),
                    });
                if ui
                    .add(theme::ghost_button("🗑", theme::RED).small())
                    .on_hover_text("Remove from the cheat table")
                    .clicked()
                {
                    intents.push(Intent::DeleteCheat(alias.clone()));
                }
                ui.end_row();
            }
        });
    });
}
