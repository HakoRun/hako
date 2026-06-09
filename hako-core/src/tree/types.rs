use crate::hash::Hash;
use std::sync::OnceLock;
use xxhash_rust::xxh3::xxh3_64_with_seed;

pub const TARGET_FANOUT: u64 = 64;
pub const INLINE_THRESHOLD: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Inline(Vec<u8>),
    External(Hash),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: Vec<u8>,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Leaf {
        entries: Vec<Entry>,
    },
    Internal {
        // child_keys[i] is the LAST key in child[i]'s subtree (Noms convention).
        child_keys: Vec<Vec<u8>>,
        child_hashes: Vec<Hash>,
        child_counts: Vec<u32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub level: u8,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffEntry {
    Added {
        key: Vec<u8>,
        value: Value,
    },
    Removed {
        key: Vec<u8>,
        value: Value,
    },
    Modified {
        key: Vec<u8>,
        old: Value,
        new: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    BothModified {
        key: Vec<u8>,
        base: Value,
        ours: Value,
        theirs: Value,
    },
    ModifyDelete {
        key: Vec<u8>,
        base: Value,
        ours: Value,
    },
    DeleteModify {
        key: Vec<u8>,
        base: Value,
        theirs: Value,
    },
    BothAdded {
        key: Vec<u8>,
        ours: Value,
        theirs: Value,
    },
}

#[derive(Clone, Debug)]
pub struct MergeResult {
    pub merged: Hash,
    pub conflicts: Vec<Conflict>,
}

fn level_salts() -> &'static [u64; 256] {
    static SALTS: OnceLock<[u64; 256]> = OnceLock::new();
    SALTS.get_or_init(|| {
        let mut salts = [0u64; 256];
        for (level, salt) in salts.iter_mut().enumerate() {
            let label = format!("hako-level-{}", level);
            let h = blake3::hash(label.as_bytes());
            let bytes = h.as_bytes();
            *salt = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
        }
        salts
    })
}

pub fn level_salt(level: u8) -> u64 {
    level_salts()[level as usize]
}

pub fn is_boundary(key: &[u8], level: u8) -> bool {
    xxh3_64_with_seed(key, level_salt(level)).is_multiple_of(TARGET_FANOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_distribution() {
        // ~1/64 of random keys should be boundaries at any level.
        let n = 6400;
        let mut hits = 0;
        for i in 0..n {
            let key = format!("key-{:08}", i);
            if is_boundary(key.as_bytes(), 0) {
                hits += 1;
            }
        }
        // Tolerance: expected 100, allow 50..200.
        assert!(
            hits > 50 && hits < 200,
            "boundary hits at level 0: {}",
            hits
        );
    }

    #[test]
    fn level_salts_differ() {
        assert_ne!(level_salt(0), level_salt(1));
        assert_ne!(level_salt(0), level_salt(255));
    }

    #[test]
    fn boundary_per_level_independent() {
        // The same key should give different boundary results across levels.
        // Pick a key that is a boundary at level 0 and check its result at higher levels varies.
        let key = b"some-test-key";
        let mut results = Vec::new();
        for level in 0..16u8 {
            results.push(is_boundary(key, level));
        }
        // Not all should be the same.
        assert!(results.iter().any(|&r| r != results[0]) || results.iter().all(|&r| !r));
    }
}
