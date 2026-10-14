//! Integration tests of the CAS: dedup, commits/branches, cheat table,
//! safety protocol, game-directory isolation and panic restore.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use savelens_cas::*;
use savelens_core::{Endianness, Location, NodePath, ScalarValue, ScanType};

fn listing(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                out.insert(format!("{rel}/"));
                walk(base, &p, out);
            } else {
                out.insert(rel);
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

fn pseudo(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

fn setup() -> (tempfile::TempDir, Store, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("data")).unwrap();
    let game_dir = tmp.path().join("game_saves");
    fs::create_dir_all(&game_dir).unwrap();
    (tmp, store, game_dir)
}

#[test]
fn blobs_roundtrip_and_deduplicate() {
    let (_t, store, _) = setup();
    let a = pseudo(500_000, 7);
    let mut b = a.clone();
    b[250_000] ^= 0xff; // one byte differs
    let ha = store.put_blob(&a).unwrap();
    let s1 = store.stats().unwrap();
    let hb = store.put_blob(&b).unwrap();
    let s2 = store.stats().unwrap();
    assert_ne!(ha, hb);
    assert_eq!(store.get_blob(&ha).unwrap(), a);
    assert_eq!(store.get_blob(&hb).unwrap(), b);
    // Only the chunk containing the changed byte (plus possibly a neighbour) is new.
    assert!(s2.chunks - s1.chunks <= 2, "{} new chunks", s2.chunks - s1.chunks);
    // Storing the same content again is free.
    store.put_blob(&a).unwrap();
    assert_eq!(store.stats().unwrap().chunks, s2.chunks);
    // Empty blob.
    let he = store.put_blob(&[]).unwrap();
    assert_eq!(store.get_blob(&he).unwrap(), Vec::<u8>::new());
    assert!(store.verify_all().unwrap().is_empty());
}

#[test]
fn corrupted_chunk_is_detected() {
    let (tmp, store, _) = setup();
    let data = pseudo(20_000, 3);
    let h = store.put_blob(&data).unwrap();
    let chunk_dir = tmp.path().join("data/chunks");
    let victim = listing(&chunk_dir).into_iter().find(|p| p.ends_with(".zst")).unwrap();
    fs::write(chunk_dir.join(victim), zstd::bulk::compress(b"evil", 3).unwrap()).unwrap();
    assert!(matches!(store.get_blob(&h), Err(CasError::Integrity(_))));
    assert_eq!(store.verify_all().unwrap().len(), 1);
}

#[test]
fn commits_branches_tags_and_log() {
    let (_t, store, game_dir) = setup();
    let g = store.ensure_game("Test Game", Some(413150), &game_dir, &["*.sav".to_string()], "native").unwrap();
    assert_eq!(g.current_branch, "main");
    let ch = |name: &str, data: &[u8]| FileChange {
        rel_path: name.to_string(),
        content: Some(data.to_vec()),
        mtime: None,
    };
    let c1 = store
        .create_commit(g.id, "main", &[ch("slot1.sav", b"v1"), ch("slot2.sav", b"other")], CommitKind::Import, "import", "", &[])
        .unwrap()
        .unwrap();
    // Identical snapshot: no commit.
    assert!(store
        .create_commit(g.id, "main", &[ch("slot1.sav", b"v1")], CommitKind::Auto, "same", "", &[])
        .unwrap()
        .is_none());
    let log = vec![Changelog {
        rel_path: "slot1.sav".into(),
        summary: "+500 Gold".into(),
        lines: vec!["Player.Gold: 1000 -> 1500 (+500)".into()],
    }];
    let c2 = store
        .create_commit(g.id, "main", &[ch("slot1.sav", b"v2")], CommitKind::Auto, "game saved", "+500 Gold", &log)
        .unwrap()
        .unwrap();
    assert_eq!(c2.parent, Some(c1.id));
    // Inherited file.
    let files = store.commit_files(c2.id).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(store.read_commit_file(c2.id, "slot2.sav").unwrap(), b"other");
    assert_eq!(store.read_commit_file(c1.id, "slot1.sav").unwrap(), b"v1");
    assert_eq!(store.changelogs(c2.id).unwrap(), log);

    // Branch from c1, commit there.
    store.create_branch(g.id, "experiment", Some(c1.id)).unwrap();
    assert!(store.create_branch(g.id, "experiment", None).is_err());
    assert!(store.create_branch(g.id, "safety/x", None).is_err());
    let c3 = store
        .create_commit(g.id, "experiment", &[ch("slot1.sav", b"v1-alt")], CommitKind::Manual, "alt", "", &[])
        .unwrap()
        .unwrap();
    assert_eq!(c3.parent, Some(c1.id));
    assert_eq!(store.log(g.id, "main", 10).unwrap().iter().map(|c| c.id).collect::<Vec<_>>(), vec![c2.id, c1.id]);
    assert_eq!(store.log(g.id, "experiment", 10).unwrap().len(), 2);

    // Revisions.
    assert_eq!(store.resolve_rev(g.id, "HEAD").unwrap(), c2.id);
    assert_eq!(store.resolve_rev(g.id, "HEAD~1").unwrap(), c1.id);
    assert_eq!(store.resolve_rev(g.id, "experiment").unwrap(), c3.id);
    assert_eq!(store.resolve_rev(g.id, &c1.id.to_string()).unwrap(), c1.id);
    store.create_tag(g.id, "before-boss", c2.id).unwrap();
    assert_eq!(store.resolve_rev(g.id, "before-boss").unwrap(), c2.id);
    assert!(store.create_tag(g.id, "safety/pre-mutation-x", c2.id).is_err());
    assert!(store.resolve_rev(g.id, "HEAD~9").is_err());

    store.set_current_branch(g.id, "experiment").unwrap();
    assert!(store.delete_branch(g.id, "experiment").is_err());
    store.set_current_branch(g.id, "main").unwrap();
    store.delete_branch(g.id, "experiment").unwrap();

    // Game lookup.
    assert_eq!(store.find_game("413150").unwrap().id, g.id);
    assert_eq!(store.find_game("test").unwrap().id, g.id);
    assert!(store.find_game("zzz").is_err());

    // GC keeps referenced blobs, drops unreferenced ones.
    store.put_blob(b"orphan blob").unwrap();
    let (blobs, _) = store.gc().unwrap();
    assert_eq!(blobs, 2, "orphan + the deleted branch's v1-alt");
    assert_eq!(store.read_commit_file(c2.id, "slot1.sav").unwrap(), b"v2");
}

#[test]
fn cheat_table_crud() {
    let (_t, store, game_dir) = setup();
    let g = store.ensure_game("G", None, &game_dir, &[], "native").unwrap();
    let loc = Location::Raw {
        region: NodePath::parse("Blob").unwrap(),
        offset: 0x1a4,
        ty: ScanType::I32,
        endian: Endianness::Little,
    };
    store.upsert_cheat(g.id, "slot1.sav", "Oro", &loc).unwrap();
    store.set_freeze(g.id, "Oro", Some(ScalarValue::Int(99999)), true).unwrap();
    let e = store.cheat_by_alias(g.id, "Oro").unwrap();
    assert_eq!(e.location, loc);
    assert_eq!(e.frozen_value, Some(ScalarValue::Int(99999)));
    assert!(e.freeze_enabled);
    store.rename_cheat(g.id, "Oro", "Gold").unwrap();
    store.toggle_freeze(g.id, "Gold", false).unwrap();
    let all = store.cheats(g.id).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].alias, "Gold");
    assert!(!all[0].freeze_enabled);
    store.delete_cheat(g.id, "Gold").unwrap();
    assert!(store.cheats(g.id).unwrap().is_empty());
    assert!(store.delete_cheat(g.id, "Gold").is_err());
}

#[test]
fn safe_write_never_dirties_the_game_directory_and_panic_restores_bit_for_bit() {
    let (_t, store, game_dir) = setup();
    let g = store.ensure_game("G", None, &game_dir, &[], "native").unwrap();
    let save = game_dir.join("slot1.sav");
    let original = pseudo(100_000, 11);
    fs::write(&save, &original).unwrap();
    fs::write(game_dir.join("settings.ini"), b"[a]\nb=1\n").unwrap();
    let before_listing = listing(&game_dir);
    let original_hash = blake3::hash(&original).to_hex().to_string();

    // Three successive mutations.
    let v1 = pseudo(100_000, 12);
    let v2 = pseudo(90_000, 13);
    let m1 = store.safe_write(Some(g.id), &save, &v1, "edit gold").unwrap();
    assert!(m1.safety.verified);
    assert!(m1.safety.tag.starts_with("safety/pre-mutation-"));
    assert_eq!(m1.safety.blob, original_hash);
    let m2 = store.safe_write(Some(g.id), &save, &v2, "cherry-pick").unwrap();
    assert_ne!(m1.safety.tag, m2.safety.tag);
    assert_eq!(fs::read(&save).unwrap(), v2);

    // Invariant 1: no extra files in the game directory, ever.
    assert_eq!(listing(&game_dir), before_listing);
    // Staging is empty after successful writes.
    assert!(listing(store.staging_dir()).is_empty());

    // Panic restore walks back: first to v1, then to the original.
    let r1 = store.panic_restore(&PanicScope::Game(g.id)).unwrap();
    assert_eq!(fs::read(&save).unwrap(), v1);
    assert_eq!(r1.restored_tag, m2.safety.tag);
    let r2 = store.panic_restore(&PanicScope::Latest).unwrap();
    assert_eq!(r2.restored_hash, original_hash);
    assert_eq!(fs::read(&save).unwrap(), original, "bit-for-bit original");
    assert!(matches!(store.panic_restore(&PanicScope::Latest), Err(CasError::NothingToRestore(_))));
    assert_eq!(listing(&game_dir), before_listing);

    // The replaced states are themselves backed up (panic is undoable).
    let replaced = store.safety_by_tag(&r2.replaced_state_tag).unwrap();
    assert_eq!(store.get_blob(&replaced.blob).unwrap(), v1);
    store.restore_safety_tag(&r2.replaced_state_tag).unwrap();
    assert_eq!(fs::read(&save).unwrap(), v1);
}

#[test]
fn panic_restore_of_created_file_removes_it() {
    let (_t, store, game_dir) = setup();
    let newfile = game_dir.join("new.sav");
    store.safe_write(None, &newfile, b"created by savelens", "restore").unwrap();
    assert!(newfile.exists());
    let r = store.panic_restore(&PanicScope::Path(newfile.clone())).unwrap();
    assert!(r.removed);
    assert!(!newfile.exists());
}

#[cfg(unix)]
#[test]
fn permissions_are_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, store, game_dir) = setup();
    let save = game_dir.join("ro.sav");
    fs::write(&save, b"abc").unwrap();
    fs::set_permissions(&save, fs::Permissions::from_mode(0o640)).unwrap();
    store.safe_write(None, &save, b"abcd", "edit").unwrap();
    assert_eq!(fs::metadata(&save).unwrap().permissions().mode() & 0o777, 0o640);
}

#[test]
fn events_requests_and_heartbeat() {
    let (_t, store, _) = setup();
    store.log_event(None, "info", "hello").unwrap();
    let ev = store.events(None, 10).unwrap();
    assert_eq!(ev[0].message, "hello");
    assert!(store.events(Some(ev[0].id), 10).unwrap().is_empty());
    assert!(store.daemon_status(5_000).unwrap().is_none());
    store.heartbeat("watching 2 games").unwrap();
    assert_eq!(store.daemon_status(5_000).unwrap().unwrap().info, "watching 2 games");
    store.clear_heartbeat().unwrap();
    assert!(store.daemon_status(5_000).unwrap().is_none());
    store.push_request("panic-restore", &serde_json::json!({"game": 1})).unwrap();
    let reqs = store.take_requests().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].1, "panic-restore");
    assert!(store.take_requests().unwrap().is_empty());
    // Selective consumption leaves other consumers' requests pending.
    store.push_request("toggle-overlay", &serde_json::json!({})).unwrap();
    store.push_request("snapshot", &serde_json::json!({"game": 2})).unwrap();
    let mine = store.take_requests_for(&["snapshot", "panic-restore"]).unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].2["game"], 2);
    let rest = store.take_requests_for(&["toggle-overlay"]).unwrap();
    assert_eq!(rest.len(), 1);
    assert!(store.take_requests().unwrap().is_empty());
}

#[test]
fn store_is_shared_across_handles_and_processes() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Store::open(tmp.path()).unwrap();
    let b = Store::open(tmp.path()).unwrap();
    let h = a.put_blob(b"shared").unwrap();
    assert_eq!(b.get_blob(&h).unwrap(), b"shared");
    let clone = a.clone();
    let t = std::thread::spawn(move || clone.put_blob(b"from thread").unwrap());
    let h2 = t.join().unwrap();
    assert_eq!(b.get_blob(&h2).unwrap(), b"from thread");
}

#[test]
fn stats_report_dedup() {
    let (_t, store, game_dir) = setup();
    let g = store.ensure_game("G", None, &game_dir, &[], "native").unwrap();
    let base = pseudo(200_000, 5);
    for i in 0..5u8 {
        let mut v = base.clone();
        v[1000] = i;
        store
            .create_commit(
                g.id,
                "main",
                &[FileChange {
                    rel_path: "s.sav".into(),
                    content: Some(v),
                    mtime: None,
                }],
                CommitKind::Auto,
                "save",
                "",
                &[],
            )
            .unwrap();
    }
    let s = store.stats().unwrap();
    assert_eq!(s.commits, 5);
    assert_eq!(s.logical_bytes, 1_000_000);
    assert!(s.dedup_ratio() > 3.0, "ratio {}", s.dedup_ratio());
    assert_eq!(store.game_logical_bytes(g.id).unwrap(), 1_000_000);
}
