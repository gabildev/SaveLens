//! Real-game save corpus: every file listed in `tests/real-saves/manifest.toml`
//! must open with the expected codec and layers, honour the Safe Edit Gate
//! expectation, re-encode to its exact original bytes, and (when the manifest
//! names an edit) survive a structural edit + `encode_verified` + re-open.
//!
//! The corpus is fetched by `scripts/fetch-real-saves.sh`. When it is missing
//! the test is skipped, unless `SAVELENS_REQUIRE_REAL_SAVES` is set (CI).

use std::path::{Path, PathBuf};

use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Manifest {
    save: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    file: String,
    game: String,
    app_id: Option<u32>,
    codec: String,
    #[serde(default)]
    layers: Vec<String>,
    lossless: bool,
    edit: Option<Edit>,
}

#[derive(Debug, Deserialize)]
struct Edit {
    path: String,
    value: toml::Value,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds a node of the same kind as `current` holding `value`.
fn typed_value(current: &Node, value: &toml::Value) -> Result<Node, String> {
    let int = || value.as_integer().ok_or_else(|| format!("edit value {value} is not an integer"));
    let conv = |r: Result<Node, std::num::TryFromIntError>| r.map_err(|e| e.to_string());
    Ok(match current {
        Node::I8(_) => conv(i8::try_from(int()?).map(Node::I8))?,
        Node::I16(_) => conv(i16::try_from(int()?).map(Node::I16))?,
        Node::I32(_) => conv(i32::try_from(int()?).map(Node::I32))?,
        Node::I64(_) => Node::I64(int()?),
        Node::U8(_) => conv(u8::try_from(int()?).map(Node::U8))?,
        Node::U16(_) => conv(u16::try_from(int()?).map(Node::U16))?,
        Node::U32(_) => conv(u32::try_from(int()?).map(Node::U32))?,
        Node::U64(_) => conv(u64::try_from(int()?).map(Node::U64))?,
        Node::F32(_) => Node::F32(value.as_float().ok_or("edit value is not a float")? as f32),
        Node::F64(_) => Node::F64(value.as_float().ok_or("edit value is not a float")?),
        Node::Bool(_) => Node::Bool(value.as_bool().ok_or("edit value is not a bool")?),
        Node::String(_) => Node::String(value.as_str().ok_or("edit value is not a string")?.to_string()),
        other => return Err(format!("cannot edit a {} node", other.kind().name())),
    })
}

fn check(pipeline: &EnvelopePipeline, corpus: &Path, e: &Entry) -> Result<String, String> {
    let bytes = std::fs::read(corpus.join(&e.file)).map_err(|err| format!("read: {err}"))?;
    let name = Path::new(&e.file).file_name().and_then(|n| n.to_str()).unwrap_or(&e.file).to_string();
    let ctx = DetectContext::for_game(name, e.app_id);
    let opened = pipeline.open(&bytes, &ctx).map_err(|err| format!("open: {err}"))?;
    let layers: Vec<String> = opened.layers().iter().map(|l| l.to_string()).collect();
    let summary = format!("{} via [{}]", opened.codec_id(), layers.join(" → "));

    if opened.codec_id() != e.codec {
        return Err(format!("expected codec {}, got {summary}", e.codec));
    }
    let mut rest = layers.iter();
    for want in &e.layers {
        if !rest.any(|l| l.contains(want.as_str())) {
            return Err(format!("expected layer containing {want:?} (in order), got {summary}"));
        }
    }
    let status = opened.verify_round_trip();
    if status.is_lossless() != e.lossless {
        return Err(format!("expected lossless={}, got {status:?} ({summary})", e.lossless));
    }
    if e.lossless && !matches!(status, RoundTripStatus::Lossless) {
        return Err(format!("unexpected round-trip status {status:?}"));
    }
    let encoded = opened.encode().map_err(|err| format!("encode: {err}"))?;
    if encoded != bytes {
        return Err(format!("unedited encode() differs from the original file ({summary})"));
    }

    if let Some(edit) = &e.edit {
        let path = NodePath::parse(&edit.path).map_err(|err| format!("edit path: {err}"))?;
        let root = opened.root();
        let current = root.get(&path).ok_or_else(|| format!("edit path {} not found", edit.path))?;
        let value = typed_value(current, &edit.value)?;
        let mut work = opened.clone();
        work.set(&path, value.clone()).map_err(|err| format!("edit: {err}"))?;
        let out = work.encode_verified(pipeline, &ctx).map_err(|err| format!("encode_verified: {err}"))?;
        if out == bytes {
            return Err("edited output is identical to the original".into());
        }
        let reopened = pipeline.open(&out, &ctx).map_err(|err| format!("re-open after edit: {err}"))?;
        if reopened.codec_id() != e.codec {
            return Err(format!("re-opened edited file as {}", reopened.codec_id()));
        }
        let got = reopened.root().get(&path).cloned();
        if got.as_ref() != Some(&value) {
            return Err(format!("edited value not found after re-open: {got:?}"));
        }
        let mut before = opened.root();
        before.set(&path, value, false).map_err(|err| err.to_string())?;
        let changes = savelens_core::diff(&before, &reopened.root());
        if !changes.is_empty() {
            return Err(format!("edit changed {} other node(s)", changes.len()));
        }
    }
    Ok(summary)
}

#[test]
fn real_game_saves() {
    let root = repo_root().join("tests/real-saves");
    let manifest: Manifest =
        toml::from_str(&std::fs::read_to_string(root.join("manifest.toml")).expect("manifest.toml is readable"))
            .expect("manifest.toml is valid");
    let corpus = std::env::var_os("SAVELENS_REAL_SAVES").map(PathBuf::from).unwrap_or_else(|| root.join("corpus"));
    let required = std::env::var_os("SAVELENS_REQUIRE_REAL_SAVES").is_some();
    let missing: Vec<&str> =
        manifest.save.iter().filter(|e| !corpus.join(&e.file).is_file()).map(|e| e.file.as_str()).collect();
    if !missing.is_empty() {
        if required {
            panic!("real-save corpus incomplete in {}: missing {missing:?}", corpus.display());
        }
        eprintln!(
            "skipping real-save corpus test: {} file(s) missing in {} (run scripts/fetch-real-saves.sh)",
            missing.len(),
            corpus.display()
        );
        return;
    }

    let pipeline = EnvelopePipeline::builtin();
    let mut failures = Vec::new();
    for e in &manifest.save {
        match check(&pipeline, &corpus, e) {
            Ok(summary) => eprintln!("ok   {:<45} {:<40} {summary}", e.file, e.game),
            Err(err) => {
                eprintln!("FAIL {:<45} {:<40} {err}", e.file, e.game);
                failures.push(format!("{} ({}): {err}", e.file, e.game));
            }
        }
    }
    assert!(failures.is_empty(), "{} real save(s) failed:\n{}", failures.len(), failures.join("\n"));
}
