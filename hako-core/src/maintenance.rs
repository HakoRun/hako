//! Workspace-wide maintenance: garbage collection and integrity check.
//!
//! - `gc` walks every reachable object across all containers (every branch
//!   commit chain + working tree) and deletes stored objects not in that set.
//! - `fsck` walks the same reachable set and reports any objects that
//!   cannot be loaded or decoded — i.e. dangling references or bit-rot.

use crate::hash::Hash;
use crate::repo::Repo;
use crate::state::State;
use std::collections::HashSet;
use std::io;

#[derive(Debug, Default)]
pub struct GcReport {
    /// Objects present in the store before GC.
    pub total_objects: usize,
    /// Objects reachable from any ref or working tree.
    pub reachable: usize,
    /// Objects deleted (or that would be deleted in dry-run mode).
    pub deleted: usize,
    /// Bytes freed (sum of deleted object sizes; 0 if dry_run).
    pub bytes_freed: u64,
}

#[derive(Debug, Default)]
pub struct FsckReport {
    /// Objects walked successfully.
    pub checked: usize,
    /// Per-object problems found. Each entry is `(hash, description)`.
    pub problems: Vec<(Hash, String)>,
}

impl FsckReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Walk every reachable object across every container (HEAD chain + working
/// tree of every branch + each container's session-current branch). Returns
/// the deduplicated set.
fn reachable_in_workspace(state: &State) -> io::Result<HashSet<Hash>> {
    let mut all = HashSet::new();
    for container in state.list_containers()? {
        let repo = state.open_container(&container)?;
        union_repo_reachable(&repo, &mut all)?;
    }
    Ok(all)
}

fn union_repo_reachable(repo: &Repo<'_>, out: &mut HashSet<Hash>) -> io::Result<()> {
    // Every branch tip.
    for branch in repo.list_branches()? {
        if let Some(commit) = repo.read_ref(&branch)? {
            for h in repo.reachable_objects(commit)? {
                out.insert(h);
            }
        }
    }
    // Working tree may differ from HEAD; walk it explicitly so an in-progress
    // edit isn't garbage-collected. `working_tree` returns `Hash::zero()` for
    // a never-touched workspace — skip that case.
    let working = repo.working_tree()?;
    if working != Hash::zero() {
        // walk_tree is private; use a synthetic commit-less walk via the
        // reachable_objects entry point. We can't pass a tree where it expects
        // a commit, so walk it via a dummy: call reachable_objects(commit) for
        // each commit hash, then add the working tree's nodes manually below.
        crate::repo::walk_tree_for_maintenance(repo.store(), working, out)?;
    }
    Ok(())
}

/// Run garbage collection. With `dry_run = true`, computes what would be
/// deleted without touching disk.
pub fn gc(state: &State, dry_run: bool) -> io::Result<GcReport> {
    let reachable = reachable_in_workspace(state)?;
    // Both containers share the workspace's chunk store, so we open any
    // container's repo just to get a handle to the store.
    let any_container = state
        .list_containers()?
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no containers in workspace"))?;
    let repo = state.open_container(&any_container)?;
    let store = repo.store();

    let all = store.find_by_prefix("")?;
    let mut report = GcReport {
        total_objects: all.len(),
        reachable: reachable.len(),
        ..Default::default()
    };

    for hash in all {
        if reachable.contains(&hash) {
            continue;
        }
        // Account for size before deleting (best-effort).
        let size = store.get(&hash)?.map(|b| b.len() as u64).unwrap_or(0);
        if !dry_run {
            store.delete(&hash)?;
        }
        report.deleted += 1;
        report.bytes_freed += size;
    }
    Ok(report)
}

/// Run an integrity check. Walks every reachable object across the workspace
/// (commits, tree nodes, working trees, and external file content blobs) and
/// verifies each one loads with a matching hash. Collects problems rather
/// than bailing on the first one.
pub fn fsck(state: &State) -> io::Result<FsckReport> {
    let mut report = FsckReport::default();
    let mut all_reachable: HashSet<Hash> = HashSet::new();

    // Phase 1: walk every root and union the reachable sets. Each per-root
    // walk that fails outright is recorded as a problem against that root.
    for container in state.list_containers()? {
        let repo = state.open_container(&container)?;
        for branch in repo.list_branches()? {
            let commit_hash = match repo.read_ref(&branch)? {
                Some(c) => c,
                None => continue,
            };
            match repo.reachable_objects(commit_hash) {
                Ok(set) => all_reachable.extend(set),
                Err(e) => {
                    report.problems.push((
                        commit_hash,
                        format!("container {}, branch {}: {}", container, branch, e),
                    ));
                }
            }
        }
        // Working tree may diverge from any branch — walk it so its objects
        // are also verified.
        let working = repo.working_tree()?;
        if working != Hash::zero() {
            if let Err(e) =
                crate::repo::walk_tree_for_maintenance(repo.store(), working, &mut all_reachable)
            {
                report.problems.push((
                    working,
                    format!("container {}, working tree: {}", container, e),
                ));
            }
        }
    }

    // Phase 2: for every reachable object, fetch it from the store. This
    // forces FsStore's content-vs-hash verification to fire, catching bit
    // rot in tree nodes, commit objects, AND external file content (which
    // the reachability walk records but doesn't load).
    let store = match state.list_containers()?.into_iter().next() {
        Some(c) => state.open_container(&c)?,
        None => {
            report.checked = all_reachable.len();
            return Ok(report);
        }
    };
    let store = store.store();
    for hash in &all_reachable {
        match store.get(hash) {
            Ok(Some(_)) => {}
            Ok(None) => {
                report
                    .problems
                    .push((*hash, "object missing or hash mismatch".into()));
            }
            Err(e) => {
                report.problems.push((*hash, format!("read error: {}", e)));
            }
        }
    }

    report.checked = all_reachable.len();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workdir() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    fn init_with_one_container() -> (TempDir, State) {
        // State::init auto-creates the "hako" container, populated with
        // the embedded toybox rootfs on platforms that have it.
        let d = workdir();
        let s = State::init(d.path()).unwrap();
        (d, s)
    }

    #[test]
    fn gc_on_empty_workspace_deletes_nothing() {
        let (_d, s) = init_with_one_container();
        let report = gc(&s, false).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.bytes_freed, 0);
    }

    #[test]
    fn gc_drops_unreachable_blobs() {
        let (_d, s) = init_with_one_container();
        // Write a blob that nothing references.
        let repo = s.open_container("hako").unwrap();
        let orphan = repo.store().put(b"i am unreachable").unwrap();
        assert!(repo.store().has(&orphan).unwrap());

        let report = gc(&s, false).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(report.bytes_freed > 0);
        assert!(!repo.store().has(&orphan).unwrap());
    }

    #[test]
    fn gc_dry_run_reports_but_keeps() {
        let (_d, s) = init_with_one_container();
        let repo = s.open_container("hako").unwrap();
        let orphan = repo.store().put(b"keep me").unwrap();

        let report = gc(&s, true).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(repo.store().has(&orphan).unwrap(), "dry run should not delete");
    }

    #[test]
    fn fsck_clean_workspace_reports_ok() {
        let (_d, s) = init_with_one_container();
        let report = fsck(&s).unwrap();
        assert!(report.ok());
    }

    #[test]
    fn fsck_dedupes_count_across_branches() {
        // Two branches pointing at the same commit shouldn't double-count.
        let (_d, s) = init_with_one_container();
        let repo = s.open_container("hako").unwrap();
        // Make a commit so HEAD has something to point at.
        let scoped = crate::ScopedFs::new(repo.store());
        let working = scoped
            .write_file(&repo.working_tree().unwrap(), "x.txt", b"hi")
            .unwrap();
        repo.set_working(working).unwrap();
        let c = repo.commit(working, vec![], "u", "m", 0).unwrap();
        repo.write_ref("main", c).unwrap();
        repo.write_ref("alias", c).unwrap();
        let report = fsck(&s).unwrap();
        assert!(report.ok());
        // checked should be the unique count, not 2x.
        let single_branch_count = repo.reachable_objects(c).unwrap().len();
        assert_eq!(report.checked, single_branch_count);
    }
}
