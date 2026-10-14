//! # savelens-gui
//!
//! `eframe`/`egui` desktop front-end of SaveLens: tracked games sidebar with
//! Steam discovery and the live auto-hunter, a commit graph timeline, the
//! Scanner Studio with its cheat table, a visual tree diff & cherry-picker,
//! an AST editor guarded by the Safe Edit Gate, an activity log, integrity
//! chips and the panic-restore button.
//!
//! Every engine call runs off the UI thread ([`jobs`]); the UI state is a
//! plain struct ([`SaveLensApp`]) that tests drive headlessly through
//! `egui::Context::run`.

pub mod app;
pub mod graph;
pub mod jobs;
pub mod scanner;
pub mod theme;
pub mod toast;
pub mod treediff;
mod views;

pub use app::{
    ActivityEntry, DiffData, EditorData, HistoryData, IntegrityInfo, PanicPrompt, Refresh, SaveLensApp, View,
};
pub use graph::{layout, BranchHead, GraphEdge, GraphLayout, GraphNode, Segment};
pub use scanner::{AlignChoice, CheatTableModel, FilterKind, ScannerModel};
pub use toast::{Toast, ToastKind, Toasts};
pub use treediff::{NodeStatus, Side, TreeDiffModel};

impl eframe::App for SaveLensApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

/// Opens the SaveLens window (1400×900) and runs until it is closed.
pub fn run(engine: savelens_watcher::Engine) -> anyhow::Result<()> {
    let app = SaveLensApp::new(engine)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SaveLens")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([640.0, 420.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "SaveLens",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("cannot start the SaveLens window: {e}"))
}
