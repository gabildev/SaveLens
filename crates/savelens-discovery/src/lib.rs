//! Save-file discovery for SaveLens.
//!
//! * Steam installations (native, Flatpak, Snap, Windows registry) and their libraries,
//! * App manifests → installed games, Proton prefixes (`compatdata/<appid>/pfx`),
//! * an embedded Ludusavi-style save-path rule database ([`rules`]),
//! * engine heuristics (Unity, Unreal, Godot, Ren'Py, GameMaker, RPG Maker, Source),
//! * process inspection ([`process`]) and a live file-write hunter ([`hunter`]).
//!
//! Every path is overridable through [`DiscoveryConfig`] so the whole pipeline can be exercised
//! against a fake Steam installation inside a temporary directory.

mod engines;
mod error;
pub mod hunter;
mod paths;
pub mod process;
pub mod rules;
mod steam;
pub mod vdf;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use engines::{detect_engine, EngineInfo};
pub use error::DiscoveryError;
pub use paths::{collect_files, normalize_path, MAX_FILES_PER_LOCATION, MAX_FILE_SIZE};
pub use steam::{detect_steam_roots, is_steam_tool, AppManifest};

/// Operating-system conventions used to resolve save paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostPlatform {
    /// Linux (and other Unix systems, which follow the same XDG conventions).
    Linux,
    /// Microsoft Windows.
    Windows,
}

impl HostPlatform {
    /// The platform this binary was compiled for.
    pub fn current() -> Self {
        if cfg!(windows) {
            HostPlatform::Windows
        } else {
            HostPlatform::Linux
        }
    }
}

impl Default for HostPlatform {
    fn default() -> Self {
        HostPlatform::current()
    }
}

/// The Windows "known folders" used by save-path templates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsEnv {
    /// `%USERPROFILE%`.
    pub user_profile: PathBuf,
    /// `%APPDATA%` (Roaming).
    pub app_data: PathBuf,
    /// `%LOCALAPPDATA%`.
    pub local_app_data: PathBuf,
    /// `%USERPROFILE%\AppData\LocalLow`.
    pub local_app_data_low: PathBuf,
    /// The Documents folder(s). Several entries are alternatives (e.g. `Documents` and the
    /// legacy `My Documents` link inside Wine prefixes).
    pub documents: Vec<PathBuf>,
    /// `%USERPROFILE%\Saved Games`.
    pub saved_games: PathBuf,
    /// `%PUBLIC%`.
    pub public: PathBuf,
}

impl WindowsEnv {
    /// Standard layout below a user profile directory.
    pub fn under_profile(user_profile: &Path, public: &Path) -> Self {
        WindowsEnv {
            user_profile: user_profile.to_path_buf(),
            app_data: user_profile.join("AppData").join("Roaming"),
            local_app_data: user_profile.join("AppData").join("Local"),
            local_app_data_low: user_profile.join("AppData").join("LocalLow"),
            documents: vec![user_profile.join("Documents")],
            saved_games: user_profile.join("Saved Games"),
            public: public.to_path_buf(),
        }
    }

    /// Layout of the `steamuser` profile inside a Proton/Wine prefix (`.../pfx`).
    pub fn for_proton_prefix(pfx: &Path) -> Self {
        let users = pfx.join("drive_c").join("users");
        let profile = users.join("steamuser");
        let mut env = WindowsEnv::under_profile(&profile, &users.join("Public"));
        env.documents.push(profile.join("My Documents"));
        env
    }

    /// Reads `%USERPROFILE%`, `%APPDATA%`, `%LOCALAPPDATA%` and `%PUBLIC%`, falling back to the
    /// standard layout below `home` for anything missing.
    pub fn from_env(home: &Path) -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let profile = var("USERPROFILE").unwrap_or_else(|| home.to_path_buf());
        let public = var("PUBLIC")
            .unwrap_or_else(|| profile.parent().map(|p| p.join("Public")).unwrap_or_else(|| profile.join("Public")));
        let mut env = WindowsEnv::under_profile(&profile, &public);
        if let Some(v) = var("APPDATA") {
            env.app_data = v;
        }
        if let Some(v) = var("LOCALAPPDATA") {
            env.local_app_data = v;
        }
        if cfg!(windows) {
            if let Some(docs) = dirs::document_dir() {
                env.documents = vec![docs];
            }
        }
        env
    }
}

/// Where and how discovery looks for things. Every path can be overridden (tests do).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// The user's home directory.
    pub home: PathBuf,
    /// Explicit Steam installation roots; `None` auto-detects them.
    pub steam_roots: Option<Vec<PathBuf>>,
    /// Path conventions to apply.
    pub host: HostPlatform,
    /// Windows known folders (used when `host` is [`HostPlatform::Windows`]).
    pub windows_env: WindowsEnv,
    /// `$XDG_DATA_HOME` (default `~/.local/share`).
    pub xdg_data: PathBuf,
    /// `$XDG_CONFIG_HOME` (default `~/.config`).
    pub xdg_config: PathBuf,
    /// User rule TOML files merged after the builtin rule database.
    pub extra_rules: Vec<PathBuf>,
}

impl DiscoveryConfig {
    /// Configuration of the current user, read from the environment.
    pub fn from_env() -> Self {
        let home = dirs::home_dir()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let xdg = |name: &str, fallback: PathBuf| {
            std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or(fallback)
        };
        let xdg_data = xdg("XDG_DATA_HOME", home.join(".local").join("share"));
        let xdg_config = xdg("XDG_CONFIG_HOME", home.join(".config"));
        let host = HostPlatform::current();
        let windows_env = match host {
            HostPlatform::Windows => WindowsEnv::from_env(&home),
            HostPlatform::Linux => WindowsEnv::under_profile(&home, &home.join("Public")),
        };
        DiscoveryConfig { home, steam_roots: None, host, windows_env, xdg_data, xdg_config, extra_rules: Vec::new() }
    }

    /// A configuration rooted entirely below `home` (XDG dirs and the Windows profile are
    /// derived from it). Steam roots are auto-detected below that home.
    pub fn for_home(home: PathBuf) -> Self {
        let windows_env = WindowsEnv::under_profile(&home, &home.join("Public"));
        DiscoveryConfig {
            xdg_data: home.join(".local").join("share"),
            xdg_config: home.join(".config"),
            windows_env,
            steam_roots: None,
            host: HostPlatform::current(),
            extra_rules: Vec::new(),
            home,
        }
    }
}

/// A Steam library folder.
#[derive(Debug, Clone, Serialize)]
pub struct SteamLibrary {
    /// Library root (contains `steamapps/`).
    pub path: PathBuf,
    /// User-visible label from `libraryfolders.vdf`, if any.
    pub label: Option<String>,
    /// App ids listed in `libraryfolders.vdf` plus those with a manifest in the library.
    pub app_ids: Vec<u32>,
}

/// How a discovered game is executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum GameRuntime {
    /// Native binary for the host OS.
    Native,
    /// Windows build running through Proton with the given Wine prefix (`.../pfx`).
    Proton {
        /// The prefix directory (`<lib>/steamapps/compatdata/<appid>/pfx`).
        prefix: PathBuf,
    },
    /// Not determinable (e.g. game files missing).
    Unknown,
}

/// Game engine families recognised by the heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Engine {
    Unity,
    Unreal,
    Godot,
    RenPy,
    GameMaker,
    RpgMaker,
    Source,
    /// Recognised framework without dedicated heuristics (XNA/FNA/MonoGame).
    Other,
}

/// Why a save location was proposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum LocationSource {
    /// From the save-rule database.
    Rule {
        /// Rule id.
        rule: String,
    },
    /// From an engine convention.
    EngineHeuristic {
        /// The engine whose convention matched.
        engine: Engine,
    },
    /// Observed by the live process hunter.
    ProcessHunter,
}

/// A directory that contains (or will contain) save files.
#[derive(Debug, Clone, Serialize)]
pub struct SaveLocation {
    /// Existing directory.
    pub root: PathBuf,
    /// File glob patterns, relative to `root` (matched against the relative path and the name).
    pub patterns: Vec<String>,
    /// Origin of this location.
    pub source: LocationSource,
    /// Matching files found under `root` (capped, sorted).
    pub files: Vec<PathBuf>,
}

/// An installed game and its save locations.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredGame {
    pub app_id: Option<u32>,
    pub name: String,
    pub install_dir: Option<PathBuf>,
    pub runtime: GameRuntime,
    pub engine: Option<Engine>,
    pub save_locations: Vec<SaveLocation>,
}

/// Result of [`discover`].
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryReport {
    pub steam_roots: Vec<PathBuf>,
    pub libraries: Vec<SteamLibrary>,
    pub games: Vec<DiscoveredGame>,
    pub warnings: Vec<String>,
}

impl DiscoveryReport {
    /// The game with the given Steam app id.
    pub fn game_by_app_id(&self, app_id: u32) -> Option<&DiscoveredGame> {
        self.games.iter().find(|g| g.app_id == Some(app_id))
    }

    /// The game owning `path`: the one whose save root, install dir or Proton prefix is the
    /// longest prefix of `path`.
    pub fn game_for_path(&self, path: &Path) -> Option<&DiscoveredGame> {
        let mut best: Option<(usize, &DiscoveredGame)> = None;
        for game in &self.games {
            let mut candidates: Vec<&Path> = game.save_locations.iter().map(|l| l.root.as_path()).collect();
            if let Some(dir) = &game.install_dir {
                candidates.push(dir);
            }
            if let GameRuntime::Proton { prefix } = &game.runtime {
                candidates.push(prefix);
            }
            for c in candidates {
                if path.starts_with(c) {
                    let depth = c.components().count();
                    if !matches!(best, Some((d, _)) if d >= depth) {
                        best = Some((depth, game));
                    }
                }
            }
        }
        best.map(|(_, g)| g)
    }
}

/// Discovers Steam libraries, installed games and their save locations.
pub fn discover(cfg: &DiscoveryConfig) -> Result<DiscoveryReport, DiscoveryError> {
    let mut warnings = Vec::new();
    let rule_set = rules::RuleSet::load(&cfg.extra_rules)?;

    let roots = match &cfg.steam_roots {
        Some(explicit) => {
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for root in explicit {
                if !root.is_dir() {
                    warnings.push(format!("configured Steam root {} does not exist", root.display()));
                    continue;
                }
                let c = normalize_path(root);
                if seen.insert(c.clone()) {
                    out.push(c);
                }
            }
            out
        }
        None => detect_steam_roots(cfg),
    };

    let libraries = steam::collect_libraries(&roots, &mut warnings);
    let mut manifests: Vec<AppManifest> = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut prefixes: HashMap<u32, PathBuf> = HashMap::new();
    let mut public_libraries = Vec::new();
    for lib in &libraries {
        let found = steam::read_manifests(&lib.path, &lib.root, &mut warnings);
        let mut ids: BTreeSet<u32> = lib.listed_apps.iter().copied().collect();
        for m in found {
            ids.insert(m.app_id);
            if is_steam_tool(m.app_id, &m.name) {
                tracing::debug!(app_id = m.app_id, name = %m.name, "skipping Steam tool");
                continue;
            }
            if seen_ids.insert(m.app_id) {
                manifests.push(m);
            }
        }
        if cfg.host == HostPlatform::Linux {
            for (id, pfx) in steam::compat_prefixes(&lib.path) {
                prefixes.entry(id).or_insert(pfx);
            }
        }
        public_libraries.push(SteamLibrary {
            path: lib.path.clone(),
            label: lib.label.clone(),
            app_ids: ids.into_iter().collect(),
        });
    }

    let mut games = Vec::new();
    for m in &manifests {
        let install = m.install_path();
        let install_dir = install.is_dir().then_some(install);
        let runtime = match prefixes.get(&m.app_id) {
            Some(pfx) => GameRuntime::Proton { prefix: pfx.clone() },
            None if install_dir.is_some() => GameRuntime::Native,
            None => GameRuntime::Unknown,
        };
        let engine_info = install_dir.as_deref().and_then(detect_engine);
        let ctx = paths::PathContext::new(cfg, &runtime, install_dir.as_deref(), &m.install_dir_name, Some(&m.root));

        let mut locations: Vec<SaveLocation> = Vec::new();
        for rule in rule_set.matching(Some(m.app_id), &m.name) {
            match rules::resolve_rule(rule, &ctx) {
                Ok(locs) => paths::push_unique(&mut locations, locs),
                Err(e) => warnings.push(format!("rule `{}`: {e}", rule.id)),
            }
        }
        if locations.is_empty() {
            if let Some(info) = &engine_info {
                match engines::heuristic_locations(info, &ctx, &m.name) {
                    Ok(locs) => paths::push_unique(&mut locations, locs),
                    Err(e) => warnings.push(format!("{} heuristics for {}: {e}", engine_name(info.engine), m.name)),
                }
            }
        }
        games.push(DiscoveredGame {
            app_id: Some(m.app_id),
            name: m.name.clone(),
            install_dir,
            runtime,
            engine: engine_info.as_ref().map(|i| i.engine),
            save_locations: locations,
        });
    }
    games.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.app_id.cmp(&b.app_id)));

    Ok(DiscoveryReport { steam_roots: roots, libraries: public_libraries, games, warnings })
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Unity => "Unity",
        Engine::Unreal => "Unreal",
        Engine::Godot => "Godot",
        Engine::RenPy => "Ren'Py",
        Engine::GameMaker => "GameMaker",
        Engine::RpgMaker => "RPG Maker",
        Engine::Source => "Source",
        Engine::Other => "engine",
    }
}
