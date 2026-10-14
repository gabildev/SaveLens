//! Ren'Py saves and raw Python pickles from the real-save corpus.
//!
//! The corpus lives in `tests/real-saves/corpus` at the workspace root
//! (`CARGO_MANIFEST_DIR/../../tests/real-saves/corpus`); set
//! `SAVELENS_CORPUS` to point at another copy. The tests are skipped with
//! a message when the corpus is not available.

use std::path::PathBuf;

use savelens_codecs::codecs::zip::ZipDocument;
use savelens_codecs::{DetectContext, EnvelopePipeline, OpenedSave};
use savelens_core::{Node, NodePath, RoundTripStatus};

const SAVES: [&str; 3] = ["renrpg-1-1-LT1.save", "karryn-1-1-LT1.save", "rpgsaveeditor-1-1-LT1.save"];

fn corpus() -> Option<PathBuf> {
    let base = match std::env::var_os("SAVELENS_CORPUS") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/real-saves/corpus"),
    };
    let dir = base.join("renpy");
    if dir.is_dir() {
        Some(dir)
    } else {
        assert!(
            std::env::var_os("SAVELENS_REQUIRE_REAL_SAVES").is_none(),
            "real-save corpus required but {} is missing",
            dir.display()
        );
        eprintln!("skipping Ren'Py real-save tests: corpus directory {} is missing", dir.display());
        None
    }
}

fn store_path(prefix: &NodePath, var: &str) -> NodePath {
    prefix.index(0).key(var)
}

/// `store.*` leaves of the Ren'Py roots dict: (name, value).
fn store_vars(root: &Node, prefix: &NodePath) -> Vec<(String, Node)> {
    let Some(Node::Map(store)) = root.get(&prefix.index(0)) else { return Vec::new() };
    store.iter().filter(|(k, v)| k.starts_with("store.") && v.is_leaf()).map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn describe(vars: &[(String, Node)]) -> String {
    vars.iter()
        .filter(|(_, v)| matches!(v, Node::I64(_) | Node::F64(_) | Node::String(_) | Node::Bool(_)))
        .filter(|(_, v)| !matches!(v, Node::String(s) if s.starts_with('$')))
        .take(25)
        .map(|(k, v)| format!("{k}={}", v.preview(24)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Picks a numeric store variable (well-known names first).
fn numeric_var(vars: &[(String, Node)]) -> Option<(String, i64)> {
    let numeric: Vec<(String, i64)> = vars
        .iter()
        .filter_map(|(k, v)| match v {
            Node::I64(x) => Some((k.clone(), *x)),
            _ => None,
        })
        .collect();
    for wanted in ["store.money", "store.gold", "store.cash"] {
        if let Some(found) = numeric.iter().find(|(k, _)| k == wanted) {
            return Some(found.clone());
        }
    }
    numeric.into_iter().next()
}

fn check_lossless(s: &OpenedSave, bytes: &[u8], codec: &str) {
    assert_eq!(s.codec_id(), codec);
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless), "{:?}", s.envelope().notes);
    assert_eq!(s.encode().unwrap(), bytes, "encode() reproduces the file");
}

#[test]
fn renpy_saves_round_trip_and_edit() {
    let Some(dir) = corpus() else { return };
    let pipeline = EnvelopePipeline::builtin();
    for name in SAVES {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        let ctx = DetectContext::for_file(name);
        let s = pipeline.open(&bytes, &ctx).unwrap();
        check_lossless(&s, &bytes, "zip");

        let zip = ZipDocument::parse(&bytes, &ctx).unwrap();
        assert_eq!(zip.entry_codec("log"), Some("pickle"), "{name}: {:?}", s.envelope().notes);
        if zip.entry_names().iter().any(|k| k == "json") {
            assert_eq!(zip.entry_codec("json"), Some("json"), "{name}");
            assert!(matches!(s.root().get(&NodePath::root().key("json")), Some(Node::Map(_))));
        }
        let log = NodePath::root().key("log");
        let root = s.root();
        let vars = store_vars(&root, &log);
        assert!(!vars.is_empty(), "{name}: no store variables in the log pickle");
        eprintln!(
            "{name}: entries {:?}; log exact re-deflate: {:?}; {} store variables: {}",
            zip.entry_names(),
            zip.entry_exact_recompression("log"),
            vars.len(),
            describe(&vars)
        );

        let (var, old) = numeric_var(&vars).unwrap_or_else(|| panic!("{name}: no numeric store variable"));
        let new = old + 100_000;
        let path = store_path(&log, &var);
        let mut edited = s.clone();
        edited.set(&path, Node::I64(new)).unwrap();
        let out = edited.encode_verified(&pipeline, &ctx).unwrap();
        assert_ne!(out, bytes);

        let re = pipeline.open(&out, &ctx).unwrap();
        assert_eq!(re.codec_id(), "zip");
        assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(re.root().get(&path), Some(&Node::I64(new)), "{name}: {var}");
        let others: Vec<(String, Node)> = store_vars(&re.root(), &log).into_iter().filter(|(k, _)| *k != var).collect();
        let before: Vec<(String, Node)> = vars.iter().filter(|(k, _)| *k != var).cloned().collect();
        assert_eq!(others, before, "{name}: other variables unchanged");

        let rebuilt = ZipDocument::parse(&out, &ctx).unwrap();
        assert_eq!(rebuilt.entry_names(), zip.entry_names());
        for key in zip.entry_names() {
            if key == "log" {
                assert_ne!(rebuilt.entry_data(key), zip.entry_data(key));
                continue;
            }
            assert_eq!(rebuilt.entry_stored_bytes(key), zip.entry_stored_bytes(key), "{name}: {key} untouched");
        }
        assert!(zip.entry_names().iter().any(|k| k == "screenshot.png"));
        assert_eq!(rebuilt.entry_stored_bytes("screenshot.png"), zip.entry_stored_bytes("screenshot.png"));
        eprintln!("{name}: edited {var} {old} -> {new}, archive {} -> {} bytes", bytes.len(), out.len());
        if let Some(dump) = std::env::var_os("SAVELENS_TEST_DUMP") {
            std::fs::write(PathBuf::from(dump).join(format!("edited-{name}")), &out).unwrap();
        }
    }
}

#[test]
fn raw_pickles_round_trip_and_edit() {
    let Some(dir) = corpus() else { return };
    let pipeline = EnvelopePipeline::builtin();
    for name in ["p2.pickle", "p5.pickle"] {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        let ctx = DetectContext::for_file(name);
        let s = pipeline.open(&bytes, &ctx).unwrap();
        check_lossless(&s, &bytes, "pickle");
        let root = s.root();
        let vars = store_vars(&root, &NodePath::root());
        eprintln!("{name}: {} store variables: {}", vars.len(), describe(&vars));
        let p = |v: &str| store_path(&NodePath::root(), v);
        assert_eq!(root.get(&p("store.money")), Some(&Node::I64(120)));
        assert_eq!(root.get(&p("store.mc_name")), Some(&Node::String("Alex".into())));
        assert_eq!(root.get(&p("store.ratio")), Some(&Node::F64(0.5)));

        let mut edited = s.clone();
        edited.set(&p("store.money"), Node::I64(999_999)).unwrap();
        edited.set(&p("store.mc_name"), Node::String("Zed ".repeat(80))).unwrap();
        edited.set(&p("store.ratio"), Node::F64(0.75)).unwrap();
        let out = edited.encode_verified(&pipeline, &ctx).unwrap();
        let re = pipeline.open(&out, &ctx).unwrap();
        check_lossless(&re, &out, "pickle");
        let r = re.root();
        assert_eq!(r.get(&p("store.money")), Some(&Node::I64(999_999)));
        assert_eq!(r.get(&p("store.mc_name")), Some(&Node::String("Zed ".repeat(80))));
        assert_eq!(r.get(&p("store.ratio")), Some(&Node::F64(0.75)));
        let untouched = |vs: Vec<(String, Node)>| -> Vec<(String, Node)> {
            vs.into_iter()
                .filter(|(k, _)| !["store.money", "store.mc_name", "store.ratio"].contains(&k.as_str()))
                .collect()
        };
        assert_eq!(untouched(store_vars(&r, &NodePath::root())), untouched(vars));
        if let Some(dump) = std::env::var_os("SAVELENS_TEST_DUMP") {
            std::fs::write(PathBuf::from(dump).join(format!("edited-{name}")), &out).unwrap();
        }
    }
}
