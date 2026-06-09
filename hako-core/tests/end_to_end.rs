//! End-to-end integration tests exercising the public library API across
//! init -> write -> commit -> branch -> merge -> diff lifecycles, plus
//! cross-container dedup and RouteTarget dispatch.

use hako::fs::DirKind;
use hako::store::ChunkStore;
use hako::tree::{three_way_merge, Conflict, DiffEntry};
use hako::{Hash, RouteTarget, ScopedFs, Session, State};
use std::io;
use tempfile::TempDir;

/// Workspace + a fresh empty `hako`-named test container. The default
/// `hako` container that `State::init` creates now ships pre-populated
/// with the embedded toybox rootfs, which is great for end users but
/// noise for tests that want a clean slate. We delete that container
/// and create a fresh empty one with the same name so the existing
/// test bodies (which assume `hako` is empty) keep working.
fn fresh() -> (TempDir, State) {
    let d = TempDir::new().unwrap();
    let s = State::init(d.path()).unwrap();
    s.delete_container("hako").unwrap();
    s.create_container("hako").unwrap();
    (d, s)
}

fn write(state: &State, container: &str, path: &str, bytes: &[u8]) -> io::Result<Hash> {
    let repo = state.open_container(container)?;
    let scoped = ScopedFs::new(repo.store());
    let root = repo.working_tree()?;
    let new_root = scoped.write_file(&root, path, bytes)?;
    repo.set_working(new_root)?;
    Ok(new_root)
}

fn commit(state: &State, container: &str, msg: &str, ts: u64) -> io::Result<Hash> {
    let repo = state.open_container(container)?;
    let work = repo.working_tree()?;
    let parents: Vec<Hash> = repo.head_commit()?.into_iter().collect();
    let c = repo.commit(work, parents, "tester", msg, ts)?;
    let branch = repo.current_branch()?.unwrap();
    repo.write_ref(&branch, c)?;
    Ok(c)
}

#[test]
fn init_write_commit_log_workflow() {
    let (_d, s) = fresh();
    write(&s, "hako", "/hello.txt", b"world").unwrap();
    let c1 = commit(&s, "hako", "first", 100).unwrap();

    write(&s, "hako", "/hello.txt", b"world!").unwrap();
    let c2 = commit(&s, "hako", "second", 200).unwrap();

    let repo = s.open_container("hako").unwrap();
    let log = repo.log(c2).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].0, c2);
    assert_eq!(log[0].1.message, "second");
    assert_eq!(log[1].0, c1);
    assert_eq!(log[1].1.message, "first");
}

#[test]
fn read_back_after_commit() {
    let (_d, s) = fresh();
    write(&s, "hako", "/a/b/c.txt", b"hello prolly").unwrap();
    commit(&s, "hako", "add", 1).unwrap();

    let repo = s.open_container("hako").unwrap();
    let scoped = ScopedFs::new(repo.store());
    let head = repo.head_tree().unwrap();
    let bytes = scoped.read_file(&head, "/a/b/c.txt").unwrap();
    assert_eq!(bytes, b"hello prolly");
}

#[test]
fn branch_clean_merge_no_conflict() {
    let (_d, s) = fresh();
    write(&s, "hako", "/shared.txt", b"base").unwrap();
    let base = commit(&s, "hako", "base", 100).unwrap();

    let repo = s.open_container("hako").unwrap();
    repo.write_ref("feature", base).unwrap();

    // Stay on main, add a file there.
    write(&s, "hako", "/main_only.txt", b"main side").unwrap();
    let ours = commit(&s, "hako", "main file", 200).unwrap();

    // Switch to feature and add a different file.
    let repo = s.open_container("hako").unwrap();
    repo.set_branch("feature").unwrap();
    let feature_tree = repo.load_commit(&base).unwrap().tree;
    repo.set_working(feature_tree).unwrap();

    write(&s, "hako", "/feature_only.txt", b"feature side").unwrap();
    let theirs = commit(&s, "hako", "feature file", 300).unwrap();

    // Merge feature into ... wait, we're on feature now. Switch back to main.
    let repo = s.open_container("hako").unwrap();
    repo.set_branch("hako").unwrap();
    let main_tree = repo.load_commit(&ours).unwrap().tree;
    repo.set_working(main_tree).unwrap();

    let base_tree = repo.load_commit(&base).unwrap().tree;
    let ours_tree = repo.load_commit(&ours).unwrap().tree;
    let theirs_tree = repo.load_commit(&theirs).unwrap().tree;
    let result = three_way_merge(repo.store(), &base_tree, &ours_tree, &theirs_tree).unwrap();

    assert!(result.conflicts.is_empty(), "expected no conflicts");

    let scoped = ScopedFs::new(repo.store());
    assert_eq!(
        scoped.read_file(&result.merged, "/main_only.txt").unwrap(),
        b"main side"
    );
    assert_eq!(
        scoped
            .read_file(&result.merged, "/feature_only.txt")
            .unwrap(),
        b"feature side"
    );
    assert_eq!(
        scoped.read_file(&result.merged, "/shared.txt").unwrap(),
        b"base"
    );
}

#[test]
fn merge_with_both_modified_conflict() {
    let (_d, s) = fresh();
    write(&s, "hako", "/conflict.txt", b"base").unwrap();
    let base = commit(&s, "hako", "base", 100).unwrap();

    let repo = s.open_container("hako").unwrap();
    repo.write_ref("feature", base).unwrap();

    write(&s, "hako", "/conflict.txt", b"ours").unwrap();
    let ours = commit(&s, "hako", "ours edit", 200).unwrap();

    // Switch to feature.
    let repo = s.open_container("hako").unwrap();
    repo.set_branch("feature").unwrap();
    let feature_tree = repo.load_commit(&base).unwrap().tree;
    repo.set_working(feature_tree).unwrap();
    write(&s, "hako", "/conflict.txt", b"theirs").unwrap();
    let theirs = commit(&s, "hako", "theirs edit", 300).unwrap();

    // Merge from main's perspective.
    let repo = s.open_container("hako").unwrap();
    repo.set_branch("hako").unwrap();

    let base_tree = repo.load_commit(&base).unwrap().tree;
    let ours_tree = repo.load_commit(&ours).unwrap().tree;
    let theirs_tree = repo.load_commit(&theirs).unwrap().tree;
    let result = three_way_merge(repo.store(), &base_tree, &ours_tree, &theirs_tree).unwrap();

    assert_eq!(result.conflicts.len(), 1);
    match &result.conflicts[0] {
        Conflict::BothModified { key, .. } => {
            // ScopedFs normalizes paths and stores without leading slash.
            assert_eq!(key.as_slice(), b"conflict.txt");
        }
        c => panic!("unexpected conflict type: {:?}", c),
    }
}

#[test]
fn cross_container_dedup_chunks() {
    let (_d, s) = fresh();
    s.create_container("alpha").unwrap();
    s.create_container("beta").unwrap();

    // Big enough to bypass INLINE_THRESHOLD so the content is chunked separately.
    let payload = vec![7u8; 4096];

    write(&s, "alpha", "/file.bin", &payload).unwrap();
    write(&s, "beta", "/copy.bin", &payload).unwrap();

    // The blob hash should exist exactly once on disk.
    let h = Hash::of(&payload);
    assert!(s.store().has(&h).unwrap());

    // Both containers should still read it back.
    for c in ["alpha", "beta"] {
        let repo = s.open_container(c).unwrap();
        let scoped = ScopedFs::new(repo.store());
        let work = repo.working_tree().unwrap();
        let path = if c == "alpha" {
            "/file.bin"
        } else {
            "/copy.bin"
        };
        assert_eq!(scoped.read_file(&work, path).unwrap(), payload);
    }
}

#[test]
fn diff_added_modified_removed() {
    let (_d, s) = fresh();
    write(&s, "hako", "/keep.txt", b"keep").unwrap();
    write(&s, "hako", "/edit.txt", b"v1").unwrap();
    write(&s, "hako", "/gone.txt", b"goodbye").unwrap();
    commit(&s, "hako", "base", 100).unwrap();

    // Stage modifications without committing.
    let repo = s.open_container("hako").unwrap();
    let scoped = ScopedFs::new(repo.store());
    let work = repo.working_tree().unwrap();
    let work = scoped.write_file(&work, "/edit.txt", b"v2").unwrap();
    let work = scoped.delete(&work, "/gone.txt").unwrap();
    let work = scoped.write_file(&work, "/added.txt", b"new").unwrap();
    repo.set_working(work).unwrap();

    let head = repo.head_tree().unwrap();
    let work = repo.working_tree().unwrap();
    let diffs = hako::tree::diff(repo.store(), &head, &work).unwrap();

    let mut added = 0;
    let mut modified = 0;
    let mut removed = 0;
    for d in &diffs {
        match d {
            DiffEntry::Added { .. } => added += 1,
            DiffEntry::Modified { .. } => modified += 1,
            DiffEntry::Removed { .. } => removed += 1,
        }
    }
    assert_eq!((added, modified, removed), (1, 1, 1), "diffs: {:?}", diffs);
}

#[test]
fn route_target_dispatches_to_named_container() {
    let (_d, s) = fresh();
    s.create_container("alpha").unwrap();

    // Write to alpha via /containers/alpha/file.txt routing.
    let target = RouteTarget::parse("/containers/alpha/file.txt");
    match target {
        RouteTarget::Container { name, path } => {
            assert_eq!(name, "alpha");
            assert_eq!(path, "file.txt");
            let repo = s.open_container(&name).unwrap();
            let scoped = ScopedFs::new(repo.store());
            let root = repo.working_tree().unwrap();
            let new = scoped.write_file(&root, &path, b"alpha-data").unwrap();
            repo.set_working(new).unwrap();
        }
        _ => panic!("expected Container target"),
    }

    // The default "hako" container should NOT see this file.
    let main = s.open_container("hako").unwrap();
    let scoped = ScopedFs::new(main.store());
    let mw = main.working_tree().unwrap();
    assert!(!scoped.exists(&mw, "/file.txt").unwrap());

    // alpha sees it.
    let alpha = s.open_container("alpha").unwrap();
    let scoped = ScopedFs::new(alpha.store());
    let aw = alpha.working_tree().unwrap();
    assert_eq!(scoped.read_file(&aw, "/file.txt").unwrap(), b"alpha-data");
}

#[test]
fn checkout_round_trip_branches() {
    let (_d, s) = fresh();
    write(&s, "hako", "/file.txt", b"on main").unwrap();
    let main_commit = commit(&s, "hako", "main work", 100).unwrap();

    let repo = s.open_container("hako").unwrap();
    repo.write_ref("dev", main_commit).unwrap();
    repo.set_branch("dev").unwrap();
    let dev_tree = repo.load_commit(&main_commit).unwrap().tree;
    repo.set_working(dev_tree).unwrap();

    write(&s, "hako", "/dev.txt", b"on dev").unwrap();
    let dev_commit = commit(&s, "hako", "dev work", 200).unwrap();

    // Switch back to main, dev.txt should NOT be visible.
    let repo = s.open_container("hako").unwrap();
    repo.set_branch("hako").unwrap();
    let main_tree = repo.load_commit(&main_commit).unwrap().tree;
    repo.set_working(main_tree).unwrap();

    let scoped = ScopedFs::new(repo.store());
    let work = repo.working_tree().unwrap();
    assert!(scoped.exists(&work, "/file.txt").unwrap());
    assert!(!scoped.exists(&work, "/dev.txt").unwrap());

    // Switch back to dev, dev.txt visible again.
    let _ = dev_commit;
    repo.set_branch("dev").unwrap();
    let dev_tree = repo.load_commit(&dev_commit).unwrap().tree;
    repo.set_working(dev_tree).unwrap();
    let work = repo.working_tree().unwrap();
    assert!(scoped.exists(&work, "/file.txt").unwrap());
    assert!(scoped.exists(&work, "/dev.txt").unwrap());
}

#[test]
fn ls_returns_directory_children() {
    let (_d, s) = fresh();
    write(&s, "hako", "/a/x.txt", b"x").unwrap();
    write(&s, "hako", "/a/y.txt", b"y").unwrap();
    write(&s, "hako", "/b/z.txt", b"z").unwrap();

    let repo = s.open_container("hako").unwrap();
    let scoped = ScopedFs::new(repo.store());
    let work = repo.working_tree().unwrap();

    let mut top: Vec<String> = scoped
        .ls(&work, "/")
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    top.sort();
    assert_eq!(top, vec!["a", "b"]);

    let mut a: Vec<String> = scoped
        .ls(&work, "/a")
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    a.sort();
    assert_eq!(a, vec!["x.txt", "y.txt"]);
}

#[test]
fn workspace_persists_across_state_open() {
    let d = TempDir::new().unwrap();
    {
        let s = State::init(d.path()).unwrap();
        write(&s, "hako", "/persistent.txt", b"survives").unwrap();
        commit(&s, "hako", "snapshot", 1).unwrap();
    }
    // Reopen.
    let s2 = State::open(d.path()).unwrap();
    let repo = s2.open_container("hako").unwrap();
    let scoped = ScopedFs::new(repo.store());
    let head = repo.head_tree().unwrap();
    assert_eq!(
        scoped.read_file(&head, "/persistent.txt").unwrap(),
        b"survives"
    );
}

#[test]
fn session_roundtrip_persists_across_open() {
    let d = TempDir::new().unwrap();
    {
        let s = State::init(d.path()).unwrap();
        s.create_container("alpha").unwrap();
        s.write_session(&Session {
            container: "alpha".into(),
            cwd: "/sub/dir".into(),
        })
        .unwrap();
    }
    let s2 = State::open(d.path()).unwrap();
    let got = s2.read_session().unwrap();
    assert_eq!(got.container, "alpha");
    assert_eq!(got.cwd, "/sub/dir");
}

#[test]
fn session_default_when_no_file() {
    let (_d, s) = fresh();
    let got = s.read_session().unwrap();
    assert_eq!(got, Session::default());
}

#[test]
fn fetch_copies_only_reachable_objects() {
    // Two independent workspaces (separate chunk stores). Build a commit chain
    // in workspace A; sync it into workspace B by enumerating reachable objects
    // and copying them across.
    let da = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();
    let a = State::init(da.path()).unwrap();
    let b = State::init(db.path()).unwrap();

    let big = vec![5u8; 4096];
    write(&a, "hako", "/file.txt", b"hello").unwrap();
    let _c1 = commit(&a, "hako", "first", 100).unwrap();
    write(&a, "hako", "/big.bin", &big).unwrap();
    let c2 = commit(&a, "hako", "second", 200).unwrap();

    let a_repo = a.open_container("hako").unwrap();
    let b_repo = b.open_container("hako").unwrap();
    let reachable = a_repo.reachable_objects(c2).unwrap();

    let mut copied = 0;
    for h in &reachable {
        if b.store().has(h).unwrap() {
            continue;
        }
        let bytes = a.store().get(h).unwrap().unwrap();
        b.store().put(&bytes).unwrap();
        copied += 1;
    }
    assert!(copied > 0);

    // Wire the local ref so b can read it. The default branch in any new
    // repo is `main`, regardless of the container's name.
    b_repo.write_ref("main", c2).unwrap();
    let b_head = b_repo.head_commit().unwrap();
    assert_eq!(b_head, Some(c2));

    let b_tree = b_repo.head_tree().unwrap();
    let scoped = ScopedFs::new(b_repo.store());
    assert_eq!(scoped.read_file(&b_tree, "/file.txt").unwrap(), b"hello");
    assert_eq!(scoped.read_file(&b_tree, "/big.bin").unwrap(), big);

    // Re-running the copy should be idempotent — every object now exists locally.
    let mut copied2 = 0;
    for h in &reachable {
        if !b.store().has(h).unwrap() {
            copied2 += 1;
        }
    }
    assert_eq!(
        copied2, 0,
        "all reachable objects should already be present"
    );
}

#[test]
fn cross_container_cp_uses_shared_chunks() {
    // Cross-container cp (different container, same workspace) should reuse the
    // already-stored file content chunks — the file is readable from both sides
    // and the chunk count doesn't grow.
    let (_d, s) = fresh();
    s.create_container("alpha").unwrap();
    let payload = vec![9u8; 4096];
    write(&s, "hako", "/source.bin", &payload).unwrap();

    let main = s.open_container("hako").unwrap();
    let alpha = s.open_container("alpha").unwrap();
    let scoped = ScopedFs::new(main.store());
    let main_root = main.working_tree().unwrap();
    let alpha_root = alpha.working_tree().unwrap();
    let new_alpha = scoped
        .cp_to(&main_root, &alpha_root, "/source.bin", "/copied.bin")
        .unwrap();
    alpha.set_working(new_alpha).unwrap();

    // Both containers see the same content.
    let alpha_tree = alpha.working_tree().unwrap();
    assert_eq!(
        scoped.read_file(&alpha_tree, "/copied.bin").unwrap(),
        payload
    );
    assert_eq!(
        scoped.read_file(&main_root, "/source.bin").unwrap(),
        payload
    );

    // The chunk lives once on disk.
    let h = Hash::of(&payload);
    assert!(s.store().has(&h).unwrap());
}

#[test]
fn nothing_to_commit_detected_via_tree_equality() {
    let (_d, s) = fresh();
    write(&s, "hako", "/a.txt", b"a").unwrap();
    let c1 = commit(&s, "hako", "first", 1).unwrap();

    let repo = s.open_container("hako").unwrap();
    let head_tree = repo.load_commit(&c1).unwrap().tree;
    assert_eq!(repo.working_tree().unwrap(), head_tree);
}

#[test]
fn file_metadata_roundtrips_through_commit() {
    let (_d, s) = fresh();
    let repo = s.open_container("hako").unwrap();
    let scoped = ScopedFs::new(repo.store());
    let root = scoped
        .write_file_meta(
            &repo.working_tree().unwrap(),
            "/exec.sh",
            b"#!/bin/sh\n",
            0o755,
            42,
        )
        .unwrap();
    repo.set_working(root).unwrap();
    commit(&s, "hako", "add exec", 100).unwrap();

    // Re-open the container and verify metadata survived the commit.
    let repo2 = s.open_container("hako").unwrap();
    let scoped2 = ScopedFs::new(repo2.store());
    let tree = repo2.working_tree().unwrap();
    let children = scoped2.ls(&tree, "").unwrap();
    let exec = children.iter().find(|c| c.name == "exec.sh").unwrap();
    assert_eq!(exec.kind, DirKind::File);
    assert_eq!(exec.mode, Some(0o755));
    assert_eq!(exec.mtime, Some(42));
}

#[test]
fn symlink_roundtrips_through_commit() {
    let (_d, s) = fresh();
    let repo = s.open_container("hako").unwrap();
    let scoped = ScopedFs::new(repo.store());
    let root = scoped
        .write_symlink(
            &repo.working_tree().unwrap(),
            "/link",
            b"../target",
            0o777,
            99,
        )
        .unwrap();
    repo.set_working(root).unwrap();
    commit(&s, "hako", "add link", 200).unwrap();

    let repo2 = s.open_container("hako").unwrap();
    let scoped2 = ScopedFs::new(repo2.store());
    let tree = repo2.working_tree().unwrap();
    assert_eq!(scoped2.read_symlink(&tree, "/link").unwrap(), b"../target");
    let children = scoped2.ls(&tree, "").unwrap();
    let link = children.iter().find(|c| c.name == "link").unwrap();
    assert_eq!(link.kind, DirKind::Symlink);
    assert_eq!(link.symlink_target.as_deref(), Some(b"../target".as_ref()));
    assert_eq!(link.mode, Some(0o777));
    assert_eq!(link.mtime, Some(99));
}
