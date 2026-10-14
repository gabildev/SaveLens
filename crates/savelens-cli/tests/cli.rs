//! End-to-end tests of the `savelens` binary on a sandboxed data directory.

use std::fs;
use std::path::Path;
use std::process::Command;

fn bin(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_savelens"));
    c.env("SAVELENS_HOME", home).env("NO_COLOR", "1").env_remove("SAVELENS_KEYS");
    c
}

fn run(home: &Path, args: &[&str]) -> String {
    let out = bin(home).args(args).output().expect("spawn savelens");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "savelens {args:?} failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    stdout
}

fn run_fail(home: &Path, args: &[&str]) -> String {
    let out = bin(home).args(args).output().expect("spawn savelens");
    assert!(!out.status.success(), "savelens {args:?} unexpectedly succeeded");
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn save(gold: i64, inventory: &[&str]) -> String {
    let inv: Vec<String> = inventory.iter().map(|i| format!("\"{i}\"")).collect();
    format!(
        "{{\n  \"Player\": {{\n    \"Gold\": {gold},\n    \"Level\": 3,\n    \"Inventory\": [{}]\n  }}\n}}\n",
        inv.join(", ")
    )
}

#[test]
fn full_workflow_through_the_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let saves = tmp.path().join("saves");
    fs::create_dir_all(&saves).unwrap();
    let file = saves.join("slot1.json");
    fs::write(&file, save(1500, &["Sword", "Shield"])).unwrap();

    let out = run(&home, &["track", "Hero Quest", saves.to_str().unwrap(), "-p", "*.json"]);
    assert!(out.contains("tracking Hero Quest"), "{out}");
    let out = run(&home, &["games"]);
    assert!(out.contains("Hero Quest"));

    // The game spends gold and loses its shield.
    fs::write(&file, save(1300, &["Sword"])).unwrap();
    let out = run(&home, &["snapshot", "hero", "-m", "after shop"]);
    assert!(out.contains("-200 Gold"), "{out}");

    // Scanner: 1500 -> 1300 across two revisions finds Player.Gold.
    let out = run(&home, &["scan", "hero", "slot1.json", "HEAD~1=1500", "HEAD=1500->1300", "--pin", "Oro"]);
    assert!(out.contains("Player.Gold"), "{out}");
    assert!(out.contains("pinned Player.Gold as `Oro`"), "{out}");
    let out = run(&home, &["cheats", "hero"]);
    assert!(out.contains("Oro") && out.contains("1300"), "{out}");

    // Apply & sign.
    let out = run(&home, &["set", "hero", "Oro", "99999"]);
    assert!(out.contains("slot1.json updated"), "{out}");
    assert!(fs::read_to_string(&file).unwrap().contains("\"Gold\": 99999,"));

    // Path-based edit and get.
    run(&home, &["edit", "hero", "slot1.json", "Player.Level", "50"]);
    let out = run(&home, &["get", "hero", "slot1.json", "Player.Level"]);
    assert!(out.contains("50"), "{out}");

    // Surgical cherry-pick of the inventory from the first commit.
    let first = run(&home, &["--json", "log", "hero", "-n", "100"]);
    let log: serde_json::Value = serde_json::from_str(&first).unwrap();
    let oldest = log.as_array().unwrap().last().unwrap()["id"].as_i64().unwrap();
    run(&home, &["cherry-pick", "hero", &oldest.to_string(), "slot1.json", "Player.Inventory"]);
    let now = fs::read_to_string(&file).unwrap();
    assert!(now.contains("\"Shield\""), "{now}");
    assert!(now.contains("99999"), "gold kept: {now}");

    // Semantic log / diff.
    let out = run(&home, &["log", "hero", "--full"]);
    assert!(out.contains("cherry-pick"), "{out}");
    let out = run(&home, &["diff", "hero", &oldest.to_string(), "WORKING"]);
    assert!(out.contains("Oro: 1500 -> 99999"), "{out}");

    // Panic restore undoes the cherry-pick (latest mutation).
    let before_cherry = save(99999, &["Sword"]).replace("\"Level\": 3", "\"Level\": 50");
    let out = run(&home, &["panic-restore", "--game", "hero"]);
    assert!(out.contains("PANIC RESTORE"), "{out}");
    assert_eq!(fs::read_to_string(&file).unwrap(), before_cherry);
    let out = run(&home, &["backups"]);
    assert!(out.contains("safety/pre-mutation-"), "{out}");

    // Branches & inspect & maintenance.
    run(&home, &["branch", "hero", "speedrun", "--from", &oldest.to_string()]);
    let out = run(&home, &["branch", "hero"]);
    assert!(out.contains("speedrun") && out.contains("* main"), "{out}");
    let out = run(&home, &["inspect", file.to_str().unwrap()]);
    assert!(out.contains("codec      json") && out.contains("Lossless"), "{out}");
    run(&home, &["verify"]);
    run(&home, &["gc"]);
    let out = run(&home, &["status"]);
    assert!(out.contains("dedup"), "{out}");
    let out = run(&home, &["codecs"]);
    for id in ["json", "gvas", "nrbf", "sqlite", "nbt", "msgpack", "ini", "xml", "plist", "raw"] {
        assert!(out.contains(id), "codec {id} missing: {out}");
    }

    // Only the save file lives in the game directory.
    let names: Vec<String> = fs::read_dir(&saves).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    assert_eq!(names, vec!["slot1.json".to_string()]);
}

#[test]
fn errors_are_reported_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let err = run_fail(&home, &["log", "nope"]);
    assert!(err.contains("error:") && err.contains("not found"), "{err}");
    let err = run_fail(&home, &["track", "x", tmp.path().join("missing").to_str().unwrap()]);
    assert!(err.contains("does not exist"), "{err}");
    let err = run_fail(&home, &["panic-restore"]);
    assert!(err.contains("no SaveLens mutation"), "{err}");
}
