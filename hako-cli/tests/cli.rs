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

// ---------------------------------------------------------------------------
// Container meta surface + the `root/` boundary (Option B layout): the rootfs
// lives at /containers/<name>/root/..., and meta nodes (status, …) sit beside
// it at /containers/<name>/. The seeded default container is named `hako`.
// ---------------------------------------------------------------------------

#[test]
fn cat_container_dir_shows_status_readout() {
    // `cat /containers/<name>` (the container directory) reads a synthetic
    // status readout instead of erroring.
    let d = workspace();
    let readout = ok(d.path(), &["cat", "/containers/hako"]);
    assert!(
        readout.contains("container: hako"),
        "status readout should name the container: {readout}"
    );
    assert!(
        readout.contains("branch:") && readout.contains("working:"),
        "status readout should report branch and working state: {readout}"
    );
    // The status node is also addressable by its listed name.
    assert_eq!(
        ok(d.path(), &["cat", "/containers/hako/status"]),
        readout,
        "cat .../status should match cat of the container dir"
    );
}

#[test]
fn ls_container_dir_lists_root_and_meta() {
    // Listing the container directory surfaces the `root/` filesystem boundary
    // and the synthetic `status` meta entry — not the rootfs contents directly.
    let d = workspace();
    let listing = ok(d.path(), &["ls", "/containers/hako"]);
    assert!(
        listing.contains("root/"),
        "container dir ls should show the root/ filesystem boundary: {listing}"
    );
    assert!(
        listing.contains("status"),
        "container dir ls should surface the synthetic status entry: {listing}"
    );
    // The whole META_NODES registry is listed: the ctl control node and the
    // runtime-backed proc/ directory, alongside status.
    assert!(
        listing.contains("ctl"),
        "container dir ls should surface the ctl control node: {listing}"
    );
    assert!(
        listing.contains("proc/"),
        "container dir ls should surface the proc/ meta directory: {listing}"
    );
    assert!(
        !listing.contains("bin"),
        "container dir ls should NOT list rootfs entries directly: {listing}"
    );
}

#[test]
fn ls_container_root_shows_rootfs() {
    // The rootfs itself is listed under the `root/` boundary.
    let d = workspace();
    let listing = ok(d.path(), &["ls", "/containers/hako/root"]);
    assert!(
        listing.contains("bin"),
        "ls of /containers/<name>/root should show the seeded rootfs: {listing}"
    );
}

#[test]
fn container_status_reflects_uncommitted_changes() {
    // Writing into a container without committing flips the readout to
    // "modified"; this exercises the working-tree-vs-HEAD comparison.
    let d = workspace();
    ok(d.path(), &["write", "/etc/motd", "hi"]);
    let readout = ok(d.path(), &["cat", "/containers/hako"]);
    assert!(
        readout.contains("working:   modified"),
        "status should report a dirty working tree after an uncommitted write: {readout}"
    );
}

#[test]
fn cat_container_fs_requires_root_boundary() {
    // Under Option B, reading the filesystem cross-container goes through
    // `root/`; the pre-migration form is rejected, and the file is readable via
    // the `root/` path.
    let d = workspace();
    ok(d.path(), &["write", "/greeting.txt", "hello"]);
    assert_eq!(
        ok(d.path(), &["cat", "/containers/hako/root/greeting.txt"]),
        "hello"
    );
    let o = hako(d.path(), &["cat", "/containers/hako/greeting.txt"]);
    assert!(
        !o.status.success(),
        "addressing the fs without root/ should fail under Option B"
    );
    assert!(
        !err(&o).contains("panicked"),
        "rejection should be a clean error: {}",
        err(&o)
    );
}

#[test]
fn ctl_node_is_listed_and_readable() {
    // The control node appears in the container directory listing and reads
    // back its usage.
    let d = workspace();
    let listing = ok(d.path(), &["ls", "/containers/hako"]);
    assert!(
        listing.contains("ctl"),
        "container dir ls should surface the ctl node: {listing}"
    );
    let usage = ok(d.path(), &["cat", "/containers/hako/ctl"]);
    assert!(
        usage.contains("commit"),
        "ctl usage should mention the commit verb: {usage}"
    );
}

#[test]
fn ctl_commit_snapshots_the_working_tree() {
    // Writing `commit <msg>` to the ctl node snapshots the container, exactly
    // like `hako commit` — the Plan 9 control-via-a-file model.
    let d = workspace();
    ok(d.path(), &["write", "/etc/motd", "hi"]);
    // Before: dirty.
    assert!(ok(d.path(), &["cat", "/containers/hako"]).contains("working:   modified"));
    // Control it by writing to ctl.
    ok(
        d.path(),
        &["write", "/containers/hako/ctl", "commit set motd via ctl"],
    );
    // After: clean, and the message shows up in the log.
    assert!(ok(d.path(), &["cat", "/containers/hako"]).contains("working:   clean"));
    assert!(ok(d.path(), &["log"]).contains("set motd via ctl"));
}

#[test]
fn ctl_rejects_unknown_verb_cleanly() {
    let d = workspace();
    let o = hako(
        d.path(),
        &["write", "/containers/hako/ctl", "frobnicate now"],
    );
    assert!(!o.status.success(), "unknown ctl verb should fail");
    assert!(
        err(&o).contains("unsupported") && !err(&o).contains("panicked"),
        "unknown verb should be a clean, descriptive error: {}",
        err(&o)
    );
}

#[test]
fn writing_a_readonly_meta_node_is_rejected() {
    // `status` is read-only; only `ctl` accepts writes among the meta nodes.
    let d = workspace();
    let o = hako(d.path(), &["write", "/containers/hako/status", "x"]);
    assert!(!o.status.success(), "writing status should fail");
    assert!(
        !err(&o).contains("panicked"),
        "rejection should be clean: {}",
        err(&o)
    );
}

#[test]
fn ctl_branch_and_tag_create_refs() {
    // The control plane also covers the cross-platform VC verbs: `branch` and
    // `tag` each create a ref at HEAD, driven entirely by writing to ctl.
    let d = workspace();
    ok(d.path(), &["write", "/a.txt", "one"]);
    ok(d.path(), &["write", "/containers/hako/ctl", "commit first"]);

    ok(
        d.path(),
        &["write", "/containers/hako/ctl", "branch feature-x"],
    );
    assert!(
        ok(d.path(), &["branch"]).contains("feature-x"),
        "ctl `branch` should create a listed branch"
    );

    ok(d.path(), &["write", "/containers/hako/ctl", "tag v1"]);
    // The tag resolves as a ref, so `<tag>:<path>` reads its tree.
    assert_eq!(ok(d.path(), &["cat", "v1:/a.txt"]).trim(), "one");
}

#[test]
fn ctl_verb_missing_argument_errors_cleanly() {
    // `branch`/`tag` require a name; omitting it is a clean, descriptive error
    // (the arg check fires before any repo work).
    let d = workspace();
    let o = hako(d.path(), &["write", "/containers/hako/ctl", "branch"]);
    assert!(!o.status.success(), "branch with no name should fail");
    assert!(
        err(&o).contains("needs an argument") && !err(&o).contains("panicked"),
        "missing-arg error should be clean and descriptive: {}",
        err(&o)
    );
}

// The proc/ reader runs against the host kernel's /proc, so it needs a real PID
// namespace — the cross-platform suite can't reach it. This Linux-only test
// codifies the security boundary: a container's process *tree* is exposed, while
// a host process (a different PID namespace) is rejected. Skips cleanly where
// unprivileged user namespaces aren't available (e.g. a hardened CI runner).
#[cfg(target_os = "linux")]
#[test]
fn proc_meta_exposes_the_container_tree_and_rejects_host_processes() {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    // Kill (and reap) spawned helpers even if an assertion panics mid-test.
    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn spawn_quiet(cmd: &mut Command) -> std::io::Result<Child> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }

    let d = workspace();

    // A control process in the HOST namespace — must never be exposed.
    let host = KillOnDrop(spawn_quiet(Command::new("sleep").arg("60")).expect("spawn host sleep"));
    let host_pid = host.0.id();

    // A real container PID namespace: an unshared PID-1 (bash) with two children.
    let unshared = match spawn_quiet(Command::new("unshare").args([
        "-Upf",
        "--mount-proc",
        "--kill-child",
        "bash",
        "-c",
        "sleep 60 & sleep 61 & wait",
    ])) {
        Ok(c) => KillOnDrop(c),
        Err(_) => {
            eprintln!("SKIP: `unshare` unavailable");
            return;
        }
    };

    // Poll for the namespaced PID-1 (unshare's forked child) rather than racing a
    // fixed sleep; skip cleanly if no namespace appears (userns disabled).
    let read_nspid = || -> Option<u32> {
        std::fs::read_to_string(format!("/proc/{u}/task/{u}/children", u = unshared.0.id()))
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let nspid = loop {
        if let Some(p) = read_nspid() {
            break p;
        }
        if Instant::now() >= deadline {
            eprintln!("SKIP: could not create a PID namespace (unprivileged userns disabled?)");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // Fabricate a running instance of the default `hako` container whose PID-1
    // is the unshared process.
    let rd = d.path().join(".hako/runtime/test-proc");
    std::fs::create_dir_all(&rd).unwrap();
    std::fs::write(
        rd.join("config.json"),
        r#"{"container":"hako","branch":"main","command":["bash"],"started_unix":1}"#,
    )
    .unwrap();
    std::fs::write(rd.join("pid"), nspid.to_string()).unwrap();
    std::fs::write(rd.join("nspid"), nspid.to_string()).unwrap();

    // Poll `ls` until the children settle into the full tree (PID-1 + 2 sleeps)
    // rather than asserting against a fixed delay.
    let count = |s: &str| s.lines().filter(|l| !l.trim().is_empty()).count();
    let deadline = Instant::now() + Duration::from_secs(3);
    let listing = loop {
        let l = ok(d.path(), &["ls", "/containers/hako/proc"]);
        if count(&l) >= 3 || Instant::now() >= deadline {
            break l;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    // ls enumerates the container's *tree* (PID-1 + children), not the host.
    let listed: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        listed
            .iter()
            .any(|l| l.trim_end_matches('/') == nspid.to_string()),
        "the container PID-1 should be listed: {listing}"
    );
    assert!(
        listed.len() >= 2,
        "the process *tree* (PID-1 + children) should be listed, got: {listing}"
    );
    assert!(
        !listed
            .iter()
            .any(|l| l.trim_end_matches('/') == host_pid.to_string()),
        "a host-namespace process must never appear: {listing}"
    );

    // cat reads a container process.
    assert!(
        !ok(
            d.path(),
            &["cat", &format!("/containers/hako/proc/{nspid}/comm")]
        )
        .is_empty(),
        "comm of a container process should read"
    );

    // SECURITY: the host process must be rejected (different PID namespace).
    let o = hako(
        d.path(),
        &["cat", &format!("/containers/hako/proc/{host_pid}/comm")],
    );
    assert!(
        !o.status.success() && !err(&o).contains("panicked"),
        "host process must not be readable through proc/: {}",
        err(&o)
    );

    // mem is never exposed.
    let o = hako(
        d.path(),
        &["cat", &format!("/containers/hako/proc/{nspid}/mem")],
    );
    assert!(!o.status.success(), "proc/<pid>/mem must not be exposed");

    // proc/<pid>/ctl signals a process, scoped to the container. SECURITY: a
    // host pid (different PID namespace) is rejected — never signaled.
    let o = hako(
        d.path(),
        &[
            "write",
            &format!("/containers/hako/proc/{host_pid}/ctl"),
            "stop",
        ],
    );
    assert!(
        !o.status.success() && err(&o).contains("no live process"),
        "signalling a host process must be rejected: {}",
        err(&o)
    );
    // A container process is accepted and the signal delivered (SIGTERM here —
    // sent last, since it tears the unshared helper down). exit 0 ⇒ the pid was
    // verified in-container and kill(2) succeeded.
    let o = hako(
        d.path(),
        &[
            "write",
            &format!("/containers/hako/proc/{nspid}/ctl"),
            "stop",
        ],
    );
    assert!(
        o.status.success(),
        "signalling a container process should succeed: {}",
        err(&o)
    );

    // `host` and `unshared` are killed + reaped on drop (including on panic).
}

// `ctl "run"` dispatches a detached workload — a runtime op. Off Linux the
// runtime can't spawn, so it surfaces the platform error immediately (no real
// container is started), which lets us assert *cross-platform* that the verb is
// wired: a recognized verb, not the unknown-command parse error. The actual
// spawn is exercised on Linux by the runtime path.
#[cfg(not(target_os = "linux"))]
#[test]
fn ctl_run_is_a_recognized_verb() {
    let d = workspace();
    let o = hako(d.path(), &["write", "/containers/hako/ctl", "run echo hi"]);
    assert!(
        !o.status.success(),
        "run can't succeed without the Linux runtime"
    );
    assert!(
        !err(&o).contains("unsupported command") && !err(&o).contains("panicked"),
        "`run` should be a recognized ctl verb (got the runtime's platform error, \
         not the unknown-verb error): {}",
        err(&o)
    );
}

// Writing to `proc/<pid>/ctl` signals a process — a runtime op that must *route*
// to the proc surface, not fall through to the generic "not writable" meta path.
// Off Linux the proc surface reports the runtime-needed error; what we assert
// cross-platform is that the write reached it (not the not-writable rejection).
#[cfg(not(target_os = "linux"))]
#[test]
fn proc_ctl_write_routes_to_the_proc_surface() {
    let d = workspace();
    let o = hako(
        d.path(),
        &["write", "/containers/hako/proc/123/ctl", "stop"],
    );
    assert!(
        !o.status.success(),
        "can't signal without the Linux runtime"
    );
    assert!(
        !err(&o).contains("is not writable") && !err(&o).contains("panicked"),
        "proc/<pid>/ctl should route to the proc surface, not the not-writable path: {}",
        err(&o)
    );
}

// Machine-readable output (`--json`): the scripting surface added for #21. Each
// command's JSON must parse and mirror the state the human output describes.
#[test]
fn json_output_is_valid_and_reflects_state() {
    let d = workspace();

    // containers --json → array including the seeded default container.
    let containers: serde_json::Value =
        serde_json::from_str(&ok(d.path(), &["containers", "--json"])).unwrap();
    assert!(
        containers.as_array().unwrap().iter().any(|c| c == "hako"),
        "containers --json lists the default container: {containers}"
    );

    // A file write → status --json reports not-clean with one "added" change.
    ok(d.path(), &["write", "/notes.txt", "hi"]);
    let st: serde_json::Value = serde_json::from_str(&ok(d.path(), &["status", "--json"])).unwrap();
    assert_eq!(st["branch"], serde_json::json!("main"));
    assert_eq!(st["clean"], serde_json::json!(false));
    let changes = st["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "one change: {st}");
    assert_eq!(changes[0]["path"], serde_json::json!("notes.txt"));
    assert_eq!(changes[0]["change"], serde_json::json!("added"));

    // Commit → log --json is a non-empty array; the newest entry carries the
    // message and a full 64-hex commit hash. And status --json is clean again.
    ok(d.path(), &["commit", "-m", "add notes"]);
    let log: serde_json::Value = serde_json::from_str(&ok(d.path(), &["log", "--json"])).unwrap();
    let commits = log.as_array().unwrap();
    assert!(!commits.is_empty(), "log --json has commits: {log}");
    assert_eq!(commits[0]["message"], serde_json::json!("add notes"));
    assert_eq!(commits[0]["hash"].as_str().unwrap().len(), 64);
    assert!(commits[0]["parents"].is_array());
    assert!(commits[0]["timestamp"].is_number());

    let st: serde_json::Value = serde_json::from_str(&ok(d.path(), &["status", "--json"])).unwrap();
    assert_eq!(st["clean"], serde_json::json!(true));
    assert_eq!(st["changes"].as_array().unwrap().len(), 0);

    // ps --json with no running instances → an empty JSON array.
    let ps: serde_json::Value = serde_json::from_str(&ok(d.path(), &["ps", "--json"])).unwrap();
    assert_eq!(ps.as_array().unwrap().len(), 0);
}
