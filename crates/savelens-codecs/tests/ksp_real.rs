//! KSP ConfigNode integration test on a real `persistent.sfs` (a sandbox
//! save: `GAME.Mode = SANDBOX`, so there is no `Funding` scenario; the test
//! edits the universal time and a vessel instead).
//!
//! The corpus lives in `tests/real-saves/corpus/kerbal-space-program/` at
//! the workspace root (override with `SAVELENS_CORPUS=<corpus dir>`). When
//! it is missing, the test prints a note and passes.

use std::path::PathBuf;

use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

fn save_file() -> Option<PathBuf> {
    let base = match std::env::var_os("SAVELENS_CORPUS") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/real-saves/corpus"),
    };
    let dir = base.join("kerbal-space-program");
    if dir.is_dir() {
        Some(dir.join("persistent.sfs"))
    } else {
        assert!(
            std::env::var_os("SAVELENS_REQUIRE_REAL_SAVES").is_none(),
            "real-save corpus required but not found at {}",
            dir.display()
        );
        eprintln!("real-save corpus not found at {}; skipping", dir.display());
        None
    }
}

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

fn lines(b: &[u8]) -> Vec<&[u8]> {
    b.split_inclusive(|&c| c == b'\n').collect()
}

#[test]
fn persistent_sfs_round_trips_and_edits() {
    let Some(path) = save_file() else { return };
    let data = std::fs::read(&path).unwrap();
    let ctx = DetectContext::for_file("persistent.sfs");
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&data, &ctx).unwrap();
    assert_eq!(s.codec_id(), "confignode", "{:?}", s.envelope().notes);
    assert!(s.layers().is_empty());
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), data);

    let root = s.root();
    assert_eq!(root.get(&p("GAME.Mode")), Some(&Node::String("SANDBOX".into())));
    assert_eq!(root.get(&p("GAME.FLIGHTSTATE.UT")), Some(&Node::F64(63290.1817719101)));
    assert_eq!(root.get(&p("GAME.PARAMETERS.FLIGHT.CanQuickSave")), Some(&Node::Bool(true)));

    // The station "LY" among the repeated VESSEL nodes.
    let Some(Node::Map(flight)) = root.get(&p("GAME.FLIGHTSTATE")) else { panic!("no FLIGHTSTATE") };
    let vessel = flight
        .iter()
        .find(|(k, v)| k.starts_with("VESSEL") && v.get(&p("name")) == Some(&Node::String("LY".into())))
        .map(|(k, _)| k.clone())
        .expect("vessel LY");
    assert!(vessel.starts_with("VESSEL#"), "{vessel}");
    let vp = |field: &str| p(&format!("GAME.FLIGHTSTATE[\"{vessel}\"].{field}"));
    assert_eq!(root.get(&vp("type")), Some(&Node::String("Station".into())));

    let mut edited = root.clone();
    edited.set(&p("GAME.FLIGHTSTATE.UT"), Node::F64(100_000.25), false).unwrap();
    edited.set(&vp("name"), Node::String("LY Station Prime".into()), false).unwrap();
    edited.set(&vp("landed"), Node::Bool(true), false).unwrap();
    s.apply_ast(&edited).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();

    // Only the three edited lines differ; every other line is byte-identical.
    let (a, b) = (lines(&data), lines(&out));
    assert_eq!(a.len(), b.len());
    let changed: Vec<(String, String)> = a
        .iter()
        .zip(&b)
        .filter(|(x, y)| x != y)
        .map(|(x, y)| (String::from_utf8_lossy(x).into_owned(), String::from_utf8_lossy(y).into_owned()))
        .collect();
    assert_eq!(changed.len(), 3, "{changed:?}");
    assert_eq!(changed[0], ("\t\tUT = 63290.1817719101\r\n".into(), "\t\tUT = 100000.2500000000\r\n".into()));
    assert!(changed.iter().any(|(x, y)| x == "\t\t\tname = LY\r\n" && y == "\t\t\tname = LY Station Prime\r\n"));
    assert!(changed.iter().any(|(x, y)| x == "\t\t\tlanded = False\r\n" && y == "\t\t\tlanded = True\r\n"));

    let re = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(re.codec_id(), "confignode");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    let rroot = re.root();
    assert_eq!(rroot.get(&p("GAME.FLIGHTSTATE.UT")), Some(&Node::F64(100_000.25)));
    assert_eq!(rroot.get(&vp("name")), Some(&Node::String("LY Station Prime".into())));
    assert_eq!(rroot.get(&vp("landed")), Some(&Node::Bool(true)));

    // Adding and removing an entry keep the node's tab indentation.
    let mut s = re;
    let mut edited = s.root();
    edited.set(&vp("savelensNote"), Node::String("edited".into()), true).unwrap();
    edited.remove(&vp("landedAt")).unwrap();
    s.apply_ast(&edited).unwrap();
    let out2 = s.encode_verified(&pipeline, &ctx).unwrap();
    let text = String::from_utf8(out2).unwrap();
    assert!(text.contains("\t\t\tsavelensNote = edited\r\n"));
    assert_eq!(lines(text.as_bytes()).len(), lines(&out).len());

    // Nodes themselves cannot be removed.
    let mut edited = s.root();
    edited.remove(&vp("ORBIT")).unwrap();
    assert!(s.apply_ast(&edited).is_err());
}
