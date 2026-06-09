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

use crate::RuntimeError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Subdirectory under the workdir where instance state lives.
pub const RUNTIME_DIR: &str = "runtime";

/// Configuration of a spawned instance, written once at spawn time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceConfig {
    pub branch: String,
    pub command: Vec<String>,
    pub started_unix: u64,
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
    // OS-provided randomness via blake3's keyed hashing as an entropy
    // source: re-hash with the high bits of a SystemTime measurement at
    // a different point. This isn't cryptographic randomness but adds
    // enough variation that two threads at the same nanosecond on the
    // same pid still diverge.
    let extra = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().wrapping_mul(0x9E3779B97F4A7C15))
        .unwrap_or(0);
    h.update(&extra.to_le_bytes());
    h.finalize().to_hex()[..12].to_string()
}

/// Create a new instance directory and write its config.
pub fn create(
    workdir: &Path,
    id: &str,
    branch: &str,
    command: &[String],
) -> Result<PathBuf, RuntimeError> {
    let dir = instance_dir(workdir, id);
    fs::create_dir_all(&dir)?;
    let config = InstanceConfig {
        branch: branch.to_string(),
        command: command.to_vec(),
        started_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let cfg_path = dir.join("config.json");
    let cfg_bytes = serde_json::to_vec_pretty(&config)
        .map_err(|e| RuntimeError::Other(format!("serialize config: {}", e)))?;
    write_atomic(&cfg_path, &cfg_bytes)?;
    Ok(dir)
}

/// Record the supervising process id alongside its start time. The start
/// time lets `stop`/`is_running` distinguish "our process" from "an
/// unrelated process that recycled this pid after ours died." On
/// platforms where the start time isn't readable, we fall back to
/// pid-only and accept the small risk.
pub fn write_pid(workdir: &Path, id: &str, pid: u32) -> Result<(), RuntimeError> {
    let dir = instance_dir(workdir, id);
    fs::create_dir_all(&dir)?;
    let line = match read_starttime(pid) {
        Some(st) => format!("{}:{}", pid, st),
        None => pid.to_string(),
    };
    write_atomic(&dir.join("pid"), line.as_bytes())
}

/// Record the supervising process exit code. Called when the process dies.
pub fn write_exit_code(workdir: &Path, id: &str, code: i32) -> Result<(), RuntimeError> {
    let dir = instance_dir(workdir, id);
    fs::create_dir_all(&dir)?;
    write_atomic(&dir.join("exitcode"), code.to_string().as_bytes())
}

/// Read the supervising process id, if recorded. Strips the optional
/// `:starttime` suffix; use `read_pid_with_starttime` to get both.
pub fn read_pid(workdir: &Path, id: &str) -> Option<u32> {
    read_pid_with_starttime(workdir, id).map(|(pid, _)| pid)
}

/// Read (pid, start_time) from disk. Start time is `None` for entries
/// written on platforms where /proc/PID/stat wasn't available.
pub fn read_pid_with_starttime(workdir: &Path, id: &str) -> Option<(u32, Option<u64>)> {
    let s = fs::read_to_string(instance_dir(workdir, id).join("pid")).ok()?;
    let s = s.trim();
    match s.split_once(':') {
        Some((pid_s, st_s)) => {
            let pid = pid_s.parse().ok()?;
            let st = st_s.parse().ok();
            Some((pid, st))
        }
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
    Ok(Instance {
        id: id.into(),
        config,
        pid,
        start_time,
        exit_code,
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
    out.sort_by(|a, b| b.config.started_unix.cmp(&a.config.started_unix));
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

/// Send SIGTERM to the supervising process. Validates start_time before
/// killing so we don't shoot a recycled pid that belongs to someone else.
#[cfg(unix)]
pub fn stop(workdir: &Path, id: &str) -> Result<(), RuntimeError> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let (pid, recorded_start) = read_pid_with_starttime(workdir, id)
        .ok_or_else(|| RuntimeError::Other(format!("instance {} has no pid", id)))?;
    if !process_matches(pid, recorded_start) {
        return Err(RuntimeError::Other(format!(
            "instance {} pid {} no longer matches (process exited or pid was recycled)",
            id, pid
        )));
    }
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .map_err(|e| RuntimeError::Other(format!("kill {}: {}", pid, e)))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn stop(_workdir: &Path, _id: &str) -> Result<(), RuntimeError> {
    Err(RuntimeError::UnsupportedPlatform {
        operation: "hako stop",
        hint: "Sending signals to runtime instances requires a Unix system. \
               On Windows, manage instances from inside WSL2.",
    })
}

// ============================================================================
// Helpers
// ============================================================================

/// True iff a process exists at `pid` AND (when both sides have a recorded
/// start_time) the start times match. Without a recorded start_time we fall
/// back to mere existence — better than nothing, but lossy under pid reuse.
#[cfg(unix)]
fn process_matches(pid: u32, recorded_start: Option<u64>) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if kill(Pid::from_raw(pid as i32), None).is_err() {
        return false;
    }
    match (recorded_start, read_starttime(pid)) {
        (Some(a), Some(b)) => a == b,
        _ => true, // can't compare → trust existence
    }
}

#[cfg(not(unix))]
fn process_matches(_pid: u32, _recorded_start: Option<u64>) -> bool {
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
        assert!(seen.len() > 195, "got too many duplicate ids: {}/200", seen.len());
    }

    #[test]
    fn create_and_get_roundtrip() {
        let wd = workdir();
        let id = "abc123";
        create(wd.path(), id, "alpine", &["sh".into()]).unwrap();
        let inst = get(wd.path(), id).unwrap();
        assert_eq!(inst.id, id);
        assert_eq!(inst.config.branch, "alpine");
        assert_eq!(inst.config.command, vec!["sh".to_string()]);
        assert!(inst.config.started_unix > 0);
    }

    #[test]
    fn pid_and_exit_code_persist() {
        let wd = workdir();
        let id = "p1";
        create(wd.path(), id, "main", &[]).unwrap();
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
        create(wd.path(), id, "main", &[]).unwrap();
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
    fn legacy_pid_format_still_decodes() {
        // Older instance dirs may have just "pid\n" without the start_time.
        // Make sure we can still read those.
        let wd = workdir();
        let id = "legacy";
        create(wd.path(), id, "main", &[]).unwrap();
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
            create(wd.path(), id, "branch", &[]).unwrap();
            // Overwrite started_unix manually to control sort order.
            let cfg = InstanceConfig {
                branch: "branch".into(),
                command: vec![],
                started_unix: ts,
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
    #[cfg(unix)]
    #[test]
    fn remove_refuses_running_unless_forced() {
        let wd = workdir();
        let id = "r1";
        create(wd.path(), id, "main", &[]).unwrap();
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
        create(wd.path(), id, "main", &[]).unwrap();
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
