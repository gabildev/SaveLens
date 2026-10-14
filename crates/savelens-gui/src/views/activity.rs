//! Activity log (store events + embedded daemon events).

use egui::RichText;

use crate::app::SaveLensApp;
use crate::theme;

/// Truncates `s` to `max` characters with an ellipsis.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        "commit" | "snapshot" | "track" => theme::CYAN,
        "mutation" | "trainer" => theme::MAGENTA,
        "panic" | "error" => theme::RED,
        "warning" | "locked" => theme::AMBER,
        "branch" | "started" | "watching" => theme::LIME,
        _ => theme::TEXT_DIM,
    }
}

pub(super) fn draw(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("Activity");
        ui.label(RichText::new(format!("{} entries", app.activity.len())).color(theme::TEXT_DIM));
    });
    ui.add_space(theme::SPACING);
    if app.activity.is_empty() {
        super::empty_state(ui, "No activity yet", "Commits, mutations and watcher events appear here");
        return;
    }
    let row_h = 26.0;
    let entries = &app.activity;
    theme::card().show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("activity-scroll").auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            entries.len(),
            |ui, range| {
                for e in &entries[range] {
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        ui.label(theme::mono(savelens_cas::format_ts(e.at)).small().color(theme::TEXT_DIM));
                        theme::chip(ui, e.source, if e.source == "daemon" { theme::VIOLET } else { theme::TEXT_DIM });
                        theme::chip(ui, &e.kind, kind_color(&e.kind));
                        ui.add(egui::Label::new(RichText::new(&e.message).color(theme::TEXT)).truncate())
                            .on_hover_text(&e.message);
                    });
                }
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("", 0), "");
    }
}
