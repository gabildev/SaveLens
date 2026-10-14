//! "What is this variable?" questions in the TUI: `A` opens a picker for
//! the best pending question (quick categories with `1`–`9`, the full
//! catalogue, ignore or snooze), then an alias prompt. Never modal on its
//! own: the questions wait until the player presses `A`.

use anyhow::{bail, Result};
use savelens_core::watch::default_alias;
use savelens_core::{CategoryGroup, VariableCategory};
use savelens_watcher::IgnoreScope;

use crate::app::{App, InputPurpose, Mode, PickItem, PickPurpose, Picker};
use crate::widgets::Level;

const MORE: &str = "more";
const LATER: &str = "later";
const IGNORE_THIS: &str = "ignore:this";
const IGNORE_SIMILAR: &str = "ignore:similar";
const IGNORE_GAME: &str = "ignore:game";

impl App {
    /// Opens the picker for the best pending question of the selected game.
    pub(crate) fn open_question(&mut self) -> Result<()> {
        let Some(q) = self.questions.first().cloned() else {
            bail!("no pending \"what is this variable?\" question for this game");
        };
        let c = q.candidate();
        let mut items: Vec<PickItem> = c
            .quick_choices()
            .into_iter()
            .enumerate()
            .map(|(n, cat)| {
                let suggested = q.guess.as_ref().is_some_and(|g| g.category == cat);
                PickItem {
                    label: format!("{} {}{}", n + 1, cat.label(), if suggested { "  ← suggested" } else { "" }),
                    detail: format!("pin as `{}`", default_alias(cat, &q.location)),
                    value: cat.id().to_string(),
                }
            })
            .collect();
        let extra = [
            (MORE, "More categories…", "all 30 categories by group"),
            (LATER, "Ask me later", "hide it for a while"),
            (IGNORE_THIS, "Not interesting: only this value", "never ask about it again"),
            (IGNORE_SIMILAR, "Not interesting: everything like it in this file", "mutes the whole list"),
            (IGNORE_GAME, "Not interesting: everything like it in this game", "mutes it in every file"),
        ];
        items.extend(extra.iter().map(|(v, l, d)| PickItem {
            label: l.to_string(),
            detail: d.to_string(),
            value: v.to_string(),
        }));
        let more =
            if self.questions.len() > 1 { format!(" (+{} more)", self.questions.len() - 1) } else { String::new() };
        self.mode = Mode::Picker(Picker {
            title: format!("❓ {} · {}{more}", q.describe(), q.rel_path),
            items,
            sel: 0,
            purpose: PickPurpose::Question { id: q.id },
        });
        Ok(())
    }

    fn open_question_categories(&mut self, id: i64) {
        let mut items = Vec::new();
        for group in CategoryGroup::ALL {
            for cat in group.categories() {
                items.push(PickItem {
                    label: cat.label().to_string(),
                    detail: group.label().to_string(),
                    value: cat.id().to_string(),
                });
            }
        }
        self.mode = Mode::Picker(Picker {
            title: "What is it?".into(),
            items,
            sel: 0,
            purpose: PickPurpose::QuestionCategory { id },
        });
    }

    /// A picker entry was chosen.
    pub(crate) fn question_picked(&mut self, id: i64, value: &str) -> Result<()> {
        let scope = match value {
            MORE => {
                self.open_question_categories(id);
                return Ok(());
            }
            LATER => {
                self.engine.snooze_question(id, None)?;
                self.toast(Level::Info, "OK, asking again later");
                return self.after_question();
            }
            IGNORE_THIS => Some(IgnoreScope::This),
            IGNORE_SIMILAR => Some(IgnoreScope::Similar),
            IGNORE_GAME => Some(IgnoreScope::Game),
            _ => None,
        };
        if let Some(scope) = scope {
            let (_, mute) = self.engine.ignore_question(id, scope)?;
            self.toast(Level::Info, format!("Muted `{}`", mute.pattern));
            return self.after_question();
        }
        let Some(category) = VariableCategory::parse(value) else {
            bail!("unknown category `{value}`");
        };
        let q = self.engine.store().watch_question(id)?;
        let initial = default_alias(category, &q.location);
        self.prompt(
            InputPurpose::QuestionAlias { id, category },
            &format!("Name for {} ({category})", q.candidate().label()),
            &initial,
            "Enter pins it into the cheat table (freeze it from the Scanner tab or the overlay)",
        );
        Ok(())
    }

    /// Alias prompt accepted: pin the variable.
    pub(crate) fn question_answer(&mut self, id: i64, category: VariableCategory, alias: &str) -> Result<()> {
        let alias = alias.trim();
        let (_, entry) = self.engine.answer_question(id, category, (!alias.is_empty()).then_some(alias))?;
        self.toast(Level::Success, format!("📌 pinned `{}` ({category})", entry.alias));
        self.refresh_cheats();
        self.after_question()
    }

    fn after_question(&mut self) -> Result<()> {
        if let Some(id) = self.current_game() {
            self.questions = self.engine.questions(Some(id), false)?;
        }
        Ok(())
    }
}
