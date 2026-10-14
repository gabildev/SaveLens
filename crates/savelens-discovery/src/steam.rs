//! Steam installation roots, library folders, app manifests and Proton prefixes.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::normalize_path;
use crate::vdf::{self, VdfNode};
use crate::{DiscoveryConfig, HostPlatform};

/// App ids of Steam tools that are never games (runtimes, Proton builds, redistributables).
const TOOL_APP_IDS: &[u32] = &[
    228980,  // Steamworks Common Redistributables
    1070560, // Steam Linux Runtime 1.0 (scout)
    1391110, // Steam Linux Runtime 2.0 (soldier)
    1628350, // Steam Linux Runtime 3.0 (sniper)
    1493710, // Proton Experimental
    1826330, // Proton EasyAntiCheat Runtime
    1161040, // Proton BattlEye Runtime
    2180100, // Proton Hotfix
    1887720, // Proton 7.0
    2348590, // Proton 8.0
    2805730, // Proton 9.0
    1580130, // Proton 6.3
    1420170, // Proton 5.13
    1245040, // Proton 5.0
    1113280, // Proton 4.11
    1054830, // Proton 4.2
    996510,  // Proton 3.16 Beta
    961940,  // Proton 3.16
    930400,  // Proton 3.7 Beta
    858280,  // Proton 3.7
];

const TOOL_NAME_PREFIXES: &[&str] = &[
    "Steam Linux Runtime",
    "Steamworks Common Redistributables",
    "Steamworks SDK Redist",
];

/// True for Steam tools (Proton, runtimes, redistributables) that must not be listed as games.
pub fn is_steam_tool(app_id: u32, name: &str) -> bool {
    if TOOL_APP_IDS.contains(&app_id) {
        return true;
    }
    if TOOL_NAME_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    name == "Proton"
        || name
            .strip_prefix("Proton")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c == ' ' || c == '-' || c.is_ascii_digit())
}

/// Auto-detects Steam installation roots (directories containing `steamapps/`), deduplicated
/// through canonicalization.
pub fn detect_steam_roots(cfg: &DiscoveryConfig) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    match cfg.host {
        HostPlatform::Linux => {
            let h = &cfg.home;
            candidates.push(h.join(".steam").join("steam"));
            candidates.push(h.join(".steam").join("root"));
            candidates.push(cfg.xdg_data.join("Steam"));
            candidates.push(h.join(".local").join("share").join("Steam"));
            let flatpak = h.join(".var").join("app").join("com.valvesoftware.Steam");
            candidates.push(flatpak.join(".local").join("share").join("Steam"));
            candidates.push(flatpak.join("data").join("Steam"));
            candidates.push(
                h.join("snap")
                    .join("steam")
                    .join("common")
                    .join(".local")
                    .join("share")
                    .join("Steam"),
            );
        }
        HostPlatform::Windows => {
            candidates.extend(registry_steam_roots());
            if let Some(pf) = std::env::var_os("ProgramFiles(x86)") {
                candidates.push(PathBuf::from(pf).join("Steam"));
            }
            candidates.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        }
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for c in candidates {
        if !c.join("steamapps").is_dir() {
            continue;
        }
        let n = normalize_path(&c);
        if seen.insert(n.clone()) {
            out.push(n);
        }
    }
    out
}

#[cfg(windows)]
fn registry_steam_roots() -> Vec<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;
    let mut out = Vec::new();
    let lookups: [(RegKey, &str, &str); 3] = [
        (
            RegKey::predef(HKEY_CURRENT_USER),
            r"Software\Valve\Steam",
            "SteamPath",
        ),
        (
            RegKey::predef(HKEY_LOCAL_MACHINE),
            r"SOFTWARE\WOW6432Node\Valve\Steam",
            "InstallPath",
        ),
        (
            RegKey::predef(HKEY_LOCAL_MACHINE),
            r"SOFTWARE\Valve\Steam",
            "InstallPath",
        ),
    ];
    for (hive, key, value) in lookups {
        match hive
            .open_subkey(key)
            .and_then(|k| k.get_value::<String, _>(value))
        {
            Ok(path) if !path.is_empty() => out.push(PathBuf::from(path.replace('/', "\\"))),
            Ok(_) => {}
            Err(e) => tracing::debug!(key, value, error = %e, "Steam registry value not available"),
        }
    }
    out
}

#[cfg(not(windows))]
fn registry_steam_roots() -> Vec<PathBuf> {
    Vec::new()
}

/// A library folder with the metadata read from `libraryfolders.vdf`.
#[derive(Debug, Clone)]
pub(crate) struct LibraryEntry {
    pub path: PathBuf,
    pub label: Option<String>,
    pub listed_apps: Vec<u32>,
    /// Steam root whose `libraryfolders.vdf` lists this library.
    pub root: PathBuf,
}

/// Reads the library folders of every root (both `steamapps/` and `config/`
/// `libraryfolders.vdf`, new and old format). The root itself is always a library.
pub(crate) fn collect_libraries(
    roots: &[PathBuf],
    warnings: &mut Vec<String>,
) -> Vec<LibraryEntry> {
    let mut libs: Vec<LibraryEntry> = Vec::new();
    let add = |libs: &mut Vec<LibraryEntry>, entry: LibraryEntry, warnings: &mut Vec<String>| {
        if !entry.path.join("steamapps").is_dir() {
            warnings.push(format!(
                "Steam library {} has no steamapps directory",
                entry.path.display()
            ));
            return;
        }
        let path = normalize_path(&entry.path);
        if let Some(existing) = libs.iter_mut().find(|l| l.path == path) {
            if existing.label.is_none() {
                existing.label = entry.label;
            }
            for id in entry.listed_apps {
                if !existing.listed_apps.contains(&id) {
                    existing.listed_apps.push(id);
                }
            }
        } else {
            libs.push(LibraryEntry { path, ..entry });
        }
    };
    for root in roots {
        let mut entries = vec![LibraryEntry {
            path: root.clone(),
            label: None,
            listed_apps: Vec::new(),
            root: root.clone(),
        }];
        for file in [
            root.join("steamapps").join("libraryfolders.vdf"),
            root.join("config").join("libraryfolders.vdf"),
        ] {
            let text = match fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    warnings.push(format!("cannot read {}: {e}", file.display()));
                    continue;
                }
            };
            match parse_library_folders(&text) {
                Ok(found) => entries.extend(found.into_iter().map(|mut e| {
                    e.root = root.clone();
                    e
                })),
                Err(e) => warnings.push(format!("{}: {e}", file.display())),
            }
        }
        for e in entries {
            add(&mut libs, e, warnings);
        }
    }
    libs
}

/// Parses `libraryfolders.vdf` (new nested format and the old `"1" "D:\\Games"` format).
pub(crate) fn parse_library_folders(
    text: &str,
) -> Result<Vec<LibraryEntry>, crate::DiscoveryError> {
    let root = vdf::parse(text)?;
    let Some(VdfNode::Map(entries)) = root.get("libraryfolders") else {
        return Err(crate::DiscoveryError::Vdf {
            line: 1,
            message: "missing `libraryfolders` block".to_string(),
        });
    };
    let mut out = Vec::new();
    for (key, node) in entries {
        if key.parse::<u32>().is_err() {
            continue;
        }
        match node {
            VdfNode::Value(path) if !path.is_empty() => out.push(LibraryEntry {
                path: PathBuf::from(path),
                label: None,
                listed_apps: Vec::new(),
                root: PathBuf::new(),
            }),
            VdfNode::Value(_) => {}
            VdfNode::Map(_) => {
                let Some(path) = node.get_str("path").filter(|p| !p.is_empty()) else {
                    continue;
                };
                let label = node
                    .get_str("label")
                    .filter(|l| !l.is_empty())
                    .map(str::to_string);
                let listed_apps = node
                    .get("apps")
                    .and_then(VdfNode::as_map)
                    .map(|apps| apps.iter().filter_map(|(k, _)| k.parse().ok()).collect())
                    .unwrap_or_default();
                out.push(LibraryEntry {
                    path: PathBuf::from(path),
                    label,
                    listed_apps,
                    root: PathBuf::new(),
                });
            }
        }
    }
    Ok(out)
}

/// One `appmanifest_<id>.acf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    pub app_id: u32,
    pub name: String,
    /// `installdir` value (directory name below `steamapps/common`).
    pub install_dir_name: String,
    /// Library containing the manifest.
    pub library: PathBuf,
    /// Steam root owning that library (used for `<root>`).
    pub root: PathBuf,
}

impl AppManifest {
    /// `<library>/steamapps/common/<installdir>`.
    pub fn install_path(&self) -> PathBuf {
        self.library
            .join("steamapps")
            .join("common")
            .join(&self.install_dir_name)
    }

    /// Parses the text of an ACF manifest.
    pub fn parse(text: &str, library: &Path, root: &Path) -> Result<Self, crate::DiscoveryError> {
        let doc = vdf::parse(text)?;
        let missing = |what: &str| crate::DiscoveryError::Vdf {
            line: 1,
            message: format!("AppState is missing `{what}`"),
        };
        let state = doc.get("AppState").ok_or_else(|| missing("AppState"))?;
        let app_id = state
            .get_str("appid")
            .ok_or_else(|| missing("appid"))?
            .trim()
            .parse::<u32>()
            .map_err(|e| crate::DiscoveryError::Vdf {
                line: 1,
                message: format!("invalid appid: {e}"),
            })?;
        let install_dir_name = state
            .get_str("installdir")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| missing("installdir"))?
            .to_string();
        let name = state
            .get_str("name")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| install_dir_name.clone());
        Ok(AppManifest {
            app_id,
            name,
            install_dir_name,
            library: library.to_path_buf(),
            root: root.to_path_buf(),
        })
    }
}

/// Reads every `appmanifest_*.acf` of a library (sorted by app id). `root` is the Steam
/// installation that lists the library.
pub(crate) fn read_manifests(
    library: &Path,
    root: &Path,
    warnings: &mut Vec<String>,
) -> Vec<AppManifest> {
    let steamapps = library.join("steamapps");
    let rd = match fs::read_dir(&steamapps) {
        Ok(rd) => rd,
        Err(e) => {
            warnings.push(format!("cannot list {}: {e}", steamapps.display()));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in rd.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if !(lower.starts_with("appmanifest_") && lower.ends_with(".acf")) {
            continue;
        }
        let path = entry.path();
        let text = match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                warnings.push(format!("cannot read {}: {e}", path.display()));
                continue;
            }
        };
        match AppManifest::parse(&text, library, root) {
            Ok(m) => out.push(m),
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    out.sort_by_key(|m| m.app_id);
    out
}

/// `(install_dir, app_id)` of every installed game reachable from the configured Steam roots
/// (no save-location scanning; used to map running processes to app ids).
pub(crate) fn installed_apps(cfg: &DiscoveryConfig) -> Vec<(PathBuf, u32)> {
    let roots = match &cfg.steam_roots {
        Some(r) => r
            .iter()
            .filter(|p| p.is_dir())
            .map(|p| normalize_path(p))
            .collect(),
        None => detect_steam_roots(cfg),
    };
    let mut ignored = Vec::new();
    let mut out = Vec::new();
    for lib in collect_libraries(&roots, &mut ignored) {
        for m in read_manifests(&lib.path, &lib.root, &mut ignored) {
            if !is_steam_tool(m.app_id, &m.name) {
                out.push((m.install_path(), m.app_id));
            }
        }
    }
    out
}

/// `(app_id, <lib>/steamapps/compatdata/<id>/pfx)` for every existing Proton prefix.
pub(crate) fn compat_prefixes(library: &Path) -> Vec<(u32, PathBuf)> {
    let compat = library.join("steamapps").join("compatdata");
    let Ok(rd) = fs::read_dir(&compat) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let id = e.file_name().to_str()?.parse::<u32>().ok()?;
            let pfx = e.path().join("pfx");
            pfx.is_dir().then(|| (id, normalize_path(&pfx)))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_detection() {
        assert!(is_steam_tool(1493710, "Proton Experimental"));
        assert!(is_steam_tool(1, "Proton 9.0"));
        assert!(is_steam_tool(2, "Proton-GE"));
        assert!(is_steam_tool(3, "Steam Linux Runtime 3.0 (sniper)"));
        assert!(is_steam_tool(228980, "anything"));
        assert!(!is_steam_tool(413150, "Stardew Valley"));
        assert!(!is_steam_tool(4, "Protonaut"));
    }

    #[test]
    fn library_formats() {
        let new = "\"libraryfolders\" { \"0\" { \"path\" \"/a\" \"label\" \"\" \"apps\" { \"10\" \"1\" \"20\" \"2\" } } \"1\" { \"path\" \"/b\" \"label\" \"B\" } }";
        let libs = parse_library_folders(new).unwrap();
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].listed_apps, vec![10, 20]);
        assert_eq!(libs[0].label, None);
        assert_eq!(libs[1].label.as_deref(), Some("B"));
        let old = "\"LibraryFolders\" { \"TimeNextStatsReport\" \"1\" \"ContentStatsID\" \"2\" \"1\" \"D:\\\\Games\" }";
        let libs = parse_library_folders(old).unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].path, PathBuf::from("D:\\Games"));
        assert!(parse_library_folders("\"other\" {}").is_err());
    }

    #[test]
    fn manifest_parse_and_errors() {
        let m = AppManifest::parse(
            "\"AppState\" { \"appid\" \"42\" \"name\" \"G\" \"installdir\" \"GDir\" }",
            Path::new("/lib"),
            Path::new("/root"),
        )
        .unwrap();
        assert_eq!(m.install_path(), Path::new("/lib/steamapps/common/GDir"));
        assert!(AppManifest::parse(
            "\"AppState\" { \"appid\" \"x\" \"installdir\" \"a\" }",
            Path::new("/"),
            Path::new("/")
        )
        .is_err());
        assert!(AppManifest::parse(
            "\"AppState\" { \"appid\" \"1\" }",
            Path::new("/"),
            Path::new("/")
        )
        .is_err());
    }
}
