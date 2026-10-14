//! Linux-only tests of `/proc` inspection and the live hunter.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use savelens_discovery::hunter::{hunt, HuntOptions};
use savelens_discovery::process::{files_open_for_write, is_locked_for_write, open_writers};
use savelens_discovery::{DiscoveryConfig, HostPlatform};

#[test]
fn open_writers_sees_write_handles_only() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("held.sav");
    let me = std::process::id();

    let mut f = fs::File::create(&path).unwrap();
    f.write_all(b"data").unwrap();
    let writers = open_writers(&path);
    assert!(writers.iter().any(|p| p.pid == me), "{writers:?}");
    let own = writers.iter().find(|p| p.pid == me).unwrap();
    assert!(!own.name.is_empty());
    assert!(is_locked_for_write(&path));
    let canonical = fs::canonicalize(&path).unwrap();
    assert!(files_open_for_write(me).contains(&canonical));
    drop(f);

    let r = fs::File::open(&path).unwrap();
    assert!(!open_writers(&path).iter().any(|p| p.pid == me));
    assert!(!files_open_for_write(me).contains(&canonical));
    drop(r);

    // Read+write handles count as writers too.
    let rw = fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    assert!(open_writers(&path).iter().any(|p| p.pid == me));
    drop(rw);
    assert!(!open_writers(&path).iter().any(|p| p.pid == me));
}

#[test]
fn hunt_detects_repeatedly_written_save() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    let root = base.join("watched");
    fs::create_dir_all(root.join("Saves")).unwrap();
    fs::create_dir_all(&home).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        let root = root.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let mut i = 0u32;
            while !stop.load(Ordering::Relaxed) {
                fs::write(root.join("Saves/slot1.sav"), format!("save {i}")).unwrap();
                fs::write(root.join("debug.log"), format!("log {i}")).unwrap();
                i += 1;
                thread::sleep(Duration::from_millis(120));
            }
        })
    };

    let mut cfg = DiscoveryConfig::for_home(home);
    cfg.host = HostPlatform::Linux;
    cfg.steam_roots = Some(Vec::new());
    let opts = HuntOptions {
        duration: Duration::from_millis(1500),
        poll_interval: Duration::from_millis(100),
        roots: vec![root.clone()],
        pid_filter: None,
    };
    let report = hunt(&cfg, None, &opts).unwrap();
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert_eq!(report.roots_watched, vec![root.clone()]);
    let pos = |name: &str| report.candidates.iter().position(|c| c.path.file_name().is_some_and(|n| n == name));
    let sav_idx = pos("slot1.sav").unwrap_or_else(|| panic!("no save candidate: {report:?}"));
    let log_idx = pos("debug.log").unwrap_or_else(|| panic!("no log candidate: {report:?}"));
    let sav = &report.candidates[sav_idx];
    assert!(sav.score > 0.0, "{sav:?}");
    assert!(sav.writes >= 1, "{sav:?}");
    assert_eq!(sav.path, root.join("Saves/slot1.sav"));
    assert!(sav_idx < log_idx, "{report:?}");
    assert!(sav.score > report.candidates[log_idx].score);
}

#[test]
fn hunt_with_pid_filter_reports_open_write_handles() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    // The file lives outside every watched root: only the fd scan can find it.
    let held = base.join("elsewhere/profile.dat");
    fs::create_dir_all(held.parent().unwrap()).unwrap();
    let mut f = fs::File::create(&held).unwrap();
    f.write_all(b"x").unwrap();

    let mut cfg = DiscoveryConfig::for_home(base.join("home"));
    cfg.host = HostPlatform::Linux;
    cfg.steam_roots = Some(Vec::new());
    let me = std::process::id();
    let opts = HuntOptions {
        duration: Duration::from_millis(400),
        poll_interval: Duration::from_millis(100),
        roots: Vec::new(),
        pid_filter: Some(me),
    };
    let report = hunt(&cfg, None, &opts).unwrap();
    drop(f);
    assert!(report.processes.iter().any(|p| p.pid == me));
    let c = report
        .candidates
        .iter()
        .find(|c| c.path == held)
        .unwrap_or_else(|| panic!("held file not reported: {report:?}"));
    assert_eq!(c.pid, Some(me));
    assert!(c.score > 0.0);
    assert!(c.reasons.iter().any(|r| r.contains("open for writing")));
}
