//! Process inspection: who holds a file open for writing, and which processes are games.
//!
//! * Linux: `/proc/<pid>/fd/*` symlinks + `/proc/<pid>/fdinfo/<fd>` `flags:` (octal access
//!   mode), `/proc/<pid>/{exe,cmdline,environ,comm}`.
//! * Windows: exclusive share-mode open for lock checks, `sysinfo` for process enumeration.
//! * Other Unix systems: `sysinfo` for process enumeration; open-file inspection is not
//!   available there, so [`open_writers`] returns an empty list.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{DiscoveryConfig, DiscoveryReport};

/// A running process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
}

/// A process recognised as a running game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GameProcess {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// Steam app id, from the environment or from the matching install directory.
    pub app_id: Option<u32>,
    /// Why the process was classified as a game.
    pub reason: String,
}

/// Processes holding `path` open for writing (`O_WRONLY` or `O_RDWR`). The current process is
/// included when it holds such a descriptor; callers compare pids themselves.
///
/// On Windows and non-Linux Unix systems the owning processes cannot be enumerated without
/// elevated APIs, so the list is always empty there; use [`is_locked_for_write`] instead.
pub fn open_writers(path: &Path) -> Vec<ProcessInfo> {
    imp::open_writers(path)
}

/// Is `path` currently open for writing by some process?
///
/// Linux: [`open_writers`] is non-empty. Windows: opening the file with an exclusive share
/// mode fails with `ERROR_SHARING_VIOLATION`. Other systems: always `false`.
pub fn is_locked_for_write(path: &Path) -> bool {
    imp::is_locked_for_write(path)
}

/// Files a process currently holds open for writing (Linux only; empty elsewhere).
pub fn files_open_for_write(pid: u32) -> Vec<PathBuf> {
    imp::files_open_for_write(pid)
}

/// Basic information about one process, if it exists and is readable.
pub fn process_info(pid: u32) -> Option<ProcessInfo> {
    imp::process_info(pid)
}

/// Running processes that look like games: executables below a `steamapps/common` directory or
/// a discovered install dir, Wine/Proton processes running a `.exe`, or processes carrying
/// `SteamAppId`/`SteamGameId` in their environment. Steam's own helpers and Wine
/// infrastructure processes are excluded, as is the current process.
pub fn game_processes(cfg: &DiscoveryConfig, report: Option<&DiscoveryReport>) -> Vec<GameProcess> {
    let installs: Vec<(PathBuf, u32)> = match report {
        Some(r) => r
            .games
            .iter()
            .filter_map(|g| Some((g.install_dir.clone()?, g.app_id?)))
            .collect(),
        None => crate::steam::installed_apps(cfg),
    };
    let own = std::process::id();
    let mut out: Vec<GameProcess> = imp::snapshots()
        .into_iter()
        .filter(|s| s.pid != own)
        .filter_map(|s| classify(&s, &installs))
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

/// Raw facts about one process used for classification.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcSnapshot {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub cmdline: Vec<String>,
    pub environ: Vec<String>,
}

/// Steam client helpers and Wine/Proton infrastructure that are never the game itself.
const INFRA_NAMES: &[&str] = &[
    "steam",
    "steamwebhelper",
    "steam-runtime-launcher-service",
    "steam-runtime-supervisor",
    "steam-launch-wrapper",
    "reaper",
    "srt-bwrap",
    "pv-bwrap",
    "pressure-vessel-wrap",
    "pressure-vessel-adverb",
    "wineserver",
    "explorer.exe",
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "svchost.exe",
    "rpcss.exe",
    "tabtip.exe",
    "conhost.exe",
    "start.exe",
    "rundll32.exe",
    "steam.exe",
    "lsteamclient",
    "steamerrorreporter",
    "steamerrorreporter.exe",
    "crashpad_handler",
    "gameoverlayui",
    "gameoverlayui.exe",
];

const WINE_LOADERS: &[&str] = &["wine", "wine64", "wine-preloader", "wine64-preloader"];

fn is_infra(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    INFRA_NAMES.iter().any(|n| *n == lower)
        || lower.starts_with("steam-runtime")
        || lower.starts_with("pressure-vessel")
}

/// True when `s` points into a game directory below `steamapps/common` (Proton builds and
/// Steam runtimes living there are not games).
fn path_has_steamapps_common(s: &str) -> bool {
    let lower = s.to_ascii_lowercase().replace('\\', "/");
    let Some(idx) = lower.find("/steamapps/common/") else {
        return false;
    };
    let rest = &lower[idx + "/steamapps/common/".len()..];
    let dir = rest.split('/').next().unwrap_or("");
    !(dir.is_empty()
        || dir.starts_with("proton")
        || dir.starts_with("steamlinuxruntime")
        || dir.starts_with("steam linux runtime")
        || dir == "steamworks shared")
}

fn file_name_of(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}

fn env_app_id(environ: &[String]) -> Option<u32> {
    ["SteamAppId=", "SteamGameId=", "STEAM_COMPAT_APP_ID="]
        .iter()
        .find_map(|k| {
            environ
                .iter()
                .find_map(|e| e.strip_prefix(k))
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|id| *id != 0)
        })
}

fn app_id_for_path(path: &str, installs: &[(PathBuf, u32)]) -> Option<u32> {
    let mut best: Option<(usize, u32)> = None;
    for (dir, id) in installs {
        let id = *id;
        let dir_s = dir
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        let p = path.replace('\\', "/").to_ascii_lowercase();
        if p.starts_with(&format!("{}/", dir_s.trim_end_matches('/')))
            && !matches!(best, Some((l, _)) if l >= dir_s.len())
        {
            best = Some((dir_s.len(), id));
        }
    }
    best.map(|(_, id)| id)
}

pub(crate) fn classify(s: &ProcSnapshot, installs: &[(PathBuf, u32)]) -> Option<GameProcess> {
    let exe_str = s.exe.as_ref().map(|e| e.to_string_lossy().into_owned());
    let exe_name = exe_str.as_deref().map(file_name_of).unwrap_or("");
    let arg0_name = s.cmdline.first().map(|a| file_name_of(a)).unwrap_or("");
    if is_infra(&s.name) || is_infra(exe_name) || is_infra(arg0_name) {
        return None;
    }
    let env_id = env_app_id(&s.environ);
    let exe_args: Vec<&String> = s
        .cmdline
        .iter()
        .filter(|a| a.to_ascii_lowercase().ends_with(".exe"))
        .collect();

    let mut reasons = Vec::new();
    let mut app_id = None;
    let mut install_hit = false;
    for candidate in s.cmdline.iter().chain(exe_str.iter()) {
        if let Some(id) = app_id_for_path(candidate, installs) {
            app_id = Some(id);
            install_hit = true;
            reasons.push(format!("runs from the install dir of app {id}"));
            break;
        }
    }
    if !install_hit {
        if let Some(c) = s
            .cmdline
            .iter()
            .chain(exe_str.iter())
            .find(|c| path_has_steamapps_common(c))
        {
            reasons.push(format!(
                "executable under steamapps/common ({})",
                file_name_of(c)
            ));
        }
    }
    let wine_loader = WINE_LOADERS.contains(&exe_name) || WINE_LOADERS.contains(&arg0_name);
    if let Some(first_exe) = exe_args.first() {
        let runs_exe = arg0_name.to_ascii_lowercase().ends_with(".exe")
            || s.name.to_ascii_lowercase().ends_with(".exe");
        if wine_loader || runs_exe {
            reasons.push(format!(
                "Wine/Proton process running {}",
                file_name_of(first_exe)
            ));
        }
    } else if wine_loader && env_id.is_some() {
        reasons.push("Wine/Proton loader".to_string());
    }
    if let Some(id) = env_id {
        // The environment marker alone also matches Steam's launch wrappers (shells, python
        // scripts); require it to be combined with an executable-looking command line.
        let looks_like_game = !reasons.is_empty()
            || !exe_args.is_empty()
            || !matches!(
                exe_name,
                "sh" | "bash" | "dash" | "python3" | "python" | "env" | "sleep"
            );
        if looks_like_game {
            reasons.push(format!("SteamAppId={id} in environment"));
        }
        app_id = app_id.or(Some(id));
    }
    if reasons.is_empty() {
        return None;
    }
    let name = exe_args
        .first()
        .map(|a| file_name_of(a).to_string())
        .unwrap_or_else(|| s.name.clone());
    Some(GameProcess {
        pid: s.pid,
        name,
        exe: s.exe.clone(),
        app_id,
        reason: reasons.join("; "),
    })
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{ProcSnapshot, ProcessInfo};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn pids() -> Vec<u32> {
        let Ok(rd) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut v: Vec<u32> = rd
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        v.sort_unstable();
        v
    }

    fn comm(pid: u32) -> Option<String> {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim_end().to_string())
    }

    fn exe(pid: u32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    fn nul_list(path: String) -> Vec<String> {
        fs::read(path)
            .map(|b| {
                b.split(|c| *c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Access mode bits of an fd from `/proc/<pid>/fdinfo/<fd>` (`flags:` is octal).
    fn fd_is_writable(pid: u32, fd: &str) -> bool {
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else {
            return false;
        };
        text.lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
            .is_some_and(|flags| {
                let mode = flags & libc::O_ACCMODE as u32;
                mode == libc::O_WRONLY as u32 || mode == libc::O_RDWR as u32
            })
    }

    /// `(fd, target)` pairs of a process (unreadable processes yield nothing).
    fn fds(pid: u32) -> Vec<(String, PathBuf)> {
        let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            return Vec::new();
        };
        rd.filter_map(Result::ok)
            .filter_map(|e| {
                let fd = e.file_name().to_str()?.to_string();
                let target = fs::read_link(e.path()).ok()?;
                Some((fd, target))
            })
            .collect()
    }

    pub fn open_writers(path: &Path) -> Vec<ProcessInfo> {
        let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut out = Vec::new();
        for pid in pids() {
            let holds = fds(pid)
                .into_iter()
                .any(|(fd, t)| t == target && fd_is_writable(pid, &fd));
            if holds {
                out.push(ProcessInfo {
                    pid,
                    name: comm(pid).unwrap_or_default(),
                    exe: exe(pid),
                });
            }
        }
        out
    }

    pub fn is_locked_for_write(path: &Path) -> bool {
        !open_writers(path).is_empty()
    }

    pub fn files_open_for_write(pid: u32) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fds(pid)
            .into_iter()
            .filter(|(_, t)| t.is_absolute())
            .filter(|(_, t)| {
                !(t.starts_with("/dev") || t.starts_with("/proc") || t.starts_with("/sys"))
            })
            .filter(|(fd, _)| fd_is_writable(pid, fd))
            .map(|(_, t)| t)
            .filter(|t| t.is_file())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn process_info(pid: u32) -> Option<ProcessInfo> {
        let name = comm(pid)?;
        Some(ProcessInfo {
            pid,
            name,
            exe: exe(pid),
        })
    }

    pub fn snapshots() -> Vec<ProcSnapshot> {
        pids()
            .into_iter()
            .filter_map(|pid| {
                let name = comm(pid)?;
                Some(ProcSnapshot {
                    pid,
                    name,
                    exe: exe(pid),
                    cmdline: nul_list(format!("/proc/{pid}/cmdline")),
                    environ: nul_list(format!("/proc/{pid}/environ")),
                })
            })
            .collect()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{ProcSnapshot, ProcessInfo};
    use std::path::{Path, PathBuf};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    fn system(pids: ProcessesToUpdate<'_>) -> System {
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            pids,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always)
                .with_environ(UpdateKind::Always),
        );
        sys
    }

    pub fn snapshots() -> Vec<ProcSnapshot> {
        let sys = system(ProcessesToUpdate::All);
        sys.processes()
            .values()
            .map(|p| ProcSnapshot {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                exe: p.exe().map(Path::to_path_buf),
                cmdline: p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect(),
                environ: p
                    .environ()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect(),
            })
            .collect()
    }

    pub fn process_info(pid: u32) -> Option<ProcessInfo> {
        let p = Pid::from_u32(pid);
        let sys = system(ProcessesToUpdate::Some(&[p]));
        sys.process(p).map(|proc_| ProcessInfo {
            pid,
            name: proc_.name().to_string_lossy().into_owned(),
            exe: proc_.exe().map(Path::to_path_buf),
        })
    }

    pub fn open_writers(_path: &Path) -> Vec<ProcessInfo> {
        Vec::new()
    }

    pub fn files_open_for_write(_pid: u32) -> Vec<PathBuf> {
        Vec::new()
    }

    #[cfg(windows)]
    pub fn is_locked_for_write(path: &Path) -> bool {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
        match std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
        {
            Ok(_) => false,
            Err(e) => e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32),
        }
    }

    #[cfg(not(windows))]
    pub fn is_locked_for_write(_path: &Path) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, exe: &str, cmd: &[&str], env: &[&str]) -> ProcSnapshot {
        ProcSnapshot {
            pid: 42,
            name: name.to_string(),
            exe: Some(PathBuf::from(exe)),
            cmdline: cmd.iter().map(|s| s.to_string()).collect(),
            environ: env.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn classification() {
        let native = snap(
            "Game",
            "/lib/steamapps/common/G/Game.x86_64",
            &["/lib/steamapps/common/G/Game.x86_64"],
            &[],
        );
        let g = classify(&native, &[]).unwrap();
        assert!(g.reason.contains("steamapps/common"));
        assert_eq!(g.app_id, None);

        let proton = snap(
            "Game.exe",
            "/home/u/.steam/steam/steamapps/common/Proton 9.0/files/bin/wine64-preloader",
            &["Z:\\lib\\steamapps\\common\\G\\Game.exe"],
            &["SteamAppId=413150", "HOME=/home/u"],
        );
        let g = classify(&proton, &[]).unwrap();
        assert_eq!(g.app_id, Some(413150));
        assert_eq!(g.name, "Game.exe");
        assert!(g.reason.contains("SteamAppId=413150"));
        assert!(
            g.reason.contains("steamapps/common (Game.exe)"),
            "{}",
            g.reason
        );
        assert!(!path_has_steamapps_common(
            "/s/steamapps/common/Proton 9.0/files/bin/wine"
        ));
        assert!(!path_has_steamapps_common(
            "/s/steamapps/common/SteamLinuxRuntime_sniper/x"
        ));

        assert!(classify(
            &snap(
                "steam",
                "/usr/lib/steam/steam",
                &["steam"],
                &["SteamAppId=1"]
            ),
            &[]
        )
        .is_none());
        assert!(classify(
            &snap(
                "wineserver",
                "/x/wineserver",
                &["wineserver"],
                &["SteamAppId=5"]
            ),
            &[]
        )
        .is_none());
        assert!(classify(&snap("bash", "/usr/bin/bash", &["bash"], &[]), &[]).is_none());
        assert!(classify(
            &snap(
                "bash",
                "/usr/bin/bash",
                &["bash", "-c", "x"],
                &["SteamAppId=7"]
            ),
            &[]
        )
        .is_none());
        assert!(classify(
            &snap("vim", "/usr/bin/vim", &["vim", "notes.exe"], &[]),
            &[]
        )
        .is_none());
        let installs = vec![(PathBuf::from("/games/steamapps/common/G"), 99u32)];
        let mapped = classify(
            &snap("G", "/games/steamapps/common/G/bin/g", &["g"], &[]),
            &installs,
        )
        .unwrap();
        assert_eq!(mapped.app_id, Some(99));
        assert!(mapped.reason.contains("app 99"));
    }
}
