//! # savelens-overlay
//!
//! The SaveLens **Mod Menu & floating Disk-Trainer**: a minimalist,
//! translucent, always-on-top, borderless window meant to sit over a game
//! running in borderless-fullscreen mode.
//!
//! * **Live cheat table** — the selected game's pinned variables with the
//!   value currently stored in the save on disk ([`savelens_watcher::Engine::cheat_values`]),
//!   a trend arrow and a sparkline of the last values. Values are re-read
//!   when the daemon reports a commit / trainer injection, or once per second
//!   *only if* the size or mtime of one of the pinned files changed.
//! * **Disk-Trainer (auto-freeze on save)** — per-variable freeze toggles and
//!   frozen values (stored in the cheat table of the SaveLens store), a master
//!   switch ([`savelens_cas::Store::set_trainer_enabled`]) and per-game presets
//!   persisted as JSON in `<data dir>/overlay_presets.json` (never inside the
//!   game folder). The injection itself is performed by the daemon
//!   ([`savelens_watcher::Engine::inject_trainer`]) — either an embedded
//!   [`savelens_watcher::Daemon`] or an external `savelens watch` process whose
//!   activity is observed through [`savelens_cas::Store::events`].
//! * **Actions** — Apply now, quick snapshot with the template applied, quick
//!   rollback and Panic Restore.
//! * **Global hotkeys** — [`savelens_watcher::HotkeyService`] when no external
//!   daemon owns the bindings, plus `toggle-overlay` requests from the SQLite
//!   request bus (see [`bus`]).
//!
//! The UI ([`OverlayApp`]) is a thin layer over a headless, fully testable
//! controller ([`OverlayController`]).

pub mod app;
pub mod bus;
pub mod controller;
pub mod presets;
pub mod theme;

use anyhow::anyhow;

pub use app::OverlayApp;
pub use controller::{
    CheatRow, DaemonMode, HotkeyStatus, LatencySample, LatencySource, Notice, NoticeLevel, OverlayCommand,
    OverlayController, SnapshotOutcome, Trend, LATENCY_WARN_MS,
};
pub use presets::{Preset, PresetEntry, PresetStore, PRESETS_FILE};

/// Overlay start-up options (filled by the `savelens overlay` command).
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayOptions {
    /// Game to show first (store id). The first game with pinned variables
    /// is selected when `None`.
    pub game: Option<i64>,
    /// Opacity of the panel background, `0.2..=1.0`.
    pub opacity: f32,
    /// Run the watcher daemon inside the overlay process (unless another
    /// `savelens watch` process is already alive).
    pub embedded_daemon: bool,
    /// Register the global hotkeys (unless an external daemon owns them).
    pub hotkeys: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        OverlayOptions {
            game: None,
            opacity: 0.88,
            embedded_daemon: true,
            hotkeys: true,
        }
    }
}

impl OverlayOptions {
    /// Opacity clamped to the supported range.
    pub fn clamped_opacity(&self) -> f32 {
        if self.opacity.is_finite() {
            self.opacity.clamp(0.2, 1.0)
        } else {
            1.0
        }
    }
}

/// Default window size (logical points).
pub const WINDOW_SIZE: [f32; 2] = [360.0, 520.0];

/// Window configuration of the overlay: transparent, always on top, no
/// decorations, 360×520.
pub fn viewport(opts: &OverlayOptions) -> egui::ViewportBuilder {
    let title = match opts.game {
        Some(id) => format!("SaveLens Overlay — game #{id}"),
        None => "SaveLens Overlay".to_string(),
    };
    egui::ViewportBuilder::default()
        .with_title(title)
        .with_app_id("savelens-overlay")
        .with_transparent(true)
        .with_always_on_top()
        .with_decorations(false)
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size([220.0, 160.0])
        .with_resizable(true)
}

/// Opens the overlay window and runs it until it is closed.
pub fn run(engine: savelens_watcher::Engine, opts: OverlayOptions) -> anyhow::Result<()> {
    let native = eframe::NativeOptions {
        viewport: viewport(&opts),
        ..Default::default()
    };
    let app = OverlayApp::new(engine, opts)?;
    eframe::run_native(
        "SaveLens Overlay",
        native,
        Box::new(move |cc| {
            let mut app = app;
            app.attach_context(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("the overlay window could not be opened: {e}"))
}
