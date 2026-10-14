//! Floating, non-modal "What changed?" window (bottom-right): when the watcher sees a game
//! save change values that look like player stats (gold, hp, ammo, hunger,
//! quest flags, …) it asks what they are. Answering pins the value into the
//! cheat table; "not interesting" mutes it (or the whole list it belongs to).

use std::time::{Duration, Instant};

use egui::{Align2, RichText};
use savelens_cas::WatchQuestion;
use savelens_core::{CategoryGroup, VariableCategory};
use savelens_watcher::IgnoreScope;

use crate::app::{Refresh, SaveLensApp};
use crate::theme;

/// How often pending questions are re-read from the store.
pub const QUESTION_POLL: Duration = Duration::from_secs(2);

/// Question state of the app.
#[derive(Debug, Default)]
pub struct QuestionsUi {
    pub pending: Vec<WatchQuestion>,
    pub alias: String,
    /// Question the alias buffer belongs to.
    pub alias_for: Option<i64>,
    /// Hide the window (questions are still collected).
    pub quiet: bool,
    pub(crate) last_poll: Option<Instant>,
}

/// What the player chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionChoice {
    Answer(VariableCategory),
    Ignore(IgnoreScope),
    Later,
}

impl SaveLensApp {
    /// Pending questions of the selected game.
    pub fn questions(&self) -> &[WatchQuestion] {
        &self.questions.pending
    }

    /// Re-reads the pending questions of the selected game.
    pub fn reload_questions(&mut self) {
        self.questions.last_poll = Some(Instant::now());
        self.questions.pending = match self.selected_game {
            Some(id) => match self.engine.questions(Some(id), false) {
                Ok(q) => q,
                Err(e) => {
                    self.toasts.error(format!("Cannot read questions: {e}"));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
    }

    pub(crate) fn poll_questions(&mut self) {
        if self.questions.last_poll.is_none_or(|t| t.elapsed() >= QUESTION_POLL) {
            self.reload_questions();
        }
    }

    /// Applies the player's choice to a question.
    pub fn answer(&mut self, id: i64, choice: QuestionChoice, alias: Option<&str>) -> Result<String, String> {
        let msg = match choice {
            QuestionChoice::Answer(cat) => {
                let (_, entry) = self.engine.answer_question(id, cat, alias).map_err(|e| e.to_string())?;
                self.apply_refresh(Refresh { cheats: true, ..Refresh::NONE });
                format!("📌 pinned `{}` ({cat})", entry.alias)
            }
            QuestionChoice::Ignore(scope) => {
                let (_, mute) = self.engine.ignore_question(id, scope).map_err(|e| e.to_string())?;
                format!("Muted `{}`", mute.pattern)
            }
            QuestionChoice::Later => {
                self.engine.snooze_question(id, None).map_err(|e| e.to_string())?;
                "OK, asking again later".to_string()
            }
        };
        self.questions.alias.clear();
        self.reload_questions();
        Ok(msg)
    }
}

/// Draws the floating window (top-right, collapsible, never modal).
pub fn draw(app: &mut SaveLensApp, ctx: &egui::Context) {
    app.poll_questions();
    if app.questions.quiet {
        return;
    }
    let Some(q) = app.questions.pending.first().cloned() else { return };
    if app.questions.alias_for != Some(q.id) {
        app.questions.alias_for = Some(q.id);
        app.questions.alias.clear();
    }
    let total = app.questions.pending.len();
    let mut choice = None;
    let mut hide = false;
    egui::Window::new(RichText::new(format!("❓ What changed? ({total})")).color(theme::AMBER))
        .id(egui::Id::new("savelens-questions"))
        // Bottom-right: never over the top bar (panic button) or the toasts.
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -44.0])
        .collapsible(true)
        .resizable(false)
        .default_width(320.0)
        .show(ctx, |ui| {
            let c = q.candidate();
            ui.label(theme::mono(c.label()).strong().color(theme::TEXT))
                .on_hover_text(format!("{} · seen in {} save(s)", q.rel_path, q.times_seen));
            ui.label(theme::mono(c.change_text()).color(theme::CYAN));
            if let Some(g) = &q.guess {
                ui.label(RichText::new(format!("looks like {} (\"{}\")", g.category, g.keyword)).color(theme::LIME));
            }
            ui.horizontal_wrapped(|ui| {
                for cat in c.quick_choices() {
                    let suggested = q.guess.as_ref().is_some_and(|g| g.category == cat);
                    let color = if suggested { theme::LIME } else { theme::CYAN };
                    if ui.add(theme::accent_button(cat.label(), color)).clicked() {
                        choice = Some(QuestionChoice::Answer(cat));
                    }
                }
                ui.menu_button("More ⏷", |ui| {
                    for group in CategoryGroup::ALL {
                        ui.menu_button(group.label(), |ui| {
                            for cat in group.categories() {
                                if ui.button(cat.label()).clicked() {
                                    choice = Some(QuestionChoice::Answer(cat));
                                    ui.close_menu();
                                }
                            }
                        });
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.questions.alias)
                        .desired_width(140.0)
                        .hint_text("name it (optional)"),
                );
                if ui.button("Later").clicked() {
                    choice = Some(QuestionChoice::Later);
                }
                ui.menu_button("Not interesting ⏷", |ui| {
                    for (label, scope) in [
                        ("Only this value", IgnoreScope::This),
                        ("Everything like it in this file", IgnoreScope::Similar),
                        ("Everything like it in this game", IgnoreScope::Game),
                    ] {
                        if ui.button(label).clicked() {
                            choice = Some(QuestionChoice::Ignore(scope));
                            ui.close_menu();
                        }
                    }
                });
            });
            if ui.small_button("🔕 hide questions").on_hover_text("They keep being collected").clicked() {
                hide = true;
            }
        });
    if hide {
        app.questions.quiet = true;
        app.toasts.info("Questions hidden (still collected)");
    }
    if let Some(choice) = choice {
        let alias = app.questions.alias.trim().to_string();
        let alias = (!alias.is_empty()).then_some(alias);
        match app.answer(q.id, choice, alias.as_deref()) {
            Ok(m) => app.toasts.success(m),
            Err(e) => app.toasts.error(e),
        }
    }
}
