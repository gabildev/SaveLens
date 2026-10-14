//! # savelens-watcher
//!
//! * [`engine::Engine`] — the facade every front-end uses (tracking,
//!   snapshots, semantic history, checkout, edits, cherry-pick, cheat table,
//!   disk trainer, panic restore). All writes into game directories go
//!   through the CAS safety protocol (atomic hot-swap).
//! * [`daemon`] — background watcher with debounce, close-write detection,
//!   lock check, auto-commit and freeze-on-save injection.
//! * [`hotkeys`] — global hotkeys (`Ctrl+Shift+F5/F9/F12`, …).

pub mod config;
pub mod daemon;
pub mod engine;
pub mod error;
pub mod hotkeys;

pub use config::{HotkeyConfig, SaveLensConfig};
pub use daemon::{Daemon, DaemonEvent, DaemonHandle, DaemonOptions};
pub use engine::{
    is_payload_raw, CheatValue, CheckoutReport, EditReport, EditTarget, Engine, FileDiff, FileStatus, TrackedFile,
    TrainerReport, WORKING,
};
pub use error::{EngineError, EngineResult};
pub use hotkeys::{HotkeyAction, HotkeyService};
