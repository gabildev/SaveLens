//! Global hotkeys (`global-hotkey`): X11 on Linux, `RegisterHotKey` on Windows.
//!
//! The manager lives on a dedicated thread (Windows requires the registering
//! thread to pump its message queue); actions are delivered on a channel.

use std::str::FromStr;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde::{Deserialize, Serialize};

use crate::config::HotkeyConfig;
use crate::error::{EngineError, EngineResult};

/// What a hotkey does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HotkeyAction {
    /// Ctrl+Shift+F5 by default.
    QuickSnapshot,
    /// Ctrl+Shift+F9 by default.
    QuickRollback,
    /// Ctrl+Shift+F12 by default.
    PanicRestore,
    /// Ctrl+Shift+F10 by default.
    ToggleTrainer,
    /// Ctrl+Shift+F11 by default.
    ToggleOverlay,
}

impl HotkeyAction {
    pub fn label(self) -> &'static str {
        match self {
            HotkeyAction::QuickSnapshot => "Quick snapshot",
            HotkeyAction::QuickRollback => "Quick rollback",
            HotkeyAction::PanicRestore => "Panic restore",
            HotkeyAction::ToggleTrainer => "Toggle disk trainer",
            HotkeyAction::ToggleOverlay => "Toggle overlay",
        }
    }
}

/// Parses the configured bindings.
pub fn parse_bindings(cfg: &HotkeyConfig) -> EngineResult<Vec<(HotkeyAction, HotKey, String)>> {
    let entries = [
        (HotkeyAction::QuickSnapshot, &cfg.quick_snapshot),
        (HotkeyAction::QuickRollback, &cfg.quick_rollback),
        (HotkeyAction::PanicRestore, &cfg.panic_restore),
        (HotkeyAction::ToggleTrainer, &cfg.toggle_trainer),
        (HotkeyAction::ToggleOverlay, &cfg.toggle_overlay),
    ];
    let mut out = Vec::new();
    for (action, text) in entries {
        if text.trim().is_empty() {
            continue;
        }
        let hk =
            HotKey::from_str(text).map_err(|e| EngineError::Hotkeys(format!("{}: `{text}`: {e}", action.label())))?;
        if out.iter().any(|(_, h, _): &(HotkeyAction, HotKey, String)| h.id() == hk.id()) {
            return Err(EngineError::Hotkeys(format!("`{text}` is bound twice")));
        }
        out.push((action, hk, text.clone()));
    }
    Ok(out)
}

/// A running hotkey listener.
pub struct HotkeyService {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
    actions: Receiver<HotkeyAction>,
    bindings: Vec<(HotkeyAction, String)>,
}

impl HotkeyService {
    /// Registers every binding; fails when no hotkey backend is available
    /// (e.g. no X11 display) or a key is already taken.
    pub fn start(cfg: &HotkeyConfig) -> EngineResult<HotkeyService> {
        let bindings = parse_bindings(cfg)?;
        display_available()?;
        let (stop_tx, stop_rx) = bounded::<()>(1);
        let (act_tx, act_rx) = unbounded::<HotkeyAction>();
        let (ready_tx, ready_rx) = bounded::<Result<(), String>>(1);
        let thread_bindings = bindings.clone();
        let thread = std::thread::Builder::new()
            .name("savelens-hotkeys".into())
            .spawn(move || {
                let manager = match GlobalHotKeyManager::new() {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                for (action, hk, text) in &thread_bindings {
                    if let Err(e) = manager.register(*hk) {
                        let _ = ready_tx.send(Err(format!("{} ({text}): {e}", action.label())));
                        return;
                    }
                }
                let _ = ready_tx.send(Ok(()));
                let receiver = GlobalHotKeyEvent::receiver();
                loop {
                    #[cfg(windows)]
                    pump_windows_messages();
                    while let Ok(ev) = receiver.try_recv() {
                        if ev.state() != HotKeyState::Pressed {
                            continue;
                        }
                        if let Some((action, _, _)) = thread_bindings.iter().find(|(_, hk, _)| hk.id() == ev.id()) {
                            let _ = act_tx.send(*action);
                        }
                    }
                    if stop_rx.recv_timeout(Duration::from_millis(15)).is_ok() {
                        break;
                    }
                }
                let keys: Vec<HotKey> = thread_bindings.iter().map(|(_, hk, _)| *hk).collect();
                let _ = manager.unregister_all(&keys);
            })
            .map_err(|e| EngineError::Hotkeys(format!("cannot spawn hotkey thread: {e}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(HotkeyService {
                stop: stop_tx,
                thread: Some(thread),
                actions: act_rx,
                bindings: bindings.into_iter().map(|(a, _, t)| (a, t)).collect(),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(EngineError::Hotkeys(e))
            }
            Err(_) => Err(EngineError::Hotkeys("hotkey backend did not start within 5 s".into())),
        }
    }

    /// Channel of triggered actions.
    pub fn actions(&self) -> &Receiver<HotkeyAction> {
        &self.actions
    }

    /// Registered bindings.
    pub fn bindings(&self) -> &[(HotkeyAction, String)] {
        &self.bindings
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The X11 backend of `global-hotkey` crashes the process when no display is
/// reachable, so refuse early with a clear error instead.
#[cfg(all(unix, not(target_os = "macos")))]
fn display_available() -> EngineResult<()> {
    match std::env::var_os("DISPLAY") {
        Some(d) if !d.is_empty() => Ok(()),
        _ => {
            Err(EngineError::Hotkeys("no X11 display (DISPLAY is not set); global hotkeys need X11 or XWayland".into()))
        }
    }
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn display_available() -> EngineResult<()> {
    Ok(())
}

#[cfg(windows)]
fn pump_windows_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    // SAFETY: standard Win32 message pump on the thread that registered the
    // hotkeys; `msg` is a plain-old-data out parameter.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn missing_display_is_an_error_not_a_crash() {
        if std::env::var_os("DISPLAY").is_none() {
            assert!(matches!(HotkeyService::start(&HotkeyConfig::default()), Err(EngineError::Hotkeys(_))));
        }
    }

    #[test]
    fn default_bindings_parse() {
        let b = parse_bindings(&HotkeyConfig::default()).unwrap();
        assert_eq!(b.len(), 5);
        assert_eq!(b[2].0, HotkeyAction::PanicRestore);
        assert_eq!(b[2].2, "ctrl+shift+F12");
    }

    #[test]
    fn invalid_and_duplicate_bindings() {
        let cfg = HotkeyConfig { quick_snapshot: "ctrl+shift+NotAKey".into(), ..HotkeyConfig::default() };
        assert!(parse_bindings(&cfg).is_err());
        let cfg = HotkeyConfig { quick_rollback: "ctrl+shift+F5".into(), ..HotkeyConfig::default() };
        assert!(parse_bindings(&cfg).is_err());
        let cfg = HotkeyConfig { toggle_overlay: String::new(), ..HotkeyConfig::default() };
        assert_eq!(parse_bindings(&cfg).unwrap().len(), 4);
    }
}
