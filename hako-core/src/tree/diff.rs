use super::set_ops::diff_walk;
use super::types::*;
use crate::hash::Hash;
use crate::store::ChunkStore;
use std::io;

pub fn diff(store: &dyn ChunkStore, a: &Hash, b: &Hash) -> io::Result<Vec<DiffEntry>> {
    let mut out = Vec::new();
    diff_walk(store, a, b, |p| match (p.left, p.right) {
        (Some(l), None) => out.push(DiffEntry::Removed {
            key: p.key,
            value: l,
        }),
        (None, Some(r)) => out.push(DiffEntry::Added {
            key: p.key,
            value: r,
        }),
        (Some(l), Some(r)) => out.push(DiffEntry::Modified {
            key: p.key,
            old: l,
            new: r,
        }),
        _ => {}
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::ops::*;
    use super::*;
    use crate::store::MemStore;

    fn entries(n: usize) -> Vec<(Vec<u8>, Value)> {
        (0..n)
            .map(|i| {
                (
                    format!("key-{:08}", i).into_bytes(),
                    Value::Inline(format!("val-{}", i).into_bytes()),
                )
            })
            .collect()
    }

    #[test]
    fn identical_is_empty() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(50)).unwrap();
        assert_eq!(diff(&s, &root, &root).unwrap().len(), 0);
    }

    #[test]
    fn diff_against_empty() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(3)).unwrap();
        let d = diff(&s, &empty(), &root).unwrap();
        assert_eq!(d.len(), 3);
        assert!(d.iter().all(|e| matches!(e, DiffEntry::Added { .. })));

        let d = diff(&s, &root, &empty()).unwrap();
        assert_eq!(d.len(), 3);
        assert!(d.iter().all(|e| matches!(e, DiffEntry::Removed { .. })));
    }

    #[test]
    fn detects_add_modify_remove() {
        let s = MemStore::new();
        let a = bulk_build(&s, entries(10)).unwrap();
        let b = put(
            &s,
            &a,
            b"key-00000003".to_vec(),
            Value::Inline(b"changed".to_vec()),
        )
        .unwrap();
        let b = put(&s, &b, b"new".to_vec(), Value::Inline(b"yes".to_vec())).unwrap();
        let b = delete(&s, &b, b"key-00000007").unwrap();

        let d = diff(&s, &a, &b).unwrap();
        assert!(d
            .iter()
            .any(|e| matches!(e, DiffEntry::Modified { key, .. } if key == b"key-00000003")));
        assert!(d
            .iter()
            .any(|e| matches!(e, DiffEntry::Added { key, .. } if key == b"new")));
        assert!(d
            .iter()
            .any(|e| matches!(e, DiffEntry::Removed { key, .. } if key == b"key-00000007")));
        assert_eq!(d.len(), 3);
    }
}
