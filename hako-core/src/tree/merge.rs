use super::types::*;
use crate::hash::Hash;
use crate::store::ChunkStore;
use std::collections::{BTreeMap, BTreeSet};
use std::io;

pub fn three_way_merge(
    store: &dyn ChunkStore,
    base: &Hash,
    ours: &Hash,
    theirs: &Hash,
) -> io::Result<MergeResult> {
    // Fast paths via content-addressed identity.
    if ours == theirs {
        return Ok(MergeResult {
            merged: *ours,
            conflicts: vec![],
        });
    }
    if base == ours {
        return Ok(MergeResult {
            merged: *theirs,
            conflicts: vec![],
        });
    }
    if base == theirs {
        return Ok(MergeResult {
            merged: *ours,
            conflicts: vec![],
        });
    }

    let base_map: BTreeMap<Vec<u8>, Value> =
        super::ops::scan(store, base)?.into_iter().collect();
    let ours_map: BTreeMap<Vec<u8>, Value> =
        super::ops::scan(store, ours)?.into_iter().collect();
    let theirs_map: BTreeMap<Vec<u8>, Value> =
        super::ops::scan(store, theirs)?.into_iter().collect();

    let mut all_keys: BTreeSet<Vec<u8>> = BTreeSet::new();
    all_keys.extend(base_map.keys().cloned());
    all_keys.extend(ours_map.keys().cloned());
    all_keys.extend(theirs_map.keys().cloned());

    let mut merged: Vec<(Vec<u8>, Value)> = Vec::new();
    let mut conflicts: Vec<Conflict> = Vec::new();

    for key in all_keys {
        let b = base_map.get(&key);
        let o = ours_map.get(&key);
        let t = theirs_map.get(&key);

        match (b, o, t) {
            (None, None, None) => {}
            (None, Some(ov), None) => merged.push((key, ov.clone())),
            (None, None, Some(tv)) => merged.push((key, tv.clone())),
            (None, Some(ov), Some(tv)) => {
                if ov == tv {
                    merged.push((key, ov.clone()));
                } else {
                    conflicts.push(Conflict::BothAdded {
                        key: key.clone(),
                        ours: ov.clone(),
                        theirs: tv.clone(),
                    });
                    merged.push((key, tv.clone()));
                }
            }
            (Some(_), None, None) => {
                // Both deleted: drop.
            }
            (Some(bv), Some(ov), None) => {
                if bv == ov {
                    // Ours unchanged, theirs deleted: take deletion.
                } else {
                    conflicts.push(Conflict::ModifyDelete {
                        key: key.clone(),
                        base: bv.clone(),
                        ours: ov.clone(),
                    });
                    merged.push((key, ov.clone()));
                }
            }
            (Some(bv), None, Some(tv)) => {
                if bv == tv {
                    // Theirs unchanged, ours deleted: take deletion.
                } else {
                    conflicts.push(Conflict::DeleteModify {
                        key: key.clone(),
                        base: bv.clone(),
                        theirs: tv.clone(),
                    });
                    merged.push((key, tv.clone()));
                }
            }
            (Some(bv), Some(ov), Some(tv)) => {
                if bv == ov && bv == tv {
                    merged.push((key, bv.clone()));
                } else if bv == ov {
                    // Ours unchanged from base — take theirs.
                    merged.push((key, tv.clone()));
                } else if bv == tv || ov == tv {
                    // Theirs unchanged from base, OR ours and theirs made the
                    // same change. Either way, take ours.
                    merged.push((key, ov.clone()));
                } else {
                    conflicts.push(Conflict::BothModified {
                        key: key.clone(),
                        base: bv.clone(),
                        ours: ov.clone(),
                        theirs: tv.clone(),
                    });
                    merged.push((key, tv.clone()));
                }
            }
        }
    }

    let merged_root = super::ops::bulk_build(store, merged)?;
    Ok(MergeResult {
        merged: merged_root,
        conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::super::ops::*;
    use super::*;
    use crate::store::MemStore;

    fn base_entries() -> Vec<(Vec<u8>, Value)> {
        (0..5)
            .map(|i| {
                (
                    format!("k-{}", i).into_bytes(),
                    Value::Inline(format!("v-{}", i).into_bytes()),
                )
            })
            .collect()
    }

    #[test]
    fn no_changes() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let m = three_way_merge(&s, &base, &base, &base).unwrap();
        assert_eq!(m.merged, base);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn ours_only() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(&s, &base, b"new".to_vec(), Value::Inline(b"o".to_vec())).unwrap();
        let m = three_way_merge(&s, &base, &ours, &base).unwrap();
        assert_eq!(m.merged, ours);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn disjoint_changes() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(
            &s,
            &base,
            b"ours-only".to_vec(),
            Value::Inline(b"o".to_vec()),
        )
        .unwrap();
        let theirs = put(
            &s,
            &base,
            b"theirs-only".to_vec(),
            Value::Inline(b"t".to_vec()),
        )
        .unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert!(m.conflicts.is_empty());
        assert_eq!(
            get(&s, &m.merged, b"ours-only").unwrap(),
            Some(Value::Inline(b"o".to_vec()))
        );
        assert_eq!(
            get(&s, &m.merged, b"theirs-only").unwrap(),
            Some(Value::Inline(b"t".to_vec()))
        );
    }

    #[test]
    fn both_modified_is_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(&s, &base, b"k-2".to_vec(), Value::Inline(b"O".to_vec())).unwrap();
        let theirs = put(&s, &base, b"k-2".to_vec(), Value::Inline(b"T".to_vec())).unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0], Conflict::BothModified { .. }));
        // Theirs wins as placeholder.
        assert_eq!(
            get(&s, &m.merged, b"k-2").unwrap(),
            Some(Value::Inline(b"T".to_vec()))
        );
    }

    #[test]
    fn modify_delete_is_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(
            &s,
            &base,
            b"k-2".to_vec(),
            Value::Inline(b"modified".to_vec()),
        )
        .unwrap();
        let theirs = delete(&s, &base, b"k-2").unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0], Conflict::ModifyDelete { .. }));
    }

    #[test]
    fn delete_modify_is_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = delete(&s, &base, b"k-2").unwrap();
        let theirs = put(
            &s,
            &base,
            b"k-2".to_vec(),
            Value::Inline(b"modified".to_vec()),
        )
        .unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0], Conflict::DeleteModify { .. }));
    }

    #[test]
    fn both_added_same_value_no_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(&s, &base, b"new".to_vec(), Value::Inline(b"same".to_vec())).unwrap();
        let theirs = put(&s, &base, b"new".to_vec(), Value::Inline(b"same".to_vec())).unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert!(m.conflicts.is_empty());
        assert_eq!(
            get(&s, &m.merged, b"new").unwrap(),
            Some(Value::Inline(b"same".to_vec()))
        );
    }

    #[test]
    fn both_added_different_value_is_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(&s, &base, b"new".to_vec(), Value::Inline(b"o".to_vec())).unwrap();
        let theirs = put(&s, &base, b"new".to_vec(), Value::Inline(b"t".to_vec())).unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0], Conflict::BothAdded { .. }));
    }

    #[test]
    fn both_deleted_no_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = delete(&s, &base, b"k-2").unwrap();
        let theirs = delete(&s, &base, b"k-2").unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert!(m.conflicts.is_empty());
        assert_eq!(get(&s, &m.merged, b"k-2").unwrap(), None);
    }

    #[test]
    fn convergent_modifications_no_conflict() {
        let s = MemStore::new();
        let base = bulk_build(&s, base_entries()).unwrap();
        let ours = put(&s, &base, b"k-2".to_vec(), Value::Inline(b"X".to_vec())).unwrap();
        let theirs = put(&s, &base, b"k-2".to_vec(), Value::Inline(b"X".to_vec())).unwrap();
        let m = three_way_merge(&s, &base, &ours, &theirs).unwrap();
        assert!(m.conflicts.is_empty());
        assert_eq!(
            get(&s, &m.merged, b"k-2").unwrap(),
            Some(Value::Inline(b"X".to_vec()))
        );
    }
}
