//! Drawing of every panel. Views only read state and push [`Intent`]s (or
//! edit plain input fields); actions run after the frame in
//! [`SaveLensApp::handle_intent`].

mod activity;
mod diff;
mod editor;
mod scanner;
mod sidebar;
mod timeline;
mod topbar;
pub(crate) mod tree;

use egui::{Align2, RichText};

use crate::app::{Intent, SaveLensApp, View};
use crate::theme;

/// Draws the whole frame.
pub(crate) fn draw(app: &mut SaveLensApp, ctx: &egui::Context) {
    egui::TopBottomPanel::top("savelens-topbar")
        .frame(theme::panel_frame().inner_margin(egui::Margin::symmetric(14, 10)))
        .show(ctx, |ui| topbar::draw(app, ui));
    egui::TopBottomPanel::top("savelens-tabs")
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(14, 6)))
        .show(ctx, |ui| tabs(app, ui));
    egui::SidePanel::left("savelens-sidebar")
        .resizable(true)
        .default_width(300.0)
        .width_range(220.0..=520.0)
        .frame(theme::panel_frame())
        .show(ctx, |ui| sidebar::draw(app, ui));
    if app.view == View::Scanner {
        egui::TopBottomPanel::bottom("savelens-cheats")
            .resizable(true)
            .default_height(280.0)
            .height_range(120.0..=600.0)
            .frame(theme::panel_frame())
            .show(ctx, |ui| scanner::draw_cheat_table(app, ui));
    }
    egui::CentralPanel::default().frame(theme::central_frame()).show(ctx, |ui| match app.view {
        View::Timeline => timeline::draw(app, ui),
        View::Scanner => scanner::draw(app, ui),
        View::TreeDiff => diff::draw(app, ui),
        View::Editor => editor::draw(app, ui),
        View::Activity => activity::draw(app, ui),
    });
    panic_modal(app, ctx);
    diff::pick_modal(app, ctx);
    timeline::name_dialog(app, ctx);
}

fn tabs(app: &mut SaveLensApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        for v in View::ALL {
            let selected = app.view == v;
            let text = RichText::new(v.label()).strong().color(if selected { theme::BG } else { theme::TEXT_DIM });
            let btn = egui::Button::new(text)
                .fill(if selected { theme::CYAN } else { theme::BG_CARD })
                .stroke(egui::Stroke::new(1.0_f32, if selected { theme::CYAN } else { theme::BORDER }))
                .corner_radius(egui::CornerRadius::same(16));
            if ui.add(btn).clicked() && !selected {
                app.intents.push(Intent::SetView(v));
            }
        }
        if app.is_busy() {
            ui.add_space(theme::SPACING);
            ui.add(egui::Spinner::new().color(theme::CYAN));
            ui.label(RichText::new("working…").color(theme::TEXT_DIM).small());
        }
    });
}

fn panic_modal(app: &mut SaveLensApp, ctx: &egui::Context) {
    let Some(p) = app.panic.clone() else { return };
    let mut confirm = false;
    let mut cancel = false;
    let resp = egui::Modal::new(egui::Id::new("savelens-panic-modal")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        ui.label(RichText::new("⚠ PANIC RESTORE ORIGINAL").size(20.0).strong().color(theme::RED));
        ui.add_space(theme::SPACING);
        ui.label("The most recent SaveLens mutation will be reverted bit for bit to its pre-mutation shadow backup:");
        ui.add_space(4.0);
        egui::Grid::new("panic-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(RichText::new("File").color(theme::TEXT_DIM));
            ui.label(theme::mono(p.path.display().to_string()));
            ui.end_row();
            ui.label(RichText::new("Safety tag").color(theme::TEXT_DIM));
            ui.label(theme::mono(&p.tag).color(theme::AMBER));
            ui.end_row();
            ui.label(RichText::new("Mutation").color(theme::TEXT_DIM));
            ui.label(theme::mono(&p.reason));
            ui.end_row();
            ui.label(RichText::new("When").color(theme::TEXT_DIM));
            ui.label(theme::mono(savelens_cas::format_ts(p.created_at)));
            ui.end_row();
            ui.label(RichText::new("Scope").color(theme::TEXT_DIM));
            ui.label(theme::mono(match &p.scope {
                savelens_cas::PanicScope::Latest => "latest mutation (any game)".to_string(),
                savelens_cas::PanicScope::Game(g) => format!("latest mutation of game #{g}"),
                savelens_cas::PanicScope::Path(path) => format!("latest mutation of {}", path.display()),
            }));
            ui.end_row();
        });
        ui.add_space(theme::SPACING);
        ui.label(
            RichText::new("The current state is backed up first, so the restore itself can be undone.")
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::SPACING);
        ui.horizontal(|ui| {
            if ui.add(theme::accent_button("RESTORE NOW", theme::RED)).clicked() {
                confirm = true;
            }
            if ui.add(theme::ghost_button("Cancel", theme::TEXT_DIM)).clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        if let Err(e) = app.confirm_panic() {
            app.toasts.error(e);
        }
    } else if cancel || resp.should_close() {
        app.cancel_panic();
    }
}

/// Centered placeholder message for empty states.
pub(crate) fn empty_state(ui: &mut egui::Ui, title: &str, hint: &str) {
    let rect = ui.available_rect_before_wrap();
    let painter = ui.painter();
    painter.text(
        rect.center() - egui::vec2(0.0, 12.0),
        Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(18.0),
        theme::TEXT_DIM,
    );
    painter.text(
        rect.center() + egui::vec2(0.0, 14.0),
        Align2::CENTER_CENTER,
        hint,
        egui::FontId::proportional(13.0),
        theme::TEXT_FAINT,
    );
}
