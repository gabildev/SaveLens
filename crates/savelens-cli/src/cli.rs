//! Command-line definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// SaveLens — semantic versioning, disk scanner and live branches for game saves.
#[derive(Debug, Parser)]
#[command(name = "savelens", version, about, long_about = None, propagate_version = true)]
pub struct Cli {
    /// Data directory (defaults to $SAVELENS_HOME or the user data dir).
    #[arg(long, global = true, env = "SAVELENS_HOME")]
    pub home: Option<PathBuf>,

    /// Print machine-readable JSON instead of text where supported.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum EndianArg {
    Le,
    Be,
    Both,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum AlignArg {
    Unaligned,
    Natural,
    Four,
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Game (id, app id or name).
    pub game: String,
    /// Save file, relative to the game's save root.
    pub file: String,
    /// Scan steps as `REV=FILTER` (first = first scan). REV is a revision
    /// (`HEAD`, `HEAD~2`, `#12`, a branch/tag) or `WORKING`; FILTER uses the
    /// scanner syntax: `1500`, `1500->1300`, `+300`, `-=300`, `inc`, `dec`,
    /// `changed`, `unchanged`, `10..20`, `?`.
    #[arg(required = true)]
    pub steps: Vec<String>,
    /// Primitive types for raw regions (comma separated: i8,i16,i32,i64,u8,u16,u32,u64,f32,f64,ascii).
    #[arg(long, default_value = "i32,u32,i64,f32,f64")]
    pub types: String,
    #[arg(long, value_enum, default_value_t = EndianArg::Le)]
    pub endian: EndianArg,
    #[arg(long, value_enum, default_value_t = AlignArg::Unaligned)]
    pub align: AlignArg,
    /// Scan the raw payload instead of the structured tree.
    #[arg(long)]
    pub raw: bool,
    /// Maximum candidates printed.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Pin the result under this alias when exactly one candidate remains.
    #[arg(long)]
    pub pin: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch the terminal UI.
    Tui,
    /// Launch the desktop GUI.
    Gui,
    /// Launch the translucent mod-menu overlay (disk trainer).
    Overlay {
        /// Game to show (id, app id or name).
        #[arg(long)]
        game: Option<String>,
        /// Window opacity (0.2–1.0).
        #[arg(long, default_value_t = 0.88)]
        opacity: f32,
        /// Do not start an embedded watcher daemon (use an external `savelens watch`).
        #[arg(long)]
        no_daemon: bool,
        /// Do not register global hotkeys.
        #[arg(long)]
        no_hotkeys: bool,
    },
    /// Run the background watcher daemon in the foreground (Ctrl+C to stop).
    Watch {
        /// Only these games.
        #[arg(long = "game")]
        games: Vec<String>,
        /// Do not register global hotkeys.
        #[arg(long)]
        no_hotkeys: bool,
    },
    /// Auto-discover Steam games and their save folders.
    Discover {
        /// Track every discovered save location.
        #[arg(long)]
        track_all: bool,
    },
    /// Watch running game processes and file writes to find where a game saves.
    Hunt {
        /// Duration in seconds.
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// Only this process id.
        #[arg(long)]
        pid: Option<u32>,
        /// Extra directories to watch.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    /// Start tracking a save folder (imports the current files).
    Track {
        name: String,
        dir: PathBuf,
        /// Glob patterns relative to DIR (default: every file).
        #[arg(long = "pattern", short = 'p')]
        patterns: Vec<String>,
        #[arg(long)]
        app_id: Option<u32>,
    },
    /// Stop tracking a game (history is kept until `gc`).
    Untrack { game: String },
    /// List tracked games.
    Games,
    /// Status of a game (files, branch, integrity) or of the whole store.
    Status { game: Option<String> },
    /// Snapshot all tracked files of a game.
    Snapshot {
        game: String,
        #[arg(short, long, default_value = "manual snapshot")]
        message: String,
    },
    /// Semantic history of a branch.
    Log {
        game: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// Show per-file changelog lines.
        #[arg(long)]
        full: bool,
    },
    /// Semantic diff between two revisions (default HEAD vs WORKING).
    Diff {
        game: String,
        #[arg(default_value = "HEAD")]
        from: String,
        #[arg(default_value = "WORKING")]
        to: String,
        #[arg(long)]
        file: Option<String>,
    },
    /// Inspect any save file: layers, codec, integrity, tree.
    Inspect {
        path: PathBuf,
        /// Tree depth printed.
        #[arg(long, default_value_t = 3)]
        depth: usize,
    },
    /// Print the node at PATH of a file version.
    Get {
        game: String,
        file: String,
        #[arg(default_value = "$")]
        path: String,
        #[arg(long, default_value = "WORKING")]
        rev: String,
    },
    /// Edit a value on disk (re-encrypts, re-compresses and re-signs automatically).
    Edit {
        game: String,
        file: String,
        /// Node path (`Player.Gold`) or raw location (`$@0x1a4:i32le`).
        target: String,
        value: String,
        /// Allow raw in-place mode when the Safe Edit Gate blocks structural edits.
        #[arg(long)]
        raw: bool,
    },
    /// Multi-snapshot save-state scanner (Cheat Engine on disk).
    Scan(ScanArgs),
    /// Pin a variable under an alias in the cheat table.
    Pin {
        game: String,
        file: String,
        alias: String,
        /// Node path or raw location (`Region@0x1a4:i32le`).
        location: String,
    },
    /// Show the cheat table with live values.
    Cheats { game: String },
    /// Set a pinned variable on disk ("Apply & Sign Save").
    Set { game: String, alias: String, value: String },
    /// "What changed?": values a game save changed that look like player
    /// stats (money, health, ammo, hunger, XP, quest flags, …). Answer them to
    /// pin them, or mute what you do not care about.
    What {
        #[command(subcommand)]
        action: Option<WhatCmd>,
    },
    /// Freeze a pinned variable (applied after every save while the trainer is on).
    Freeze {
        game: String,
        alias: String,
        /// Value to freeze (default: current value).
        value: Option<String>,
        /// Disable the freeze rule.
        #[arg(long)]
        off: bool,
    },
    /// Enable / disable the disk trainer (freeze-on-save) of a game.
    Trainer {
        game: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// Inject the frozen values now.
        #[arg(long)]
        apply: bool,
    },
    /// Restore sub-trees of an older revision into the current save.
    CherryPick {
        game: String,
        rev: String,
        file: String,
        #[arg(required = true)]
        paths: Vec<String>,
        /// Deep-merge maps instead of replacing.
        #[arg(long)]
        merge: bool,
        /// Refuse structural changes (added/removed keys, array lengths).
        #[arg(long)]
        strict: bool,
    },
    /// Write the files of a revision to disk (with safety backups).
    Checkout {
        game: String,
        rev: String,
        #[arg(long = "file")]
        files: Vec<String>,
    },
    /// List or create branches.
    Branch {
        game: String,
        name: Option<String>,
        #[arg(long)]
        from: Option<String>,
        /// Switch to the new branch.
        #[arg(long)]
        switch: bool,
        /// Delete the branch.
        #[arg(long)]
        delete: bool,
    },
    /// Switch branch (writes the branch head to disk).
    Switch { game: String, branch: String },
    /// List or create tags.
    Tag {
        game: String,
        name: Option<String>,
        #[arg(default_value = "HEAD")]
        rev: String,
    },
    /// Revert the last SaveLens mutation to its pre-mutation bytes.
    PanicRestore {
        #[arg(long)]
        game: Option<String>,
        /// Ask the running daemon to do it instead (e.g. from scripts / hotkey tools).
        #[arg(long)]
        via_daemon: bool,
    },
    /// Restore the previous commit of the current branch.
    Rollback { game: String },
    /// List safety backups.
    Backups {
        #[arg(long)]
        game: Option<String>,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Restore a specific safety backup tag.
    RestoreBackup { tag: String },
    /// Delete unreachable history and unreferenced chunks.
    Gc,
    /// Verify every stored blob.
    Verify,
    /// Show or initialise `config.toml`.
    Config {
        #[arg(long)]
        init: bool,
    },
    /// List encryption profiles (`game_keys.toml`).
    Keys,
    /// List structural codecs.
    Codecs,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ScopeArg {
    /// Only this value.
    This,
    /// Every element of the same lists in this file (`enemies[*].hp`).
    Similar,
    /// Every element of the same lists in every file of the game.
    Game,
}

#[derive(Debug, Subcommand)]
pub enum WhatCmd {
    /// List questions (pending ones by default).
    List {
        /// Only this game (id, app id or name).
        game: Option<String>,
        /// Include answered / ignored / snoozed questions.
        #[arg(long)]
        all: bool,
    },
    /// Answer a question: pins the value into the cheat table.
    Answer {
        id: i64,
        /// What it is: money, gems, materials, food, fuel, health, max-health,
        /// armor, mana, stamina, lives, hunger, xp, level, skill-points,
        /// attribute, reputation, quest, unlock, ammo, items, keys,
        /// durability, damage, speed, score, kills, population, time, other
        /// (or its number in `savelens what categories`).
        category: String,
        /// Alias in the cheat table (default: from the category and the name).
        #[arg(long)]
        alias: Option<String>,
    },
    /// Not interesting: never ask about this value (or values like it) again.
    Ignore {
        id: i64,
        #[arg(long, value_enum, default_value_t = ScopeArg::This)]
        scope: ScopeArg,
    },
    /// Ask again later.
    Later {
        id: i64,
        /// Minutes (default: `watch.snooze_minutes` of config.toml).
        #[arg(long)]
        minutes: Option<u64>,
    },
    /// Mute a path pattern (`*` wildcard, e.g. `world.*`, `*.timePlayed`).
    Mute {
        game: String,
        pattern: String,
        /// Only in this save file.
        #[arg(long)]
        file: Option<String>,
    },
    /// List the muted patterns of a game.
    Mutes { game: String },
    /// Remove a muted pattern.
    Unmute { game: String, id: i64 },
    /// List every category.
    Categories,
}
