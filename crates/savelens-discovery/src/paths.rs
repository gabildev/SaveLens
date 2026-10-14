//! Path templates (`<winAppData>/Foo/Saves`), glob expansion and save-file collection.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    DiscoveryConfig, DiscoveryError, GameRuntime, HostPlatform, LocationSource, SaveLocation,
    WindowsEnv,
};

/// Maximum number of files recorded per [`SaveLocation`].
pub const MAX_FILES_PER_LOCATION: usize = 2000;
/// Files larger than this (2 GiB) are never recorded as save files.
pub const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;
/// Maximum directory depth explored below a save root.
const MAX_DEPTH: usize = 24;

/// Placeholders understood by path templates.
pub const PLACEHOLDERS: &[&str] = &[
    "home",
    "base",
    "game",
    "root",
    "storeUserId",
    "xdgData",
    "xdgConfig",
    "winAppData",
    "winLocalAppData",
    "winLocalAppDataLow",
    "winDocuments",
    "winSavedGames",
    "winPublic",
];

/// Canonicalizes `path` when possible (resolving symlinks such as `~/.steam/steam`), stripping
/// the Windows verbatim prefix (`\\?\`). Falls back to the input when the path does not exist.
pub fn normalize_path(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(p) => strip_verbatim(p),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(windows)]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

#[cfg(not(windows))]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    p
}

/// Which operating-system flavour a rule path applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathOs {
    Windows,
    Linux,
    Any,
}

/// Everything needed to resolve a template for one game.
#[derive(Debug, Clone)]
pub(crate) struct PathContext {
    home: PathBuf,
    base: Option<PathBuf>,
    game: String,
    root: Option<PathBuf>,
    xdg_data: PathBuf,
    xdg_config: PathBuf,
    /// Windows known folders, when the game uses Windows conventions.
    win: Option<WindowsEnv>,
    /// True for a native game on a Linux host.
    linux_native: bool,
    /// Extra placeholders defined by heuristics (`<company>`, `<product>` …).
    extras: Vec<(String, Vec<String>)>,
}

impl PathContext {
    pub(crate) fn new(
        cfg: &DiscoveryConfig,
        runtime: &GameRuntime,
        base: Option<&Path>,
        game: &str,
        root: Option<&Path>,
    ) -> Self {
        let (win, linux_native) = match (cfg.host, runtime) {
            (HostPlatform::Windows, _) => (Some(cfg.windows_env.clone()), false),
            (HostPlatform::Linux, GameRuntime::Proton { prefix }) => {
                (Some(WindowsEnv::for_proton_prefix(prefix)), false)
            }
            (HostPlatform::Linux, _) => (None, true),
        };
        PathContext {
            home: cfg.home.clone(),
            base: base.map(Path::to_path_buf),
            game: game.to_string(),
            root: root.map(Path::to_path_buf),
            xdg_data: cfg.xdg_data.clone(),
            xdg_config: cfg.xdg_config.clone(),
            win,
            linux_native,
            extras: Vec::new(),
        }
    }

    /// Adds a heuristic-specific placeholder. Values are literal (they get glob-escaped).
    pub(crate) fn with_extra(mut self, name: &str, value: &str) -> Self {
        self.extras
            .push((name.to_string(), vec![value.to_string()]));
        self
    }

    /// Does a rule path flagged with `os` apply to this game?
    pub(crate) fn applies(&self, os: PathOs) -> bool {
        match os {
            PathOs::Any => true,
            PathOs::Windows => self.win.is_some(),
            PathOs::Linux => self.linux_native,
        }
    }

    /// Values of a placeholder: `Ok(None)` when it is known but unavailable for this game,
    /// `Ok(Some(values))` with already glob-escaped alternatives otherwise.
    fn values(&self, name: &str) -> Result<Option<Vec<String>>, DiscoveryError> {
        let lit = |p: &Path| vec![glob::Pattern::escape(&p.to_string_lossy())];
        let win = self.win.as_ref();
        let v = match name {
            "home" => Some(lit(&self.home)),
            "base" => self.base.as_deref().map(lit),
            "game" => Some(vec![glob::Pattern::escape(&self.game)]),
            "root" => self.root.as_deref().map(lit),
            "storeUserId" => Some(vec!["*".to_string()]),
            "xdgData" => Some(lit(&self.xdg_data)),
            "xdgConfig" => Some(lit(&self.xdg_config)),
            "winAppData" => win.map(|w| lit(&w.app_data)),
            "winLocalAppData" => win.map(|w| lit(&w.local_app_data)),
            "winLocalAppDataLow" => win.map(|w| lit(&w.local_app_data_low)),
            "winDocuments" => win.map(|w| {
                w.documents
                    .iter()
                    .map(|d| glob::Pattern::escape(&d.to_string_lossy()))
                    .collect()
            }),
            "winSavedGames" => win.map(|w| lit(&w.saved_games)),
            "winPublic" => win.map(|w| lit(&w.public)),
            other => match self.extras.iter().find(|(k, _)| k == other) {
                Some((_, values)) => {
                    Some(values.iter().map(|v| glob::Pattern::escape(v)).collect())
                }
                None => {
                    return Err(DiscoveryError::Glob {
                        pattern: format!("<{other}>"),
                        message: "unknown placeholder".to_string(),
                    })
                }
            },
        };
        Ok(v)
    }

    /// Expands a template into glob patterns (cartesian product of placeholder alternatives).
    /// Returns an empty list when a placeholder is unavailable for this game.
    pub(crate) fn expand(&self, template: &str) -> Result<Vec<String>, DiscoveryError> {
        let mut outputs = vec![String::new()];
        let mut rest = template;
        while let Some(start) = rest.find('<') {
            let Some(len) = rest[start..].find('>') else {
                return Err(DiscoveryError::Glob {
                    pattern: template.to_string(),
                    message: "unterminated `<placeholder>`".to_string(),
                });
            };
            let literal = &rest[..start];
            let name = &rest[start + 1..start + len];
            for o in &mut outputs {
                o.push_str(literal);
            }
            let Some(values) = self.values(name)? else {
                return Ok(Vec::new());
            };
            let mut next = Vec::with_capacity(outputs.len() * values.len());
            for o in &outputs {
                for v in &values {
                    next.push(format!("{o}{v}"));
                }
            }
            outputs = next;
            rest = &rest[start + len + 1..];
        }
        for o in &mut outputs {
            o.push_str(rest);
        }
        Ok(outputs)
    }

    /// Expands `template` and returns the existing directories it designates (deduplicated).
    pub(crate) fn existing_dirs(&self, template: &str) -> Result<Vec<PathBuf>, DiscoveryError> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for pattern in self.expand(template)? {
            for dir in glob_dirs(&pattern)? {
                let n = normalize_path(&dir);
                if seen.insert(n.clone()) {
                    out.push(n);
                }
            }
        }
        Ok(out)
    }
}

/// Validates the placeholders of a template without resolving it.
pub(crate) fn validate_template(template: &str, extra: &[&str]) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            return Err(format!("unterminated placeholder in `{template}`"));
        };
        let name = &rest[start + 1..start + len];
        if !PLACEHOLDERS.contains(&name) && !extra.contains(&name) {
            return Err(format!("unknown placeholder `<{name}>` in `{template}`"));
        }
        rest = &rest[start + len + 1..];
    }
    glob::Pattern::new(template)
        .map(|_| ())
        .map_err(|e| format!("invalid glob `{template}`: {e}"))
}

fn glob_dirs(pattern: &str) -> Result<Vec<PathBuf>, DiscoveryError> {
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let paths = glob::glob_with(pattern, opts).map_err(|e| DiscoveryError::Glob {
        pattern: pattern.to_string(),
        message: e.to_string(),
    })?;
    // Unreadable entries are skipped: the glob walks user directories that may contain
    // folders we have no permission to list.
    Ok(paths
        .filter_map(Result::ok)
        .filter(|p| p.is_dir())
        .collect())
}

/// Builds a [`SaveLocation`] for an existing `root`, collecting matching files.
pub(crate) fn make_location(
    root: PathBuf,
    patterns: Vec<String>,
    source: LocationSource,
) -> Result<SaveLocation, DiscoveryError> {
    let files = collect_files(&root, &patterns)?;
    Ok(SaveLocation {
        root,
        patterns,
        source,
        files,
    })
}

/// Appends locations whose root is not yet present.
pub(crate) fn push_unique(into: &mut Vec<SaveLocation>, new: Vec<SaveLocation>) {
    for loc in new {
        if !into.iter().any(|l| l.root == loc.root) {
            into.push(loc);
        }
    }
}

/// Lists the regular files below `root` whose relative path (with `/` separators) or file name
/// matches one of `patterns`. An empty pattern list matches everything. Symlinked directories
/// are not followed, files above [`MAX_FILE_SIZE`] are skipped, and at most
/// [`MAX_FILES_PER_LOCATION`] files are returned (sorted).
pub fn collect_files(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>, DiscoveryError> {
    let compiled = patterns
        .iter()
        .map(|p| {
            glob::Pattern::new(p).map_err(|e| DiscoveryError::Glob {
                pattern: p.clone(),
                message: e.to_string(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let matches = |rel: &str, name: &str| {
        compiled.is_empty()
            || compiled
                .iter()
                .any(|p| p.matches_with(rel, opts) || p.matches_with(name, opts))
    };

    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            tracing::debug!(dir = %dir.display(), "unreadable directory skipped");
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if depth < MAX_DEPTH {
                    subdirs.push(path);
                }
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_FILE_SIZE {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches(&rel, &name) {
                out.push(path);
                if out.len() >= MAX_FILES_PER_LOCATION {
                    out.sort();
                    return Ok(out);
                }
            }
        }
        for d in subdirs.into_iter().rev() {
            stack.push((d, depth + 1));
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_cfg(home: &Path) -> DiscoveryConfig {
        let mut cfg = DiscoveryConfig::for_home(home.to_path_buf());
        cfg.host = HostPlatform::Linux;
        cfg
    }

    // Uses Unix path literals to simulate a Linux host.
    #[cfg(unix)]
    #[test]
    fn expansion_native_vs_proton() {
        let home = Path::new("/h");
        let cfg = linux_cfg(home);
        let native = PathContext::new(
            &cfg,
            &GameRuntime::Native,
            Some(Path::new("/lib/common/G")),
            "G",
            None,
        );
        assert_eq!(
            native.expand("<xdgConfig>/x").unwrap(),
            vec!["/h/.config/x"]
        );
        assert!(native.expand("<winAppData>/x").unwrap().is_empty());
        assert_eq!(
            native.expand("<base>/<game>/s").unwrap(),
            vec!["/lib/common/G/G/s"]
        );
        assert!(native.expand("<root>/userdata").unwrap().is_empty());
        assert!(native.expand("<bogus>").is_err());

        let proton = PathContext::new(
            &cfg,
            &GameRuntime::Proton {
                prefix: PathBuf::from("/p/pfx"),
            },
            None,
            "G[1]",
            Some(Path::new("/s")),
        );
        assert_eq!(
            proton.expand("<winDocuments>/My Games").unwrap(),
            vec![
                "/p/pfx/drive_c/users/steamuser/Documents/My Games",
                "/p/pfx/drive_c/users/steamuser/My Documents/My Games"
            ]
        );
        assert_eq!(proton.expand("<game>").unwrap(), vec!["G[[]1[]]"]);
        assert_eq!(
            proton.expand("<root>/userdata/<storeUserId>/1").unwrap(),
            vec!["/s/userdata/*/1"]
        );
        assert!(proton.applies(PathOs::Windows));
        assert!(!proton.applies(PathOs::Linux));
    }

    #[test]
    fn collect_files_patterns_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("one.sav"), b"1").unwrap();
        fs::write(root.join("a/b/two.sav"), b"2").unwrap();
        fs::write(root.join("a/notes.txt"), b"3").unwrap();
        let all = collect_files(root, &[]).unwrap();
        assert_eq!(all.len(), 3);
        let savs = collect_files(root, &["*.sav".to_string()]).unwrap();
        assert_eq!(savs, vec![root.join("a/b/two.sav"), root.join("one.sav")]);
        let rel = collect_files(root, &["a/*.txt".to_string()]).unwrap();
        assert_eq!(rel, vec![root.join("a/notes.txt")]);
        assert!(collect_files(root, &["[".to_string()]).is_err());

        for i in 0..(MAX_FILES_PER_LOCATION + 5) {
            fs::write(root.join(format!("f{i:05}.dat")), b"x").unwrap();
        }
        let capped = collect_files(root, &["*.dat".to_string()]).unwrap();
        assert_eq!(capped.len(), MAX_FILES_PER_LOCATION);
    }

    #[test]
    fn template_validation() {
        assert!(validate_template("<winAppData>/X/*", &[]).is_ok());
        assert!(validate_template("<nope>/X", &[]).is_err());
        assert!(validate_template("<home/X", &[]).is_err());
        assert!(validate_template("<company>/x", &["company"]).is_ok());
    }
}
