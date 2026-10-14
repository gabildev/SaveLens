//! End-to-end discovery against a fake Steam installation built in a temporary directory.

use std::fs;
use std::path::{Path, PathBuf};

use savelens_discovery::{
    discover, DiscoveryConfig, Engine, GameRuntime, HostPlatform, LocationSource, WindowsEnv,
};

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn manifest(lib: &Path, app_id: u32, name: &str, installdir: &str) {
    write(
        &lib.join(format!("steamapps/appmanifest_{app_id}.acf")),
        &format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"{installdir}\"\n}}\n"
        ),
    );
    fs::create_dir_all(lib.join("steamapps/common").join(installdir)).unwrap();
}

struct Fake {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    lib2: PathBuf,
}

/// Home with a Steam root in `~/.local/share/Steam` and a second library outside the home.
fn fake_steam() -> Fake {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    let root = home.join(".local/share/Steam");
    let lib2 = base.join("library2");
    fs::create_dir_all(lib2.join("steamapps")).unwrap();

    write(
        &root.join("steamapps/libraryfolders.vdf"),
        &format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t\t\"1000001\"\t\t\"100\"\n\t\t\t\"1493710\"\t\t\"200\"\n\t\t}}\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"Games2\"\n\t\t\"apps\"\n\t\t{{\n\t\t\t\"413150\"\t\t\"300\"\n\t\t\t\"1000002\"\t\t\"400\"\n\t\t}}\n\t}}\n}}\n",
            root.display(),
            lib2.display()
        ),
    );

    // Native Unity game in the root library.
    manifest(&root, 1000001, "Native Unity Game", "NativeUnity");
    write(
        &root.join("steamapps/common/NativeUnity/Game_Data/app.info"),
        "Company\nProduct",
    );
    write(
        &root.join("steamapps/common/NativeUnity/Game.x86_64"),
        "elf",
    );
    write(
        &home.join(".config/unity3d/Company/Product/save.dat"),
        "save",
    );

    // Proton runtime tool: must be skipped.
    manifest(
        &root,
        1493710,
        "Proton Experimental",
        "Proton - Experimental",
    );
    manifest(
        &root,
        228980,
        "Steamworks Common Redistributables",
        "Steamworks Shared",
    );

    // Stardew Valley under Proton: installed in library 2, prefix in the root library.
    manifest(&lib2, 413150, "Stardew Valley", "Stardew Valley");
    write(
        &lib2.join("steamapps/common/Stardew Valley/Stardew Valley.exe"),
        "MZ",
    );
    let sdv_user = root.join("steamapps/compatdata/413150/pfx/drive_c/users/steamuser");
    write(
        &sdv_user.join("AppData/Roaming/StardewValley/Saves/Farmer_123/Farmer_123"),
        "<SaveGame/>",
    );
    write(
        &sdv_user.join("AppData/Roaming/StardewValley/Saves/Farmer_123/SaveGameInfo"),
        "<Farmer/>",
    );

    // Unreal game under Proton, everything in library 2.
    manifest(&lib2, 1000002, "Unreal Proton Game", "UnrealGame");
    write(
        &lib2.join(
            "steamapps/common/UnrealGame/MyProject/Binaries/Win64/MyProject-Win64-Shipping.exe",
        ),
        "MZ",
    );
    write(
        &lib2.join("steamapps/compatdata/1000002/pfx/drive_c/users/steamuser/AppData/Local/MyProject/Saved/SaveGames/Slot1.sav"),
        "GVAS",
    );

    Fake {
        _tmp: tmp,
        home,
        root,
        lib2,
    }
}

fn linux_cfg(home: &Path) -> DiscoveryConfig {
    let mut cfg = DiscoveryConfig::for_home(home.to_path_buf());
    cfg.host = HostPlatform::Linux;
    cfg
}

#[test]
fn discovers_native_proton_and_unreal_games() {
    let f = fake_steam();
    let report = discover(&linux_cfg(&f.home)).unwrap();

    assert_eq!(report.steam_roots, vec![f.root.clone()]);
    let lib_paths: Vec<&PathBuf> = report.libraries.iter().map(|l| &l.path).collect();
    assert_eq!(lib_paths, vec![&f.root, &f.lib2]);
    assert_eq!(report.libraries[1].label.as_deref(), Some("Games2"));
    assert_eq!(report.libraries[1].app_ids, vec![413150, 1000002]);
    assert!(report.libraries[0].app_ids.contains(&1000001));

    // Tools are skipped.
    let names: Vec<&str> = report.games.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Native Unity Game", "Stardew Valley", "Unreal Proton Game"],
        "warnings: {:?}",
        report.warnings
    );

    // Native Unity game.
    let unity = report.game_by_app_id(1000001).unwrap();
    assert_eq!(unity.runtime, GameRuntime::Native);
    assert_eq!(unity.engine, Some(Engine::Unity));
    assert_eq!(
        unity.install_dir.as_deref(),
        Some(f.root.join("steamapps/common/NativeUnity").as_path())
    );
    assert_eq!(unity.save_locations.len(), 1);
    let loc = &unity.save_locations[0];
    assert_eq!(loc.root, f.home.join(".config/unity3d/Company/Product"));
    assert_eq!(
        loc.source,
        LocationSource::EngineHeuristic {
            engine: Engine::Unity
        }
    );
    assert_eq!(loc.files, vec![loc.root.join("save.dat")]);

    // Stardew Valley through Proton, prefix found in another library, rule-based location.
    let sdv = report.game_by_app_id(413150).unwrap();
    let prefix = f.root.join("steamapps/compatdata/413150/pfx");
    assert_eq!(
        sdv.runtime,
        GameRuntime::Proton {
            prefix: prefix.clone()
        }
    );
    assert_eq!(sdv.engine, None);
    assert_eq!(sdv.save_locations.len(), 1);
    let loc = &sdv.save_locations[0];
    let saves = prefix.join("drive_c/users/steamuser/AppData/Roaming/StardewValley/Saves");
    assert_eq!(loc.root, saves);
    assert_eq!(
        loc.source,
        LocationSource::Rule {
            rule: "stardew-valley".to_string()
        }
    );
    assert_eq!(
        loc.files,
        vec![
            saves.join("Farmer_123/Farmer_123"),
            saves.join("Farmer_123/SaveGameInfo")
        ]
    );

    // Unreal game through Proton.
    let ue = report.game_by_app_id(1000002).unwrap();
    let ue_prefix = f.lib2.join("steamapps/compatdata/1000002/pfx");
    assert_eq!(
        ue.runtime,
        GameRuntime::Proton {
            prefix: ue_prefix.clone()
        }
    );
    assert_eq!(ue.engine, Some(Engine::Unreal));
    let ue_saves =
        ue_prefix.join("drive_c/users/steamuser/AppData/Local/MyProject/Saved/SaveGames");
    assert_eq!(ue.save_locations.len(), 1);
    assert_eq!(ue.save_locations[0].root, ue_saves);
    assert_eq!(ue.save_locations[0].files, vec![ue_saves.join("Slot1.sav")]);

    // Path → game lookup.
    assert_eq!(
        report
            .game_for_path(&ue_saves.join("Slot1.sav"))
            .and_then(|g| g.app_id),
        Some(1000002)
    );

    // The report serializes.
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("Farmer_123"));
}

#[test]
fn steam_symlink_roots_are_deduplicated() {
    let f = fake_steam();
    #[cfg(unix)]
    {
        fs::create_dir_all(f.home.join(".steam")).unwrap();
        std::os::unix::fs::symlink(&f.root, f.home.join(".steam/steam")).unwrap();
        std::os::unix::fs::symlink(&f.root, f.home.join(".steam/root")).unwrap();
    }
    let report = discover(&linux_cfg(&f.home)).unwrap();
    assert_eq!(report.steam_roots, vec![f.root.clone()]);
    assert_eq!(report.games.len(), 3);
}

#[test]
fn flatpak_root_and_old_libraryfolders_format() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    let root = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
    let lib = base.join("old_lib");
    fs::create_dir_all(lib.join("steamapps")).unwrap();
    write(
        &root.join("steamapps/libraryfolders.vdf"),
        &format!(
            "\"LibraryFolders\"\n{{\n\t\"TimeNextStatsReport\"\t\t\"1\"\n\t\"1\"\t\t\"{}\"\n\t\"2\"\t\t\"{}\"\n}}\n",
            lib.display(),
            base.join("missing_lib").display()
        ),
    );
    manifest(&lib, 646570, "Slay the Spire", "SlayTheSpire");
    write(
        &lib.join("steamapps/common/SlayTheSpire/saves/IRONCLAD.autosave"),
        "x",
    );
    // Broken manifest → warning, not failure.
    write(
        &lib.join("steamapps/appmanifest_5.acf"),
        "\"AppState\" { \"appid\" ",
    );

    let report = discover(&linux_cfg(&home)).unwrap();
    assert_eq!(report.steam_roots, vec![root.clone()]);
    assert_eq!(report.libraries.len(), 2);
    assert!(report.warnings.iter().any(|w| w.contains("missing_lib")));
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("appmanifest_5.acf")));
    let sts = report.game_by_app_id(646570).unwrap();
    assert_eq!(sts.runtime, GameRuntime::Native);
    assert_eq!(sts.save_locations.len(), 1);
    assert_eq!(
        sts.save_locations[0].root,
        lib.join("steamapps/common/SlayTheSpire/saves")
    );
    assert_eq!(sts.save_locations[0].files.len(), 1);
}

#[test]
fn windows_placeholders_resolve_against_windows_env() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let steam = base.join("Program Files (x86)/Steam");
    let profile = base.join("Users/alice");
    manifest(&steam, 413150, "Stardew Valley", "Stardew Valley");
    manifest(&steam, 1245620, "ELDEN RING", "ELDEN RING");
    // Even with a compatdata dir, a Windows host never uses Proton prefixes.
    fs::create_dir_all(steam.join("steamapps/compatdata/413150/pfx")).unwrap();

    let mut env = WindowsEnv::under_profile(&profile, &base.join("Users/Public"));
    env.app_data = base.join("Roaming");
    write(
        &env.app_data.join("StardewValley/Saves/Abby_1/Abby_1"),
        "<x/>",
    );
    write(
        &env.app_data.join("EldenRing/76561198000000000/ER0000.sl2"),
        "BND4",
    );
    write(
        &env.app_data
            .join("EldenRing/76561198000000000/steam_autocloud.vdf"),
        "x",
    );
    write(
        &env.app_data.join("EldenRing/76561198000000001/ER0000.sl2"),
        "BND4",
    );

    let mut cfg = DiscoveryConfig::for_home(profile.clone());
    cfg.host = HostPlatform::Windows;
    cfg.windows_env = env.clone();
    cfg.steam_roots = Some(vec![steam.clone()]);
    let report = discover(&cfg).unwrap();

    let sdv = report.game_by_app_id(413150).unwrap();
    assert_eq!(sdv.runtime, GameRuntime::Native);
    assert_eq!(sdv.save_locations.len(), 1);
    assert_eq!(
        sdv.save_locations[0].root,
        env.app_data.join("StardewValley/Saves")
    );
    assert_eq!(sdv.save_locations[0].files.len(), 1);

    let er = report.game_by_app_id(1245620).unwrap();
    let roots: Vec<&PathBuf> = er.save_locations.iter().map(|l| &l.root).collect();
    assert_eq!(
        roots,
        vec![
            &env.app_data.join("EldenRing/76561198000000000"),
            &env.app_data.join("EldenRing/76561198000000001")
        ]
    );
    // `ER0000.*` pattern excludes steam_autocloud.vdf.
    assert_eq!(
        er.save_locations[0].files,
        vec![env.app_data.join("EldenRing/76561198000000000/ER0000.sl2")]
    );

    // The Linux-only rule path is not applied on a Windows host.
    write(&profile.join(".config/StardewValley/Saves/x"), "x");
    let report = discover(&cfg).unwrap();
    assert_eq!(
        report.game_by_app_id(413150).unwrap().save_locations.len(),
        1
    );
}

#[test]
fn user_rules_override_and_extend() {
    let f = fake_steam();
    write(
        &f.root
            .join("steamapps/common/NativeUnity/CustomSaves/slot.bin"),
        "x",
    );
    let rules = f.home.join("my_rules.toml");
    write(
        &rules,
        "[[game]]\nid = \"native-unity\"\nname = \"Native Unity Game\"\napp_ids = [1000001]\npatterns = [\"*.bin\"]\npaths = [{ path = \"<base>/CustomSaves\", os = \"any\" }]\n",
    );
    let mut cfg = linux_cfg(&f.home);
    cfg.extra_rules = vec![rules.clone()];
    let report = discover(&cfg).unwrap();
    let g = report.game_by_app_id(1000001).unwrap();
    assert_eq!(g.save_locations.len(), 1);
    assert_eq!(
        g.save_locations[0].source,
        LocationSource::Rule {
            rule: "native-unity".to_string()
        }
    );
    assert_eq!(g.save_locations[0].files.len(), 1);

    write(
        &rules,
        "[[game]]\nid = \"broken\"\nname = \"B\"\npaths = [{ path = \"<what>/x\" }]\n",
    );
    assert!(discover(&cfg).is_err());
}
