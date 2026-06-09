use super::cursor::Cursor;
use super::types::Value;
use crate::hash::Hash;
use crate::store::ChunkStore;
use std::cmp::Ordering;
use std::io;

/// One key's worth of overlap between two sorted streams.
pub struct Pair {
    pub key: Vec<u8>,
    pub left: Option<Value>,
    pub right: Option<Value>,
}

/// Walk two trees in lockstep, calling `visit` once per distinct key.
/// Identical roots short-circuit without enumerating either tree.
pub fn merge_walk(
    store: &dyn ChunkStore,
    a: &Hash,
    b: &Hash,
    mut visit: impl FnMut(Pair),
) -> io::Result<()> {
    if a == b {
        return Ok(());
    }
    let mut ca = Cursor::open(store, *a)?;
    let mut cb = Cursor::open(store, *b)?;

    loop {
        let order = match (ca.current()?, cb.current()?) {
            (None, None) => return Ok(()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((ka, _)), Some((kb, _))) => ka.cmp(kb),
        };
        match order {
            Ordering::Less => {
                let (key, val) = ca.next()?.unwrap();
                visit(Pair { key, left: Some(val), right: None });
            }
            Ordering::Greater => {
                let (key, val) = cb.next()?.unwrap();
                visit(Pair { key, left: None, right: Some(val) });
            }
            Ordering::Equal => {
                let (key, lv) = ca.next()?.unwrap();
                let (_, rv) = cb.next()?.unwrap();
                visit(Pair { key, left: Some(lv), right: Some(rv) });
            }
        }
    }
}

/// Walk two trees in lockstep but skip shared subtrees by hash without ever
/// loading their contents. The visit callback only fires for keys that
/// DIFFER between the two trees.
///
/// Makes diff between similar trees `O(divergent_subtrees * log fanout)`
/// rather than `O(total_keys)`.
pub fn diff_walk(
    store: &dyn ChunkStore,
    a: &Hash,
    b: &Hash,
    mut visit: impl FnMut(Pair),
) -> io::Result<()> {
    if a == b {
        return Ok(());
    }
    let mut ca = Cursor::open(store, *a)?;
    let mut cb = Cursor::open(store, *b)?;

    loop {
        // Subtree-skip: if both cursors have a pending subtree at the same
        // hash, those subtrees are content-identical — skip both without
        // loading either.
        if let (Some(ha), Some(hb)) = (ca.peek_next_subtree(), cb.peek_next_subtree()) {
            if ha == hb {
                ca.skip_next_subtree()?;
                cb.skip_next_subtree()?;
                continue;
            }
        }

        let order = match (ca.current()?, cb.current()?) {
            (None, None) => return Ok(()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((ka, _)), Some((kb, _))) => ka.cmp(kb),
        };
        match order {
            Ordering::Less => {
                let (key, val) = ca.next()?.unwrap();
                visit(Pair { key, left: Some(val), right: None });
            }
            Ordering::Greater => {
                let (key, val) = cb.next()?.unwrap();
                visit(Pair { key, left: None, right: Some(val) });
            }
            Ordering::Equal => {
                let (key, lv) = ca.next()?.unwrap();
                let (_, rv) = cb.next()?.unwrap();
                if lv != rv {
                    visit(Pair { key, left: Some(lv), right: Some(rv) });
                }
            }
        }
    }
}

/// Keys present in both trees with equal values.
pub fn intersection(
    store: &dyn ChunkStore,
    a: &Hash,
    b: &Hash,
) -> io::Result<Vec<(Vec<u8>, Value)>> {
    let mut out = Vec::new();
    merge_walk(store, a, b, |p| {
        if let (Some(l), Some(r)) = (p.left, p.right) {
            if l == r {
                out.push((p.key, l));
            }
        }
    })?;
    Ok(out)
}

/// Keys in `a` not in `b` (presence-only; values from `a`).
pub fn difference(
    store: &dyn ChunkStore,
    a: &Hash,
    b: &Hash,
) -> io::Result<Vec<(Vec<u8>, Value)>> {
    let mut out = Vec::new();
    diff_walk(store, a, b, |p| {
        if let (Some(l), None) = (p.left, p.right) {
            out.push((p.key, l));
        }
    })?;
    Ok(out)
}

/// Keys in either tree (right-wins on collision).
pub fn union(
    store: &dyn ChunkStore,
    a: &Hash,
    b: &Hash,
) -> io::Result<Vec<(Vec<u8>, Value)>> {
    let mut out = Vec::new();
    merge_walk(store, a, b, |p| match (p.left, p.right) {
        (Some(l), None) => out.push((p.key, l)),
        (None, Some(r)) => out.push((p.key, r)),
        (Some(_), Some(r)) => out.push((p.key, r)),
        (None, None) => {}
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::ops::{bulk_build, empty, put};
    use super::*;
    use crate::store::{ChunkStore, MemStore};
    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};

    struct CountingStore {
        inner: MemStore,
        gets: AtomicUsize,
    }

    impl CountingStore {
        fn new() -> Self {
            Self { inner: MemStore::new(), gets: AtomicUsize::new(0) }
        }
        fn gets(&self) -> usize {
            self.gets.load(AOrd::Relaxed)
        }
        fn reset(&self) {
            self.gets.store(0, AOrd::Relaxed);
        }
    }

    impl ChunkStore for CountingStore {
        fn put(&self, data: &[u8]) -> std::io::Result<Hash> {
            self.inner.put(data)
        }
        fn get(&self, hash: &Hash) -> std::io::Result<Option<Vec<u8>>> {
            self.gets.fetch_add(1, AOrd::Relaxed);
            self.inner.get(hash)
        }
        fn has(&self, hash: &Hash) -> std::io::Result<bool> {
            self.inner.has(hash)
        }
        fn find_by_prefix(&self, prefix: &str) -> std::io::Result<Vec<Hash>> {
            self.inner.find_by_prefix(prefix)
        }
        fn delete(&self, hash: &Hash) -> std::io::Result<bool> {
            self.inner.delete(hash)
        }
    }

    fn entries(n: usize) -> Vec<(Vec<u8>, Value)> {
        (0..n)
            .map(|i| {
                (
                    format!("k-{:04}", i).into_bytes(),
                    Value::Inline(format!("v-{}", i).into_bytes()),
                )
            })
            .collect()
    }

    #[test]
    fn identical_roots_no_visits() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(50)).unwrap();
        let mut visits = 0;
        merge_walk(&s, &root, &root, |_| visits += 1).unwrap();
        assert_eq!(visits, 0);
    }

    #[test]
    fn one_empty() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(3)).unwrap();
        let mut left_only = 0;
        merge_walk(&s, &root, &empty(), |p| {
            if p.left.is_some() && p.right.is_none() {
                left_only += 1;
            }
        })
        .unwrap();
        assert_eq!(left_only, 3);

        let mut right_only = 0;
        merge_walk(&s, &empty(), &root, |p| {
            if p.right.is_some() && p.left.is_none() {
                right_only += 1;
            }
        })
        .unwrap();
        assert_eq!(right_only, 3);
    }

    #[test]
    fn intersection_only_matching_values() {
        let s = MemStore::new();
        let a = bulk_build(&s, entries(10)).unwrap();
        // Same keys, but mutate one value.
        let b = put(&s, &a, b"k-0005".to_vec(), Value::Inline(b"different".to_vec())).unwrap();
        let inter = intersection(&s, &a, &b).unwrap();
        assert_eq!(inter.len(), 9); // k-0005 differs, others match
    }

    #[test]
    fn difference_left_minus_right() {
        let s = MemStore::new();
        let a = bulk_build(&s, entries(10)).unwrap();
        let b = bulk_build(&s, entries(5)).unwrap();
        let d = difference(&s, &a, &b).unwrap();
        assert_eq!(d.len(), 5);
        for (k, _) in &d {
            let s = std::str::from_utf8(k).unwrap();
            let n: usize = s.trim_start_matches("k-").parse().unwrap();
            assert!(n >= 5);
        }
    }

    #[test]
    fn union_right_wins() {
        let s = MemStore::new();
        let a = bulk_build(&s, entries(5)).unwrap();
        let b = put(&s, &a, b"k-0002".to_vec(), Value::Inline(b"override".to_vec())).unwrap();
        let u = union(&s, &a, &b).unwrap();
        assert_eq!(u.len(), 5);
        let v = u.iter().find(|(k, _)| k == b"k-0002").unwrap();
        assert_eq!(v.1, Value::Inline(b"override".to_vec()));
    }

    #[test]
    fn merge_walk_disjoint_trees() {
        let s = MemStore::new();
        let a = bulk_build(
            &s,
            (0..5)
                .map(|i| (format!("a-{}", i).into_bytes(), Value::Inline(b"v".to_vec())))
                .collect(),
        )
        .unwrap();
        let b = bulk_build(
            &s,
            (0..5)
                .map(|i| (format!("b-{}", i).into_bytes(), Value::Inline(b"v".to_vec())))
                .collect(),
        )
        .unwrap();
        let mut both = 0;
        let mut left = 0;
        let mut right = 0;
        merge_walk(&s, &a, &b, |p| match (p.left, p.right) {
            (Some(_), Some(_)) => both += 1,
            (Some(_), None) => left += 1,
            (None, Some(_)) => right += 1,
            (None, None) => {}
        })
        .unwrap();
        assert_eq!(both, 0);
        assert_eq!(left, 5);
        assert_eq!(right, 5);
    }

    #[test]
    fn diff_walk_skips_shared_leaves() {
        let s = CountingStore::new();
        let n = 5000;
        let a = bulk_build(&s, entries(n)).unwrap();
        // Modify only one key — most leaves remain shared.
        let b = put(&s, &a, b"k-2500".to_vec(), Value::Inline(b"X".to_vec())).unwrap();

        // Baseline: merge_walk visits every key (no skip).
        s.reset();
        let mut all_visits = 0;
        merge_walk(&s, &a, &b, |_| all_visits += 1).unwrap();
        let merge_gets = s.gets();
        assert_eq!(all_visits, n);

        // diff_walk should visit only the divergent key and load far fewer nodes.
        s.reset();
        let mut diff_visits = 0;
        diff_walk(&s, &a, &b, |_| diff_visits += 1).unwrap();
        let diff_gets = s.gets();
        assert_eq!(diff_visits, 1);
        assert!(
            diff_gets * 4 < merge_gets,
            "diff_walk should be much cheaper: {} vs {}",
            diff_gets,
            merge_gets
        );
    }

    #[test]
    fn merge_walk_multilevel() {
        let s = MemStore::new();
        let n = 2000;
        let a = bulk_build(&s, entries(n)).unwrap();
        // Modify ~10% of values.
        let mut b = a;
        for i in (0..n).step_by(10) {
            b = put(&s, &b, format!("k-{:04}", i).into_bytes(), Value::Inline(b"X".to_vec()))
                .unwrap();
        }
        let mut equal = 0;
        let mut differ = 0;
        let mut left_only = 0;
        let mut right_only = 0;
        merge_walk(&s, &a, &b, |p| match (p.left, p.right) {
            (Some(l), Some(r)) => {
                if l == r {
                    equal += 1;
                } else {
                    differ += 1;
                }
            }
            (Some(_), None) => left_only += 1,
            (None, Some(_)) => right_only += 1,
            (None, None) => {}
        })
        .unwrap();
        assert_eq!(left_only, 0);
        assert_eq!(right_only, 0);
        assert_eq!(differ, 200);
        assert_eq!(equal, n - 200);
    }
}
