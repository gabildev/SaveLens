//! Merkle hashing of `Node` trees with `blake3`.
//!
//! Every node hash commits to its kind, its value and (for containers) the
//! ordered keys and child hashes. Large byte blobs are split into fixed-size
//! blocks whose hashes become the children of the blob, so the diff engine can
//! locate changed byte ranges without comparing every byte.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::node::Node;
use crate::raw::DEFAULT_BLOCK_SIZE;

/// A 32-byte blake3 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash32(pub [u8; 32]);

impl Hash32 {
    /// Lowercase hex representation.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Short (12 hex chars) form for UIs.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    /// Parses 64 hex characters.
    pub fn from_hex(s: &str) -> Option<Hash32> {
        let v = hex::decode(s).ok()?;
        let arr: [u8; 32] = v.try_into().ok()?;
        Some(Hash32(arr))
    }

    /// blake3 of arbitrary bytes.
    pub fn of_bytes(bytes: &[u8]) -> Hash32 {
        Hash32(*blake3::hash(bytes).as_bytes())
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash32({})", self.short())
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Block size used to split `Bytes` nodes into Merkle children.
pub const BYTES_BLOCK_SIZE: usize = DEFAULT_BLOCK_SIZE as usize;

/// A tree of hashes mirroring a `Node` tree.
///
/// For `Array`/`Map` nodes, `children[i]` is the hash tree of the i-th child.
/// For `Bytes`/`RawRegion` nodes larger than one block, `children` contains one
/// leaf per block (with no grandchildren).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleTree {
    pub hash: Hash32,
    pub children: Vec<MerkleTree>,
}

impl MerkleTree {
    /// Builds the Merkle tree of `node`.
    pub fn build(node: &Node) -> MerkleTree {
        build(node)
    }

    /// Number of hashed nodes.
    pub fn len(&self) -> usize {
        1 + self.children.iter().map(MerkleTree::len).sum::<usize>()
    }

    /// Never empty (always has a root); for API symmetry.
    pub fn is_empty(&self) -> bool {
        false
    }
}

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_I8: u8 = 2;
const TAG_I16: u8 = 3;
const TAG_I32: u8 = 4;
const TAG_I64: u8 = 5;
const TAG_U8: u8 = 6;
const TAG_U16: u8 = 7;
const TAG_U32: u8 = 8;
const TAG_U64: u8 = 9;
const TAG_F32: u8 = 10;
const TAG_F64: u8 = 11;
const TAG_STRING: u8 = 12;
const TAG_BYTES: u8 = 13;
const TAG_ARRAY: u8 = 14;
const TAG_MAP: u8 = 15;
const TAG_RAW: u8 = 16;
const TAG_BLOCK: u8 = 17;

fn leaf_hash(tag: u8, payload: &[u8]) -> Hash32 {
    let mut h = blake3::Hasher::new();
    h.update(&[tag]);
    h.update(&(payload.len() as u64).to_le_bytes());
    h.update(payload);
    Hash32(*h.finalize().as_bytes())
}

fn blocks(tag: u8, bytes: &[u8]) -> MerkleTree {
    if bytes.len() <= BYTES_BLOCK_SIZE {
        return MerkleTree { hash: leaf_hash(tag, bytes), children: Vec::new() };
    }
    let children: Vec<MerkleTree> = bytes
        .chunks(BYTES_BLOCK_SIZE)
        .map(|c| MerkleTree { hash: leaf_hash(TAG_BLOCK, c), children: Vec::new() })
        .collect();
    let mut h = blake3::Hasher::new();
    h.update(&[tag]);
    h.update(&(bytes.len() as u64).to_le_bytes());
    for c in &children {
        h.update(&c.hash.0);
    }
    MerkleTree { hash: Hash32(*h.finalize().as_bytes()), children }
}

fn build(node: &Node) -> MerkleTree {
    let leaf = |tag: u8, payload: &[u8]| MerkleTree { hash: leaf_hash(tag, payload), children: Vec::new() };
    match node {
        Node::Null => leaf(TAG_NULL, &[]),
        Node::Bool(b) => leaf(TAG_BOOL, &[*b as u8]),
        Node::I8(v) => leaf(TAG_I8, &v.to_le_bytes()),
        Node::I16(v) => leaf(TAG_I16, &v.to_le_bytes()),
        Node::I32(v) => leaf(TAG_I32, &v.to_le_bytes()),
        Node::I64(v) => leaf(TAG_I64, &v.to_le_bytes()),
        Node::U8(v) => leaf(TAG_U8, &v.to_le_bytes()),
        Node::U16(v) => leaf(TAG_U16, &v.to_le_bytes()),
        Node::U32(v) => leaf(TAG_U32, &v.to_le_bytes()),
        Node::U64(v) => leaf(TAG_U64, &v.to_le_bytes()),
        // Floats hash their bit pattern so NaN payloads and -0.0 are preserved.
        Node::F32(v) => leaf(TAG_F32, &v.to_bits().to_le_bytes()),
        Node::F64(v) => leaf(TAG_F64, &v.to_bits().to_le_bytes()),
        Node::String(s) => leaf(TAG_STRING, s.as_bytes()),
        Node::Bytes(b) => blocks(TAG_BYTES, b),
        Node::RawRegion(r) => blocks(TAG_RAW, &r.bytes),
        Node::Array(items) => {
            let children: Vec<MerkleTree> = items.iter().map(build).collect();
            let mut h = blake3::Hasher::new();
            h.update(&[TAG_ARRAY]);
            h.update(&(children.len() as u64).to_le_bytes());
            for c in &children {
                h.update(&c.hash.0);
            }
            MerkleTree { hash: Hash32(*h.finalize().as_bytes()), children }
        }
        Node::Map(map) => {
            let children: Vec<MerkleTree> = map.values().map(build).collect();
            let mut h = blake3::Hasher::new();
            h.update(&[TAG_MAP]);
            h.update(&(children.len() as u64).to_le_bytes());
            for ((k, _), c) in map.iter().zip(&children) {
                h.update(&(k.len() as u64).to_le_bytes());
                h.update(k.as_bytes());
                h.update(&c.hash.0);
            }
            MerkleTree { hash: Hash32(*h.finalize().as_bytes()), children }
        }
    }
}

/// Hash of a node (builds the full tree; use [`MerkleTree`] to keep children).
pub fn hash_node(node: &Node) -> Hash32 {
    build(node).hash
}

/// Bit-exact structural equality (unlike `PartialEq`, NaN == NaN when the bits match).
pub fn bit_equal(a: &Node, b: &Node) -> bool {
    match (a, b) {
        (Node::F32(x), Node::F32(y)) => x.to_bits() == y.to_bits(),
        (Node::F64(x), Node::F64(y)) => x.to_bits() == y.to_bits(),
        (Node::Array(x), Node::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| bit_equal(a, b)),
        (Node::Map(x), Node::Map(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((ka, va), (kb, vb))| ka == kb && bit_equal(va, vb))
        }
        (Node::RawRegion(x), Node::RawRegion(y)) => x.bytes == y.bytes,
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawRegion;

    #[test]
    fn identical_trees_hash_equal() {
        let a = Node::map_from([("a", Node::I32(1)), ("b", Node::String("x".into()))]);
        let b = a.clone();
        assert_eq!(hash_node(&a), hash_node(&b));
    }

    #[test]
    fn types_are_distinguished() {
        assert_ne!(hash_node(&Node::I32(1)), hash_node(&Node::U32(1)));
        assert_ne!(hash_node(&Node::String("1".into())), hash_node(&Node::Bytes(b"1".to_vec())));
        assert_ne!(hash_node(&Node::Bytes(vec![1])), hash_node(&Node::RawRegion(RawRegion::new(vec![1]))));
    }

    #[test]
    fn key_order_matters() {
        let a = Node::map_from([("a", Node::I32(1)), ("b", Node::I32(2))]);
        let b = Node::map_from([("b", Node::I32(2)), ("a", Node::I32(1))]);
        assert_ne!(hash_node(&a), hash_node(&b));
    }

    #[test]
    fn key_value_boundary_ambiguity() {
        // ("ab", "c") vs ("a", "bc") must differ thanks to length prefixes.
        let a = Node::map_from([("ab", Node::String("c".into()))]);
        let b = Node::map_from([("a", Node::String("bc".into()))]);
        assert_ne!(hash_node(&a), hash_node(&b));
    }

    #[test]
    fn float_bits() {
        assert_ne!(hash_node(&Node::F64(0.0)), hash_node(&Node::F64(-0.0)));
        assert_eq!(hash_node(&Node::F64(f64::NAN)), hash_node(&Node::F64(f64::NAN)));
        assert!(bit_equal(&Node::F64(f64::NAN), &Node::F64(f64::NAN)));
        assert!(!bit_equal(&Node::F64(0.0), &Node::F64(-0.0)));
    }

    #[test]
    fn large_bytes_have_block_children() {
        let data = vec![7u8; BYTES_BLOCK_SIZE * 3 + 10];
        let t = MerkleTree::build(&Node::Bytes(data.clone()));
        assert_eq!(t.children.len(), 4);
        let mut changed = data;
        changed[BYTES_BLOCK_SIZE * 2 + 5] = 8;
        let t2 = MerkleTree::build(&Node::Bytes(changed));
        let diff: Vec<usize> = t
            .children
            .iter()
            .zip(&t2.children)
            .enumerate()
            .filter(|(_, (a, b))| a.hash != b.hash)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(diff, vec![2]);
    }

    #[test]
    fn hex_roundtrip() {
        let h = Hash32::of_bytes(b"abc");
        assert_eq!(Hash32::from_hex(&h.to_hex()), Some(h));
        assert_eq!(h.short().len(), 12);
        assert!(Hash32::from_hex("zz").is_none());
    }
}
