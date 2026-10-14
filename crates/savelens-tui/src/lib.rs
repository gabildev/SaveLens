//! # savelens-tui
//!
//! `ratatui` + `crossterm` terminal front-end of SaveLens:
//!
//! 1. **Games & Watcher** — tracked and auto-discovered games, the embedded
//!    watcher daemon, storage statistics and the activity log.
//! 2. **Timeline & Branches** — the semantic log (`+500 Gold, +1 Level`),
//!    per-file changelogs, checkout, branches, tags and semantic diffs.
//! 3. **Save-State Scanner** — Cheat-Engine-style multi-step scans over save
//!    snapshots, pinning, on-disk edits and the freeze-on-save cheat table.
//! 4. **AST Explorer** — collapsible semantic tree with integrity badges,
//!    fuzzy search, leaf editing and surgical cherry-pick.
//!
//! [`run`] owns the terminal; [`App`] is the testable state machine
//! (`handle_key`, `tick`, `draw` against any `ratatui` backend).

mod app;
mod jobs;
mod questions;
mod tabs;
mod theme;
mod tree;
mod widgets;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use savelens_watcher::Engine;

pub use app::{App, Tab};

/// How long the event loop waits for input before ticking.
const POLL: Duration = Duration::from_millis(80);

/// Puts the terminal back into cooked mode (idempotent, best effort).
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Restores the terminal when dropped (normal exit, `?` errors and unwinding).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Installs a panic hook that restores the terminal before the default hook
/// prints the panic. Panics on background job threads are caught and reported
/// in the UI, so the terminal is left alone for them.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let on_worker = std::thread::current().name().is_some_and(|n| n.starts_with(jobs::WORKER_THREAD_PREFIX));
        if on_worker {
            return;
        }
        restore_terminal();
        previous(info);
    }));
}

/// Runs the TUI until the user quits.
pub fn run(engine: Engine) -> Result<()> {
    let mut app = App::new(engine)?;
    install_panic_hook();
    enable_raw_mode().context("enabling raw mode")?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen).context("entering the alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout())).context("initialising the terminal")?;
    terminal.clear().context("clearing the terminal")?;
    let result = event_loop(&mut terminal, &mut app);
    app.shutdown();
    let _ = terminal.show_cursor();
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| app.draw(f)).context("drawing")?;
        if event::poll(POLL).context("polling terminal events")? {
            match event::read().context("reading terminal events")? {
                Event::Key(key) => {
                    if app.handle_key(key)? {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => terminal.autoresize().context("resizing")?,
                _ => {}
            }
        }
        app.tick();
    }
}
