//! Best-effort cgroup v2 resource limits for the container.
//!
//! This mirrors how rootless Podman/Docker behave: apply pids/memory limits when
//! a **delegated** cgroup v2 hierarchy is available, and skip silently otherwise.
//! Rootless cgroup control *requires* delegation — the kernel grants an
//! unprivileged process no cgroup powers unless it owns a delegated subtree
//! (systemd user session, or an explicit `HAKO_CGROUP_PARENT`). There is no way
//! for the container to self-provision this, so absence is normal, not an error.
//!
//! Every operation is best-effort: any failure means "no limit applied", never a
//! failed run.

use std::fs;
use std::path::{Path, PathBuf};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Resource limits to apply to the container cgroup.
pub struct Limits {
    /// Max number of processes (`pids.max`). `None` = unlimited.
    pub pids_max: Option<u64>,
    /// Max memory in bytes (`memory.max`). `None` = unlimited.
    pub memory_max: Option<u64>,
}

impl Limits {
    /// Limits from the environment, with a default pids cap (the main DoS — a
    /// fork bomb). Memory is opt-in: an over-tight `memory.max` OOM-kills
    /// legitimate workloads, so we don't impose one by default.
    ///
    /// - `HAKO_PIDS_MAX`: integer, or `0`/`max` for unlimited (default 1024).
    /// - `HAKO_MEMORY_MAX`: size like `512M`, `2G` (default: unset).
    pub fn from_env() -> Self {
        let pids_max = match std::env::var("HAKO_PIDS_MAX") {
            Ok(s) if s == "0" || s.eq_ignore_ascii_case("max") => None,
            // Fail closed: a typo'd value falls back to the default cap rather
            // than silently meaning "unlimited" (this is a security knob).
            Ok(s) => Some(s.parse().unwrap_or(1024)),
            Err(_) => Some(1024),
        };
        let memory_max = std::env::var("HAKO_MEMORY_MAX")
            .ok()
            .and_then(|s| parse_size(&s));
        Limits {
            pids_max,
            memory_max,
        }
    }

    fn is_empty(&self) -> bool {
        self.pids_max.is_none() && self.memory_max.is_none()
    }
}

/// A container cgroup created by [`apply`]. Removed on drop (which only succeeds
/// once the cgroup is empty — i.e. after the container's processes have exited).
pub struct CgroupGuard {
    path: PathBuf,
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

/// Place `container_pid` (a host pid) into a fresh cgroup with `limits` applied.
///
/// Returns `Some(guard)` if a delegated cgroup was usable and the process was
/// moved into it; `None` (no limits) otherwise. Never errors — a missing
/// delegation is the normal rootless case.
pub fn apply(container_pid: i32, limits: &Limits) -> Option<CgroupGuard> {
    if limits.is_empty() {
        return None;
    }
    let parent = find_delegated_parent()?;
    apply_under(&parent, container_pid, limits)
}

/// Core of [`apply`] with an explicit parent cgroup directory (testable without
/// a real delegated hierarchy).
fn apply_under(parent: &Path, container_pid: i32, limits: &Limits) -> Option<CgroupGuard> {
    let controllers = fs::read_to_string(parent.join("cgroup.controllers")).ok()?;
    let has = |c: &str| controllers.split_whitespace().any(|x| x == c);
    let want_pids = limits.pids_max.is_some() && has("pids");
    let want_mem = limits.memory_max.is_some() && has("memory");
    if !want_pids && !want_mem {
        return None;
    }

    // Enable the controllers for child cgroups. Best-effort: may already be on,
    // or be refused if the parent directly hosts processes (the "no internal
    // process" rule) — in which case we won't be able to set limits and bail.
    let mut enable = String::new();
    if want_pids {
        enable.push_str("+pids ");
    }
    if want_mem {
        enable.push_str("+memory");
    }
    let _ = fs::write(parent.join("cgroup.subtree_control"), enable.trim());

    let child = parent.join(format!("hako-{}", container_pid));
    fs::create_dir(&child).ok()?;
    let guard = CgroupGuard {
        path: child.clone(),
    };

    // Write the limits and require that at least one intended limit actually
    // bound. The controller files only exist if `subtree_control` enable above
    // succeeded; if it was refused (e.g. the parent directly hosts processes —
    // the cgroup-v2 "no internal process" rule) these writes fail, and we must
    // NOT report success, or callers get a false "limits applied" signal while
    // the container runs unconstrained.
    let mut any_applied = false;
    if let (true, Some(p)) = (want_pids, limits.pids_max) {
        if fs::write(child.join("pids.max"), p.to_string()).is_ok() {
            any_applied = true;
        }
    }
    if let (true, Some(m)) = (want_mem, limits.memory_max) {
        if fs::write(child.join("memory.max"), m.to_string()).is_ok() {
            any_applied = true;
        }
    }
    if !any_applied {
        return None; // guard's Drop rmdirs the empty cgroup
    }
    // Move the container (and thus its whole subtree) into the cgroup. If this
    // fails the limits wouldn't bind, so drop the empty cgroup and report none.
    if fs::write(child.join("cgroup.procs"), container_pid.to_string()).is_err() {
        return None; // guard's Drop rmdirs the empty cgroup
    }
    Some(guard)
}

/// Find a writable cgroup v2 directory to create the container cgroup under:
/// `HAKO_CGROUP_PARENT` if set, else the highest writable ancestor of our own
/// cgroup (the delegation boundary — e.g. `…/user@1000.service`, whose parent is
/// root-owned). Returns `None` when nothing is delegated to us.
fn find_delegated_parent() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HAKO_CGROUP_PARENT") {
        let p = PathBuf::from(p);
        let p = if p.is_absolute() {
            p
        } else {
            Path::new(CGROUP_ROOT).join(p)
        };
        if p.is_dir() {
            return Some(p);
        }
    }
    let self_cg = fs::read_to_string("/proc/self/cgroup").ok()?;
    // cgroup v2 line is `0::<path>`.
    let rel = self_cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let root = PathBuf::from(CGROUP_ROOT);
    let mut dir = root.join(rel.trim_start_matches('/'));
    let mut best: Option<PathBuf> = None;
    while dir.starts_with(&root) && dir != root {
        if writable_dir(&dir) {
            best = Some(dir.clone());
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    best
}

/// True if `p` is a directory we can write to (W_OK).
fn writable_dir(p: &Path) -> bool {
    if !p.is_dir() {
        return false;
    }
    match std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
        // Safe: access(2) with a valid NUL-terminated path.
        Ok(c) => unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 },
        Err(_) => false,
    }
}

/// Parse a memory size like `512`, `512K`, `64M`, `2G` into bytes.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        'B' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("512B"), Some(512));
        assert_eq!(parse_size("64K"), Some(64 * 1024));
        assert_eq!(parse_size("64k"), Some(64 * 1024));
        assert_eq!(parse_size("256M"), Some(256 * 1024 * 1024));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("  128M  "), Some(128 * 1024 * 1024));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("12X"), None);
        assert_eq!(parse_size("M"), None);
        // Overflow saturates to None rather than panicking / wrapping to a tiny
        // memory.max that would instantly OOM-kill the workload.
        assert_eq!(parse_size("99999999999G"), None);
    }

    // Verifies hako writes the right limits to the right cgroup files against a
    // delegated parent (modeled by a temp dir with a cgroup.controllers file).
    // The kernel's enforcement of those values is the kernel's job, not hako's.
    #[test]
    fn apply_under_writes_limits_and_moves_process() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        fs::write(parent.join("cgroup.controllers"), "cpu pids memory\n").unwrap();
        let limits = Limits {
            pids_max: Some(42),
            memory_max: Some(256 * 1024 * 1024),
        };
        let guard = apply_under(parent, 12345, &limits).expect("cgroup created");

        let child = parent.join("hako-12345");
        assert!(child.is_dir(), "container cgroup dir created");
        assert_eq!(
            fs::read_to_string(child.join("pids.max")).unwrap().trim(),
            "42"
        );
        assert_eq!(
            fs::read_to_string(child.join("memory.max")).unwrap().trim(),
            (256 * 1024 * 1024).to_string()
        );
        assert_eq!(
            fs::read_to_string(child.join("cgroup.procs"))
                .unwrap()
                .trim(),
            "12345"
        );
        let subtree = fs::read_to_string(parent.join("cgroup.subtree_control")).unwrap();
        assert!(subtree.contains("pids") && subtree.contains("memory"));
        drop(guard);
    }

    #[test]
    fn apply_under_skips_when_controller_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // Only cpu available — a pids limit can't be honored.
        fs::write(parent.join("cgroup.controllers"), "cpu\n").unwrap();
        let limits = Limits {
            pids_max: Some(42),
            memory_max: None,
        };
        assert!(apply_under(parent, 1, &limits).is_none());
        assert!(!parent.join("hako-1").exists());
    }
}
