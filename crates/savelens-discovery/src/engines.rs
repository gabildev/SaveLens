//! Engine detection and engine-convention save locations for games without a rule.

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::paths::{make_location, PathContext, PathOs};
use crate::{DiscoveryError, Engine, LocationSource, SaveLocation};

/// What the engine heuristics learned about an install directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineInfo {
    /// Detected engine.
    pub engine: Engine,
    /// Unity company name (line 1 of `app.info`).
    pub company: Option<String>,
    /// Unity product name (line 2 of `app.info`).
    pub product: Option<String>,
    /// Unreal project name, Godot `.pck` name or Ren'Py `config.save_directory`.
    pub project: Option<String>,
}

impl EngineInfo {
    fn new(engine: Engine) -> Self {
        EngineInfo { engine, company: None, product: None, project: None }
    }
}

struct Listing {
    files: Vec<String>,
    dirs: Vec<String>,
}

fn list(dir: &Path) -> Listing {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            match e.path().is_dir() {
                true => dirs.push(name),
                false => files.push(name),
            }
        }
    }
    files.sort();
    dirs.sort();
    Listing { files, dirs }
}

fn has_ext(name: &str, ext: &str) -> bool {
    Path::new(name).extension().is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(ext))
}

/// Finds the first file whose name satisfies `pred`, searching up to `depth` levels.
fn find_file(dir: &Path, depth: usize, pred: &dyn Fn(&str) -> bool) -> Option<String> {
    let l = list(dir);
    if let Some(f) = l.files.iter().find(|f| pred(f)) {
        return Some(f.clone());
    }
    if depth == 0 {
        return None;
    }
    l.dirs.iter().find_map(|d| find_file(&dir.join(d), depth - 1, pred))
}

/// Detects the engine of a game from its install directory.
pub fn detect_engine(install: &Path) -> Option<EngineInfo> {
    let root = list(install);

    // Unity: <Name>_Data/app.info (company / product) or the Unity player library.
    for d in root.dirs.iter().filter(|d| d.ends_with("_Data")) {
        let data = install.join(d);
        if let Ok(bytes) = fs::read(data.join("app.info")) {
            let text = String::from_utf8_lossy(&bytes);
            let mut lines = text.lines().map(str::trim);
            let mut info = EngineInfo::new(Engine::Unity);
            info.company = lines.next().filter(|s| !s.is_empty()).map(str::to_string);
            info.product = lines.next().filter(|s| !s.is_empty()).map(str::to_string);
            return Some(info);
        }
        if data.join("globalgamemanagers").is_file() || data.join("mainData").is_file() {
            return Some(EngineInfo::new(Engine::Unity));
        }
    }
    if root.files.iter().any(|f| f == "UnityPlayer.dll" || f == "UnityPlayer.so") {
        return Some(EngineInfo::new(Engine::Unity));
    }

    // Unreal: <Project>/Binaries/(Win64|Linux|Win32) or <Project>-<Platform>-Shipping.exe.
    for d in root.dirs.iter().filter(|d| d.as_str() != "Engine") {
        let bin = install.join(d).join("Binaries");
        if ["Win64", "Linux", "Win32", "WinGDK"].iter().any(|p| bin.join(p).is_dir()) {
            let mut info = EngineInfo::new(Engine::Unreal);
            info.project = Some(d.clone());
            return Some(info);
        }
    }
    let shipping = |f: &str| {
        let lower = f.to_ascii_lowercase();
        lower.ends_with("-shipping.exe") || lower.ends_with("-shipping")
    };
    if let Some(exe) = find_file(install, 3, &shipping) {
        let mut info = EngineInfo::new(Engine::Unreal);
        info.project = exe.split('-').next().map(str::to_string).filter(|s| !s.is_empty());
        return Some(info);
    }

    // Godot: exported `.pck` next to the executable.
    if let Some(pck) = root.files.iter().find(|f| has_ext(f, "pck")) {
        let mut info = EngineInfo::new(Engine::Godot);
        info.project = Path::new(pck).file_stem().map(|s| s.to_string_lossy().into_owned());
        return Some(info);
    }

    // Ren'Py: `renpy/` runtime directory.
    if root.dirs.iter().any(|d| d == "renpy") {
        let mut info = EngineInfo::new(Engine::RenPy);
        info.project = renpy_save_directory(&install.join("game"));
        return Some(info);
    }

    // RPG Maker MV / MZ.
    let rpg_markers = ["www/js/rpg_core.js", "www/js/rpg_managers.js", "js/rmmz_core.js", "js/rpg_core.js"];
    if rpg_markers.iter().any(|m| install.join(m).is_file())
        || has_rpg_saves(&install.join("www").join("save"))
        || has_rpg_saves(&install.join("save"))
    {
        return Some(EngineInfo::new(Engine::RpgMaker));
    }

    // GameMaker: data.win (Windows) / game.unx (Linux).
    if root.files.iter().any(|f| f == "data.win" || f == "game.unx")
        || install.join("assets").join("game.unx").is_file()
    {
        return Some(EngineInfo::new(Engine::GameMaker));
    }

    // Source: VPK archives in the root or a mod directory.
    if root.files.iter().any(|f| has_ext(f, "vpk"))
        || root.dirs.iter().any(|d| list(&install.join(d)).files.iter().any(|f| has_ext(f, "vpk")))
    {
        return Some(EngineInfo::new(Engine::Source));
    }

    // XNA / FNA / MonoGame.
    if root
        .files
        .iter()
        .any(|f| f == "MonoGame.Framework.dll" || f == "FNA.dll" || f.starts_with("Microsoft.Xna.Framework"))
    {
        return Some(EngineInfo::new(Engine::Other));
    }
    None
}

fn has_rpg_saves(dir: &Path) -> bool {
    list(dir).files.iter().any(|f| has_ext(f, "rpgsave") || has_ext(f, "rmmzsave"))
}

/// Reads `config.save_directory = "..."` from the Ren'Py scripts of a game directory.
fn renpy_save_directory(game_dir: &Path) -> Option<String> {
    let re = regex::Regex::new(r#"config\.save_directory\s*=\s*(?:u|r)?["']([^"']+)["']"#).ok()?;
    let l = list(game_dir);
    let mut scripts: Vec<&String> = l.files.iter().filter(|f| has_ext(f, "rpy")).collect();
    // options.rpy is where the launcher writes the setting; look there first.
    scripts.sort_by_key(|f| f.as_str() != "options.rpy");
    for f in scripts {
        let Ok(bytes) = fs::read(game_dir.join(f)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if let Some(c) = re.captures(&text) {
            return c.get(1).map(|m| m.as_str().to_string());
        }
    }
    None
}

/// Candidate save locations from engine conventions.
pub(crate) fn heuristic_locations(
    info: &EngineInfo,
    ctx: &PathContext,
    game_name: &str,
) -> Result<Vec<SaveLocation>, DiscoveryError> {
    let all = || vec!["*".to_string()];
    let mut specs: Vec<(PathContext, &str, PathOs, Vec<String>)> = Vec::new();
    match info.engine {
        Engine::Unity => {
            if let (Some(company), Some(product)) = (&info.company, &info.product) {
                let c = ctx.clone().with_extra("company", company).with_extra("product", product);
                specs.push((c.clone(), "<winLocalAppDataLow>/<company>/<product>", PathOs::Windows, all()));
                specs.push((c, "<xdgConfig>/unity3d/<company>/<product>", PathOs::Linux, all()));
            }
        }
        Engine::Unreal => {
            if let Some(project) = &info.project {
                let c = ctx.clone().with_extra("project", project);
                let sav = vec!["*.sav".to_string()];
                specs.push((c.clone(), "<winLocalAppData>/<project>/Saved/SaveGames", PathOs::Windows, sav.clone()));
                specs.push((c.clone(), "<base>/<project>/Saved/SaveGames", PathOs::Any, sav.clone()));
                specs.push((c, "<xdgConfig>/Epic/<project>/Saved/SaveGames", PathOs::Linux, sav));
            }
        }
        Engine::Godot => {
            let mut names: Vec<&str> = info.project.iter().map(String::as_str).collect();
            if !names.contains(&game_name) {
                names.push(game_name);
            }
            for name in names {
                let c = ctx.clone().with_extra("project", name);
                specs.push((c.clone(), "<winAppData>/Godot/app_userdata/<project>", PathOs::Windows, all()));
                specs.push((c, "<xdgData>/godot/app_userdata/<project>", PathOs::Linux, all()));
            }
        }
        Engine::RenPy => {
            let pats = vec!["*.save".to_string(), "persistent".to_string()];
            match &info.project {
                Some(dir) => {
                    let c = ctx.clone().with_extra("project", dir);
                    specs.push((c.clone(), "<winAppData>/RenPy/<project>", PathOs::Windows, pats.clone()));
                    specs.push((c, "<home>/.renpy/<project>", PathOs::Linux, pats.clone()));
                }
                None => {
                    specs.push((ctx.clone(), "<winAppData>/RenPy/*", PathOs::Windows, pats.clone()));
                    specs.push((ctx.clone(), "<home>/.renpy/*", PathOs::Linux, pats.clone()));
                }
            }
            specs.push((ctx.clone(), "<base>/game/saves", PathOs::Any, pats));
        }
        Engine::GameMaker => {
            specs.push((ctx.clone(), "<winLocalAppData>/<game>", PathOs::Windows, all()));
            specs.push((ctx.clone(), "<xdgConfig>/<game>", PathOs::Linux, all()));
        }
        Engine::RpgMaker => {
            let pats = vec!["*.rpgsave".to_string(), "*.rmmzsave".to_string()];
            specs.push((ctx.clone(), "<base>/www/save", PathOs::Any, pats.clone()));
            specs.push((ctx.clone(), "<base>/save", PathOs::Any, pats));
        }
        Engine::Source => {
            specs.push((ctx.clone(), "<base>/*/save", PathOs::Any, vec!["*.sav".to_string()]));
        }
        Engine::Other => {}
    }

    let mut out: Vec<SaveLocation> = Vec::new();
    for (c, template, os, patterns) in specs {
        if !c.applies(os) {
            continue;
        }
        for dir in c.existing_dirs(template)? {
            if out.iter().any(|l| l.root == dir) {
                continue;
            }
            out.push(make_location(dir, patterns.clone(), LocationSource::EngineHeuristic { engine: info.engine })?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, b"x").unwrap();
    }

    #[test]
    fn detects_each_engine() {
        let t = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let d = t.path().join(name);
            fs::create_dir_all(&d).unwrap();
            d
        };

        let unity = mk("unity");
        fs::create_dir_all(unity.join("Game_Data")).unwrap();
        fs::write(unity.join("Game_Data/app.info"), "Acme Corp\nSpace Game\n").unwrap();
        let i = detect_engine(&unity).unwrap();
        assert_eq!(i.engine, Engine::Unity);
        assert_eq!(i.company.as_deref(), Some("Acme Corp"));
        assert_eq!(i.product.as_deref(), Some("Space Game"));

        let ue = mk("ue");
        fs::create_dir_all(ue.join("Engine/Binaries/Win64")).unwrap();
        fs::create_dir_all(ue.join("Proj/Binaries/Linux")).unwrap();
        let i = detect_engine(&ue).unwrap();
        assert_eq!((i.engine, i.project.as_deref()), (Engine::Unreal, Some("Proj")));

        let ue2 = mk("ue2");
        touch(&ue2.join("bin/Other-Win64-Shipping.exe"));
        let i = detect_engine(&ue2).unwrap();
        assert_eq!((i.engine, i.project.as_deref()), (Engine::Unreal, Some("Other")));

        let godot = mk("godot");
        touch(&godot.join("MyGodot.pck"));
        let i = detect_engine(&godot).unwrap();
        assert_eq!((i.engine, i.project.as_deref()), (Engine::Godot, Some("MyGodot")));

        let renpy = mk("renpy");
        fs::create_dir_all(renpy.join("renpy")).unwrap();
        fs::create_dir_all(renpy.join("game")).unwrap();
        fs::write(renpy.join("game/options.rpy"), "define config.save_directory = \"VN-1234\"\n").unwrap();
        let i = detect_engine(&renpy).unwrap();
        assert_eq!((i.engine, i.project.as_deref()), (Engine::RenPy, Some("VN-1234")));

        let rpg = mk("rpg");
        touch(&rpg.join("www/save/file1.rpgsave"));
        assert_eq!(detect_engine(&rpg).unwrap().engine, Engine::RpgMaker);

        let gm = mk("gm");
        touch(&gm.join("data.win"));
        assert_eq!(detect_engine(&gm).unwrap().engine, Engine::GameMaker);

        let src = mk("src");
        touch(&src.join("hl2/hl2_misc_dir.vpk"));
        assert_eq!(detect_engine(&src).unwrap().engine, Engine::Source);

        let xna = mk("xna");
        touch(&xna.join("MonoGame.Framework.dll"));
        assert_eq!(detect_engine(&xna).unwrap().engine, Engine::Other);

        let none = mk("none");
        touch(&none.join("game.bin"));
        assert!(detect_engine(&none).is_none());
    }
}
