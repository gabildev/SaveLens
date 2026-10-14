//! Real-file tests for the BND4 (Elden Ring `.sl2`) and Anvil (Minecraft
//! `.mca`) container codecs. The corpus lives in `tests/real-saves/corpus`;
//! the tests are skipped (with a message) when it is not present. The
//! `SAVELENS_CORPUS` environment variable can point to a corpus
//! elsewhere (e.g. from an isolated work tree).
//!
//! The byte-level checks use test-side parsers that are independent of the
//! codec implementations.

use std::path::PathBuf;

use md5::{Digest, Md5};
use savelens_codecs::{DetectContext, EnvelopePipeline, OpenedSave};
use savelens_core::{Endianness, Location, Node, NodePath, RoundTripStatus, ScalarValue, ScanType};

fn corpus() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/real-saves/corpus");
    if dir.is_dir() {
        return Some(dir);
    }
    match std::env::var_os("SAVELENS_CORPUS").map(PathBuf::from) {
        Some(alt) if alt.is_dir() => Some(alt),
        _ => {
            assert!(
                std::env::var_os("SAVELENS_REQUIRE_REAL_SAVES").is_none(),
                "real-save corpus required but {} is missing",
                dir.display()
            );
            eprintln!("skipping real-save tests: corpus directory {} is missing", dir.display());
            None
        }
    }
}

fn read(path: &PathBuf) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(b) => Some(b),
        Err(e) => {
            assert!(
                std::env::var_os("SAVELENS_REQUIRE_REAL_SAVES").is_none(),
                "real-save corpus required but {} is unreadable: {e}",
                path.display()
            );
            eprintln!("skipping {}: {e}", path.display());
            None
        }
    }
}

fn map_keys(n: &Node) -> Vec<String> {
    match n {
        Node::Map(m) => m.keys().cloned().collect(),
        other => panic!("expected a map, got {}", other.kind().name()),
    }
}

// ---------------------------------------------------------------------------
// Elden Ring
// ---------------------------------------------------------------------------

/// Test-side BND4 entry table for the Elden Ring layout (0x20-byte headers).
fn er_entries(file: &[u8]) -> Vec<(usize, usize)> {
    assert_eq!(&file[..4], b"BND4");
    let count = u32::from_le_bytes(file[0x0c..0x10].try_into().unwrap()) as usize;
    (0..count)
        .map(|i| {
            let h = 0x40 + i * 0x20;
            let size = u64::from_le_bytes(file[h + 8..h + 16].try_into().unwrap()) as usize;
            let off = u32::from_le_bytes(file[h + 16..h + 20].try_into().unwrap()) as usize;
            (off, size)
        })
        .collect()
}

fn md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finalize().into()
}

#[test]
fn elden_ring_sl2_round_trip_and_raw_slot_edit() {
    let Some(dir) = corpus() else { return };
    let Some(file) = read(&dir.join("elden-ring/ER0000.sl2")) else { return };
    let ctx = DetectContext::for_file("ER0000.sl2");
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&file, &ctx).unwrap();
    assert_eq!(s.codec_id(), "bnd4");
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), file);

    let root = s.root();
    let names = map_keys(&root);
    let expected: Vec<String> = (0..12).map(|i| format!("USER_DATA{i:03}")).collect();
    assert_eq!(names, expected);
    // Every entry is a raw region behind an auto-detected MD5 header layer.
    for name in &names {
        assert!(matches!(root.get(&NodePath::root().key(name.as_str())), Some(Node::RawRegion(_))), "{name}");
        assert!(
            s.envelope().notes.iter().any(|n| n.starts_with(&format!("{name}: raw via Checksum")) && n.contains("MD5")),
            "{name}: {:?}",
            s.envelope().notes
        );
    }
    for n in &s.envelope().notes {
        eprintln!("note: {n}");
    }

    // Raw primitive edit inside USER_DATA000 (offset relative to the slot
    // data after the MD5 header).
    let offset = 0x100u64;
    let loc = Location::Raw {
        region: NodePath::root().key("USER_DATA000"),
        offset,
        ty: ScanType::U32,
        endian: Endianness::Little,
    };
    let Some(ScalarValue::Int(old)) = loc.read(&root) else { panic!("cannot read the slot") };
    let new = if old == 4_242_424 { 4_242_425 } else { 4_242_424 };
    s.write_location(&loc, ScalarValue::Int(new)).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    assert_eq!(out.len(), file.len());

    let before = er_entries(&file);
    let after = er_entries(&out);
    assert_eq!(before, after, "entry table unchanged");
    let (off0, size0) = before[0];
    let slot = &out[off0..off0 + size0];
    let o = 16 + offset as usize;
    assert_eq!(&slot[o..o + 4], &(new as u32).to_le_bytes());
    assert_eq!(&slot[..16], &md5(&slot[16..]), "slot MD5 recomputed");
    assert_ne!(&slot[..16], &file[off0..off0 + 16], "MD5 changed");
    for (i, &(off, size)) in before.iter().enumerate().skip(1) {
        assert_eq!(&out[off..off + size], &file[off..off + size], "entry {i} byte-identical");
    }
    assert_eq!(&out[..off0], &file[..off0], "header and name table identical");
    assert_eq!(&out[off0 + size0..], &file[off0 + size0..]);
}

// ---------------------------------------------------------------------------
// Minecraft
// ---------------------------------------------------------------------------

const SECTOR: usize = 4096;

fn location(file: &[u8], index: usize) -> u32 {
    u32::from_be_bytes(file[index * 4..index * 4 + 4].try_into().unwrap())
}

/// Allocated bytes of chunk `index` as the header describes them.
fn chunk_area(file: &[u8], index: usize) -> &[u8] {
    let loc = location(file, index);
    let start = (loc >> 8) as usize * SECTOR;
    let end = (start + (loc & 0xff) as usize * SECTOR).min(file.len());
    &file[start.min(end)..end]
}

fn key_index(key: &str) -> usize {
    let (x, z) = key.split_once(',').unwrap();
    x.parse::<usize>().unwrap() + 32 * z.parse::<usize>().unwrap()
}

/// Path of the chunk's `InhabitedTime` long, or `LastUpdate` for chunks
/// older than 1.0 (pre-1.18 chunks nest both in `Level`).
fn inhabited_path(root: &Node, key: &str) -> Option<NodePath> {
    let base = NodePath::root().key(key);
    for field in ["InhabitedTime", "LastUpdate"] {
        for p in [base.key(field), base.key("Level").key(field)] {
            if matches!(root.get(&p), Some(Node::I64(_))) {
                return Some(p);
            }
        }
    }
    None
}

fn open_region(pipeline: &EnvelopePipeline, name: &str, bytes: &[u8]) -> OpenedSave {
    let s = pipeline.open(bytes, &DetectContext::for_file(name)).unwrap();
    assert_eq!(s.codec_id(), "anvil", "{name}");
    s
}

#[test]
fn minecraft_regions_round_trip_and_nbt_edit() {
    let Some(dir) = corpus() else { return };
    let pipeline = EnvelopePipeline::builtin();
    // (file, expected editable chunks, DataVersion of the edited chunk)
    let cases = [
        ("regiontest.mca", 14usize, None),
        ("r.4.-4.mca", 1, Some(1952)),
        ("r.1.19.4.mca", 676, Some(3337)),
        ("r.0.0.mca", 1, Some(2230)),
    ];
    for (name, expected_chunks, data_version) in cases {
        let Some(file) = read(&dir.join("minecraft").join(name)) else { continue };
        let ctx = DetectContext::for_file(name);
        let mut s = open_region(&pipeline, name, &file);
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless), "{name}");
        assert_eq!(s.encode().unwrap(), file, "{name}");
        let root = s.root();
        let keys: Vec<String> = map_keys(&root).into_iter().filter(|k| k != "$timestamps").collect();
        eprintln!("{name}: {} editable chunks; notes: {:?}", keys.len(), s.envelope().notes);
        assert_eq!(keys.len(), expected_chunks, "{name}");

        // First chunk with an InhabitedTime (or LastUpdate) long.
        let (key, path) = keys
            .iter()
            .find_map(|k| inhabited_path(&root, k).map(|p| (k.clone(), p)))
            .unwrap_or_else(|| panic!("{name}: no chunk with InhabitedTime/LastUpdate"));
        eprintln!("{name}: editing {path}");
        if let Some(dv) = data_version {
            assert_eq!(
                root.get(&NodePath::root().key(key.as_str()).key("DataVersion")),
                Some(&Node::I32(dv)),
                "{name}"
            );
        }
        let Some(Node::I64(old)) = root.get(&path).cloned() else { unreachable!() };
        let new = old + 123_456_789;
        s.set(&path, Node::I64(new)).unwrap();
        let out = s.encode_verified(&pipeline, &ctx).unwrap();
        let re = open_region(&pipeline, name, &out);
        assert_eq!(re.root().get(&path), Some(&Node::I64(new)), "{name}");
        let edited = key_index(&key);
        // An 8-byte value change keeps the chunk in place.
        assert_eq!(out.len(), file.len(), "{name}");
        assert_eq!(location(&out, edited), location(&file, edited));
        assert_ne!(chunk_area(&out, edited), chunk_area(&file, edited));
        for k in keys.iter().filter(|k| **k != key) {
            let i = key_index(k);
            assert_eq!(location(&out, i), location(&file, i), "{name} {k}");
            assert_eq!(chunk_area(&out, i), chunk_area(&file, i), "{name} {k} byte-identical");
        }
        assert_eq!(&out[SECTOR..2 * SECTOR], &file[SECTOR..2 * SECTOR], "{name}: timestamps kept");
        // Every other chunk decodes to the same tree.
        for k in keys.iter().filter(|k| **k != key).take(20) {
            let p = NodePath::root().key(k.as_str());
            assert!(savelens_core::bit_equal(root.get(&p).unwrap(), re.root().get(&p).unwrap()), "{name} {k}");
        }
    }
}

#[test]
fn minecraft_chunk_growth_relocates_to_end_of_file() {
    let Some(dir) = corpus() else { return };
    let name = "r.1.19.4.mca";
    let Some(file) = read(&dir.join("minecraft").join(name)) else { return };
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::for_file(name);
    let mut s = open_region(&pipeline, name, &file);
    let root = s.root();
    let key = "0,0";
    let edited = key_index(key);
    let old_loc = location(&file, edited);
    // Incompressible blob larger than the chunk's allocated sectors.
    let alloc = (old_loc & 0xff) as usize * SECTOR;
    let mut x = 0x9e37_79b9u32;
    let blob: Vec<u8> = (0..alloc + 3 * SECTOR)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let path = NodePath::root().key(key).key("SaveLensBlob");
    let mut edited_root = root.clone();
    edited_root.set(&path, Node::Bytes(blob.clone()), true).unwrap();
    s.apply_ast(&edited_root).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();

    let new_loc = location(&out, edited);
    assert_eq!((new_loc >> 8) as usize, file.len() / SECTOR, "appended after the last sector");
    assert!((new_loc & 0xff) as usize * SECTOR > alloc);
    assert_eq!(out.len() % SECTOR, 0);
    assert_eq!(out.len(), file.len() + (new_loc & 0xff) as usize * SECTOR);
    // The original body (including the stale old sectors) is untouched.
    assert_eq!(&out[2 * SECTOR..file.len()], &file[2 * SECTOR..]);
    for i in (0..1024).filter(|&i| i != edited) {
        assert_eq!(location(&out, i), location(&file, i));
    }
    assert_eq!(&out[SECTOR..2 * SECTOR], &file[SECTOR..2 * SECTOR]);
    let re = open_region(&pipeline, name, &out);
    assert_eq!(re.root().get(&path), Some(&Node::Bytes(blob)));
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
}
