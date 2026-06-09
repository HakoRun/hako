//! Container transformation: namespaces, mount, pivot_root, exec.
//!
//! Linux-only. The non-Linux stub lives in `lib.rs` under
//! `#[cfg(not(target_os = "linux"))]`.
//!
//! # The double-fork architecture
//!
//! `become_container` forks twice. Why:
//!
//! 1. **Outer fork**: lets the caller's process keep running (the CLI returns
//!    after the child completes). It also gives us a clean single-threaded
//!    process for `unshare()`, which only affects the calling thread.
//! 2. **Inner fork** (after `unshare`): one process runs the FUSE server in a
//!    background thread; the other does mount setup, `pivot_root`, and
//!    `execvp`. The split is required because `execvp` replaces the process
//!    image and destroys all threads — including a FUSE thread — which would
//!    leave the mount unresponsive.
//!
//! Both inner processes share the unshared mount namespace (forked from the
//! same child after `unshare`), so the FUSE mount in one process is visible
//! in the other.
//!
//! # Sequence
//!
//! ```text
//! caller
//!  └── fork() ──── parent: waitpid(child); return exit code
//!       │
//!       child:
//!         unshare(CLONE_NEWUSER | CLONE_NEWNS)
//!         write /proc/self/{uid_map, setgroups, gid_map}
//!         fork() ──── fuse_server:
//!         │             mount_session() → background FUSE thread
//!         │             signal command_setup
//!         │             waitpid(command_setup)
//!         │             exit with command_setup's status
//!         │
//!         command_setup:
//!           wait for fuse-ready signal
//!           setup_bind_mounts(/tmp/hako-transform)
//!           setup_special_mounts(/tmp/hako-transform)
//!           pivot_root into mount
//!           execvp(shell or command)
//! ```
//!
//! When the user shell exits, the kernel sends SIGCHLD to the FUSE server,
//! it `exit`s, the FUSE thread dies with the process, and `AutoUnmount`
//! unmounts the FUSE mount automatically.

use crate::instances;
use crate::{RuntimeError, VolumeMount};
use hako::{ChunkStore, FsStore, Hash, Repo};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{setns, unshare, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, getgid, getuid, pivot_root, setsid, ForkResult, Gid, Pid, Uid};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Per-process mountpoint used during the transform. Each invocation cleans
/// up after itself; we use a stable path inside the unshared mount namespace
/// so it doesn't leak into other processes.
const MOUNTPOINT: &str = "/tmp/hako-transform";

// ============================================================================
// Public API (Linux)
// ============================================================================

/// Become the container at `branch`: spawn an interactive shell whose root
/// filesystem is the tree at `branch`'s HEAD. Blocks until the shell exits.
///
/// Returns the shell's exit status (0 on clean exit).
pub fn become_container(
    repo: &Repo<'_>,
    branch: &str,
    volumes: &[VolumeMount],
) -> Result<i32, RuntimeError> {
    let (store, root) = resolve_branch(repo, branch)?;
    run_outer(store, root, None, false, None, volumes.to_vec())
}

/// Run `command` inside the container at `branch`. Blocks until the command
/// completes. Returns the command's exit status.
pub fn run_container(
    repo: &Repo<'_>,
    branch: &str,
    command: Vec<String>,
    volumes: &[VolumeMount],
) -> Result<i32, RuntimeError> {
    if command.is_empty() {
        return Err(RuntimeError::Other("command is empty".into()));
    }
    let (store, root) = resolve_branch(repo, branch)?;
    run_outer(store, root, Some(command), false, None, volumes.to_vec())
}

/// Run `command` inside the container at `branch` with a **writable** FUSE
/// rootfs. Returns `(exit_code, new_tree_root)`. The new root reflects all
/// mutations the command made (via the FUSE mount); the caller can commit
/// it to the container's branch to persist them.
///
/// This is what `hako apply` uses to execute setup steps and capture their
/// effects. The pipeline is:
///
/// ```text
/// outer ─ pipe ─ inner_supervisor ─ FUSE RW ─ command_setup ─ exec command
///   │                    │                          │
///   │                    │                          └─ exits with code N
///   │                    │
///   │                    └─ reads RwSession::current_root()
///   │                       writes (root || N) into pipe
///   │                       exits N
///   │
///   └─ wait + read (root, N) from pipe → returns
/// ```
pub fn run_container_rw(
    repo: &Repo<'_>,
    branch: &str,
    command: Vec<String>,
    volumes: &[VolumeMount],
) -> Result<(i32, Hash), RuntimeError> {
    if command.is_empty() {
        return Err(RuntimeError::Other("command is empty".into()));
    }
    let (store, root) = resolve_branch(repo, branch)?;
    run_outer_rw(store, root, command, volumes.to_vec())
}

/// Spawn `command` (or the user's shell) inside the container at `branch`,
/// detached. Returns the instance id immediately; the supervising process
/// runs in the background.
///
/// State (pid, logs, exit code) is recorded under `<workdir>/runtime/<id>/`.
pub fn run_container_detached(
    repo: &Repo<'_>,
    branch: &str,
    command: Option<Vec<String>>,
    volumes: &[VolumeMount],
) -> Result<String, RuntimeError> {
    let (store, root) = resolve_branch(repo, branch)?;
    let workdir = repo.root().to_path_buf();
    let id = instances::generate_id();
    let cmd_for_record = command.clone().unwrap_or_default();
    instances::create(&workdir, &id, branch, &cmd_for_record)?;
    let volumes_owned = volumes.to_vec();

    // Outer fork: parent returns immediately; child supervises. If the fork
    // itself fails, clean up the partially-created instance directory so a
    // failed spawn doesn't leak state visible to `hako ps -a`.
    let fork_result = match unsafe { fork() } {
        Ok(r) => r,
        Err(e) => {
            let _ = instances::remove(&workdir, &id, true);
            return Err(io_other(format!("fork: {}", e)));
        }
    };
    match fork_result {
        ForkResult::Parent { .. } => Ok(id),
        ForkResult::Child => {
            // Record our pid before we fork again (the supervising process is
            // the one that holds the FUSE server and waits on the user shell).
            let pid = std::process::id();
            // Best-effort — if we can't write the pid, we still try to run.
            let _ = instances::write_pid(&workdir, &id, pid);

            let exit_code = match run_inner(
                store,
                root,
                command,
                true,
                Some((workdir.clone(), id.clone())),
                volumes_owned,
            ) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("hako runtime: detached spawn failed: {}", e);
                    1
                }
            };
            let _ = instances::write_exit_code(&workdir, &id, exit_code);
            std::process::exit(exit_code);
        }
    }
}

/// Run `command` inside the namespaces of an already-running instance.
///
/// Behaves like `docker exec`: opens the supervising process's user and
/// mount namespaces via `/proc/<pid>/ns/{user,mnt}`, fork+`setns`+exec.
/// Order matters: enter the user namespace FIRST so we acquire the caps
/// needed to enter the mount namespace afterwards.
///
/// Refuses to run if the recorded pid no longer matches the recorded
/// start_time — that pid was recycled by an unrelated process and we'd
/// be entering the wrong sandbox.
pub fn exec_in_instance(
    workdir: &Path,
    id: &str,
    command: Vec<String>,
) -> Result<i32, RuntimeError> {
    if command.is_empty() {
        return Err(RuntimeError::Other("command is empty".into()));
    }
    let (pid, recorded_st) = instances::read_pid_with_starttime(workdir, id)
        .ok_or_else(|| RuntimeError::InstanceNotFound(id.into()))?;
    // Validate the pid still belongs to our supervising process. The same
    // check `is_running` would do — duplicated here so we can give a
    // clearer error before forking.
    {
        let inst = instances::get(workdir, id)?;
        if !inst.is_running() {
            return Err(RuntimeError::Other(format!(
                "instance {} is not running (pid {} has exited or was recycled)",
                id, pid
            )));
        }
        let _ = recorded_st;
    }

    // Open the namespace fds in the parent so the error path is clean if
    // /proc/PID/ns/* doesn't exist.
    let user_ns = fs::File::open(format!("/proc/{}/ns/user", pid))
        .map_err(|e| io_other(format!("open user ns: {}", e)))?;
    let mnt_ns = fs::File::open(format!("/proc/{}/ns/mnt", pid))
        .map_err(|e| io_other(format!("open mnt ns: {}", e)))?;

    match unsafe { fork() }.map_err(|e| io_other(format!("fork: {}", e)))? {
        ForkResult::Parent { child } => wait_for_child(child).map(|s| match s {
            WaitStatus::Exited(_, code) => code,
            WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
            _ => 0,
        }),
        ForkResult::Child => {
            let code = enter_and_exec(&user_ns, &mnt_ns, command).unwrap_or_else(|e| {
                eprintln!("hako exec: {}", e);
                1
            });
            std::process::exit(code);
        }
    }
}

fn enter_and_exec(
    user_ns: &fs::File,
    mnt_ns: &fs::File,
    command: Vec<String>,
) -> Result<i32, RuntimeError> {
    // User ns first — we need its caps to enter mnt ns.
    setns(user_ns.as_fd(), CloneFlags::CLONE_NEWUSER)
        .map_err(|e| io_other(format!("setns user: {}", e)))?;
    setns(mnt_ns.as_fd(), CloneFlags::CLONE_NEWNS)
        .map_err(|e| io_other(format!("setns mnt: {}", e)))?;

    // setns doesn't change cwd; the inherited cwd may not exist in the
    // new mount ns. chdir to / which is guaranteed to be the pivoted root.
    env::set_current_dir("/")?;

    exec_command(command)
}

// ============================================================================
// Internals
// ============================================================================

fn resolve_branch(
    repo: &Repo<'_>,
    branch: &str,
) -> Result<(Arc<dyn ChunkStore + Send + Sync + 'static>, Hash), RuntimeError> {
    // Resolve branch → commit → tree.
    let commit_hash = repo
        .read_ref(branch)?
        .ok_or_else(|| RuntimeError::BranchNotFound(branch.into()))?;
    let commit = repo.load_commit(&commit_hash)?;
    let tree_root = commit.tree;

    // FUSE needs a `'static` store it can own across threads. Open a fresh
    // FsStore pointed at the same objects directory. Mirrors what main.rs's
    // Mount command does (see hako-core/src/main.rs Cmd::Mount).
    let objs_path = repo.root().join(hako::state::OBJECTS);
    let store: Arc<dyn ChunkStore + Send + Sync + 'static> = Arc::new(FsStore::new(objs_path)?);
    Ok((store, tree_root))
}

fn run_outer(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    command: Option<Vec<String>>,
    detached: bool,
    detached_state: Option<(PathBuf, String)>,
    volumes: Vec<VolumeMount>,
) -> Result<i32, RuntimeError> {
    // Fork to escape the parent process; this also keeps the parent's
    // resources untouched if the child crashes during namespace setup.
    match unsafe { fork() }.map_err(|e| io_other(format!("fork: {}", e)))? {
        ForkResult::Parent { child } => wait_for_child(child).map(|s| match s {
            WaitStatus::Exited(_, code) => code,
            WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
            _ => 0,
        }),
        ForkResult::Child => {
            let code = run_inner(store, root, command, detached, detached_state, volumes)
                .unwrap_or_else(|e| {
                    eprintln!("hako runtime: {}", e);
                    1
                });
            std::process::exit(code);
        }
    }
}

/// Inner: unshare, set up uid/gid maps, fork into FUSE server + command setup.
/// Returns the command's exit code, or an error on setup failure.
fn run_inner(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    command: Option<Vec<String>>,
    detached: bool,
    detached_state: Option<(PathBuf, String)>,
    volumes: Vec<VolumeMount>,
) -> Result<i32, RuntimeError> {
    let uid = getuid();
    let gid = getgid();

    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
        .map_err(|e| io_other(format!("unshare: {}", e)))?;

    fs::write("/proc/self/uid_map", format!("0 {} 1\n", uid))?;
    fs::write("/proc/self/setgroups", "deny\n")?;
    fs::write("/proc/self/gid_map", format!("0 {} 1\n", gid))?;

    let (fuse_sock, shell_sock) =
        UnixStream::pair().map_err(|e| io_other(format!("socketpair: {}", e)))?;

    fs::create_dir_all(MOUNTPOINT)?;

    match unsafe { fork() }.map_err(|e| io_other(format!("inner fork: {}", e)))? {
        ForkResult::Child => {
            drop(fuse_sock);
            run_command_setup(
                shell_sock,
                command,
                detached,
                detached_state.as_ref(),
                &volumes,
            )
        }
        ForkResult::Parent { child } => {
            drop(shell_sock);
            run_fuse_server(store, root, fuse_sock, child, detached, detached_state)
        }
    }
}

/// FUSE-server side: mount FUSE in a background thread, signal command setup,
/// wait for command setup to exit, exit with its status.
fn run_fuse_server(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    sync_sock: UnixStream,
    child: Pid,
    detached: bool,
    detached_state: Option<(PathBuf, String)>,
) -> Result<i32, RuntimeError> {
    // Mount FUSE in the background. The session handle keeps it live; when
    // dropped (at function return / process exit), the mount is unmounted.
    let _session = hako::fuse::mount_session(store, root, Path::new(MOUNTPOINT))
        .map_err(|e| io_other(format!("mount FUSE: {}", e)))?;

    // For detached mode, become a session leader after FUSE is set up so we
    // detach cleanly from the controlling terminal.
    if detached {
        setsid().map_err(|e| io_other(format!("setsid: {}", e)))?;
    }

    // Signal the command-setup process that the mount is ready.
    let mut sock = sync_sock;
    sock.write_all(&[1])
        .map_err(|e| io_other(format!("signal: {}", e)))?;
    drop(sock);

    // Wait for the command process to exit.
    let status = wait_for_child(child)?;

    let exit_code = match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => 0,
    };

    // Record the exit code for detached instances, even if the inner process
    // wrote one — this is the authoritative source.
    if let Some((workdir, id)) = detached_state {
        let _ = instances::write_exit_code(&workdir, &id, exit_code);
    }

    // Drop the session here so the mount is cleanly unmounted before exit.
    drop(_session);
    Ok(exit_code)
}

// ============================================================================
// RW round-trip — used by `hako apply` to capture mutations setup commands
// make to the container's tree.
// ============================================================================

/// Outer process for the RW path. Forks an inner supervisor, waits for it to
/// exit, then reads the (32-byte root || 4-byte exit-code) tail it wrote
/// down a pipe just before exiting.
fn run_outer_rw(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    initial_root: Hash,
    command: Vec<String>,
    volumes: Vec<VolumeMount>,
) -> Result<(i32, Hash), RuntimeError> {
    use nix::unistd::pipe;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

    let (read_fd, write_fd): (OwnedFd, OwnedFd) =
        pipe().map_err(|e| io_other(format!("pipe: {}", e)))?;

    match unsafe { fork() }.map_err(|e| io_other(format!("fork: {}", e)))? {
        ForkResult::Parent { child } => {
            // Close the write end in the parent so EOF arrives if the child dies.
            drop(write_fd);
            let status = wait_for_child(child)?;
            let exit_code = match status {
                WaitStatus::Exited(_, code) => code,
                WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
                _ => 0,
            };
            // Read the final root the inner supervisor wrote before exiting.
            // 32 bytes of hash, then we trust the exit code we already have.
            let mut buf = [0u8; 32];
            let mut f = unsafe { fs::File::from_raw_fd(read_fd.into_raw_fd()) };
            match f.read_exact(&mut buf) {
                Ok(()) => Ok((exit_code, Hash(buf))),
                Err(_) => {
                    // Child died before writing — return initial root with the
                    // exit code so the caller knows nothing was committed.
                    Ok((exit_code, initial_root))
                }
            }
        }
        ForkResult::Child => {
            drop(read_fd);
            let result = run_inner_rw(store, initial_root, command, volumes, write_fd.as_raw_fd());
            let exit = match result {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("hako runtime (rw): {}", e);
                    1
                }
            };
            // write_fd was already consumed in run_inner_rw; just exit.
            std::process::exit(exit);
        }
    }
}

fn run_inner_rw(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    command: Vec<String>,
    volumes: Vec<VolumeMount>,
    outer_pipe_fd: std::os::fd::RawFd,
) -> Result<i32, RuntimeError> {
    let uid = getuid();
    let gid = getgid();

    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
        .map_err(|e| io_other(format!("unshare: {}", e)))?;

    fs::write("/proc/self/uid_map", format!("0 {} 1\n", uid))?;
    fs::write("/proc/self/setgroups", "deny\n")?;
    fs::write("/proc/self/gid_map", format!("0 {} 1\n", gid))?;

    let (fuse_sock, shell_sock) =
        UnixStream::pair().map_err(|e| io_other(format!("socketpair: {}", e)))?;
    fs::create_dir_all(MOUNTPOINT)?;

    match unsafe { fork() }.map_err(|e| io_other(format!("inner fork: {}", e)))? {
        ForkResult::Child => {
            // Command-setup process — same path as the RO flow.
            drop(fuse_sock);
            // The pipe to outer is for the FUSE server only.
            let _ = nix::unistd::close(outer_pipe_fd);
            run_command_setup(shell_sock, Some(command), false, None, &volumes)
        }
        ForkResult::Parent { child } => {
            drop(shell_sock);
            run_fuse_server_rw(store, root, fuse_sock, child, outer_pipe_fd)
        }
    }
}

/// FUSE-server side, RW edition. Mounts read-write, signals command-setup,
/// waits for exit, then writes the final root hash to the outer pipe before
/// dropping the FUSE session (which unmounts).
fn run_fuse_server_rw(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    sync_sock: UnixStream,
    child: Pid,
    outer_pipe_fd: std::os::fd::RawFd,
) -> Result<i32, RuntimeError> {
    use std::os::fd::FromRawFd;

    let session = hako::fuse::mount_session_rw(store, root, Path::new(MOUNTPOINT))
        .map_err(|e| io_other(format!("mount FUSE rw: {}", e)))?;

    let mut sock = sync_sock;
    sock.write_all(&[1])
        .map_err(|e| io_other(format!("signal: {}", e)))?;
    drop(sock);

    let status = wait_for_child(child)?;
    let exit_code = match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => 0,
    };

    // Write the post-exec root to the outer parent over the pipe BEFORE
    // dropping the session (so the chunk store still has everything).
    let final_root = session.current_root();
    {
        let mut f = unsafe { fs::File::from_raw_fd(outer_pipe_fd) };
        let _ = f.write_all(&final_root.0);
        // f is dropped here, closing the pipe.
    }

    drop(session);
    Ok(exit_code)
}

/// Command-setup side: wait for FUSE, set up bind mounts and special mounts,
/// pivot_root, exec the command. Never returns on success.
fn run_command_setup(
    sync_sock: UnixStream,
    command: Option<Vec<String>>,
    detached: bool,
    detached_state: Option<&(PathBuf, String)>,
    volumes: &[VolumeMount],
) -> Result<i32, RuntimeError> {
    // Wait for FUSE-ready signal from the FUSE-server process.
    let mut sock = sync_sock;
    let mut buf = [0u8; 1];
    sock.read_exact(&mut buf)
        .map_err(|e| io_other(format!("await fuse ready: {}", e)))?;
    drop(sock);

    setup_bind_mounts(MOUNTPOINT)?;
    setup_special_mounts(MOUNTPOINT)?;
    setup_user_volumes(MOUNTPOINT, volumes)?;

    // For detached mode, redirect stdout/stderr to log files BEFORE pivot_root
    // (the log paths are on the host filesystem, not the new root).
    if detached {
        if let Some((workdir, id)) = detached_state {
            redirect_output(workdir, id)?;
        }
    }

    pivot_into(MOUNTPOINT)?;

    // For interactive use, chdir to /workspace if it exists (the default
    // auto-mount), else fall back to /home/$USER. This makes `hako is alpine`
    // drop you straight into your project, like `cd && code .`.
    if command.is_none() {
        if Path::new("/workspace").is_dir() {
            let _ = env::set_current_dir("/workspace");
        } else {
            let user = env::var("USER").unwrap_or_else(|_| "root".into());
            let home = format!("/home/{}", user);
            if Path::new(&home).exists() {
                let _ = env::set_current_dir(&home);
            }
        }
    }

    // execvp — never returns on success.
    match command {
        Some(cmd) => exec_command(cmd),
        None => exec_shell(),
    }
}

fn redirect_output(workdir: &Path, id: &str) -> Result<(), RuntimeError> {
    use std::os::unix::io::AsRawFd;
    let (stdout_path, stderr_path) = instances::log_paths(workdir, id);
    let stdout_file = fs::File::create(&stdout_path)?;
    let stderr_file = fs::File::create(&stderr_path)?;
    // SAFETY: dup2 with valid fd numbers; the kernel ensures atomicity.
    unsafe {
        libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(stderr_file.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(())
}

// ============================================================================
// Mount setup
// ============================================================================

fn setup_bind_mounts(root: &str) -> Result<(), RuntimeError> {
    if let Some(home) = dirs::home_dir() {
        let user = env::var("USER").unwrap_or_else(|_| "user".into());
        let target = format!("{}/home/{}", root, user);
        fs::create_dir_all(&target)?;
        bind_mount(&home, &target, MsFlags::MS_BIND | MsFlags::MS_REC)?;
    }

    // /tmp — bind so user-side temp files survive into the container.
    let tmp_target = format!("{}/tmp", root);
    fs::create_dir_all(&tmp_target)?;
    bind_mount("/tmp", &tmp_target, MsFlags::MS_BIND)?;

    // Essential read-from-host files (DNS, host map).
    for file in &["/etc/resolv.conf", "/etc/hosts"] {
        if Path::new(file).exists() {
            let target = format!("{}{}", root, file);
            if let Some(parent) = Path::new(&target).parent() {
                fs::create_dir_all(parent)?;
            }
            // Create an empty file to mount over.
            fs::write(&target, "")?;
            bind_mount(file, &target, MsFlags::MS_BIND)?;
        }
    }

    Ok(())
}

fn setup_special_mounts(root: &str) -> Result<(), RuntimeError> {
    use std::os::unix::fs::symlink;

    // === /dev: OCI-standard minimal device set ===
    let dev = format!("{}/dev", root);
    fs::create_dir_all(&dev)?;
    mount_kind("tmpfs", &dev, "tmpfs", MsFlags::empty(), Some("mode=755"))?;

    for name in &["null", "zero", "full", "random", "urandom", "tty"] {
        let src = format!("/dev/{}", name);
        let dst = format!("{}/{}", dev, name);
        if Path::new(&src).exists() {
            fs::write(&dst, "")?;
            bind_mount(&src, &dst, MsFlags::MS_BIND)?;
        }
    }

    let pts = format!("{}/pts", dev);
    fs::create_dir_all(&pts)?;
    mount_kind(
        "devpts",
        &pts,
        "devpts",
        MsFlags::empty(),
        Some("newinstance,ptmxmode=0666"),
    )?;
    symlink("pts/ptmx", format!("{}/ptmx", dev))?;

    symlink("/proc/self/fd", format!("{}/fd", dev))?;
    symlink("/proc/self/fd/0", format!("{}/stdin", dev))?;
    symlink("/proc/self/fd/1", format!("{}/stdout", dev))?;
    symlink("/proc/self/fd/2", format!("{}/stderr", dev))?;

    let shm = format!("{}/shm", dev);
    fs::create_dir_all(&shm)?;
    mount_kind("tmpfs", &shm, "tmpfs", MsFlags::empty(), Some("mode=1777"))?;

    // === /proc: bind from host (can't mount fresh procfs without PID ns) ===
    let proc_path = format!("{}/proc", root);
    fs::create_dir_all(&proc_path)?;
    bind_mount("/proc", &proc_path, MsFlags::MS_BIND | MsFlags::MS_REC)?;

    // === /sys: bind from host ===
    let sys_path = format!("{}/sys", root);
    fs::create_dir_all(&sys_path)?;
    bind_mount("/sys", &sys_path, MsFlags::MS_BIND | MsFlags::MS_REC)?;

    Ok(())
}

/// Bind-mount each user volume into the rootfs at its requested target.
/// Read-only volumes are mounted then immediately remounted with MS_RDONLY,
/// since the kernel's MS_BIND ignores rw/ro flags on the initial mount.
fn setup_user_volumes(root: &str, volumes: &[VolumeMount]) -> Result<(), RuntimeError> {
    for v in volumes {
        let host = v.host.canonicalize().map_err(|e| {
            io_other(format!(
                "volume host {} cannot be resolved: {}",
                v.host.display(),
                e
            ))
        })?;
        if !host.exists() {
            return Err(io_other(format!(
                "volume host path {} does not exist",
                host.display()
            )));
        }
        // Container path is absolute (validated at parse). Strip leading `/`
        // to join under the root.
        let rel = v.container.trim_start_matches('/');
        let target = format!("{}/{}", root, rel);
        // Create the mountpoint. Files need a stub file; directories need a dir.
        if host.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = Path::new(&target).parent() {
                fs::create_dir_all(parent)?;
            }
            // Create an empty file to mount over.
            if !Path::new(&target).exists() {
                fs::write(&target, "")?;
            }
        }
        bind_mount(&host, &target, MsFlags::MS_BIND | MsFlags::MS_REC)?;
        if v.readonly {
            // Remount the same path read-only. Source/fstype/data are ignored
            // for a remount; we just need the target + MS_REMOUNT|MS_BIND|MS_RDONLY.
            mount(
                None::<&str>,
                target.as_str(),
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                None::<&str>,
            )
            .map_err(|e| io_other(format!("remount {} ro: {}", target, e)))?;
        }
    }
    Ok(())
}

fn pivot_into(new_root: &str) -> Result<(), RuntimeError> {
    // pivot_root requires the new root to be a mount point.
    bind_mount(new_root, new_root, MsFlags::MS_BIND | MsFlags::MS_REC)?;

    env::set_current_dir(new_root)?;
    fs::create_dir_all("oldroot")?;

    pivot_root(".", "oldroot").map_err(|e| io_other(format!("pivot_root: {}", e)))?;

    env::set_current_dir("/")?;
    umount2("/oldroot", MntFlags::MNT_DETACH)
        .map_err(|e| io_other(format!("umount oldroot: {}", e)))?;
    let _ = fs::remove_dir("/oldroot");

    Ok(())
}

// ============================================================================
// exec
// ============================================================================

fn exec_shell() -> Result<i32, RuntimeError> {
    use std::ffi::CString;
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let shell_cstr =
        CString::new(shell.as_bytes()).map_err(|e| io_other(format!("shell name: {}", e)))?;

    let user = env::var("USER").unwrap_or_else(|_| "root".into());
    env::set_var("HOME", format!("/home/{}", user));

    let args: [&std::ffi::CStr; 1] = [&shell_cstr];
    nix::unistd::execvp(&shell_cstr, &args)
        .map_err(|e| io_other(format!("execvp {}: {}", shell, e)))?;
    Err(io_other(
        "execvp returned without error (impossible)".into(),
    ))
}

fn exec_command(command: Vec<String>) -> Result<i32, RuntimeError> {
    use std::ffi::CString;
    if command.is_empty() {
        return Err(io_other("empty command".into()));
    }
    let program = CString::new(command[0].as_bytes())
        .map_err(|e| io_other(format!("program name: {}", e)))?;
    let args: Vec<CString> = command
        .iter()
        .map(|s| CString::new(s.as_bytes()))
        .collect::<Result<_, _>>()
        .map_err(|e| io_other(format!("argument: {}", e)))?;
    let args_ref: Vec<&std::ffi::CStr> = args.iter().map(|s| s.as_ref()).collect();
    nix::unistd::execvp(&program, &args_ref)
        .map_err(|e| io_other(format!("execvp {}: {}", command[0], e)))?;
    Err(io_other(
        "execvp returned without error (impossible)".into(),
    ))
}

// ============================================================================
// Mount helpers
// ============================================================================

fn bind_mount(
    src: impl AsRef<Path>,
    dst: impl AsRef<str>,
    flags: MsFlags,
) -> Result<(), RuntimeError> {
    let src = src.as_ref();
    mount(Some(src), dst.as_ref(), None::<&str>, flags, None::<&str>).map_err(|e| {
        io_other(format!(
            "bind mount {} → {}: {}",
            src.display(),
            dst.as_ref(),
            e
        ))
    })
}

fn mount_kind(
    src: &str,
    dst: &str,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<(), RuntimeError> {
    mount(Some(src), dst, Some(fstype), flags, data)
        .map_err(|e| io_other(format!("mount {} ({}): {}", dst, fstype, e)))
}

// ============================================================================
// Misc helpers
// ============================================================================

fn wait_for_child(pid: Pid) -> Result<WaitStatus, RuntimeError> {
    waitpid(pid, None).map_err(|e| io_other(format!("waitpid: {}", e)))
}

fn io_other(msg: String) -> RuntimeError {
    RuntimeError::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
}

// Suppress unused-imports / dead_code on intentional uid/gid type imports
// when read by lints in earlier compiler passes.
#[allow(dead_code)]
fn _suppress() {
    let _: Option<Uid> = None;
    let _: Option<Gid> = None;
}
