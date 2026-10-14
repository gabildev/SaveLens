//! Property-based tests for paths, Merkle hashing, diffing, scanning and
//! cherry-picking.

use proptest::prelude::*;
use savelens_core::raw::{read_primitive, write_primitive};
use savelens_core::*;

fn key_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "[A-Za-z_][A-Za-z0-9_]{0,8}",
        // Keys that need quoting.
        "[a-z .\\[\\]\"\\\\]{0,6}",
        any::<String>().prop_map(|s| s.chars().take(6).collect()),
    ]
}

fn seg_strategy() -> impl Strategy<Value = PathSeg> {
    prop_oneof![key_strategy().prop_map(PathSeg::Key), (0usize..1000).prop_map(PathSeg::Index)]
}

fn leaf_strategy() -> impl Strategy<Value = Node> {
    prop_oneof![
        Just(Node::Null),
        any::<bool>().prop_map(Node::Bool),
        any::<i8>().prop_map(Node::I8),
        any::<i16>().prop_map(Node::I16),
        any::<i32>().prop_map(Node::I32),
        any::<i64>().prop_map(Node::I64),
        any::<u8>().prop_map(Node::U8),
        any::<u16>().prop_map(Node::U16),
        any::<u32>().prop_map(Node::U32),
        any::<u64>().prop_map(Node::U64),
        (-1e6f32..1e6).prop_map(Node::F32),
        (-1e9f64..1e9).prop_map(Node::F64),
        "[a-z]{0,8}".prop_map(Node::String),
        proptest::collection::vec(any::<u8>(), 0..32).prop_map(Node::Bytes),
    ]
}

fn tree_strategy() -> impl Strategy<Value = Node> {
    leaf_strategy().prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Node::Array),
            proptest::collection::vec(("[a-z]{1,6}", inner), 0..6).prop_map(|pairs| {
                let mut m = NodeMap::new();
                for (k, v) in pairs {
                    m.insert(k, v);
                }
                Node::Map(m)
            }),
        ]
    })
}

/// Picks the path of one numeric leaf (if any) using `selector`.
fn numeric_leaf_paths(n: &Node) -> Vec<NodePath> {
    n.leaves()
        .into_iter()
        .filter(|(_, l)| l.kind().is_numeric() && !matches!(l, Node::F32(_) | Node::F64(_)))
        .map(|(p, _)| p)
        .collect()
}

/// Changes every scalar leaf while keeping its kind.
fn perturb(n: &mut Node) {
    match n {
        Node::Bool(b) => *b = !*b,
        Node::I8(v) => *v = v.wrapping_add(1),
        Node::I16(v) => *v = v.wrapping_add(1),
        Node::I32(v) => *v = v.wrapping_add(1),
        Node::I64(v) => *v = v.wrapping_add(1),
        Node::U8(v) => *v = v.wrapping_add(1),
        Node::U16(v) => *v = v.wrapping_add(1),
        Node::U32(v) => *v = v.wrapping_add(1),
        Node::U64(v) => *v = v.wrapping_add(1),
        Node::F32(v) => *v += 1.0,
        Node::F64(v) => *v += 1.0,
        Node::String(s) => s.push('!'),
        Node::Bytes(b) => b.push(0),
        Node::Array(a) => a.iter_mut().for_each(perturb),
        Node::Map(m) => m.values_mut().for_each(perturb),
        Node::Null | Node::RawRegion(_) => {}
    }
}

fn non_root_paths(n: &Node) -> Vec<NodePath> {
    let mut v = Vec::new();
    n.walk(|p, _| {
        if !p.is_root() {
            v.push(p.clone());
        }
    });
    v
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn path_display_parse_roundtrip(segs in proptest::collection::vec(seg_strategy(), 0..6)) {
        let p = NodePath::from_segments(segs);
        let text = p.to_string();
        let back = NodePath::parse(&text).unwrap();
        prop_assert_eq!(back, p);
    }

    #[test]
    fn diff_of_identical_trees_is_empty(t in tree_strategy()) {
        prop_assert!(diff(&t, &t.clone()).is_empty());
        prop_assert_eq!(hash_node(&t), hash_node(&t.clone()));
    }

    #[test]
    fn diff_empty_iff_hash_equal(a in tree_strategy(), b in tree_strategy()) {
        let d = diff(&a, &b);
        prop_assert_eq!(d.is_empty(), hash_node(&a) == hash_node(&b));
        prop_assert_eq!(d.is_empty(), bit_equal(&a, &b));
    }

    #[test]
    fn single_leaf_mutation_reported_at_its_path(t in tree_strategy(), pick in any::<prop::sample::Index>(), delta in 1i64..1000) {
        let paths = numeric_leaf_paths(&t);
        prop_assume!(!paths.is_empty());
        let path = paths[pick.index(paths.len())].clone();
        let mut m = t.clone();
        let leaf = m.get(&path).unwrap().clone();
        let cur = leaf.as_scalar().unwrap();
        // Try +delta, fall back to -delta when out of range.
        let new = leaf.with_scalar(cur.plus(ScalarValue::Int(delta as i128)), &path)
            .or_else(|_| leaf.with_scalar(cur.plus(ScalarValue::Int(-(delta as i128))), &path));
        prop_assume!(new.is_ok());
        m.set(&path, new.unwrap(), false).unwrap();
        let d = diff(&t, &m);
        prop_assert_eq!(d.changes.len(), 1);
        prop_assert_eq!(&d.changes[0].path, &path);
        prop_assert!(d.changes[0].numeric_delta().is_some());
    }

    #[test]
    fn primitive_write_read_roundtrip(v in any::<i64>(), off in 0usize..16, be in any::<bool>()) {
        let endian = if be { Endianness::Big } else { Endianness::Little };
        let mut buf = vec![0u8; 32];
        write_primitive(&mut buf, off, ScanType::I64, endian, ScalarValue::Int(v as i128), "r").unwrap();
        prop_assert_eq!(read_primitive(&buf, off, ScanType::I64, endian), Some(ScalarValue::Int(v as i128)));
        let f = v as f32;
        prop_assume!(f.is_normal() || f == 0.0);
        write_primitive(&mut buf, off, ScanType::F32, endian, ScalarValue::Float(f as f64), "r").unwrap();
        prop_assert_eq!(read_primitive(&buf, off, ScanType::F32, endian), Some(ScalarValue::Float(f as f64)));
    }

    /// The scanner is *sound*: a value planted at a random offset and changed by
    /// random deltas across several snapshots is never filtered out, whatever
    /// noise surrounds it.
    #[test]
    fn raw_scanner_never_loses_the_planted_variable(
        noise in proptest::collection::vec(any::<u8>(), 64..512),
        off_sel in any::<prop::sample::Index>(),
        start in 0i32..100_000,
        deltas in proptest::collection::vec(-5000i32..5000, 1..5),
        be in any::<bool>(),
    ) {
        let endian = if be { Endianness::Big } else { Endianness::Little };
        let off = off_sel.index(noise.len() - 4);
        let plant = |v: i32| {
            let mut b = noise.clone();
            let bytes = if be { v.to_be_bytes() } else { v.to_le_bytes() };
            b[off..off + 4].copy_from_slice(&bytes);
            Node::map_from([("Blob", Node::RawRegion(RawRegion::new(b)))])
        };
        let cfg = ScanConfig {
            types: vec![ScanType::I32],
            endians: vec![endian],
            alignment: Alignment::Unaligned,
            ..ScanConfig::default()
        };
        let mut s = ScanSession::new(cfg);
        let mut value = start;
        s.first_scan(&plant(value), &ScanFilter::Exact(ScalarValue::Int(value as i128)), "0").unwrap();
        for (i, d) in deltas.iter().enumerate() {
            let next = value + d;
            let filter = if *d > 0 {
                ScanFilter::IncreasedBy(ScalarValue::Int(*d as i128))
            } else if *d < 0 {
                ScanFilter::DecreasedBy(ScalarValue::Int(-(*d) as i128))
            } else {
                ScanFilter::Unchanged
            };
            s.next_scan(&plant(next), &filter, &i.to_string()).unwrap();
            value = next;
        }
        let target = Location::Raw { region: NodePath::parse("Blob").unwrap(), offset: off as u64, ty: ScanType::I32, endian };
        prop_assert!(s.locations().contains(&target), "planted variable lost");
        prop_assert_eq!(target.read(&plant(value)), Some(ScalarValue::Int(value as i128)));
    }

    /// Unknown-initial-value scans followed by relational filters are sound too.
    #[test]
    fn unknown_scan_then_relational_is_sound(
        noise in proptest::collection::vec(any::<u8>(), 16..128),
        off_sel in any::<prop::sample::Index>(),
        start in 0u32..1000,
        ups in proptest::collection::vec(1u32..50, 1..4),
    ) {
        let off = off_sel.index(noise.len() - 4) & !3;
        let plant = |v: u32| {
            let mut b = noise.clone();
            b[off..off + 4].copy_from_slice(&v.to_le_bytes());
            Node::Bytes(b)
        };
        let cfg = ScanConfig { types: vec![ScanType::U32], alignment: Alignment::Natural, ..ScanConfig::default() };
        let mut s = ScanSession::new(cfg);
        let mut v = start;
        s.first_scan(&plant(v), &ScanFilter::Unknown, "0").unwrap();
        s.next_scan(&plant(v), &ScanFilter::Unchanged, "same").unwrap();
        for up in ups {
            v += up;
            s.next_scan(&plant(v), &ScanFilter::Increased, "up").unwrap();
        }
        let target = Location::Raw { region: NodePath::root(), offset: off as u64, ty: ScanType::U32, endian: Endianness::Little };
        prop_assert!(s.locations().contains(&target));
    }

    /// AST scanning across snapshots with transitions keeps the true leaf.
    #[test]
    fn ast_scanner_tracks_leaf(t in tree_strategy(), pick in any::<prop::sample::Index>(), seq in proptest::collection::vec(-100i64..100, 1..4)) {
        let paths: Vec<NodePath> = t.leaves().into_iter()
            .filter(|(_, l)| matches!(l, Node::I32(_) | Node::I64(_)))
            .map(|(p, _)| p).collect();
        prop_assume!(!paths.is_empty());
        let path = paths[pick.index(paths.len())].clone();
        let mut snap = t.clone();
        snap.set(&path, Node::I64(0).with_scalar(ScalarValue::Int(0), &path).unwrap(), false).unwrap();
        // Normalise the leaf to I64 so arbitrary values fit.
        let mut s = ScanSession::new(ScanConfig { scan_raw: false, ..ScanConfig::default() });
        let mut prev = 0i64;
        s.first_scan(&snap, &ScanFilter::Exact(ScalarValue::Int(0)), "0").unwrap();
        for v in seq {
            snap.set(&path, Node::I64(v), false).unwrap();
            s.next_scan(&snap, &ScanFilter::Transition { from: ScalarValue::Int(prev as i128), to: ScalarValue::Int(v as i128) }, "n").unwrap();
            prev = v;
        }
        let target = Location::Ast { path };
        prop_assert!(s.locations().contains(&target));
    }

    /// Cherry-picking a sub-tree yields exactly the source sub-tree at that
    /// path and leaves every other path of the destination untouched.
    #[test]
    fn cherry_pick_is_surgical(a in tree_strategy(), b in tree_strategy(), pick in any::<prop::sample::Index>()) {
        // Destination = b with a's subtree grafted at a fresh top-level key so
        // that the selected path exists in both with the same kind.
        let (Node::Map(_), Node::Map(_)) = (&a, &b) else { return Ok(()); };
        let src_paths = non_root_paths(&a);
        prop_assume!(!src_paths.is_empty());
        let path = src_paths[pick.index(src_paths.len())].clone();
        let src_sub = a.get(&path).unwrap().clone();
        // Build a destination that contains `path` with a different but
        // kind-compatible value: start from `a`, then replace the rest with b.
        let mut dest = a.clone();
        if let (Node::Map(dm), Node::Map(bm)) = (&mut dest, &b) {
            for (k, v) in bm {
                if path.segments()[0].as_key() != Some(k.as_str()) {
                    dm.insert(k.clone(), v.clone());
                }
            }
        }
        // Perturb the destination inside the selected path so the pick has work to do.
        let mut perturbed = dest.get(&path).unwrap().clone();
        perturb(&mut perturbed);
        dest.set(&path, perturbed, false).unwrap();
        let before = dest.clone();
        let opts = CherryPickOptions::default();
        let rep = cherry_pick(&mut dest, &a, std::slice::from_ref(&path), &opts).unwrap();
        prop_assert!(bit_equal(dest.get(&path).unwrap(), &src_sub));
        // Every change lies under the picked path.
        for c in &rep.diff.changes {
            prop_assert!(path.is_prefix_of(&c.path), "change outside selection: {}", c.path);
        }
        // And every leaf outside the selected path is preserved.
        for (p, leaf) in before.leaves() {
            if !path.is_prefix_of(&p) && !p.is_prefix_of(&path) {
                prop_assert!(bit_equal(dest.get(&p).unwrap(), leaf));
            }
        }
    }

    #[test]
    fn scalar_parse_display_roundtrip(v in any::<i64>()) {
        let s = ScalarValue::Int(v as i128).to_string();
        prop_assert_eq!(ScalarValue::parse(&s).unwrap(), ScalarValue::Int(v as i128));
    }
}

#[test]
fn large_tree_diff_is_fast() {
    // 200k leaves; one change deep inside. The Merkle tree lets the diff
    // skip every identical sibling.
    let build = |marker: i32| {
        Node::Map(
            (0..200)
                .map(|i| {
                    let inner = Node::Array((0..1000).map(|j| Node::I32(if i == 150 && j == 999 { marker } else { j })).collect());
                    (format!("chunk{i}"), inner)
                })
                .collect(),
        )
    };
    let a = build(1);
    let b = build(2);
    let start = std::time::Instant::now();
    let ta = MerkleTree::build(&a);
    let tb = MerkleTree::build(&b);
    let d = savelens_core::diff::diff_trees(&a, &ta, &b, &tb, &DiffOptions::default());
    let elapsed = start.elapsed();
    assert_eq!(d.changes.len(), 1);
    assert_eq!(d.changes[0].path.to_string(), "chunk150[999]");
    assert!(d.stats.subtrees_skipped >= 199);
    assert!(elapsed.as_secs_f64() < 5.0, "diff took {elapsed:?}");
}
