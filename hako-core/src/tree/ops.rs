use super::node::{load_node, store_node};
use super::types::*;
use crate::hash::Hash;
use crate::store::ChunkStore;
use std::io;

pub fn empty() -> Hash {
    Hash::zero()
}

pub fn store_value(store: &dyn ChunkStore, data: &[u8]) -> io::Result<Value> {
    if data.len() <= INLINE_THRESHOLD {
        Ok(Value::Inline(data.to_vec()))
    } else {
        let h = store.put(data)?;
        Ok(Value::External(h))
    }
}

pub fn load_value(store: &dyn ChunkStore, value: &Value) -> io::Result<Vec<u8>> {
    match value {
        Value::Inline(data) => Ok(data.clone()),
        Value::External(h) => store
            .get(h)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing value chunk")),
    }
}

pub fn get(store: &dyn ChunkStore, root: &Hash, key: &[u8]) -> io::Result<Option<Value>> {
    if *root == Hash::zero() {
        return Ok(None);
    }
    let mut current = load_node(store, root)?;
    loop {
        match current.kind {
            NodeKind::Leaf { entries } => {
                for e in entries {
                    if e.key.as_slice() == key {
                        return Ok(Some(e.value));
                    }
                    if e.key.as_slice() > key {
                        return Ok(None);
                    }
                }
                return Ok(None);
            }
            NodeKind::Internal {
                child_keys,
                child_hashes,
                ..
            } => {
                let mut idx = None;
                for (i, k) in child_keys.iter().enumerate() {
                    if k.as_slice() >= key {
                        idx = Some(i);
                        break;
                    }
                }
                let i = match idx {
                    Some(i) => i,
                    None => return Ok(None),
                };
                current = load_node(store, &child_hashes[i])?;
            }
        }
    }
}

pub fn scan(store: &dyn ChunkStore, root: &Hash) -> io::Result<Vec<(Vec<u8>, Value)>> {
    let mut out = Vec::new();
    if *root == Hash::zero() {
        return Ok(out);
    }
    scan_into(store, root, &mut out)?;
    Ok(out)
}

fn scan_into(
    store: &dyn ChunkStore,
    root: &Hash,
    out: &mut Vec<(Vec<u8>, Value)>,
) -> io::Result<()> {
    let node = load_node(store, root)?;
    match node.kind {
        NodeKind::Leaf { entries } => {
            for e in entries {
                out.push((e.key, e.value));
            }
        }
        NodeKind::Internal { child_hashes, .. } => {
            for ch in child_hashes {
                scan_into(store, &ch, out)?;
            }
        }
    }
    Ok(())
}

pub fn scan_prefix(
    store: &dyn ChunkStore,
    root: &Hash,
    prefix: &[u8],
) -> io::Result<Vec<(Vec<u8>, Value)>> {
    let mut out = Vec::new();
    if *root == Hash::zero() {
        return Ok(out);
    }
    let mut c = super::cursor::Cursor::open(store, *root)?;
    c.seek(prefix)?;
    while let Some((k, v)) = c.next()? {
        if !k.starts_with(prefix) {
            break;
        }
        out.push((k, v));
    }
    Ok(out)
}

pub fn count(store: &dyn ChunkStore, root: &Hash) -> io::Result<u64> {
    if *root == Hash::zero() {
        return Ok(0);
    }
    let node = load_node(store, root)?;
    match node.kind {
        NodeKind::Leaf { entries } => Ok(entries.len() as u64),
        NodeKind::Internal { child_counts, .. } => Ok(child_counts.iter().map(|&c| c as u64).sum()),
    }
}

// ============================================================================
// Cursor-based mutations
//
// Old behavior: scan-and-rebuild was O(n) reads + O(n) writes per put/delete.
// New behavior: walk root-to-leaf (O(log n) reads), apply the change to that
// one leaf, splice up through the path (O(log n) writes), re-evaluating
// content-defined boundaries at each level so leaves can split or merge.
// ============================================================================

#[derive(Clone)]
struct ChildRef {
    last_key: Vec<u8>,
    hash: Hash,
    count: u32,
}

struct PathStep {
    /// Level of this internal node (>= 1). Its children are at level-1.
    level: u8,
    /// All children of this internal node, in order.
    children: Vec<ChildRef>,
    /// Index in `children` of the one we descended into.
    chosen_idx: usize,
}

pub fn put(store: &dyn ChunkStore, root: &Hash, key: Vec<u8>, value: Value) -> io::Result<Hash> {
    if *root == Hash::zero() {
        return bulk_build(store, vec![(key, value)]);
    }
    let (path, mut leaf_entries) = find_path(store, root, &key)?;
    apply_put_to_entries(&mut leaf_entries, key, value);
    // Why no cursor_safe fallback for PUT (unlike DELETE):
    //   - Replace (key already in leaf): leaf's last_key unchanged → safe.
    //   - Insert with key < leaf's old last_key: same → safe.
    //   - Insert with key > leaf's old last_key: only possible if descent
    //     took the rightmost branch at every level (find_path falls through
    //     to children.len()-1 only when key > all child_keys), so the leaf
    //     is the absolute rightmost and the rightmost rule applies → safe.
    // In all cases cursor_safe(leaf_entries, path) is true; no fallback.
    debug_assert!(
        cursor_safe(&leaf_entries, &path),
        "PUT cursor_safe invariant violated"
    );
    let new_leaves = build_leaf_level(store, leaf_entries)?;
    splice_up(store, path, new_leaves, 0)
}

pub fn delete(store: &dyn ChunkStore, root: &Hash, key: &[u8]) -> io::Result<Hash> {
    if *root == Hash::zero() {
        return Ok(Hash::zero());
    }
    let (path, mut leaf_entries) = find_path(store, root, key)?;
    let original_len = leaf_entries.len();
    leaf_entries.retain(|e| e.key.as_slice() != key);
    if leaf_entries.len() == original_len {
        return Ok(*root);
    }
    if !cursor_safe(&leaf_entries, &path) {
        // Same fallback rationale as `put`.
        let mut all = scan(store, root)?;
        if let Ok(idx) = all.binary_search_by(|e| e.0.as_slice().cmp(key)) {
            all.remove(idx);
        }
        return bulk_build(store, all);
    }
    let new_leaves = build_leaf_level(store, leaf_entries)?;
    if new_leaves.is_empty() && path.is_empty() {
        return Ok(Hash::zero());
    }
    splice_up(store, path, new_leaves, 0)
}

/// True iff the modified leaf's new last_key is a boundary at level 0,
/// OR the leaf is the absolute rightmost in the tree (rightmost child at
/// every step in the path). Either condition means bulk_build would also
/// emit a leaf ending here, so the cursor splice produces the same shape.
fn cursor_safe(new_entries: &[Entry], path: &[PathStep]) -> bool {
    if new_entries.is_empty() {
        // Empty leaf is fine if rightmost (the parent will lose a child),
        // but if not rightmost, the right sibling's first key would have
        // been part of this leaf in a clean rebuild.
        return path.iter().all(|s| s.chosen_idx == s.children.len() - 1);
    }
    let last = &new_entries.last().unwrap().key;
    if is_boundary(last, 0) {
        return true;
    }
    path.iter().all(|s| s.chosen_idx == s.children.len() - 1)
}

/// Walk root-to-leaf for `key`, recording every internal node we descend
/// through. Returns the (path, leaf_entries). `path` is empty iff the root
/// is itself a leaf.
fn find_path(
    store: &dyn ChunkStore,
    root: &Hash,
    key: &[u8],
) -> io::Result<(Vec<PathStep>, Vec<Entry>)> {
    let mut path = Vec::new();
    let mut current_hash = *root;
    loop {
        let node = load_node(store, &current_hash)?;
        match node.kind {
            NodeKind::Leaf { entries } => return Ok((path, entries)),
            NodeKind::Internal {
                child_keys,
                child_hashes,
                child_counts,
            } => {
                // Find the smallest i such that child_keys[i] >= key.
                // (child_keys[i] is the LAST key in child[i]'s subtree.)
                // Defense in depth: `decode` rejects empty internal nodes, so
                // child_keys is non-empty here — but guard the subtraction so a
                // future decode path can never turn this into an underflow.
                if child_keys.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "internal node with no children",
                    ));
                }
                let chosen_idx = match child_keys.iter().position(|k| k.as_slice() >= key) {
                    Some(i) => i,
                    // Key falls past every subtree — descend into the rightmost,
                    // which is where an insert would land.
                    None => child_keys.len() - 1,
                };
                let next_hash = child_hashes[chosen_idx];
                let children: Vec<ChildRef> = child_keys
                    .into_iter()
                    .zip(child_hashes)
                    .zip(child_counts)
                    .map(|((k, h), c)| ChildRef {
                        last_key: k,
                        hash: h,
                        count: c,
                    })
                    .collect();
                path.push(PathStep {
                    level: node.level,
                    children,
                    chosen_idx,
                });
                current_hash = next_hash;
            }
        }
    }
}

/// In-place sorted insert (replacing on key match) into `entries`.
fn apply_put_to_entries(entries: &mut Vec<Entry>, key: Vec<u8>, value: Value) {
    match entries.binary_search_by(|e| e.key.cmp(&key)) {
        Ok(idx) => entries[idx].value = value,
        Err(idx) => entries.insert(idx, Entry { key, value }),
    }
}

/// Walk back up the path, splicing `new_children` (at `child_level`) into
/// each ancestor in turn and re-evaluating boundaries. After exhausting
/// the path, build any remaining levels above to converge to a single root.
fn splice_up(
    store: &dyn ChunkStore,
    mut path: Vec<PathStep>,
    mut current_children: Vec<ChildRef>,
    mut current_level: u8,
) -> io::Result<Hash> {
    while let Some(step) = path.pop() {
        debug_assert_eq!(step.level, current_level + 1, "level invariant");
        let mut all = step.children;
        // Replace the one child we descended through with the new children.
        all.splice(step.chosen_idx..step.chosen_idx + 1, current_children);
        if all.is_empty() {
            // Every child of this internal node is gone. Bubble emptiness up.
            current_children = Vec::new();
            current_level = step.level;
            continue;
        }
        current_children = build_internal_level(store, all, step.level)?;
        current_level = step.level;
    }

    // No more parents. Either current_children is empty (whole tree gone),
    // or it has one or more entries that may need additional levels above.
    if current_children.is_empty() {
        return Ok(Hash::zero());
    }
    while current_children.len() > 1 {
        current_level = current_level
            .checked_add(1)
            .ok_or_else(|| io::Error::other("tree exceeds 256 levels"))?;
        current_children = build_internal_level(store, current_children, current_level)?;
    }
    Ok(current_children[0].hash)
}

/// Take a flat list of entries and emit one or more leaf nodes, splitting
/// at content-defined boundaries. Returns the resulting child refs (last
/// key, hash, count) in order.
fn build_leaf_level(store: &dyn ChunkStore, entries: Vec<Entry>) -> io::Result<Vec<ChildRef>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let level: u8 = 0;
    let mut out = Vec::new();
    let mut current: Vec<Entry> = Vec::new();
    for e in entries {
        let boundary = is_boundary(&e.key, level);
        current.push(e);
        if boundary {
            let last_key = current.last().unwrap().key.clone();
            let count = current.len() as u32;
            let node = Node {
                level,
                kind: NodeKind::Leaf {
                    entries: std::mem::take(&mut current),
                },
            };
            let h = store_node(store, &node)?;
            out.push(ChildRef {
                last_key,
                hash: h,
                count,
            });
        }
    }
    if !current.is_empty() {
        let last_key = current.last().unwrap().key.clone();
        let count = current.len() as u32;
        let node = Node {
            level,
            kind: NodeKind::Leaf { entries: current },
        };
        let h = store_node(store, &node)?;
        out.push(ChildRef {
            last_key,
            hash: h,
            count,
        });
    }
    Ok(out)
}

/// Take a flat list of children-at-(level-1) and emit one or more internal
/// nodes at `level`, splitting at content-defined boundaries on the
/// last-key-in-subtree of each child.
fn build_internal_level(
    store: &dyn ChunkStore,
    children: Vec<ChildRef>,
    level: u8,
) -> io::Result<Vec<ChildRef>> {
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut hashes: Vec<Hash> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    let mut subtree_count: u64 = 0;
    for c in children {
        let boundary = is_boundary(&c.last_key, level);
        keys.push(c.last_key);
        hashes.push(c.hash);
        counts.push(c.count);
        subtree_count += c.count as u64;
        if boundary {
            let last = keys.last().unwrap().clone();
            let total = subtree_count.min(u32::MAX as u64) as u32;
            let node = Node {
                level,
                kind: NodeKind::Internal {
                    child_keys: std::mem::take(&mut keys),
                    child_hashes: std::mem::take(&mut hashes),
                    child_counts: std::mem::take(&mut counts),
                },
            };
            let h = store_node(store, &node)?;
            out.push(ChildRef {
                last_key: last,
                hash: h,
                count: total,
            });
            subtree_count = 0;
        }
    }
    if !keys.is_empty() {
        let last = keys.last().unwrap().clone();
        let total = subtree_count.min(u32::MAX as u64) as u32;
        let node = Node {
            level,
            kind: NodeKind::Internal {
                child_keys: keys,
                child_hashes: hashes,
                child_counts: counts,
            },
        };
        let h = store_node(store, &node)?;
        out.push(ChildRef {
            last_key: last,
            hash: h,
            count: total,
        });
    }
    Ok(out)
}

pub fn bulk_build(store: &dyn ChunkStore, mut entries: Vec<(Vec<u8>, Value)>) -> io::Result<Hash> {
    if entries.is_empty() {
        return Ok(Hash::zero());
    }

    // Stable sort preserves insertion order for equal keys; subsequent
    // dedup keeps the LAST occurrence — i.e. an "overwrite" semantic.
    // Defense in depth: if any caller forgets to drop the dst entries
    // before extending (the cp_to bug), this prevents the resulting tree
    // from carrying two leaves at the same key, which would otherwise
    // make lookups return the older value and silently drop the newer
    // one.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if entries.len() > 1 {
        let mut deduped: Vec<(Vec<u8>, Value)> = Vec::with_capacity(entries.len());
        for entry in entries {
            match deduped.last_mut() {
                Some(last) if last.0 == entry.0 => *last = entry,
                _ => deduped.push(entry),
            }
        }
        entries = deduped;
    }

    // Build leaves.
    let level: u8 = 0;
    let mut leaves: Vec<(Vec<u8>, Hash, u32)> = Vec::new();
    let mut current: Vec<Entry> = Vec::new();

    for (key, value) in entries {
        let boundary = is_boundary(&key, level);
        current.push(Entry { key, value });
        if boundary {
            let last_key = current.last().unwrap().key.clone();
            let count = current.len() as u32;
            let node = Node {
                level,
                kind: NodeKind::Leaf {
                    entries: std::mem::take(&mut current),
                },
            };
            let h = store_node(store, &node)?;
            leaves.push((last_key, h, count));
        }
    }
    if !current.is_empty() {
        let last_key = current.last().unwrap().key.clone();
        let count = current.len() as u32;
        let node = Node {
            level,
            kind: NodeKind::Leaf { entries: current },
        };
        let h = store_node(store, &node)?;
        leaves.push((last_key, h, count));
    }

    if leaves.len() == 1 {
        return Ok(leaves[0].1);
    }

    // Build internal levels until 1 root.
    let mut current_level = leaves;
    let mut level: u8 = 1;

    loop {
        if current_level.len() == 1 {
            return Ok(current_level[0].1);
        }

        let mut next: Vec<(Vec<u8>, Hash, u32)> = Vec::new();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut hashes: Vec<Hash> = Vec::new();
        let mut counts: Vec<u32> = Vec::new();
        let mut subtree_count: u64 = 0;

        for (last_key, h, c) in current_level {
            let boundary = is_boundary(&last_key, level);
            keys.push(last_key);
            hashes.push(h);
            counts.push(c);
            subtree_count += c as u64;
            if boundary {
                let last = keys.last().unwrap().clone();
                let total = subtree_count.min(u32::MAX as u64) as u32;
                let node = Node {
                    level,
                    kind: NodeKind::Internal {
                        child_keys: std::mem::take(&mut keys),
                        child_hashes: std::mem::take(&mut hashes),
                        child_counts: std::mem::take(&mut counts),
                    },
                };
                let nh = store_node(store, &node)?;
                next.push((last, nh, total));
                subtree_count = 0;
            }
        }
        if !keys.is_empty() {
            let last = keys.last().unwrap().clone();
            let total = subtree_count.min(u32::MAX as u64) as u32;
            let node = Node {
                level,
                kind: NodeKind::Internal {
                    child_keys: keys,
                    child_hashes: hashes,
                    child_counts: counts,
                },
            };
            let nh = store_node(store, &node)?;
            next.push((last, nh, total));
        }

        current_level = next;
        level = match level.checked_add(1) {
            Some(l) => l,
            None => return Err(io::Error::other("tree exceeds 256 levels")),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn entries(n: usize) -> Vec<(Vec<u8>, Value)> {
        (0..n)
            .map(|i| {
                let k = format!("key-{:08}", i).into_bytes();
                let v = Value::Inline(format!("val-{}", i).into_bytes());
                (k, v)
            })
            .collect()
    }

    #[test]
    fn empty_tree() {
        let s = MemStore::new();
        let root = empty();
        assert_eq!(get(&s, &root, b"x").unwrap(), None);
        assert_eq!(scan(&s, &root).unwrap().len(), 0);
        assert_eq!(count(&s, &root).unwrap(), 0);
    }

    #[test]
    fn bulk_build_last_wins_on_duplicate_keys() {
        // Defense-in-depth: callers should not pass duplicates, but if they
        // do, last-wins prevents a tree where lookups return the older
        // value. Pre-fix, the cp_to bug ended here as data loss.
        let s = MemStore::new();
        let root = bulk_build(
            &s,
            vec![
                (b"k".to_vec(), Value::Inline(b"first".to_vec())),
                (b"k".to_vec(), Value::Inline(b"second".to_vec())),
                (b"k".to_vec(), Value::Inline(b"third".to_vec())),
            ],
        )
        .unwrap();
        assert_eq!(
            get(&s, &root, b"k").unwrap(),
            Some(Value::Inline(b"third".to_vec()))
        );
        assert_eq!(count(&s, &root).unwrap(), 1);
    }

    #[test]
    fn single_entry_roundtrip() {
        let s = MemStore::new();
        let root = put(
            &s,
            &empty(),
            b"hello".to_vec(),
            Value::Inline(b"world".to_vec()),
        )
        .unwrap();
        assert_eq!(
            get(&s, &root, b"hello").unwrap(),
            Some(Value::Inline(b"world".to_vec()))
        );
        assert_eq!(get(&s, &root, b"missing").unwrap(), None);
        assert_eq!(count(&s, &root).unwrap(), 1);
    }

    #[test]
    fn many_entries_roundtrip() {
        let s = MemStore::new();
        let n = 1000;
        let root = bulk_build(&s, entries(n)).unwrap();
        assert_eq!(count(&s, &root).unwrap(), n as u64);
        for i in 0..n {
            let k = format!("key-{:08}", i);
            let v = Value::Inline(format!("val-{}", i).into_bytes());
            assert_eq!(get(&s, &root, k.as_bytes()).unwrap(), Some(v));
        }
        // And scan returns all in order.
        let scanned = scan(&s, &root).unwrap();
        assert_eq!(scanned.len(), n);
        for (i, entry) in scanned.iter().enumerate() {
            assert_eq!(entry.0, format!("key-{:08}", i).into_bytes());
        }
    }

    #[test]
    fn deterministic_root_hash() {
        let s1 = MemStore::new();
        let s2 = MemStore::new();
        let root1 = bulk_build(&s1, entries(500)).unwrap();
        let root2 = bulk_build(&s2, entries(500)).unwrap();
        assert_eq!(root1, root2);
    }

    #[test]
    fn input_order_doesnt_matter() {
        let s = MemStore::new();
        let mut e1 = entries(200);
        let mut e2 = e1.clone();
        e2.reverse();
        let r1 = bulk_build(&s, std::mem::take(&mut e1)).unwrap();
        let r2 = bulk_build(&s, std::mem::take(&mut e2)).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn put_then_get_iteratively() {
        let s = MemStore::new();
        let mut root = empty();
        for i in 0..100 {
            let k = format!("k-{:04}", i).into_bytes();
            let v = Value::Inline(format!("v-{}", i).into_bytes());
            root = put(&s, &root, k, v).unwrap();
        }
        assert_eq!(count(&s, &root).unwrap(), 100);
        for i in 0..100 {
            let k = format!("k-{:04}", i);
            let v = Value::Inline(format!("v-{}", i).into_bytes());
            assert_eq!(get(&s, &root, k.as_bytes()).unwrap(), Some(v));
        }
    }

    #[test]
    fn put_replaces_existing() {
        let s = MemStore::new();
        let root = put(&s, &empty(), b"k".to_vec(), Value::Inline(b"v1".to_vec())).unwrap();
        let root = put(&s, &root, b"k".to_vec(), Value::Inline(b"v2".to_vec())).unwrap();
        assert_eq!(
            get(&s, &root, b"k").unwrap(),
            Some(Value::Inline(b"v2".to_vec()))
        );
        assert_eq!(count(&s, &root).unwrap(), 1);
    }

    #[test]
    fn delete_removes_entry() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(10)).unwrap();
        let root = delete(&s, &root, b"key-00000005").unwrap();
        assert_eq!(get(&s, &root, b"key-00000005").unwrap(), None);
        assert!(get(&s, &root, b"key-00000004").unwrap().is_some());
        assert_eq!(count(&s, &root).unwrap(), 9);
    }

    #[test]
    fn delete_missing_is_noop() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(10)).unwrap();
        let root2 = delete(&s, &root, b"not-here").unwrap();
        assert_eq!(root, root2);
    }

    #[test]
    fn delete_to_empty() {
        let s = MemStore::new();
        let root = put(&s, &empty(), b"k".to_vec(), Value::Inline(b"v".to_vec())).unwrap();
        let root = delete(&s, &root, b"k").unwrap();
        assert_eq!(root, empty());
    }

    #[test]
    fn external_value_storage() {
        let s = MemStore::new();
        let big = vec![42u8; 1000];
        let v = store_value(&s, &big).unwrap();
        assert!(matches!(v, Value::External(_)));
        let loaded = load_value(&s, &v).unwrap();
        assert_eq!(loaded, big);

        let small = b"tiny".to_vec();
        let v = store_value(&s, &small).unwrap();
        assert!(matches!(v, Value::Inline(_)));
        let loaded = load_value(&s, &v).unwrap();
        assert_eq!(loaded, small);
    }

    #[test]
    fn boundary_at_threshold_inlines() {
        let s = MemStore::new();
        let exactly = vec![7u8; INLINE_THRESHOLD];
        let v = store_value(&s, &exactly).unwrap();
        assert!(matches!(v, Value::Inline(_)));
        let over = vec![7u8; INLINE_THRESHOLD + 1];
        let v = store_value(&s, &over).unwrap();
        assert!(matches!(v, Value::External(_)));
    }

    #[test]
    fn scan_prefix_filters() {
        let s = MemStore::new();
        let mut e = entries(50);
        e.push((b"zzz".to_vec(), Value::Inline(b"out".to_vec())));
        let root = bulk_build(&s, e).unwrap();
        let prefixed = scan_prefix(&s, &root, b"key-0000000").unwrap();
        assert_eq!(prefixed.len(), 10);
        let none = scan_prefix(&s, &root, b"missing-").unwrap();
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn produces_multilevel_tree() {
        // 5000 entries should certainly produce >1 level.
        let s = MemStore::new();
        let root = bulk_build(&s, entries(5000)).unwrap();
        let n = super::super::node::load_node(&s, &root).unwrap();
        assert!(matches!(n.kind, NodeKind::Internal { .. }));
    }

    #[test]
    fn cursor_put_matches_bulk_build_hash() {
        // Insert 1000 entries via cursor put; bulk_build the same set; assert
        // the root hashes match. If they do, the cursor implementation is
        // structurally identical to the from-scratch build.
        let s_cursor = MemStore::new();
        let mut root = empty();
        for (k, v) in entries(1000) {
            root = put(&s_cursor, &root, k, v).unwrap();
        }
        let s_bulk = MemStore::new();
        let bulk_root = bulk_build(&s_bulk, entries(1000)).unwrap();
        assert_eq!(
            root, bulk_root,
            "cursor put should yield same tree as bulk_build"
        );
    }

    #[test]
    fn cursor_delete_matches_bulk_build_hash() {
        let s_full = MemStore::new();
        let full_root = bulk_build(&s_full, entries(500)).unwrap();
        // Delete every 5th key.
        let mut root_after_deletes = full_root;
        let mut remaining: Vec<_> = entries(500);
        for i in (0..500).step_by(5).rev() {
            let k = format!("key-{:08}", i);
            root_after_deletes = delete(&s_full, &root_after_deletes, k.as_bytes()).unwrap();
            remaining.retain(|(rk, _)| rk.as_slice() != k.as_bytes());
        }
        let s_rebuilt = MemStore::new();
        let rebuilt = bulk_build(&s_rebuilt, remaining).unwrap();
        assert_eq!(
            root_after_deletes, rebuilt,
            "cursor delete should yield same tree as bulk_build of remaining"
        );
    }

    #[test]
    fn cursor_put_replace_doesnt_change_size() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(100)).unwrap();
        let r2 = put(
            &s,
            &root,
            b"key-00000050".to_vec(),
            Value::Inline(b"replaced".to_vec()),
        )
        .unwrap();
        assert_eq!(count(&s, &r2).unwrap(), 100);
        assert_eq!(
            get(&s, &r2, b"key-00000050").unwrap(),
            Some(Value::Inline(b"replaced".to_vec()))
        );
    }
}
