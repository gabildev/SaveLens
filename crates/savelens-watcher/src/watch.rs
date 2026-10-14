//! Change watch on top of the engine: when the game saves, the semantic diff
//! is screened (see [`savelens_core::watch`]) and the few values that look
//! like player stats become "what is this variable?" questions. Answering
//! pins the value into the cheat table; ignoring mutes it (or every value
//! like it) so it is never asked again.
//!
//! Nothing here is modal or blocking: questions are rows in the store that
//! the overlay / TUI / GUI / CLI show and answer whenever the player wants.
//! Only automatic commits (the game itself wrote the file) are screened —
//! checkouts, rollbacks, cherry-picks, edits and trainer injections never
//! raise questions.

use std::collections::{HashMap, HashSet};

use savelens_cas::{AskOutcome, CheatEntry, CommitId, CommitRecord, GameId, GameRecord, WatchMute, WatchQuestion};
use savelens_core::watch::{self, location_key, MutePattern, VariableCategory, Volatility, WatchState};
use savelens_core::{Location, SemanticDiff};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::error::{EngineError, EngineResult};

/// Result of committing a game save.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SaveOutcome {
    pub commit: Option<CommitRecord>,
    /// Questions raised by this save (new or woken up from a snooze).
    pub questions: Vec<WatchQuestion>,
}

/// What "not interesting" applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IgnoreScope {
    /// Only this exact value.
    This,
    /// Every element of the same lists in this file (`enemies[*].hp`).
    Similar,
    /// Every element of the same lists in every file of the game.
    Game,
}

impl IgnoreScope {
    /// Parses `this` / `similar` / `game` (also `exact`, `all`, `everywhere`).
    pub fn parse(s: &str) -> Option<IgnoreScope> {
        match s.trim().to_ascii_lowercase().as_str() {
            "this" | "exact" | "one" => Some(IgnoreScope::This),
            "similar" | "like" | "list" => Some(IgnoreScope::Similar),
            "game" | "all" | "everywhere" => Some(IgnoreScope::Game),
            _ => None,
        }
    }
}

struct EngineWatchState {
    pinned: HashSet<Location>,
    mutes: Vec<MutePattern>,
    known: HashSet<String>,
    volatility: HashMap<String, Volatility>,
}

impl WatchState for EngineWatchState {
    fn is_pinned(&self, location: &Location) -> bool {
        self.pinned.contains(location)
    }

    fn muted_by(&self, location: &Location) -> Option<String> {
        self.mutes.iter().find(|m| m.matches(location)).map(|m| m.0.clone())
    }

    fn already_asked(&self, location: &Location) -> bool {
        self.known.contains(&location_key(location))
    }

    fn volatility(&self, location: &Location) -> Volatility {
        self.volatility.get(&location_key(location)).copied().unwrap_or_default()
    }
}

impl Engine {
    /// Screens the diff of one saved file and stores the resulting questions.
    pub(crate) fn watch_diff(
        &self,
        game: &GameRecord,
        rel: &str,
        diff: &SemanticDiff,
        commit: Option<CommitId>,
    ) -> EngineResult<Vec<WatchQuestion>> {
        let cfg = &self.config().watch;
        let store = self.store();
        let save = store.watch_next_save(game.id, rel)?;
        let (observed, _) = watch::numeric_changes(diff, &cfg.screening);
        let deltas: Vec<(Location, f64)> =
            observed.iter().map(|c| (c.location.clone(), c.new.delta(c.old).as_f64())).collect();
        let volatility = store.watch_observe(game.id, rel, save, &deltas)?;
        let mut mutes: Vec<MutePattern> = cfg.global_mutes.iter().map(|p| MutePattern(p.clone())).collect();
        mutes.extend(
            store
                .watch_mutes(game.id)?
                .into_iter()
                .filter(|m| m.rel_path.as_deref().is_none_or(|r| r == rel))
                .map(|m| m.pattern),
        );
        let state = EngineWatchState {
            pinned: store.cheats(game.id)?.into_iter().filter(|c| c.rel_path == rel).map(|c| c.location).collect(),
            mutes,
            known: store.watch_known_keys(game.id, rel)?,
            volatility,
        };
        let screening = watch::screen(diff, &state, &cfg.screening);
        let mut room = cfg.max_pending_per_game.saturating_sub(store.watch_pending_count(game.id)?);
        let mut raised = Vec::new();
        for candidate in &screening.questions {
            if room == 0 {
                break;
            }
            if let AskOutcome::New(q) = store.watch_ask(game.id, rel, candidate, commit)? {
                room -= 1;
                raised.push(q);
            }
        }
        if !raised.is_empty() {
            let list: Vec<String> = raised.iter().map(WatchQuestion::describe).collect();
            store.log_event(Some(game.id), "watch", &format!("{rel}: what is {}?", list.join(", ")))?;
        }
        Ok(raised)
    }

    /// Questions of a game (all games when `None`), pending only by default.
    pub fn questions(&self, game: Option<GameId>, all: bool) -> EngineResult<Vec<WatchQuestion>> {
        let status = (!all).then_some(savelens_cas::QuestionStatus::Pending);
        Ok(self.store().watch_questions(game, status, 500)?)
    }

    fn unique_alias(&self, game: GameId, wanted: &str, location: &Location, rel: &str) -> EngineResult<String> {
        let cheats = self.store().cheats(game)?;
        let taken = |a: &str| cheats.iter().any(|c| c.alias == a && !(c.location == *location && c.rel_path == rel));
        if !taken(wanted) {
            return Ok(wanted.to_string());
        }
        (2..1000)
            .map(|i| format!("{wanted} {i}"))
            .find(|a| !taken(a))
            .ok_or_else(|| EngineError::Invalid(format!("no free alias like `{wanted}`")))
    }

    /// Answers a question: pins the value into the cheat table under `alias`
    /// (default: from the category and the variable name).
    pub fn answer_question(
        &self,
        id: i64,
        category: VariableCategory,
        alias: Option<&str>,
    ) -> EngineResult<(WatchQuestion, CheatEntry)> {
        let q = self.store().watch_question(id)?;
        let wanted = match alias.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => a.to_string(),
            None => watch::default_alias(category, &q.location),
        };
        let alias = self.unique_alias(q.game_id, &wanted, &q.location, &q.rel_path)?;
        let entry = self.pin(q.game_id, &q.rel_path, &alias, &q.location)?;
        let q = self.store().watch_answer(id, category, &alias)?;
        self.store().log_event(
            Some(q.game_id),
            "watch",
            &format!("{} is {category}: pinned as `{alias}`", q.candidate().label()),
        )?;
        Ok((q, entry))
    }

    /// "Not interesting": mutes the value (or every value like it) and marks
    /// the question, and any other pending question it covers, ignored.
    pub fn ignore_question(&self, id: i64, scope: IgnoreScope) -> EngineResult<(WatchQuestion, WatchMute)> {
        let q = self.store().watch_question(id)?;
        let (pattern, rel) = match scope {
            IgnoreScope::This => (MutePattern::exact(&q.location), Some(q.rel_path.as_str())),
            IgnoreScope::Similar => (MutePattern::generalized(&q.location), Some(q.rel_path.as_str())),
            IgnoreScope::Game => (MutePattern::generalized(&q.location), None),
        };
        let mute = self.store().watch_mute(q.game_id, rel, &pattern)?;
        let q = self.store().watch_ignore(id)?;
        self.store().log_event(Some(q.game_id), "watch", &format!("muted `{pattern}`"))?;
        Ok((q, mute))
    }

    /// "Ask me later": hides the question for `minutes` (config default when `None`).
    pub fn snooze_question(&self, id: i64, minutes: Option<u64>) -> EngineResult<WatchQuestion> {
        let minutes = minutes.unwrap_or(self.config().watch.snooze_minutes).max(1);
        let ms = i64::try_from(minutes.saturating_mul(60_000)).unwrap_or(i64::MAX / 2);
        Ok(self.store().watch_snooze(id, savelens_cas::now_ms().saturating_add(ms))?)
    }

    /// Mutes a pattern by hand (`*` wildcards; `rel = None` for the whole game).
    pub fn mute_pattern(&self, game: GameId, rel: Option<&str>, pattern: &str) -> EngineResult<WatchMute> {
        Ok(self.store().watch_mute(game, rel, &MutePattern(pattern.trim().to_string()))?)
    }
}
