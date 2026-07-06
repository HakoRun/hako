//! Detached-runtime-instance state management.
//!
//! When `hako spawn <branch>` starts a detached runtime instance, state is
//! stored on disk in `<workdir>/.hako/runtime/<id>/`:
//!
//! ```text
//! <workdir>/.hako/runtime/<id>/
//!   ├── config.json   { branch, command, started_unix }
//!   ├── pid           process id of the supervising process
//!   ├── stdout        captured stdout
//!   ├── stderr        captured stderr
//!   └── exitcode      exit code (written when the process terminates)
//! ```
//!
//! This module is cross-platform — it's all file I/O and JSON. The actual
//! `spawn` call lives in [`crate::transform`] and is Linux-only, but
//! listing/inspecting/removing instances works on every platform that can
//! read the state directory.
//!
//! "Instance" rather than "container" is deliberate: `hako-core` already
//! uses `containers/` for workspaces, so we call runtime processes
//! "instances" to avoid collision.

use crate::{RestartPolicy, RuntimeError, VolumeMount};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Subdirectory under the workdir where instance state lives.
pub const RUNTIME_DIR: &str = "runtime";

/// Configuration of a spawned instance, written once at spawn time.
///
/// Every field added after the original `{container, branch, command,
/// started_unix}` set is `#[serde(default)]`, so an instance directory written
/// by an older hako still decodes (the new fields take their defaults). This
/// keeps a single on-disk schema across the P0-2 supervision work — a running
/// box's existing instances survive an in-place binary upgrade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceConfig {
    /// The workspace container this instance belongs to. Distinct from `branch`
    /// (the ref that was run): an instance of container `app` running branch
    /// `main` has `container = "app"`, `branch = "main"`. The `proc/` meta
    /// surface groups instances by this. `#[serde(default)]` keeps pre-field
    /// instance dirs readable (they decode to an empty container).
    #[serde(default)]
    pub container: String,
    pub branch: String,
    pub command: Vec<String>,
    pub started_unix: u64,

    /// The tree root (hex) resolved from `branch` at spawn time. A supervised
    /// restart re-launches THIS root, never a re-resolution of `branch`, so a
    /// `revert` after spawn can't silently boot a different tree; it also lets
    /// `ps` show "branch at X, running Y". `None` for pre-field instance dirs.
    #[serde(default)]
    pub pinned_root: Option<String>,
    /// Restart policy for the supervising process (default `No` = run once).
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Reserved for the boot-reconcile follow-up: if set, `hako serve` re-launches
    /// this instance (at `pinned_root`) on startup. Persisted now so that lands
    /// without another schema change; no flag sets it yet.
    #[serde(default)]
    pub start_on_boot: bool,
    /// Network mode token (`none`/`host`) — recorded so a reconcile rebuilds it.
    #[serde(default)]
    pub network: String,
    /// Published ports (`host:container`) — reserved for P0-1 slice 2.
    #[serde(default)]
    pub ports: Vec<String>,
    /// The resolved volume set, so a boot reconcile can re-launch the exact run
    /// shape (including the implicit `/workspace` mount and its masks). A live
    /// crash-restart doesn't read this — the supervisor keeps the volumes in
    /// memory — but the reconcile in a fresh process does.
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
}

/// The full spawn shape recorded for a detached instance. Bundles the fields
/// that vary per spawn so `create_unique`/`create` stay single-argument-list
/// callable and the config is written atomically in one shot.
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    pub container: String,
    pub branch: String,
    pub command: Vec<String>,
    pub pinned_root: Option<String>,
    pub restart: RestartPolicy,
    pub start_on_boot: bool,
    pub network: String,
    pub ports: Vec<String>,
    pub volumes: Vec<VolumeMount>,
}

impl SpawnSpec {
    /// A minimal spec (no supervision, no pinned root) — the shape the original
    /// `create(container, branch, command)` callers and tests want.
    pub fn minimal(container: &str, branch: &str, command: &[String]) -> Self {
        SpawnSpec {
            container: container.to_string(),
            branch: branch.to_string(),
            command: command.to_vec(),
            ..Default::default()
        }
    }
}

/// Snapshot of an instance's state at the moment of inspection.
#[derive(Debug, Clone)]
pub struct Instance {
    pub id: String,
    pub config: InstanceConfig,
    pub pid: Option<u32>,
    /// Process start time recorded at spawn (Linux clock ticks since boot).
    /// Used to detect pid recycling — a process at our recorded pid that
    /// has a different start time is not ours.
    pub start_time: Option<u64>,
    pub exit_code: Option<i32>,
    /// How many times the supervisor has respawned the workload (0 unless the
    /// instance runs under a restart policy).
    pub restart_count: u64,
}

impl Instance {
    /// Whether the supervising process is still running. On Linux, validates
    /// that the process at our pid has the expected start_time so we don't
    /// confuse ourselves with a recycled pid.
    pub fn is_running(&self) -> bool {
        match self.pid {
            None => false,
            Some(pid) => process_matches(pid, self.start_time),
        }
    }

    /// Short status string for `ps`-style display.
    pub fn status(&self) -> String {
        if self.is_running() {
            format!("running (pid {})", self.pid.unwrap_or(0))
        } else if let Some(code) = self.exit_code {
            format!("exited ({})", code)
        } else if self.pid.is_some() {
            "dead".into()
        } else {
            "spawning".into()
        }
    }
}

// ============================================================================
// Path helpers
// ============================================================================

/// Path to the runtime directory inside a hako workdir.
pub fn runtime_dir(workdir: &Path) -> PathBuf {
    workdir.join(RUNTIME_DIR)
}

/// Path to a specific instance's directory.
pub fn instance_dir(workdir: &Path, id: &str) -> PathBuf {
    runtime_dir(workdir).join(id)
}

/// Paths to the stdout/stderr log files for an instance.
pub fn log_paths(workdir: &Path, id: &str) -> (PathBuf, PathBuf) {
    let dir = instance_dir(workdir, id);
    (dir.join("stdout"), dir.join("stderr"))
}

// ============================================================================
// Lifecycle
// ============================================================================

/// Generate a 12-char hex id. Hashes (nanos | pid | random) so concurrent
/// spawns at the same instant on the same host don't collide. 48 bits of
/// id space is plenty for human-scale instance counts; the inputs only need
/// to disambiguate near-simultaneous calls, not guarantee uniqueness on
/// their own.
pub fn generate_id() -> String {
    let mut h = blake3::Hasher::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(&nanos.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    // Real OS entropy so two spawns in the same nanosecond on the same host still
    // diverge — not just a second clock read. Best-effort with a time+ASLR
    // fallback; a residual clash is still caught by `create` (#78).
    h.update(&os_random_bytes());
    h.finalize().to_hex()[..12].to_string()
}

/// 16 bytes of OS randomness, best-effort. Reads `/dev/urandom` (present on the
/// Linux runtime host where instances are actually created); if that fails, mixes
/// two clock reads with a stack address (ASLR) so the result still varies.
fn os_random_bytes() -> [u8; 16] {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let addr = &buf as *const _ as usize as u128;
    let mix = nanos ^ addr.wrapping_mul(0x9E3779B97F4A7C15);
    buf.copy_from_slice(&mix.to_le_bytes());
    buf
}

/// Create an instance under a freshly generated, collision-checked id, returning
/// the id. Retries generation if a directory already exists, so a generated-id
/// clash can never silently merge into (and corrupt) another instance's state.
pub fn create_unique(
    workdir: &Path,
    container: &str,
    branch: &str,
    command: &[String],
) -> Result<String, RuntimeError> {
    create_unique_full(workdir, &SpawnSpec::minimal(container, branch, command))
}

/// [`create_unique`] with the full spawn shape (pinned root, restart policy,
/// network/volumes) recorded in the config. Used by the detached runtime spawn.
pub fn create_unique_full(workdir: &Path, spec: &SpawnSpec) -> Result<String, RuntimeError> {
    for _ in 0..16 {
        let id = generate_id();
        match create_full(workdir, &id, spec) {
            Ok(_) => return Ok(id),
            Err(RuntimeError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(RuntimeError::Other(
        "could not allocate a unique instance id after 16 attempts".into(),
    ))
}

/// Create a new instance directory and write its config. Fails with
/// `AlreadyExists` if the id dir already exists (see [`create_unique`]).
pub fn create(
    workdir: &Path,
    id: &str,
    container: &str,
    branch: &str,
    command: &[String],
) -> Result<PathBuf, RuntimeError> {
    create_full(workdir, id, &SpawnSpec::minimal(container, branch, command))
}

/// [`create`] with the full spawn shape. Writes the complete `InstanceConfig`
/// atomically in one shot (no partial-config window).
pub fn create_full(workdir: &Path, id: &str, spec: &SpawnSpec) -> Result<PathBuf, RuntimeError> {
    let dir = instance_dir(workdir, id);
    // create_dir (not create_dir_all on `dir`) so an id collision is a hard error
    // rather than a silent merge that overwrites another instance's config/pid
    // (#78). The runtime dir is the parent and may need creating first.
    fs::create_dir_all(runtime_dir(workdir))?;
    fs::create_dir(&dir)?;
    let config = InstanceConfig {
        container: spec.container.clone(),
        branch: spec.branch.clone(),
        command: spec.command.clone(),
        started_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        pinned_root: spec.pinned_root.clone(),
        restart: spec.restart,
        start_on_boot: spec.start_on_boot,
        network: spec.network.clone(),
        ports: spec.ports.clone(),
        volumes: spec.volumes.clone(),
    };
    let cfg_path = dir.join("config.json");
    let cfg_bytes = serde_json::to_vec_pretty(&config)
        .map_err(|e| RuntimeError::Other(format!("serialize config: {}", e)))?;
    write_atomic(&cfg_path, &cfg_bytes)?;
    Ok(dir)
}

/// Record the running restart count for a supervised instance (written by the
/// supervisor after each respawn). Surfaced by `ps`. Best-effort — a missing or
/// unparseable file reads as zero.
pub fn write_restart_count(workdir: &Path, id: &str, n: u64) -> Result<(), RuntimeError> {
    let dir = instance_dir(workdir, id);
    require_instance_dir(&dir)?;
    write_atomic(&dir.join("restarts"), n.to_string().as_bytes())
}

/// Read the recorded restart count (0 if never written).
pub fn read_restart_count(workdir: &Path, id: &str) -> u64 {
    fs::read_to_string(instance_dir(workdir, id).join("restarts"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Record the supervising process id alongside its start time. The start
/// time lets `stop`/`is_running` distinguish "our process" from "an
/// unrelated process that recycled this pid after ours died." On
/// platforms where the start time isn't readable, we fall back to
/// pid-only and accept the small risk.
pub fn write_pid(workdir: &Path, id: &str, pid: u32) -> Result<(), RuntimeError> {
    write_pidfile(&instance_dir(workdir, id), "pid", pid)
}

/// Record the container's PID-1 (the process that owns the container's PID,
/// network, IPC, and UTS namespaces) host-visible pid. This is the target for
/// `hako exec` (setns into its namespaces) and `hako stop` (signal it so its
/// init forwards to the workload). Distinct from the supervisor `pid`, which
/// owns the FUSE mount and is used for liveness.
pub fn write_nspid(workdir: &Path, id: &str, pid: u32) -> Result<(), RuntimeError> {
    write_pidfile(&instance_dir(workdir, id), "nspid", pid)
}

fn write_pidfile(dir: &Path, name: &str, pid: u32) -> Result<(), RuntimeError> {
    require_instance_dir(dir)?;
    let line = match read_starttime(pid) {
        Some(st) => format!("{}:{}", pid, st),
        None => pid.to_string(),
    };
    write_atomic(&dir.join(name), line.as_bytes())
}

/// Record the supervising process exit code. Called when the process dies.
pub fn write_exit_code(workdir: &Path, id: &str, code: i32) -> Result<(), RuntimeError> {
    let dir = instance_dir(workdir, id);
    require_instance_dir(&dir)?;
    write_atomic(&dir.join("exitcode"), code.to_string().as_bytes())
}

/// Guard the post-`create` write helpers: refuse to write into (and thereby
/// recreate) an instance dir that no longer exists. Otherwise a `reap --force`
/// that removes a still-running instance's dir would have its supervisor spring a
/// config-less dir back to life, which `ps`/`stop` then misread (#78).
fn require_instance_dir(dir: &Path) -> Result<(), RuntimeError> {
    if dir.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::Other(format!(
            "instance dir {} no longer exists; refusing to recreate it",
            dir.display()
        )))
    }
}

/// Read the supervising process id, if recorded. Strips the optional
/// `:starttime` suffix; use `read_pid_with_starttime` to get both.
pub fn read_pid(workdir: &Path, id: &str) -> Option<u32> {
    read_pid_with_starttime(workdir, id).map(|(pid, _)| pid)
}

/// Read (pid, start_time) from disk. Start time is `None` for entries
/// written on platforms where /proc/PID/stat wasn't available.
pub fn read_pid_with_starttime(workdir: &Path, id: &str) -> Option<(u32, Option<u64>)> {
    read_pidfile(workdir, id, "pid")
}

/// Read the container PID-1 host pid (see `write_nspid`), if recorded.
pub fn read_nspid_with_starttime(workdir: &Path, id: &str) -> Option<(u32, Option<u64>)> {
    read_pidfile(workdir, id, "nspid")
}

fn read_pidfile(workdir: &Path, id: &str, name: &str) -> Option<(u32, Option<u64>)> {
    let s = fs::read_to_string(instance_dir(workdir, id).join(name)).ok()?;
    let s = s.trim();
    match s.split_once(':') {
        Some((pid_s, st_s)) => Some((pid_s.parse().ok()?, st_s.parse().ok())),
        None => Some((s.parse().ok()?, None)),
    }
}

/// Read the recorded exit code, if the process has terminated.
pub fn read_exit_code(workdir: &Path, id: &str) -> Option<i32> {
    fs::read_to_string(instance_dir(workdir, id).join("exitcode"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Read the config of an instance.
pub fn read_config(workdir: &Path, id: &str) -> Result<InstanceConfig, RuntimeError> {
    let path = instance_dir(workdir, id).join("config.json");
    let bytes = fs::read(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RuntimeError::InstanceNotFound(id.into()),
        _ => RuntimeError::Io(e),
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| RuntimeError::Other(format!("parse config for {}: {}", id, e)))
}

/// Get a full snapshot of an instance.
pub fn get(workdir: &Path, id: &str) -> Result<Instance, RuntimeError> {
    let config = read_config(workdir, id)?;
    let (pid, start_time) = match read_pid_with_starttime(workdir, id) {
        Some((p, st)) => (Some(p), st),
        None => (None, None),
    };
    let exit_code = read_exit_code(workdir, id);
    let restart_count = read_restart_count(workdir, id);
    Ok(Instance {
        id: id.into(),
        config,
        pid,
        start_time,
        exit_code,
        restart_count,
    })
}

/// List all instances under the workdir.
pub fn list(workdir: &Path) -> Result<Vec<Instance>, RuntimeError> {
    let dir = runtime_dir(workdir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue, // non-utf8 ids — skip
        };
        match get(workdir, &id) {
            Ok(inst) => out.push(inst),
            Err(RuntimeError::InstanceNotFound(_)) => {} // partially-initialized
            Err(e) => return Err(e),
        }
    }
    // Sort by start time (newest first, like `docker ps`).
    out.sort_by_key(|b| std::cmp::Reverse(b.config.started_unix));
    Ok(out)
}

/// Remove an instance's state directory. Refuses if the process is still
/// running unless `force` is true.
pub fn remove(workdir: &Path, id: &str, force: bool) -> Result<(), RuntimeError> {
    let inst = get(workdir, id)?;
    if !force && inst.is_running() {
        return Err(RuntimeError::Other(format!(
            "instance {} is still running; stop it first or pass --force",
            id
        )));
    }
    fs::remove_dir_all(instance_dir(workdir, id))?;
    Ok(())
}

/// Send SIGTERM to the container's PID-1. Its init (`reap_as_init`) installs a
/// handler that forwards the signal to the workload, so the container shuts down
/// gracefully and the supervisor then tears down the FUSE mount. Falls back to
/// the supervisor pid for instances with no recorded nspid (e.g. one that never
/// finished starting). Validates start_time first to avoid signalling a
/// recycled pid.
#[cfg(target_os = "linux")]
pub fn stop(workdir: &Path, id: &str, force: bool) -> Result<(), RuntimeError> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    // A supervised instance (restart policy != no) MUST be stopped via its
    // supervisor: signalling the container's PID-1 directly would just make the
    // supervisor respawn it. A missing/legacy config decodes to `restart = No`,
    // preserving the historical path.
    let supervised = read_config(workdir, id)
        .map(|c| c.restart.is_supervised())
        .unwrap_or(false);

    if supervised {
        let (spid, sstart) = read_pid_with_starttime(workdir, id)
            .ok_or_else(|| RuntimeError::Other(format!("instance {} has no supervisor pid", id)))?;
        if !process_matches(spid, sstart) {
            return Err(RuntimeError::Other(format!(
                "instance {} supervisor pid {} no longer matches (exited or recycled)",
                id, spid
            )));
        }
        if force {
            // SIGKILL the supervisor FIRST so it can't respawn, then SIGKILL the
            // container's PID namespace via its PID-1 — killing only the
            // supervisor would orphan a still-running workload.
            let _ = kill(Pid::from_raw(spid as i32), Signal::SIGKILL);
            if let Some((npid, nstart)) = read_nspid_with_starttime(workdir, id) {
                if process_matches(npid, nstart) {
                    let _ = kill(Pid::from_raw(npid as i32), Signal::SIGKILL);
                }
            }
        } else {
            // SIGTERM the supervisor: its handler records "do not respawn" and
            // forwards SIGTERM to the current PID-1 so the workload drains, then
            // the supervisor exits.
            kill(Pid::from_raw(spid as i32), Signal::SIGTERM)
                .map_err(|e| RuntimeError::Other(format!("kill supervisor {}: {}", spid, e)))?;
        }
        return Ok(());
    }

    // Unsupervised (restart = no) — unchanged: signal the container's PID-1
    // (nspid), whose init forwards to the workload; fall back to the supervisor.
    let (pid, recorded_start) = read_nspid_with_starttime(workdir, id)
        .or_else(|| read_pid_with_starttime(workdir, id))
        .ok_or_else(|| RuntimeError::Other(format!("instance {} has no pid", id)))?;
    if !process_matches(pid, recorded_start) {
        return Err(RuntimeError::Other(format!(
            "instance {} pid {} no longer matches (process exited or pid was recycled)",
            id, pid
        )));
    }
    // SIGTERM is graceful (the init forwards it to the workload). `--force`
    // sends SIGKILL straight to the container's PID 1, which the kernel
    // delivers to the whole PID namespace — the reliable last resort for a
    // workload that ignores SIGTERM (otherwise the instance + its FUSE mount
    // would leak). Mirrors `docker stop` vs `docker kill`.
    let sig = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    kill(Pid::from_raw(pid as i32), sig)
        .map_err(|e| RuntimeError::Other(format!("kill {}: {}", pid, e)))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn stop(_workdir: &Path, _id: &str, _force: bool) -> Result<(), RuntimeError> {
    Err(RuntimeError::UnsupportedPlatform {
        operation: "hako stop",
        hint: "Signalling runtime instances happens on the Linux runtime host. \
               On Windows/macOS, manage instances from inside the WSL2 distro / Lima VM.",
    })
}

// ============================================================================
// Helpers
// ============================================================================

/// True iff a process exists at `pid` AND, when a start_time was recorded, the
/// current one matches it. Without a *recorded* start_time we fall back to mere
/// existence — lossy under pid reuse, but the best a legacy record allows.
#[cfg(target_os = "linux")]
pub(crate) fn process_matches(pid: u32, recorded_start: Option<u64>) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if kill(Pid::from_raw(pid as i32), None).is_err() {
        return false;
    }
    starttime_matches(recorded_start, read_starttime(pid))
}

/// Decide whether a recorded start_time agrees with the freshly-read one for the
/// pid-reuse check. Split out from [`process_matches`] so the fail-closed policy
/// is unit-testable without racing a real exiting process.
///
/// A recorded start_time that can't be re-read now (`current == None`) fails
/// **closed**: `kill(pid, 0)` just said something exists at that pid, but its
/// `/proc/<pid>/stat` vanished — the process is exiting, possibly mid-recycle, so
/// we cannot confirm identity and must not trust bare existence (a recycler that
/// grabbed the pid would satisfy it). Only a fully-absent *recorded* start_time
/// (a legacy/non-Linux record) falls back to existence-only.
#[cfg(target_os = "linux")]
fn starttime_matches(recorded: Option<u64>, current: Option<u64>) -> bool {
    match (recorded, current) {
        (Some(a), Some(b)) => a == b,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_matches(_pid: u32, _recorded_start: Option<u64>) -> bool {
    // Non-Unix host can't check process state at all. Used only for the
    // read-only "ps from Mac/Windows" flow.
    false
}

/// Read field 22 (starttime, clock ticks since boot) from /proc/<pid>/stat.
/// Returns None on non-Linux or if the file isn't readable.
#[cfg(target_os = "linux")]
fn read_starttime(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // The `comm` field is wrapped in parens and may contain spaces; find
    // the LAST `)` and split the rest by space. starttime is the 20th
    // field after the comm (since fields 1-2 are pid + comm).
    let after_comm = s.rsplit_once(')')?.1.trim_start();
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn read_starttime(_pid: u32) -> Option<u64> {
    None
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), RuntimeError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// ============================================================================
// Tests (cross-platform — pure file/state ops)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workdir() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    // The pid-reuse identity check must fail CLOSED when a start_time was recorded
    // but can't be re-read (the process is exiting, possibly mid-recycle) — else a
    // recycler that grabbed the pid passes `exec`'s guard (#72 review). Only a
    // legacy record with no start_time at all may fall back to existence-only.
    #[cfg(target_os = "linux")]
    #[test]
    fn starttime_matches_fails_closed_when_recorded_but_unreadable() {
        assert!(starttime_matches(Some(500), Some(500))); // same incarnation
        assert!(!starttime_matches(Some(500), Some(501))); // recycled: different start
        assert!(!starttime_matches(Some(500), None)); // recorded, vanished → closed
        assert!(starttime_matches(None, Some(500))); // legacy record → existence
        assert!(starttime_matches(None, None)); // legacy record → existence
    }

    #[test]
    fn create_refuses_an_id_collision() {
        let wd = workdir();
        create(wd.path(), "dup", "c", "main", &[]).unwrap();
        // A second create at the same id must fail (not silently merge) so it
        // can't overwrite the first instance's config/pid (#78).
        let err = create(wd.path(), "dup", "c", "main", &[]).unwrap_err();
        assert!(
            matches!(&err, RuntimeError::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists),
            "expected AlreadyExists, got {err:?}"
        );
    }

    #[test]
    fn create_unique_yields_distinct_live_ids() {
        let wd = workdir();
        let a = create_unique(wd.path(), "c", "main", &[]).unwrap();
        let b = create_unique(wd.path(), "c", "main", &[]).unwrap();
        assert_ne!(a, b);
        assert!(instance_dir(wd.path(), &a).is_dir());
        assert!(instance_dir(wd.path(), &b).is_dir());
    }

    #[test]
    fn write_pid_refuses_a_missing_instance_dir() {
        let wd = workdir();
        // No `create` first: the instance dir doesn't exist, so the post-create
        // write helper must refuse rather than spring a config-less dir to life
        // (the reap-while-running hazard, #78).
        let err = write_pid(wd.path(), "ghost", 1234).unwrap_err();
        assert!(
            matches!(&err, RuntimeError::Other(m) if m.contains("no longer exists")),
            "expected a missing-dir refusal, got {err:?}"
        );
    }

    #[test]
    fn id_is_12_hex_chars() {
        let id = generate_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ids_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(generate_id());
        }
        // Allow a couple collisions in tight loops on coarse-clock systems,
        // but not many.
        assert!(
            seen.len() > 195,
            "got too many duplicate ids: {}/200",
            seen.len()
        );
    }

    #[test]
    fn create_and_get_roundtrip() {
        let wd = workdir();
        let id = "abc123";
        create(wd.path(), id, "app", "alpine", &["sh".into()]).unwrap();
        let inst = get(wd.path(), id).unwrap();
        assert_eq!(inst.id, id);
        assert_eq!(inst.config.container, "app");
        assert_eq!(inst.config.branch, "alpine");
        assert_eq!(inst.config.command, vec!["sh".to_string()]);
        assert!(inst.config.started_unix > 0);
    }

    #[test]
    fn pid_and_exit_code_persist() {
        let wd = workdir();
        let id = "p1";
        create(wd.path(), id, "c", "main", &[]).unwrap();
        write_pid(wd.path(), id, 12345).unwrap();
        write_exit_code(wd.path(), id, 42).unwrap();
        let inst = get(wd.path(), id).unwrap();
        assert_eq!(inst.pid, Some(12345));
        assert_eq!(inst.exit_code, Some(42));
        assert!(inst.status().contains("exited"));
    }

    #[test]
    fn pid_with_starttime_roundtrip() {
        // Use our own pid so read_starttime works on Linux. On non-Linux
        // platforms start_time is None — both the writer and reader agree.
        let wd = workdir();
        let id = "st1";
        create(wd.path(), id, "c", "main", &[]).unwrap();
        let pid = std::process::id();
        write_pid(wd.path(), id, pid).unwrap();
        let (read_pid, read_st) = read_pid_with_starttime(wd.path(), id).unwrap();
        assert_eq!(read_pid, pid);
        // Linux must record a start_time for our own pid; other platforms
        // legitimately return None.
        if cfg!(target_os = "linux") {
            assert!(read_st.is_some(), "expected start_time on Linux");
        }
    }

    #[test]
    fn legacy_config_without_supervision_fields_decodes() {
        // An instance dir written before P0-2 has only the original four keys.
        // It must still decode, with the new fields taking their defaults — the
        // single-schema, in-place-upgrade guarantee.
        let wd = workdir();
        let id = "old";
        let dir = instance_dir(wd.path(), id);
        fs::create_dir_all(&dir).unwrap();
        let legacy = br#"{"container":"app","branch":"main","command":["sh"],"started_unix":123}"#;
        fs::write(dir.join("config.json"), legacy).unwrap();
        let inst = get(wd.path(), id).unwrap();
        assert_eq!(inst.config.branch, "main");
        assert_eq!(inst.config.pinned_root, None);
        assert_eq!(inst.config.restart, RestartPolicy::No);
        assert!(!inst.config.start_on_boot);
        assert!(inst.config.volumes.is_empty());
        assert_eq!(inst.restart_count, 0);
    }

    #[test]
    fn full_spawn_spec_roundtrips() {
        let wd = workdir();
        let spec = SpawnSpec {
            container: "app".into(),
            branch: "main".into(),
            command: vec!["server".into()],
            pinned_root: Some("deadbeef".into()),
            restart: RestartPolicy::OnFailure,
            start_on_boot: true,
            network: "host".into(),
            ports: vec!["8080:80".into()],
            volumes: vec![VolumeMount::parse("/srv:/data").unwrap()],
        };
        let id = create_unique_full(wd.path(), &spec).unwrap();
        let cfg = read_config(wd.path(), &id).unwrap();
        assert_eq!(cfg.pinned_root.as_deref(), Some("deadbeef"));
        assert_eq!(cfg.restart, RestartPolicy::OnFailure);
        assert!(cfg.start_on_boot);
        assert_eq!(cfg.network, "host");
        assert_eq!(cfg.ports, vec!["8080:80".to_string()]);
        assert_eq!(cfg.volumes.len(), 1);
        assert_eq!(cfg.volumes[0].container, "/data");
    }

    #[test]
    fn restart_count_persists() {
        let wd = workdir();
        let id = "rc";
        create(wd.path(), id, "c", "main", &[]).unwrap();
        assert_eq!(read_restart_count(wd.path(), id), 0);
        write_restart_count(wd.path(), id, 3).unwrap();
        assert_eq!(read_restart_count(wd.path(), id), 3);
        assert_eq!(get(wd.path(), id).unwrap().restart_count, 3);
    }

    #[test]
    fn legacy_pid_format_still_decodes() {
        // Older instance dirs may have just "pid\n" without the start_time.
        // Make sure we can still read those.
        let wd = workdir();
        let id = "legacy";
        create(wd.path(), id, "c", "main", &[]).unwrap();
        write_atomic(&instance_dir(wd.path(), id).join("pid"), b"99999").unwrap();
        let (pid, st) = read_pid_with_starttime(wd.path(), id).unwrap();
        assert_eq!(pid, 99999);
        assert_eq!(st, None);
    }

    #[test]
    fn list_sorted_newest_first() {
        let wd = workdir();
        // Three instances with different timestamps via the on-disk config.
        for (id, ts) in [("a", 1000u64), ("b", 2000), ("c", 1500)] {
            create(wd.path(), id, "c", "branch", &[]).unwrap();
            // Overwrite started_unix manually to control sort order.
            let cfg = InstanceConfig {
                container: "c".into(),
                branch: "branch".into(),
                command: vec![],
                started_unix: ts,
                pinned_root: None,
                restart: RestartPolicy::No,
                start_on_boot: false,
                network: String::new(),
                ports: vec![],
                volumes: vec![],
            };
            let bytes = serde_json::to_vec(&cfg).unwrap();
            write_atomic(&instance_dir(wd.path(), id).join("config.json"), &bytes).unwrap();
        }
        let listed: Vec<String> = list(wd.path()).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(listed, vec!["b".to_string(), "c".into(), "a".into()]);
    }

    // process_alive is reliable on Unix; on Windows the fallback always
    // reports "not running" because we can't OpenProcess without the
    // `windows` crate. This test exercises the running/not-running gate
    // and is meaningful only where process_alive can detect liveness.
    #[cfg(target_os = "linux")]
    #[test]
    fn remove_refuses_running_unless_forced() {
        let wd = workdir();
        let id = "r1";
        create(wd.path(), id, "c", "main", &[]).unwrap();
        // Use our own pid — definitely alive.
        write_pid(wd.path(), id, std::process::id()).unwrap();

        let result = remove(wd.path(), id, false);
        assert!(result.is_err(), "expected refusal while running");

        // Force succeeds.
        remove(wd.path(), id, true).unwrap();
        assert!(!instance_dir(wd.path(), id).exists());
    }

    #[test]
    fn remove_succeeds_when_no_pid_recorded() {
        let wd = workdir();
        let id = "r2";
        create(wd.path(), id, "c", "main", &[]).unwrap();
        // No pid written → considered not running everywhere.
        remove(wd.path(), id, false).unwrap();
        assert!(!instance_dir(wd.path(), id).exists());
    }

    #[test]
    fn list_on_empty_workdir_is_empty() {
        let wd = workdir();
        let v = list(wd.path()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn list_skips_non_directory_entries() {
        let wd = workdir();
        let dir = runtime_dir(wd.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stray-file"), b"junk").unwrap();
        let v = list(wd.path()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn read_config_missing_returns_not_found() {
        let wd = workdir();
        match read_config(wd.path(), "ghost") {
            Err(RuntimeError::InstanceNotFound(id)) => assert_eq!(id, "ghost"),
            other => panic!("expected InstanceNotFound, got {:?}", other),
        }
    }
}
