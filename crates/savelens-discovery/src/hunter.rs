//! Live Process Hunter: watches candidate directories while a game runs and ranks the files it
//! writes as probable save files.
//!
//! Two signal sources are combined during [`HuntOptions::duration`]:
//! * a recursive `notify` watcher (inotify on Linux, `ReadDirectoryChangesW` on Windows) on the
//!   candidate roots (Proton user folders, XDG dirs, Windows known folders, discovered save
//!   roots and [`HuntOptions::roots`]),
//! * on Linux, every [`HuntOptions::poll_interval`], the files the game processes hold open for
//!   writing (`/proc/<pid>/fd` + `fdinfo` flags) — this also finds saves outside every root.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;

use crate::process::{self, GameProcess};
use crate::{
    DiscoveryConfig, DiscoveryError, DiscoveryReport, GameRuntime, HostPlatform, WindowsEnv,
};

/// Parameters of a hunt.
#[derive(Debug, Clone)]
pub struct HuntOptions {
    /// How long to observe.
    pub duration: Duration,
    /// How often open file descriptors are sampled and watcher events are aggregated.
    pub poll_interval: Duration,
    /// Extra directories to watch (recursively).
    pub roots: Vec<PathBuf>,
    /// Only consider this process (it is tracked even if it does not look like a game).
    pub pid_filter: Option<u32>,
}

impl Default for HuntOptions {
    fn default() -> Self {
        HuntOptions {
            duration: Duration::from_secs(30),
            poll_interval: Duration::from_millis(250),
            roots: Vec::new(),
            pid_filter: None,
        }
    }
}

/// A file that was written during the hunt.
#[derive(Debug, Clone, Serialize)]
pub struct HuntCandidate {
    pub path: PathBuf,
    /// Higher means more likely to be a save file.
    pub score: f64,
    /// Number of poll intervals during which the file was modified.
    pub writes: u32,
    /// Game process holding the file open for writing, if observed.
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    /// Human readable explanation of the score.
    pub reasons: Vec<String>,
}

/// Result of [`hunt`].
#[derive(Debug, Clone, Serialize)]
pub struct HuntReport {
    /// Candidates sorted by descending score (one entry per path).
    pub candidates: Vec<HuntCandidate>,
    /// Game processes observed during the hunt.
    pub processes: Vec<GameProcess>,
    /// Directories watched recursively.
    pub roots_watched: Vec<PathBuf>,
    /// Non-fatal problems (unwatchable roots, watcher errors).
    pub warnings: Vec<String>,
}

/// Extensions typically used by save files.
pub const SAVE_EXTENSIONS: &[&str] = &[
    "sav", "save", "dat", "json", "es3", "bin", "sl2", "ess", "fos", "nbt", "sqlite", "db", "xml",
    "ini", "rpgsave", "rmmzsave", "txt",
];

const NOISE_EXTENSIONS: &[&str] = &["log", "tmp", "temp", "lock", "pid", "etl", "dmp"];
const NAME_HINTS: &[&str] = &[
    "save",
    "slot",
    "profile",
    "progress",
    "persistent",
    "player",
    "world",
    "char",
];

#[derive(Debug, Default)]
struct Stats {
    writes: u32,
    open_for_write: bool,
    pid: Option<u32>,
    process_name: Option<String>,
}

/// Context used to score a path.
#[derive(Debug, Default)]
pub(crate) struct ScoreContext {
    pub install_dirs: Vec<PathBuf>,
    pub save_roots: Vec<PathBuf>,
}

/// Scores a candidate. Returns the score and the reasons.
pub(crate) fn score(
    path: &Path,
    writes: u32,
    open_for_write: bool,
    ctx: &ScoreContext,
) -> (f64, Vec<String>) {
    let mut s = 0.0;
    let mut reasons = Vec::new();
    if open_for_write {
        s += 3.0;
        reasons.push("held open for writing by a game process".to_string());
    }
    if writes > 0 {
        s += f64::from(writes.min(5));
        reasons.push(format!("modified {writes} time(s) during the hunt"));
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if SAVE_EXTENSIONS.contains(&ext.as_str()) {
        s += 2.0;
        reasons.push(format!("save-like extension .{ext}"));
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if NAME_HINTS.iter().any(|h| name.contains(h)) {
        s += 1.0;
        reasons.push("save-like file name".to_string());
    }
    if ctx.save_roots.iter().any(|r| path.starts_with(r)) {
        s += 2.0;
        reasons.push("inside a known save location".to_string());
    }
    if NOISE_EXTENSIONS.contains(&ext.as_str()) || name.ends_with(".log.txt") || name.ends_with('~')
    {
        s -= 4.0;
        reasons.push(format!("log/temporary file (.{ext})"));
    }
    let noisy_dir = path.parent().is_some_and(|p| {
        p.components().any(|c| {
            let c = c.as_os_str().to_string_lossy().to_ascii_lowercase();
            c.contains("cache")
                || c.contains("crash")
                || c == "logs"
                || c == "log"
                || c == "temp"
                || c == "tmp"
        })
    });
    if noisy_dir {
        s -= 3.0;
        reasons.push("inside a cache/crash/log directory".to_string());
    }
    if ctx.install_dirs.iter().any(|d| path.starts_with(d)) {
        s -= 1.5;
        reasons.push("inside the game install directory".to_string());
    }
    (s, reasons)
}

fn counts_as_write(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Other)
            | EventKind::Modify(ModifyKind::Name(
                RenameMode::To | RenameMode::Both | RenameMode::Any
            ))
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    )
}

fn win_env_dirs(env: &WindowsEnv) -> Vec<PathBuf> {
    let mut v = vec![
        env.app_data.clone(),
        env.local_app_data.clone(),
        env.local_app_data_low.clone(),
        env.saved_games.clone(),
    ];
    v.extend(env.documents.iter().cloned());
    v
}

/// The directories a hunt watches, before existence filtering and nesting removal.
fn candidate_roots(
    cfg: &DiscoveryConfig,
    report: Option<&DiscoveryReport>,
    opts: &HuntOptions,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = opts.roots.clone();
    if let Some(r) = report {
        for g in &r.games {
            roots.extend(g.save_locations.iter().map(|l| l.root.clone()));
            if let GameRuntime::Proton { prefix } = &g.runtime {
                roots.extend(win_env_dirs(&WindowsEnv::for_proton_prefix(prefix)));
            }
        }
    }
    match cfg.host {
        HostPlatform::Linux => {
            roots.push(cfg.xdg_data.clone());
            roots.push(cfg.xdg_config.clone());
            roots.push(cfg.home.join("Documents"));
        }
        HostPlatform::Windows => roots.extend(win_env_dirs(&cfg.windows_env)),
    }
    roots
}

/// Keeps existing directories, canonicalized, without duplicates or nested roots.
fn prune_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut existing: Vec<PathBuf> = roots
        .into_iter()
        .filter(|r| r.is_dir())
        .map(|r| crate::normalize_path(&r))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    existing.sort_by_key(|p| p.components().count());
    let mut out: Vec<PathBuf> = Vec::new();
    for r in existing {
        if !out.iter().any(|o| r.starts_with(o)) {
            out.push(r);
        }
    }
    out.sort();
    out
}

fn tracked_processes(
    cfg: &DiscoveryConfig,
    report: Option<&DiscoveryReport>,
    pid_filter: Option<u32>,
) -> Vec<GameProcess> {
    let games = process::game_processes(cfg, report);
    match pid_filter {
        None => games,
        Some(pid) => match games.into_iter().find(|g| g.pid == pid) {
            Some(g) => vec![g],
            None => process::process_info(pid)
                .map(|p| GameProcess {
                    pid: p.pid,
                    name: p.name,
                    exe: p.exe,
                    app_id: None,
                    reason: "selected by pid filter".to_string(),
                })
                .into_iter()
                .collect(),
        },
    }
}

/// Observes file activity for `opts.duration` and ranks the written files.
pub fn hunt(
    cfg: &DiscoveryConfig,
    report: Option<&DiscoveryReport>,
    opts: &HuntOptions,
) -> Result<HuntReport, DiscoveryError> {
    let mut warnings = Vec::new();
    let roots = prune_roots(candidate_roots(cfg, report, opts));

    let (tx, rx) = crossbeam_channel::unbounded::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        // The receiver only disappears once the hunt is over; late events are irrelevant.
        let _ = tx.send(res);
    })?;
    let mut roots_watched = Vec::new();
    for root in &roots {
        match watcher.watch(root, RecursiveMode::Recursive) {
            Ok(()) => roots_watched.push(root.clone()),
            Err(e) => warnings.push(format!("cannot watch {}: {e}", root.display())),
        }
    }

    let score_ctx = ScoreContext {
        install_dirs: report
            .map(|r| {
                r.games
                    .iter()
                    .filter_map(|g| g.install_dir.clone())
                    .collect()
            })
            .unwrap_or_default(),
        save_roots: report
            .map(|r| {
                r.games
                    .iter()
                    .flat_map(|g| g.save_locations.iter().map(|l| l.root.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    };

    let poll = opts.poll_interval.max(Duration::from_millis(10));
    let process_refresh = Duration::from_secs(2).max(poll);
    let start = Instant::now();
    let deadline = start + opts.duration;
    let mut processes = tracked_processes(cfg, report, opts.pid_filter);
    let mut seen_processes: BTreeMap<u32, GameProcess> =
        processes.iter().map(|p| (p.pid, p.clone())).collect();
    let mut last_process_refresh = start;
    let mut last_poll = start;
    let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stats: BTreeMap<PathBuf, Stats> = BTreeMap::new();
    let mut watch_errors = 0usize;

    let mut handle = |res: notify::Result<notify::Event>,
                      pending: &mut BTreeSet<PathBuf>,
                      warnings: &mut Vec<String>| match res {
        Ok(ev) => {
            if counts_as_write(&ev.kind) {
                let path = match ev.kind {
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                        ev.paths.last().cloned()
                    }
                    _ => ev.paths.first().cloned(),
                };
                if let Some(p) = path {
                    pending.insert(p);
                }
            }
        }
        Err(e) => {
            watch_errors += 1;
            if watch_errors <= 5 {
                warnings.push(format!("watcher error: {e}"));
            }
        }
    };

    let flush = |pending: &mut BTreeSet<PathBuf>,
                 stats: &mut BTreeMap<PathBuf, Stats>,
                 processes: &[GameProcess]| {
        for p in std::mem::take(pending) {
            if p.is_file() {
                stats.entry(p).or_default().writes += 1;
            }
        }
        for proc_ in processes {
            for f in process::files_open_for_write(proc_.pid) {
                let st = stats.entry(f).or_default();
                st.open_for_write = true;
                st.pid = Some(proc_.pid);
                st.process_name = Some(proc_.name.clone());
            }
        }
    };

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let next_poll = (last_poll + poll).min(deadline);
        match rx.recv_timeout(next_poll.saturating_duration_since(now)) {
            Ok(res) => handle(res, &mut pending, &mut warnings),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                warnings.push("file watcher stopped unexpectedly".to_string());
                break;
            }
        }
        let now = Instant::now();
        if now >= last_poll + poll {
            if now >= last_process_refresh + process_refresh {
                processes = tracked_processes(cfg, report, opts.pid_filter);
                for p in &processes {
                    seen_processes.entry(p.pid).or_insert_with(|| p.clone());
                }
                last_process_refresh = now;
            }
            flush(&mut pending, &mut stats, &processes);
            last_poll = now;
        }
    }
    drop(watcher);
    while let Ok(res) = rx.try_recv() {
        handle(res, &mut pending, &mut warnings);
    }
    flush(&mut pending, &mut stats, &processes);

    let mut candidates: Vec<HuntCandidate> = stats
        .into_iter()
        .filter(|(p, _)| p.is_file())
        .map(|(path, st)| {
            let (score, reasons) = score(&path, st.writes, st.open_for_write, &score_ctx);
            HuntCandidate {
                path,
                score,
                writes: st.writes,
                pid: st.pid,
                process_name: st.process_name,
                reasons,
            }
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.writes.cmp(&a.writes))
            .then(a.path.cmp(&b.path))
    });

    Ok(HuntReport {
        candidates,
        processes: seen_processes.into_values().collect(),
        roots_watched,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_prefers_saves_over_noise() {
        let ctx = ScoreContext {
            install_dirs: vec![PathBuf::from("/lib/common/G")],
            save_roots: vec![PathBuf::from("/saves")],
        };
        let (sav, _) = score(Path::new("/h/x/slot1.sav"), 3, false, &ctx);
        let (log, r) = score(Path::new("/h/x/output.log"), 3, false, &ctx);
        assert!(sav > 0.0 && sav > log, "{sav} {log}");
        assert!(r.iter().any(|r| r.contains("log")));
        let (cache, _) = score(Path::new("/h/shadercache/a.bin"), 3, false, &ctx);
        assert!(cache < sav);
        let (inst, _) = score(Path::new("/lib/common/G/config.ini"), 1, false, &ctx);
        let (outside, _) = score(Path::new("/h/config.ini"), 1, false, &ctx);
        assert!(inst < outside);
        let (known, _) = score(Path::new("/saves/a.dat"), 1, true, &ctx);
        assert!(known > sav);
    }

    #[test]
    fn root_pruning() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        std::fs::create_dir_all(a.join("b")).unwrap();
        let pruned = prune_roots(vec![
            a.join("b"),
            a.clone(),
            t.path().join("missing"),
            a.clone(),
        ]);
        assert_eq!(pruned, vec![crate::normalize_path(&a)]);
    }
}
