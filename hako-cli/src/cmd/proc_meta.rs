//! The `/containers/<name>/proc/` meta surface: a container's live processes,
//! projected from the host kernel's /proc as files (the Plan 9 model).
//!
//! Reading live process state needs the kernel the container actually runs on.
//! Unlike the store-backed `status`/`ctl` nodes, this is a *runtime* meta node:
//! on Windows/macOS a `proc/` read is classified as needing the Linux runtime
//! (see `Cmd::needs_linux_runtime`) and forwarded into the WSL2 distro / Lima
//! VM, where this native reader runs against the real /proc. On native Linux it
//! runs directly.
//!
//! ## Security
//!
//! Only pids that provably belong to the named container are exposed — never
//! arbitrary host processes. Matching is by PID-namespace inode: a process is
//! the container's iff its `/proc/<pid>/ns/pid` inode matches a running
//! instance's recorded PID-1 (`nspid`); the host's own namespace is excluded, so
//! a bogus or host `nspid` can never enumerate the machine. `cat` re-checks the
//! inode *after* reading to close a pid-recycle race (a pid that dies and is
//! recycled to a non-container process mid-read is discarded, not leaked). `mem`
//! is deliberately never exposed.

use super::Ctx;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::process::ExitCode;

/// The proc meta node name — a runtime-backed directory beside `root/`.
pub const META_PROC: &str = "proc";

/// Per-pid files exposed under `/containers/<name>/proc/<pid>/`. `mem` is
/// intentionally excluded.
#[cfg(target_os = "linux")]
const PROC_FILES: &[&str] = &["status", "cmdline", "comm"];

/// If `sub` (the path after `/containers/<name>`, no leading slash) addresses
/// the proc surface, return the part after `proc`/`proc/` (empty = the proc
/// directory itself). Cross-platform: used both to classify a read for bridging
/// and to route it to the reader. An exact `proc` or `proc/...` matches; a name
/// like `procfs` does not (cf. the `root` boundary).
pub fn proc_subpath(sub: &str) -> Option<&str> {
    if sub == META_PROC {
        return Some("");
    }
    sub.strip_prefix("proc/")
}

// ============================================================================
// Linux reader
// ============================================================================

/// The PID-namespace inodes of the named container's *running* instances,
/// excluding our own (the host) namespace as a guard against a host/bogus
/// `nspid`. A process belongs to the container iff its `ns/pid` inode is in this
/// set. Cheap (one stat per instance) — both the membership check and the full
/// enumeration build on it.
#[cfg(target_os = "linux")]
fn container_ns_inodes(ctx: &Ctx<'_>, name: &str) -> io::Result<std::collections::HashSet<u64>> {
    let runtime_root = ctx.workdir.join(crate::DOT_HAKO);
    let instances = hako_runtime::instances::list(&runtime_root)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let own_ns = pid_ns_inode(std::process::id());
    let mut set = std::collections::HashSet::new();
    for inst in instances {
        if inst.config.branch == name && inst.is_running() {
            if let Some((nspid, _)) =
                hako_runtime::instances::read_nspid_with_starttime(&runtime_root, &inst.id)
            {
                if let Some(ino) = pid_ns_inode(nspid) {
                    if Some(ino) != own_ns {
                        set.insert(ino);
                    }
                }
            }
        }
    }
    Ok(set)
}

/// Every host-visible pid in the container's process tree — each process whose
/// PID namespace matches the container (v2: the whole tree, not just PID-1). A
/// full `/proc` scan, so only `ls` (which must list them all) uses it; a single
/// pid is checked in O(1) via [`pid_in_container`].
#[cfg(target_os = "linux")]
fn container_pids(ctx: &Ctx<'_>, name: &str) -> io::Result<Vec<u32>> {
    let ns_inodes = container_ns_inodes(ctx, name)?;
    if ns_inodes.is_empty() {
        return Ok(Vec::new());
    }
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue; // non-numeric /proc entries (cpuinfo, self, …)
        };
        if let Some(ino) = pid_ns_inode(pid) {
            if ns_inodes.contains(&ino) {
                pids.push(pid);
            }
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

/// Whether `pid` belongs to the container, in O(1) (no `/proc` scan). Returns
/// the process's PID-namespace inode when it does — the caller re-checks it
/// after reading to close a recycle race.
#[cfg(target_os = "linux")]
fn pid_in_container(ctx: &Ctx<'_>, name: &str, pid: u32) -> io::Result<Option<u64>> {
    let ns_inodes = container_ns_inodes(ctx, name)?;
    Ok(pid_ns_inode(pid).filter(|ino| ns_inodes.contains(ino)))
}

/// The inode of a process's PID namespace (`/proc/<pid>/ns/pid`). Processes in
/// the same PID namespace share this inode. `None` if unreadable (the process
/// is gone, or it's another user's and we lack permission) — which the caller
/// treats as "not in this container."
#[cfg(target_os = "linux")]
fn pid_ns_inode(pid: u32) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}/ns/pid"))
        .ok()
        .map(|m| m.ino())
}

#[cfg(target_os = "linux")]
fn not_in_container(name: &str, pid: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("no live process {pid} in container {name}"),
    )
}

/// `ls /containers/<name>/proc[/<pid>]`.
#[cfg(target_os = "linux")]
pub fn ls(ctx: &Ctx<'_>, name: &str, subpath: &str) -> io::Result<ExitCode> {
    let _ = ctx.state.open_container(name)?; // validate the container exists
    let sub = subpath.trim_matches('/');
    if sub.is_empty() {
        // The proc directory: one entry per live process (full enumeration).
        for pid in container_pids(ctx, name)? {
            println!("{}/", pid);
        }
        return Ok(ExitCode::SUCCESS);
    }
    // A specific process directory: list its files (O(1) membership check).
    let pid: u32 = sub
        .split('/')
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "not a process id"))?;
    if pid_in_container(ctx, name, pid)?.is_none() {
        return Err(not_in_container(name, pid));
    }
    for f in PROC_FILES {
        println!("{}", f);
    }
    Ok(ExitCode::SUCCESS)
}

/// `cat /containers/<name>/proc/<pid>/<file>`.
#[cfg(target_os = "linux")]
pub fn cat(ctx: &Ctx<'_>, name: &str, subpath: &str) -> io::Result<ExitCode> {
    let _ = ctx.state.open_container(name)?; // validate the container exists
    let sub = subpath.trim_matches('/');
    if sub.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "/containers/{name}/proc is a directory; specify a pid, e.g. \
                 `hako cat /containers/{name}/proc/<pid>/status` (`hako ls` to list them)"
            ),
        ));
    }
    let (pid_s, file) = sub.split_once('/').unwrap_or((sub, ""));
    let pid: u32 = pid_s
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "not a process id"))?;
    // Security boundary: the pid must belong to this container. Record its
    // namespace inode so we can re-verify after the read (recycle race).
    let Some(ns_before) = pid_in_container(ctx, name, pid)? else {
        return Err(not_in_container(name, pid));
    };
    let bytes = match file {
        "status" | "comm" => std::fs::read(format!("/proc/{pid}/{file}"))?,
        "cmdline" => {
            // /proc/<pid>/cmdline is NUL-separated; render args space-joined.
            let raw = std::fs::read(format!("/proc/{pid}/cmdline"))?;
            let mut out: Vec<u8> = Vec::with_capacity(raw.len() + 1);
            for (i, part) in raw.split(|b| *b == 0).filter(|p| !p.is_empty()).enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                out.extend_from_slice(part);
            }
            out.push(b'\n');
            out
        }
        "mem" => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "proc/<pid>/mem is not exposed",
            ))
        }
        "" => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("specify a file: {}", PROC_FILES.join(", ")),
            ))
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such proc file: {other}"),
            ))
        }
    };
    // Recycle guard: if the pid left the container's namespace under us (it
    // exited and the pid was reused by a process outside the container),
    // discard the read rather than leak an unrelated process's data.
    if pid_ns_inode(pid) != Some(ns_before) {
        return Err(not_in_container(name, pid));
    }
    io::stdout().write_all(&bytes)?;
    Ok(ExitCode::SUCCESS)
}

// ============================================================================
// Non-Linux stubs — only reached if the bridge was skipped (HAKO_NO_BRIDGE) or
// no Linux runtime is reachable. The normal Windows/macOS path forwards the
// read into WSL2/Lima before it gets here.
// ============================================================================

#[cfg(not(target_os = "linux"))]
pub fn ls(_ctx: &Ctx<'_>, _name: &str, _subpath: &str) -> io::Result<ExitCode> {
    Err(proc_needs_runtime())
}

#[cfg(not(target_os = "linux"))]
pub fn cat(_ctx: &Ctx<'_>, _name: &str, _subpath: &str) -> io::Result<ExitCode> {
    Err(proc_needs_runtime())
}

#[cfg(not(target_os = "linux"))]
fn proc_needs_runtime() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the container proc/ surface reads live processes from the Linux runtime; \
         it is normally bridged into WSL2/Lima — this failed because the bridge was \
         skipped (HAKO_NO_BRIDGE) or no Linux runtime is reachable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_subpath_classifies() {
        assert_eq!(proc_subpath("proc"), Some(""));
        assert_eq!(proc_subpath("proc/42"), Some("42"));
        assert_eq!(proc_subpath("proc/42/status"), Some("42/status"));
        assert_eq!(proc_subpath("procfs"), None); // not the proc node
        assert_eq!(proc_subpath("status"), None); // a different meta node
        assert_eq!(proc_subpath(""), None); // the container dir itself
    }
}
