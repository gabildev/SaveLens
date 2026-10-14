//! Sidebar: tracked games, Steam discovery, auto-hunt and the track-folder form.

use egui::RichText;

use crate::app::{Intent, SaveLensApp};
use crate::theme;

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical().id_salt("sidebar-scroll").auto_shrink([false, false]).show(ui, |ui| {
        games(app, ui);
        ui.add_space(theme::SPACING * 2.0);
        discovery(app, ui);
        ui.add_space(theme::SPACING * 2.0);
        hunt(app, ui);
        ui.add_space(theme::SPACING * 2.0);
        track_form(app, ui);
    });
}

fn games(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    theme::section_title(ui, "Tracked games");
    if app.games.is_empty() {
        ui.label(RichText::new("No game tracked yet. Discover, hunt or track a folder below.").color(theme::TEXT_DIM));
        return;
    }
    for g in &app.games {
        let selected = app.selected_game == Some(g.id);
        let frame = if selected { theme::card_highlight(theme::CYAN) } else { theme::card() };
        let resp = frame
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&g.name).strong().color(if selected { theme::CYAN } else { theme::TEXT }));
                ui.label(RichText::new(g.save_root.display().to_string()).small().monospace().color(theme::TEXT_DIM));
                ui.horizontal_wrapped(|ui| {
                    let runtime = if g.runtime.starts_with("proton") { "Proton" } else { g.runtime.as_str() };
                    theme::chip(ui, runtime, theme::VIOLET).on_hover_text(&g.runtime);
                    if let Some(id) = g.app_id {
                        theme::chip(ui, &format!("app {id}"), theme::TEXT_DIM);
                    }
                    if g.trainer_enabled {
                        theme::chip(ui, "trainer", theme::MAGENTA);
                    }
                    let mut watch = g.watch_enabled;
                    if ui.checkbox(&mut watch, RichText::new("watch").small()).changed() {
                        app.intents.push(Intent::SetWatch(g.id, watch));
                    }
                });
            })
            .response
            .interact(egui::Sense::click());
        if resp.clicked() && !selected {
            app.intents.push(Intent::SelectGame(g.id));
        }
        ui.add_space(4.0);
    }
}

fn discovery(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    theme::section_title(ui, "Steam discovery");
    ui.horizontal(|ui| {
        let running = app.sidebar.discover_job.is_some();
        if ui.add_enabled(!running, theme::ghost_button("Discover Steam games", theme::CYAN)).clicked() {
            app.intents.push(Intent::Discover);
        }
        if let Some(j) = &app.sidebar.discover_job {
            ui.add(egui::Spinner::new().color(theme::CYAN));
            ui.label(RichText::new(format!("{:.0}s", j.elapsed().as_secs_f32())).small().color(theme::TEXT_DIM));
        }
    });
    let untracked = app.untracked_discovered();
    let Some(rep) = &app.sidebar.discovery else { return };
    ui.label(
        RichText::new(format!(
            "{} game(s) found, {} untracked, {} library(ies)",
            rep.games.len(),
            untracked.len(),
            rep.libraries.len()
        ))
        .small()
        .color(theme::TEXT_DIM),
    );
    for w in rep.warnings.iter().take(3) {
        ui.label(RichText::new(format!("⚠ {w}")).small().color(theme::AMBER));
    }
    for gi in untracked {
        let Some(g) = rep.games.get(gi) else { continue };
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&g.name).strong());
                if let Some(e) = g.engine {
                    theme::chip(ui, &format!("{e:?}"), theme::VIOLET);
                }
                if matches!(g.runtime, savelens_discovery::GameRuntime::Proton { .. }) {
                    theme::chip(ui, "Proton", theme::MAGENTA);
                }
            });
            if g.save_locations.is_empty() {
                ui.label(RichText::new("no save location found").small().color(theme::TEXT_FAINT));
            }
            for (li, loc) in g.save_locations.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.add(theme::accent_button("Track", theme::CYAN)).clicked() {
                        app.intents.push(Intent::TrackDiscovered { game: gi, location: li });
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(loc.root.display().to_string()).small().monospace().color(theme::TEXT_DIM),
                        )
                        .truncate(),
                    )
                    .on_hover_text(format!(
                        "{}\npatterns: {}",
                        loc.root.display(),
                        loc.patterns.join(", ")
                    ));
                });
            }
        });
        ui.add_space(4.0);
    }
}

fn hunt(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    theme::section_title(ui, "Auto-hunt");
    let running = app.sidebar.hunt_job.is_some();
    if ui
        .add_enabled(!running, theme::accent_button("⌖ Auto-Hunt Active Game", theme::MAGENTA))
        .on_hover_text("Play the game and save: SaveLens watches file writes of game processes for 20 s and ranks the save candidates")
        .clicked()
    {
        app.intents.push(Intent::Hunt);
    }
    if let Some(j) = &app.sidebar.hunt_job {
        let p = j.progress().unwrap_or(0.0);
        ui.add(
            egui::ProgressBar::new(p)
                .fill(theme::MAGENTA)
                .text(format!("hunting… {:.0}s — save your game now", j.elapsed().as_secs_f32())),
        );
    }
    let Some(rep) = &app.sidebar.hunt else { return };
    if rep.candidates.is_empty() {
        ui.label(
            RichText::new("No save candidate observed. Save in-game while hunting.").small().color(theme::TEXT_DIM),
        );
    }
    for w in rep.warnings.iter().take(3) {
        ui.label(RichText::new(format!("⚠ {w}")).small().color(theme::AMBER));
    }
    for (i, c) in rep.candidates.iter().enumerate().take(20) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::chip(ui, &format!("{:.1}", c.score), theme::LIME);
                ui.add(egui::Label::new(theme::mono(c.path.display().to_string()).small()).truncate())
                    .on_hover_text(c.reasons.join("\n"));
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{} write(s){}",
                        c.writes,
                        c.process_name.as_ref().map(|p| format!(" · {p}")).unwrap_or_default()
                    ))
                    .small()
                    .color(theme::TEXT_DIM),
                );
                if ui.add(theme::ghost_button("Track", theme::CYAN)).clicked() {
                    app.intents.push(Intent::TrackCandidate(i));
                }
            });
        });
        ui.add_space(4.0);
    }
}

fn track_form(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    theme::section_title(ui, "Track folder…");
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Name").small().color(theme::TEXT_DIM));
        ui.add(
            egui::TextEdit::singleline(&mut app.sidebar.track_name).hint_text("My RPG").desired_width(f32::INFINITY),
        );
        ui.label(RichText::new("Save directory").small().color(theme::TEXT_DIM));
        ui.add(
            egui::TextEdit::singleline(&mut app.sidebar.track_dir)
                .hint_text("/home/me/.local/share/Game/Saves")
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY),
        );
        ui.label(RichText::new("Patterns (comma separated)").small().color(theme::TEXT_DIM));
        ui.add(
            egui::TextEdit::singleline(&mut app.sidebar.track_patterns)
                .hint_text("*.sav, profile.json")
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        if ui.add(theme::accent_button("Track folder", theme::CYAN)).clicked() {
            app.intents.push(Intent::TrackFolder);
        }
    });
}
