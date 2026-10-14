//! Change-watch persistence: "what is this variable?" questions raised by the
//! daemon when a game saves, the player's mute patterns, and the per-variable
//! change history used to detect volatile values (clocks, counters).
//!
//! The tables double as the IPC channel: the daemon inserts questions, the
//! overlay / TUI / GUI / CLI read and answer them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use rusqlite::{params, OptionalExtension};
use savelens_core::watch::{location_key, CategoryGuess, MutePattern, VariableCategory, Volatility, WatchCandidate};
use savelens_core::{Location, ScalarValue};
use serde::{Deserialize, Serialize};

use crate::error::{CasError, CasResult};
use crate::model::{normalize_rel_path, now_ms, CommitId, GameId};
use crate::store::Store;

/// State of a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionStatus {
    /// Waiting for the player.
    Pending,
    /// Answered with a category; the variable was pinned.
    Answered,
    /// "Not interesting": a mute pattern was created.
    Ignored,
    /// "Ask me later": hidden until `snooze_until`.
    Snoozed,
}

impl QuestionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionStatus::Pending => "pending",
            QuestionStatus::Answered => "answered",
            QuestionStatus::Ignored => "ignored",
            QuestionStatus::Snoozed => "snoozed",
        }
    }
}

impl fmt::Display for QuestionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for QuestionStatus {
    type Err = CasError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "pending" => QuestionStatus::Pending,
            "answered" => QuestionStatus::Answered,
            "ignored" => QuestionStatus::Ignored,
            "snoozed" => QuestionStatus::Snoozed,
            other => return Err(CasError::Invalid(format!("unknown question status `{other}`"))),
        })
    }
}

/// A "what is this variable?" question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchQuestion {
    pub id: i64,
    pub game_id: GameId,
    pub rel_path: String,
    pub location: Location,
    /// Value before / after the save that raised the question (updated when
    /// the value changes again while the question is pending).
    pub old: ScalarValue,
    pub new: ScalarValue,
    /// Category suggested from the variable name.
    pub guess: Option<CategoryGuess>,
    pub score: i32,
    /// On/off switch (unlock, quest flag) seen as 0 / 1.
    pub flag: bool,
    pub status: QuestionStatus,
    /// The player's answer.
    pub category: Option<VariableCategory>,
    /// Alias of the cheat-table entry created by the answer.
    pub alias: Option<String>,
    pub snooze_until: Option<i64>,
    /// How many saves changed the value since the question was raised.
    pub times_seen: u32,
    pub commit_id: Option<CommitId>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl WatchQuestion {
    /// The changed value as a scanner candidate (label / change text helpers).
    pub fn candidate(&self) -> WatchCandidate {
        WatchCandidate {
            location: self.location.clone(),
            old: self.old,
            new: self.new,
            guess: self.guess.clone(),
            score: self.score,
            flag: self.flag,
        }
    }

    /// `player.gold: 1500 -> 1300 (-200)`.
    pub fn describe(&self) -> String {
        let c = self.candidate();
        format!("{}: {}", c.label(), c.change_text())
    }
}

/// A muted pattern (`rel_path == None` mutes it in every file of the game).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchMute {
    pub id: i64,
    pub game_id: GameId,
    pub rel_path: Option<String>,
    pub pattern: MutePattern,
    pub created_at: i64,
}

const Q_COLS: &str = "id, game_id, rel_path, location, old_value, new_value, guess, score, flag, status, category, alias, snooze_until, times_seen, commit_id, created_at, updated_at";

type RawQuestion = (
    (i64, GameId, String, String, String, String, Option<String>, i32, i64),
    (String, Option<String>, Option<String>, Option<i64>, i64, Option<CommitId>, i64, i64),
);

fn row_question(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawQuestion> {
    Ok((
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?),
        (r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?),
    ))
}

fn decode_question(raw: RawQuestion) -> CasResult<WatchQuestion> {
    let ((id, game_id, rel_path, loc, old, new, guess, score, flag), rest) = raw;
    let (status, category, alias, snooze_until, times_seen, commit_id, created_at, updated_at) = rest;
    let category = category
        .map(|c| {
            VariableCategory::parse(&c).ok_or_else(|| CasError::Integrity(format!("unknown stored category `{c}`")))
        })
        .transpose()?;
    Ok(WatchQuestion {
        id,
        game_id,
        rel_path,
        location: serde_json::from_str(&loc)?,
        old: serde_json::from_str(&old)?,
        new: serde_json::from_str(&new)?,
        guess: guess.map(|g| serde_json::from_str(&g)).transpose()?,
        score,
        flag: flag != 0,
        status: status.parse()?,
        category,
        alias,
        snooze_until,
        times_seen: u32::try_from(times_seen).unwrap_or(u32::MAX),
        commit_id,
        created_at,
        updated_at,
    })
}

/// Result of [`Store::watch_ask`].
#[derive(Debug, Clone, PartialEq)]
pub enum AskOutcome {
    /// A new question was created.
    New(WatchQuestion),
    /// A pending question was refreshed with the latest values.
    Refreshed(WatchQuestion),
    /// The question exists and is answered / ignored / snoozed: nothing to do.
    Unchanged,
}

impl Store {
    /// Increments and returns the save counter of a file (1 for the first save).
    pub fn watch_next_save(&self, game: GameId, rel_path: &str) -> CasResult<u64> {
        let rel = normalize_rel_path(rel_path)?;
        let g = self.inner.lock();
        g.conn.execute(
            "INSERT INTO watch_saves(game_id, rel_path, saves) VALUES (?1, ?2, 1)
             ON CONFLICT(game_id, rel_path) DO UPDATE SET saves = saves + 1",
            params![game, rel],
        )?;
        let n: i64 = g.conn.query_row(
            "SELECT saves FROM watch_saves WHERE game_id = ?1 AND rel_path = ?2",
            params![game, rel],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Records that `changes` (location, delta) changed in save number `save`
    /// and returns their updated histories, keyed by [`location_key`].
    pub fn watch_observe(
        &self,
        game: GameId,
        rel_path: &str,
        save: u64,
        changes: &[(Location, f64)],
    ) -> CasResult<HashMap<String, Volatility>> {
        let rel = normalize_rel_path(rel_path)?;
        let mut out = HashMap::new();
        let mut g = self.inner.lock();
        let tx = g.conn.transaction()?;
        for (loc, delta) in changes {
            let key = location_key(loc);
            let stored: Option<String> = tx
                .query_row(
                    "SELECT stats FROM watch_volatility WHERE game_id = ?1 AND rel_path = ?2 AND location_key = ?3",
                    params![game, rel, key],
                    |r| r.get(0),
                )
                .optional()?;
            let mut v: Volatility = stored.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
            if v.changes == 0 || v.last_save != save {
                v.observe(save, *delta);
            }
            tx.execute(
                "INSERT INTO watch_volatility(game_id, rel_path, location_key, stats) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(game_id, rel_path, location_key) DO UPDATE SET stats = excluded.stats",
                params![game, rel, key, serde_json::to_string(&v)?],
            )?;
            out.insert(key, v);
        }
        tx.commit()?;
        Ok(out)
    }

    /// Creates a question, or refreshes the values of a pending one.
    pub fn watch_ask(
        &self,
        game: GameId,
        rel_path: &str,
        candidate: &WatchCandidate,
        commit: Option<CommitId>,
    ) -> CasResult<AskOutcome> {
        let rel = normalize_rel_path(rel_path)?;
        let key = location_key(&candidate.location);
        let now = now_ms();
        let existing = {
            let g = self.inner.lock();
            g.conn
                .query_row(
                    &format!(
                        "SELECT {Q_COLS} FROM watch_questions WHERE game_id = ?1 AND rel_path = ?2 AND location_key = ?3"
                    ),
                    params![game, rel, key],
                    row_question,
                )
                .optional()?
        };
        let old = serde_json::to_string(&candidate.old)?;
        let new = serde_json::to_string(&candidate.new)?;
        match existing.map(decode_question).transpose()? {
            Some(q) if q.status == QuestionStatus::Pending => {
                {
                    let g = self.inner.lock();
                    g.conn.execute(
                        "UPDATE watch_questions SET old_value = ?1, new_value = ?2, times_seen = times_seen + 1,
                         commit_id = COALESCE(?3, commit_id), updated_at = ?4 WHERE id = ?5",
                        params![old, new, commit, now, q.id],
                    )?;
                }
                Ok(AskOutcome::Refreshed(self.watch_question(q.id)?))
            }
            Some(q) if q.status == QuestionStatus::Snoozed && q.snooze_until.is_some_and(|t| t <= now) => {
                {
                    let g = self.inner.lock();
                    g.conn.execute(
                        "UPDATE watch_questions SET status = 'pending', snooze_until = NULL, old_value = ?1,
                         new_value = ?2, times_seen = times_seen + 1, commit_id = COALESCE(?3, commit_id),
                         updated_at = ?4 WHERE id = ?5",
                        params![old, new, commit, now, q.id],
                    )?;
                }
                Ok(AskOutcome::New(self.watch_question(q.id)?))
            }
            Some(_) => Ok(AskOutcome::Unchanged),
            None => {
                let id = {
                    let g = self.inner.lock();
                    g.conn.execute(
                        "INSERT INTO watch_questions(game_id, rel_path, location_key, location, old_value, new_value,
                         guess, score, flag, status, commit_id, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?11, ?11)",
                        params![
                            game,
                            rel,
                            key,
                            serde_json::to_string(&candidate.location)?,
                            old,
                            new,
                            candidate.guess.as_ref().map(serde_json::to_string).transpose()?,
                            candidate.score,
                            candidate.flag as i64,
                            commit,
                            now
                        ],
                    )?;
                    g.conn.last_insert_rowid()
                };
                Ok(AskOutcome::New(self.watch_question(id)?))
            }
        }
    }

    /// One question.
    pub fn watch_question(&self, id: i64) -> CasResult<WatchQuestion> {
        let raw = {
            let g = self.inner.lock();
            g.conn
                .query_row(&format!("SELECT {Q_COLS} FROM watch_questions WHERE id = ?1"), params![id], row_question)
                .optional()?
        };
        decode_question(raw.ok_or_else(|| CasError::NotFound { kind: "question", id: id.to_string() })?)
    }

    /// Questions, newest first. Snoozed questions whose time is up are
    /// reported as pending.
    pub fn watch_questions(
        &self,
        game: Option<GameId>,
        status: Option<QuestionStatus>,
        limit: usize,
    ) -> CasResult<Vec<WatchQuestion>> {
        self.watch_wake_snoozed()?;
        let raws = {
            let g = self.inner.lock();
            let mut stmt = g.conn.prepare(&format!(
                "SELECT {Q_COLS} FROM watch_questions WHERE (?1 IS NULL OR game_id = ?1) AND (?2 IS NULL OR status = ?2)
                 ORDER BY score DESC, id DESC LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(params![game, status.map(QuestionStatus::as_str), limit as i64], row_question)?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        raws.into_iter().map(decode_question).collect()
    }

    /// Number of pending questions of a game.
    pub fn watch_pending_count(&self, game: GameId) -> CasResult<usize> {
        self.watch_wake_snoozed()?;
        let g = self.inner.lock();
        let n: i64 = g.conn.query_row(
            "SELECT COUNT(*) FROM watch_questions WHERE game_id = ?1 AND status = 'pending'",
            params![game],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn watch_wake_snoozed(&self) -> CasResult<()> {
        let g = self.inner.lock();
        g.conn.execute(
            "UPDATE watch_questions SET status = 'pending', snooze_until = NULL
             WHERE status = 'snoozed' AND snooze_until IS NOT NULL AND snooze_until <= ?1",
            params![now_ms()],
        )?;
        Ok(())
    }

    /// Location keys of a file that must not be asked again: pending,
    /// snoozed, answered or ignored questions.
    pub fn watch_known_keys(&self, game: GameId, rel_path: &str) -> CasResult<HashSet<String>> {
        let rel = normalize_rel_path(rel_path)?;
        let g = self.inner.lock();
        let mut stmt = g.conn.prepare(
            "SELECT location_key FROM watch_questions WHERE game_id = ?1 AND rel_path = ?2
             AND NOT (status = 'snoozed' AND snooze_until IS NOT NULL AND snooze_until <= ?3)",
        )?;
        let keys = stmt
            .query_map(params![game, rel, now_ms()], |r| r.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()?;
        Ok(keys)
    }

    fn watch_set_status(
        &self,
        id: i64,
        status: QuestionStatus,
        category: Option<VariableCategory>,
        alias: Option<&str>,
        snooze_until: Option<i64>,
    ) -> CasResult<WatchQuestion> {
        {
            let g = self.inner.lock();
            let n = g.conn.execute(
                "UPDATE watch_questions SET status = ?1, category = ?2, alias = ?3, snooze_until = ?4, updated_at = ?5
                 WHERE id = ?6",
                params![status.as_str(), category.map(VariableCategory::id), alias, snooze_until, now_ms(), id],
            )?;
            if n == 0 {
                return Err(CasError::NotFound { kind: "question", id: id.to_string() });
            }
        }
        self.watch_question(id)
    }

    /// Marks a question answered.
    pub fn watch_answer(&self, id: i64, category: VariableCategory, alias: &str) -> CasResult<WatchQuestion> {
        self.watch_set_status(id, QuestionStatus::Answered, Some(category), Some(alias), None)
    }

    /// Marks a question ignored (the caller creates the mute pattern).
    pub fn watch_ignore(&self, id: i64) -> CasResult<WatchQuestion> {
        self.watch_set_status(id, QuestionStatus::Ignored, None, None, None)
    }

    /// Hides a question until `until_ms`.
    pub fn watch_snooze(&self, id: i64, until_ms: i64) -> CasResult<WatchQuestion> {
        self.watch_set_status(id, QuestionStatus::Snoozed, None, None, Some(until_ms))
    }

    /// Re-opens a question (e.g. after the cheat was deleted, or by mistake).
    pub fn watch_reopen(&self, id: i64) -> CasResult<WatchQuestion> {
        self.watch_set_status(id, QuestionStatus::Pending, None, None, None)
    }

    /// Mutes a pattern for one file (`Some`) or the whole game (`None`).
    /// Pending questions matching it are marked ignored.
    pub fn watch_mute(&self, game: GameId, rel_path: Option<&str>, pattern: &MutePattern) -> CasResult<WatchMute> {
        if pattern.0.trim().is_empty() {
            return Err(CasError::Invalid("mute pattern cannot be empty".into()));
        }
        let rel = rel_path.map(normalize_rel_path).transpose()?.unwrap_or_default();
        {
            let g = self.inner.lock();
            g.conn.execute(
                "INSERT INTO watch_mutes(game_id, rel_path, pattern, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(game_id, rel_path, pattern) DO NOTHING",
                params![game, rel, pattern.0.trim(), now_ms()],
            )?;
        }
        let mute = self
            .watch_mutes(game)?
            .into_iter()
            .find(|m| m.pattern.0 == pattern.0.trim() && m.rel_path.as_deref().unwrap_or("") == rel)
            .ok_or_else(|| CasError::NotFound { kind: "mute", id: pattern.0.clone() })?;
        for q in self.watch_questions(Some(game), Some(QuestionStatus::Pending), usize::MAX >> 1)? {
            let same_file = mute.rel_path.as_deref().is_none_or(|r| r == q.rel_path);
            if same_file && mute.pattern.matches(&q.location) {
                self.watch_ignore(q.id)?;
            }
        }
        Ok(mute)
    }

    /// Mute patterns of a game.
    pub fn watch_mutes(&self, game: GameId) -> CasResult<Vec<WatchMute>> {
        let g = self.inner.lock();
        let mut stmt = g.conn.prepare(
            "SELECT id, game_id, rel_path, pattern, created_at FROM watch_mutes WHERE game_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![game], |r| {
                let rel: String = r.get(2)?;
                Ok(WatchMute {
                    id: r.get(0)?,
                    game_id: r.get(1)?,
                    rel_path: (!rel.is_empty()).then_some(rel),
                    pattern: MutePattern(r.get(3)?),
                    created_at: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Removes a mute pattern.
    pub fn watch_unmute(&self, game: GameId, id: i64) -> CasResult<()> {
        let g = self.inner.lock();
        let n = g.conn.execute("DELETE FROM watch_mutes WHERE game_id = ?1 AND id = ?2", params![game, id])?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "mute", id: id.to_string() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savelens_core::NodePath;

    fn setup() -> (tempfile::TempDir, Store, GameId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let g = store.ensure_game("Game", None, &dir.path().join("saves"), &[], "native").unwrap();
        (dir, store, g.id)
    }

    fn cand(path: &str, old: i128, new: i128) -> WatchCandidate {
        let location = Location::Ast { path: NodePath::parse(path).unwrap() };
        WatchCandidate {
            guess: savelens_core::watch::guess_category(&NodePath::parse(path).unwrap()),
            location,
            old: ScalarValue::Int(old),
            new: ScalarValue::Int(new),
            score: 50,
            flag: false,
        }
    }

    #[test]
    fn questions_lifecycle() {
        let (_d, s, g) = setup();
        let AskOutcome::New(q) = s.watch_ask(g, "save.json", &cand("gold", 10, 20), Some(1)).unwrap() else {
            panic!("expected a new question")
        };
        assert_eq!(q.status, QuestionStatus::Pending);
        assert_eq!(q.guess.as_ref().unwrap().category, VariableCategory::Money);
        assert_eq!(q.describe(), "gold: 10 -> 20 (+10)");
        let AskOutcome::Refreshed(q2) = s.watch_ask(g, "save.json", &cand("gold", 20, 35), Some(2)).unwrap() else {
            panic!("expected a refresh")
        };
        assert_eq!((q2.id, q2.times_seen, q2.new), (q.id, 2, ScalarValue::Int(35)));
        assert_eq!(s.watch_pending_count(g).unwrap(), 1);
        assert!(s.watch_known_keys(g, "save.json").unwrap().contains("gold"));

        let a = s.watch_answer(q.id, VariableCategory::Money, "Money").unwrap();
        assert_eq!(
            (a.status, a.category, a.alias.as_deref()),
            (QuestionStatus::Answered, Some(VariableCategory::Money), Some("Money"))
        );
        assert_eq!(s.watch_ask(g, "save.json", &cand("gold", 35, 40), None).unwrap(), AskOutcome::Unchanged);
        assert_eq!(s.watch_pending_count(g).unwrap(), 0);

        // Snooze: hidden until the time is up, then pending again.
        let AskOutcome::New(h) = s.watch_ask(g, "save.json", &cand("hp", 5, 4), None).unwrap() else { panic!() };
        s.watch_snooze(h.id, now_ms() + 60_000).unwrap();
        assert_eq!(s.watch_ask(g, "save.json", &cand("hp", 4, 3), None).unwrap(), AskOutcome::Unchanged);
        assert!(s.watch_known_keys(g, "save.json").unwrap().contains("hp"));
        s.watch_snooze(h.id, now_ms() - 1).unwrap();
        assert!(!s.watch_known_keys(g, "save.json").unwrap().contains("hp"));
        assert!(matches!(s.watch_ask(g, "save.json", &cand("hp", 3, 2), None).unwrap(), AskOutcome::New(_)));
        assert_eq!(s.watch_questions(Some(g), Some(QuestionStatus::Pending), 10).unwrap().len(), 1);
        assert_eq!(s.watch_reopen(a.id).unwrap().status, QuestionStatus::Pending);
        assert!(s.watch_answer(9999, VariableCategory::Money, "x").is_err());
    }

    #[test]
    fn mutes_ignore_matching_pending_questions() {
        let (_d, s, g) = setup();
        for i in 0..3 {
            s.watch_ask(g, "a.sav", &cand(&format!("enemies[{i}].hp"), 10, 5), None).unwrap();
        }
        s.watch_ask(g, "b.sav", &cand("enemies[0].hp", 10, 5), None).unwrap();
        s.watch_ask(g, "a.sav", &cand("gold", 1, 2), None).unwrap();
        let m = s.watch_mute(g, Some("a.sav"), &MutePattern("enemies[*].hp".into())).unwrap();
        assert_eq!(m.rel_path.as_deref(), Some("a.sav"));
        let pending = s.watch_questions(Some(g), Some(QuestionStatus::Pending), 10).unwrap();
        let labels: Vec<String> = pending.iter().map(|q| format!("{}:{}", q.rel_path, q.candidate().label())).collect();
        assert_eq!(pending.len(), 2, "{labels:?}");
        assert!(labels.contains(&"b.sav:enemies[0].hp".to_string()));
        // Same pattern again is a no-op; a game-wide mute catches the other file.
        s.watch_mute(g, Some("a.sav"), &MutePattern("enemies[*].hp".into())).unwrap();
        s.watch_mute(g, None, &MutePattern("enemies[*].*".into())).unwrap();
        assert_eq!(s.watch_mutes(g).unwrap().len(), 2);
        assert_eq!(s.watch_pending_count(g).unwrap(), 1);
        s.watch_unmute(g, m.id).unwrap();
        assert!(s.watch_unmute(g, m.id).is_err());
        assert!(s.watch_mute(g, None, &MutePattern("  ".into())).is_err());
    }

    #[test]
    fn volatility_is_tracked_per_save() {
        let (_d, s, g) = setup();
        let loc = Location::Ast { path: NodePath::parse("clock").unwrap() };
        let mut last = Volatility::default();
        for _ in 0..4 {
            let save = s.watch_next_save(g, "a.sav").unwrap();
            last = s.watch_observe(g, "a.sav", save, &[(loc.clone(), 1.0)]).unwrap()["clock"];
        }
        assert_eq!((last.streak, last.changes, last.last_save), (4, 4, 4));
        assert!(last.is_volatile(4, false));
        // Observing the same save twice does not double count.
        let again = s.watch_observe(g, "a.sav", 4, &[(loc.clone(), 1.0)]).unwrap()["clock"];
        assert_eq!(again, last);
        // A save without the change breaks the streak.
        s.watch_next_save(g, "a.sav").unwrap();
        let save = s.watch_next_save(g, "a.sav").unwrap();
        let v = s.watch_observe(g, "a.sav", save, &[(loc, 1.0)]).unwrap()["clock"];
        assert_eq!(v.streak, 1);
        s.remove_game(g).unwrap();
    }
}
