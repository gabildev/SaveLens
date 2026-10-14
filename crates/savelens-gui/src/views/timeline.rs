//! Timeline: commit graph of every branch with one card per commit.

use egui::{pos2, vec2, Color32, RichText, Stroke};
use savelens_cas::CommitKind;

use crate::app::{Intent, SaveLensApp};
use crate::graph::Segment;
use crate::theme;

const LANE_W: f32 = 20.0;
const NODE_Y: f32 = 22.0;

fn kind_color(kind: CommitKind) -> Color32 {
    match kind {
        CommitKind::Manual => theme::CYAN,
        CommitKind::Auto => theme::TEXT_DIM,
        CommitKind::Trainer => theme::MAGENTA,
        CommitKind::Edit => theme::AMBER,
        CommitKind::CherryPick => theme::VIOLET,
        CommitKind::Restore => theme::RED,
        CommitKind::Import => theme::LIME,
    }
}

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.heading(RichText::new("Timeline").color(theme::TEXT));
        ui.add_space(theme::SPACING);
        let has_game = app.selected_game.is_some();
        if ui.add_enabled(has_game, theme::accent_button("➕ Quick snapshot", theme::CYAN)).clicked() {
            app.intents.push(Intent::QuickSnapshot);
        }
        if ui
            .add_enabled(has_game, theme::ghost_button("↩ Quick rollback", theme::AMBER))
            .on_hover_text("Restore the previous commit of the current branch")
            .clicked()
        {
            app.intents.push(Intent::QuickRollback);
        }
        if let Some(h) = &app.history {
            ui.add_space(theme::SPACING);
            ui.label(RichText::new("Switch branch:").color(theme::TEXT_DIM));
            for b in &h.branches {
                let current = b.name == h.current_branch;
                let btn = if current {
                    theme::accent_button(&format!("🔀 {}", b.name), theme::MAGENTA)
                } else {
                    theme::ghost_button(&format!("🔀 {}", b.name), theme::MAGENTA)
                };
                if ui.add_enabled(!current && b.head.is_some(), btn).clicked() {
                    app.intents.push(Intent::SwitchBranch(b.name.clone()));
                }
            }
        }
        if app.history_loading {
            ui.add(egui::Spinner::new().color(theme::CYAN));
        }
    });
    ui.add_space(theme::SPACING);

    let Some(h) = &app.history else {
        if app.selected_game.is_none() {
            super::empty_state(ui, "No game selected", "Track a game from the sidebar to see its history");
        }
        return;
    };
    if h.commits.is_empty() {
        super::empty_state(ui, "No commits yet", "Take a snapshot or let the watcher record saves");
        return;
    }
    let lanes = h.layout.lane_count();
    let graph_w = 16.0 + lanes as f32 * LANE_W;
    let intents = &mut app.intents;
    let expanded = &mut app.expanded_commits;

    egui::ScrollArea::vertical().id_salt("timeline-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (row, node) in h.layout.nodes.iter().enumerate() {
            let Some(c) = h.commits.iter().find(|c| c.id == node.commit) else { continue };
            let is_expanded = expanded.contains(&c.id);
            let resp = ui.horizontal(|ui| {
                ui.allocate_exact_size(vec2(graph_w, NODE_Y * 2.0), egui::Sense::hover());
                egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 5)).show(ui, |ui| {
                    let head_here: Vec<_> = h.layout.heads_at(row).collect();
                    let frame = if head_here.iter().any(|b| b.is_current) {
                        theme::card_highlight(theme::lane_color(node.lane))
                    } else {
                        theme::card()
                    };
                    frame.show(ui, |ui| {
                        ui.set_width((ui.available_width() - 4.0).max(120.0));
                        ui.horizontal_wrapped(|ui| {
                            theme::chip(ui, c.kind.as_str(), kind_color(c.kind));
                            ui.label(theme::mono(format!("#{}", c.id)).strong());
                            ui.label(
                                RichText::new(savelens_cas::format_ts(c.created_at)).small().color(theme::TEXT_DIM),
                            );
                            for b in &head_here {
                                let color = theme::lane_color(h.layout.lane_of(&b.name).unwrap_or(b.lane));
                                let label = if b.is_current {
                                    format!("🔀 {} (HEAD)", b.name)
                                } else {
                                    format!("🔀 {}", b.name)
                                };
                                theme::chip(ui, &label, color);
                            }
                            for t in h.tags.iter().filter(|t| t.commit_id == c.id) {
                                theme::chip(ui, &format!("🏷 {}", t.name), theme::VIOLET);
                            }
                        });
                        if !c.summary.is_empty() {
                            ui.label(RichText::new(&c.summary).color(theme::TEXT).strong());
                        }
                        if !c.message.is_empty() {
                            ui.label(RichText::new(&c.message).small().color(theme::TEXT_DIM));
                        }
                        let logs = h.changelogs.get(&c.id);
                        let n_lines: usize = logs.map(|l| l.iter().map(|x| x.lines.len().max(1)).sum()).unwrap_or(0);
                        ui.horizontal_wrapped(|ui| {
                            if n_lines > 0 {
                                let label = if is_expanded {
                                    format!("⏷ hide changelog ({n_lines})")
                                } else {
                                    format!("⏵ changelog ({n_lines})")
                                };
                                if ui.add(egui::Button::new(RichText::new(label).small()).frame(false)).clicked()
                                    && !expanded.remove(&c.id)
                                {
                                    expanded.insert(c.id);
                                }
                            }
                            if ui
                                .small_button("Checkout")
                                .on_hover_text("Restore this commit's files onto disk")
                                .clicked()
                            {
                                intents.push(Intent::Checkout(c.id));
                            }
                            if ui.small_button("New branch here").clicked() {
                                intents.push(Intent::OpenBranchDialog(c.id));
                            }
                            if ui.small_button("Tag").clicked() {
                                intents.push(Intent::OpenTagDialog(c.id));
                            }
                            if ui.small_button("Diff vs working").clicked() {
                                intents.push(Intent::DiffVsWorking(c.id));
                            }
                        });
                        if is_expanded {
                            if let Some(logs) = logs {
                                for l in logs {
                                    ui.label(
                                        theme::mono(format!("▣ {}  — {}", l.rel_path, l.summary))
                                            .color(theme::CYAN)
                                            .small(),
                                    );
                                    for line in &l.lines {
                                        ui.label(theme::mono(format!("   {line}")).small().color(theme::TEXT_DIM));
                                    }
                                }
                            }
                        }
                    });
                });
            });
            // Graph column of this row.
            let rect = resp.response.rect;
            let painter = ui.painter();
            let x = |lane: usize| rect.left() + 12.0 + lane as f32 * LANE_W;
            let node_y = rect.top() + NODE_Y.min(rect.height() * 0.5);
            for seg in h.layout.row_segments.get(row).map(Vec::as_slice).unwrap_or(&[]) {
                match *seg {
                    Segment::Leave { from, to } => {
                        let color = theme::lane_color(to);
                        if from == to {
                            painter.line_segment(
                                [pos2(x(from), node_y), pos2(x(to), rect.bottom())],
                                Stroke::new(2.0_f32, color),
                            );
                        } else {
                            let mid = pos2(x(to), (node_y + 14.0).min(rect.bottom()));
                            painter.line_segment([pos2(x(from), node_y), mid], Stroke::new(2.0_f32, color));
                            painter.line_segment([mid, pos2(x(to), rect.bottom())], Stroke::new(2.0_f32, color));
                        }
                    }
                    Segment::Through { lane } => {
                        painter.line_segment(
                            [pos2(x(lane), rect.top()), pos2(x(lane), rect.bottom())],
                            Stroke::new(2.0_f32, theme::lane_color(lane).gamma_multiply(0.8)),
                        );
                    }
                    Segment::Arrive { lane } => {
                        painter.line_segment(
                            [pos2(x(lane), rect.top()), pos2(x(lane), node_y)],
                            Stroke::new(2.0_f32, theme::lane_color(lane)),
                        );
                    }
                }
            }
            let color = theme::lane_color(node.lane);
            let is_head = h.layout.heads_at(row).any(|b| b.is_current);
            painter.circle_filled(pos2(x(node.lane), node_y), if is_head { 7.0 } else { 5.5 }, color);
            painter.circle_stroke(
                pos2(x(node.lane), node_y),
                if is_head { 7.0 } else { 5.5 },
                Stroke::new(2.0_f32, theme::BG),
            );
            if is_head {
                painter.circle_stroke(pos2(x(node.lane), node_y), 10.0, Stroke::new(1.5_f32, color));
            }
        }
        if h.commits.len() >= crate::app::TIMELINE_LIMIT {
            ui.add_space(theme::SPACING);
            ui.label(
                RichText::new(format!("Showing the latest {} commits.", crate::app::TIMELINE_LIMIT))
                    .small()
                    .color(theme::TEXT_DIM),
            );
        }
    });
}

/// Dialog for "New branch here" / "Tag".
pub(super) fn name_dialog(app: &mut SaveLensApp, ctx: &egui::Context) {
    let Some(d) = app.name_dialog.as_mut() else { return };
    let mut submit = false;
    let mut cancel = false;
    let title = if d.is_tag { "Tag commit" } else { "New branch here" };
    let resp = egui::Modal::new(egui::Id::new("savelens-name-dialog")).show(ctx, |ui| {
        ui.set_max_width(420.0);
        ui.label(RichText::new(title).size(18.0).strong().color(theme::CYAN));
        ui.label(RichText::new(format!("at commit #{}", d.commit)).color(theme::TEXT_DIM));
        let r = ui.add(
            egui::TextEdit::singleline(&mut d.name)
                .hint_text(if d.is_tag { "before-boss" } else { "no-death-run" })
                .desired_width(f32::INFINITY),
        );
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        ui.horizontal(|ui| {
            if ui
                .add(theme::accent_button(if d.is_tag { "Create tag" } else { "Create branch" }, theme::CYAN))
                .clicked()
            {
                submit = true;
            }
            if ui.add(theme::ghost_button("Cancel", theme::TEXT_DIM)).clicked() {
                cancel = true;
            }
        });
    });
    if submit {
        if let Some(d) = app.name_dialog.take() {
            let r = if d.is_tag { app.tag_commit(d.commit, &d.name) } else { app.create_branch_at(d.commit, &d.name) };
            if let Err(e) = r {
                app.toasts.error(e);
                app.name_dialog = Some(d);
            }
        }
    } else if cancel || resp.should_close() {
        app.name_dialog = None;
    }
}
