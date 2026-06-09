//! End-to-end CLI tests: spawn the real `hako` binary against a temporary
//! workspace and exercise the platform-independent command surface (the VFS +
//! version-control commands). These need no container runtime, so they run on
//! every platform CI covers. The Linux container runtime is covered separately
//! by `scripts/isolation-check.sh` (the CI `isolation` job).

use std::path::Path;
use std::process::{Command, Output};

/// Run `hako <args>` in `dir`.
fn hako(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hako"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn hako")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Run, asserting success, and return stdout.
fn ok(dir: &Path, args: &[&str]) -> String {
    let o = hako(dir, args);
    assert!(
        o.status.success(),
        "`hako {}` failed (code {:?}): {}",
        args.join(" "),
        o.status.code(),
        err(&o)
    );
    out(&o)
}

/// A fresh initialized workspace.
fn workspace() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    ok(d.path(), &["init"]);
    d
}

#[test]
fn init_creates_dot_hako_and_default_container() {
    let d = tempfile::tempdir().unwrap();
    ok(d.path(), &["init"]);
    assert!(
        d.path().join(".hako").is_dir(),
        ".hako workspace dir created"
    );
    // Default toybox container exists.
    assert!(ok(d.path(), &["containers"]).contains("hako"));
}

#[test]
fn write_then_cat_roundtrips_exactly() {
    let d = workspace();
    ok(d.path(), &["write", "/notes/hi.txt", "hello hako"]);
    assert_eq!(ok(d.path(), &["cat", "/notes/hi.txt"]), "hello hako");
}

#[test]
fn ls_shows_written_file_and_seeded_rootfs() {
    let d = workspace();
    ok(d.path(), &["write", "/myfile", "x"]);
    let listing = ok(d.path(), &["ls", "/"]);
    assert!(
        listing.contains("myfile"),
        "ls should show the new file: {listing}"
    );
    assert!(
        listing.contains("bin"),
        "ls should show the seeded toybox rootfs: {listing}"
    );
}

#[test]
fn commit_appears_in_log_and_leaves_clean_status() {
    let d = workspace();
    ok(d.path(), &["write", "/a.txt", "one"]);
    ok(d.path(), &["commit", "-m", "first commit"]);
    assert!(ok(d.path(), &["log"]).contains("first commit"));
    // After committing, the working tree matches HEAD.
    let status = ok(d.path(), &["status"]).to_lowercase();
    assert!(
        status.contains("nothing to commit") || status.contains("clean"),
        "status should be clean after commit: {status}"
    );
}

#[test]
fn cat_via_ref_reads_from_a_commit() {
    let d = workspace();
    ok(d.path(), &["write", "/r.txt", "refdata"]);
    ok(d.path(), &["commit", "-m", "c"]);
    // Mutate the working tree; the committed ref should still read the old value.
    ok(d.path(), &["write", "/r.txt", "changed"]);
    assert_eq!(ok(d.path(), &["cat", "main:/r.txt"]), "refdata");
    assert_eq!(ok(d.path(), &["cat", "/r.txt"]), "changed");
}

#[test]
fn branch_create_and_list() {
    let d = workspace();
    ok(d.path(), &["write", "/x", "1"]);
    ok(d.path(), &["commit", "-m", "c"]);
    ok(d.path(), &["branch", "feature"]);
    let branches = ok(d.path(), &["branch"]);
    assert!(
        branches.contains("feature"),
        "new branch listed: {branches}"
    );
    assert!(branches.contains("main"), "main listed: {branches}");
    assert!(branches.contains('*'), "current branch marked: {branches}");
}

#[test]
fn import_and_export_host_files() {
    let d = workspace();
    let host_in = d.path().join("host_in.txt");
    std::fs::write(&host_in, b"from host").unwrap();
    ok(
        d.path(),
        &["import", host_in.to_str().unwrap(), "/imported.txt"],
    );
    assert_eq!(ok(d.path(), &["cat", "/imported.txt"]), "from host");

    let host_out = d.path().join("host_out.txt");
    ok(
        d.path(),
        &["export", "/imported.txt", host_out.to_str().unwrap()],
    );
    assert_eq!(std::fs::read(&host_out).unwrap(), b"from host");
}

#[test]
fn mkdir_and_del() {
    let d = workspace();
    ok(d.path(), &["mkdir", "/newdir"]);
    assert!(ok(d.path(), &["ls", "/"]).contains("newdir"));
    ok(d.path(), &["del", "/newdir"]);
    assert!(!ok(d.path(), &["ls", "/"]).contains("newdir"));
}

#[test]
fn diff_reports_uncommitted_changes() {
    let d = workspace();
    ok(d.path(), &["write", "/d.txt", "v1"]);
    ok(d.path(), &["commit", "-m", "c"]);
    ok(d.path(), &["write", "/d.txt", "v2"]);
    // diff against HEAD should mention the changed path.
    let o = hako(d.path(), &["diff"]);
    assert!(o.status.success(), "diff failed: {}", err(&o));
    assert!(
        out(&o).contains("d.txt"),
        "diff should mention the changed file: {}",
        out(&o)
    );
}

#[test]
fn unknown_path_is_a_clean_error_not_a_panic() {
    let d = workspace();
    let o = hako(d.path(), &["cat", "/does/not/exist"]);
    assert!(!o.status.success(), "cat of a missing path should fail");
    // A clean error, not a Rust panic.
    assert!(
        !err(&o).contains("panicked"),
        "should not panic on a missing path: {}",
        err(&o)
    );
}

#[test]
fn runs_outside_a_workspace_fail_cleanly() {
    // No `hako init` here — commands should error, not panic.
    let d = tempfile::tempdir().unwrap();
    let o = hako(d.path(), &["ls", "/"]);
    assert!(!o.status.success(), "ls with no workspace should fail");
    assert!(
        !err(&o).contains("panicked"),
        "no-workspace error should be clean: {}",
        err(&o)
    );
}
