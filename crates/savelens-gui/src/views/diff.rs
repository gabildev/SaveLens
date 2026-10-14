//! Visual tree diff & cherry-picker.

use egui::RichText;
use savelens_watcher::FileStatus;

use crate::app::{Intent, SaveLensApp};
use crate::theme;
use crate::treediff::Side;
use crate::views::tree::{show_tree, TreeCtx, TreeMode};

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.heading("Tree Diff & Cherry-Pick");
        ui.add_space(theme::SPACING);
        ui.label(RichText::new("Source revision").color(theme::TEXT_DIM));
        let history = app.history.as_ref();
        egui::ComboBox::from_id_salt("diff-source")
            .selected_text(theme::mono(&app.diff.source_rev))
            .width(160.0)
            .show_ui(ui, |ui| {
                for rev in ["HEAD", "HEAD~1", "HEAD~2"] {
                    if ui.selectable_label(app.diff.source_rev == rev, rev).clicked() {
                        app.diff.source_rev = rev.to_string();
                        app.intents.push(Intent::LoadDiff);
                    }
                }
                if let Some(h) = history {
                    for c in h.commits.iter().take(100) {
                        let rev = format!("#{}", c.id);
                        let text = format!("{rev} [{}] {}", c.kind, crate::views::activity::clip(&c.summary, 40));
                        if ui.selectable_label(app.diff.source_rev == rev, text).clicked() {
                            app.diff.source_rev = rev;
                            app.intents.push(Intent::LoadDiff);
                        }
                    }
                }
            });
        ui.label(RichText::new("→  WORKING").color(theme::CYAN).strong());
        if ui.add(theme::ghost_button("↻ Reload", theme::CYAN)).clicked() {
            app.intents.push(Intent::LoadDiff);
        }
        if app.diff.loading {
            ui.add(egui::Spinner::new().color(theme::CYAN));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("🔍").color(theme::TEXT_DIM));
        ui.add(
            egui::TextEdit::singleline(&mut app.diff.search)
                .hint_text("fuzzy search paths and values")
                .desired_width(260.0),
        );
        let n = app.diff.selected.len();
        if ui.add_enabled(n > 0, theme::accent_button(&format!("Cherry-pick {n} selected"), theme::MAGENTA)).clicked() {
            let paths = app.diff.selected.iter().filter_map(|s| savelens_core::NodePath::parse(s).ok()).collect();
            app.intents.push(Intent::RequestPick(paths));
        }
        if n > 0 && ui.small_button("clear selection").clicked() {
            app.diff.selected.clear();
        }
        ui.add_space(theme::SPACING);
        legend(ui);
    });
    if app.diff.search != app.diff.applied_search {
        app.refilter_diff();
    }
    if let Some(e) = &app.diff.error {
        ui.colored_label(theme::RED, format!("✖ {e}"));
    }
    let Some(d) = &app.diff.data else {
        if !app.diff.loading && app.diff.error.is_none() {
            super::empty_state(ui, "Nothing loaded", "Select a file and a source revision");
        }
        return;
    };
    ui.horizontal_wrapped(|ui| {
        for f in &d.files {
            let color = match f.status {
                FileStatus::Added => theme::LIME,
                FileStatus::Removed => theme::RED,
                FileStatus::Modified => theme::AMBER,
                FileStatus::Unchanged => theme::TEXT_FAINT,
            };
            theme::chip(ui, &format!("{} · {}", f.rel_path, crate::views::activity::clip(&f.summary, 48)), color);
        }
    });
    ui.label(
        RichText::new(format!("{}: {} change(s) between {} and WORKING", d.rel, d.model.change_count(), d.from_rev))
            .color(theme::TEXT_DIM)
            .small(),
    );
    ui.add_space(4.0);

    let model = &d.model;
    let vis_l = app.diff.visible_left.as_ref();
    let vis_r = app.diff.visible_right.as_ref();
    let selected = &mut app.diff.selected;
    let intents = &mut app.intents;
    let from_rev = d.from_rev.clone();
    ui.columns(2, |cols| {
        cols[0].label(RichText::new(format!("◀ {from_rev}")).strong().color(theme::MAGENTA));
        theme::card().show(&mut cols[0], |ui| {
            egui::ScrollArea::both().id_salt("diff-left").auto_shrink([false, false]).show(ui, |ui| {
                let mut ctx = TreeCtx {
                    id_prefix: "diff-left",
                    diff: Some((model, Side::Left)),
                    visible: vis_l,
                    mode: TreeMode::Pick { selected, intents },
                };
                show_tree(ui, &model.left, &mut ctx);
            });
        });
        cols[1].label(RichText::new("WORKING ▶").strong().color(theme::CYAN));
        theme::card().show(&mut cols[1], |ui| {
            egui::ScrollArea::both().id_salt("diff-right").auto_shrink([false, false]).show(ui, |ui| {
                let mut ctx = TreeCtx {
                    id_prefix: "diff-right",
                    diff: Some((model, Side::Right)),
                    visible: vis_r,
                    mode: TreeMode::ReadOnly,
                };
                show_tree(ui, &model.right, &mut ctx);
            });
        });
    });
}

fn legend(ui: &mut egui::Ui) {
    theme::chip(ui, "+ added", theme::LIME);
    theme::chip(ui, "− removed", theme::RED);
    theme::chip(ui, "~ modified", theme::AMBER);
    theme::chip(ui, "• contains changes", theme::CYAN);
}

/// Preview / confirmation of a cherry-pick.
pub(super) fn pick_modal(app: &mut SaveLensApp, ctx: &egui::Context) {
    let Some(paths) = app.diff.pending_pick.clone() else { return };
    let mut confirm = false;
    let mut cancel = false;
    let from = app.diff.data.as_ref().map(|d| d.from_rev.clone()).unwrap_or_default();
    let resp = egui::Modal::new(egui::Id::new("savelens-pick-modal")).show(ctx, |ui| {
        ui.set_max_width(620.0);
        ui.label(RichText::new("Cherry-pick preview").size(18.0).strong().color(theme::MAGENTA));
        ui.label(
            RichText::new(format!(
                "Restore {} from {from} into the working save. Everything else is kept.",
                paths.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            ))
            .color(theme::TEXT_DIM),
        );
        ui.add_space(4.0);
        theme::card().show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                if app.diff.preview.is_empty() {
                    ui.label(RichText::new("no change").color(theme::TEXT_DIM));
                }
                for l in &app.diff.preview {
                    ui.label(theme::mono(l).small());
                }
            });
        });
        ui.label(
            RichText::new(
                "A safety/pre-mutation backup is taken before writing; the save is re-encoded and re-signed.",
            )
            .small()
            .color(theme::TEXT_DIM),
        );
        ui.horizontal(|ui| {
            if ui.add(theme::accent_button("Apply cherry-pick", theme::MAGENTA)).clicked() {
                confirm = true;
            }
            if ui.add(theme::ghost_button("Cancel", theme::TEXT_DIM)).clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        if let Err(e) = app.confirm_cherry_pick() {
            app.toasts.error(e);
        }
    } else if cancel || resp.should_close() {
        app.cancel_cherry_pick();
    }
}
