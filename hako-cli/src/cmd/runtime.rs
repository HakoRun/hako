//! Container transformation: is, as, spawn, ps, logs, stop, reap.

use super::Ctx;
use crate::DOT_HAKO;
use hako_runtime::VolumeMount;
use std::io::{self, Write};
use std::process::ExitCode;

/// `hako run [-d] [--no-workspace] [-v ...] <branch> [cmd...]`.
///
/// Three modes, all dispatched here:
///   - foreground shell:    `hako run alpine`
///   - foreground command:  `hako run alpine ls /`
///   - detached:            `hako run -d alpine [cmd...]`
pub fn run(
    ctx: &Ctx<'_>,
    branch: String,
    detach: bool,
    volumes: Vec<String>,
    no_workspace: bool,
    command: Vec<String>,
) -> io::Result<ExitCode> {
    let volumes = build_volumes(ctx, &volumes, no_workspace)?;
    let repo = ctx.state.open_container(ctx.default_container)?;
    if detach {
        let cmd = if command.is_empty() {
            None
        } else {
            Some(command)
        };
        let id = hako_runtime::transform::run_container_detached(&repo, &branch, cmd, &volumes)
            .map_err(runtime_to_io)?;
        println!("{}", id);
        Ok(ExitCode::SUCCESS)
    } else if command.is_empty() {
        let code = hako_runtime::transform::become_container(&repo, &branch, &volumes)
            .map_err(runtime_to_io)?;
        Ok(exit_code_from(code))
    } else {
        let code = hako_runtime::transform::run_container(&repo, &branch, command, &volumes)
            .map_err(runtime_to_io)?;
        Ok(exit_code_from(code))
    }
}

/// `hako run-host <path> [args...]` — run a host-filesystem Linux binary
/// through hako. Bind-mounts the host system read-only so the dynamic loader
/// and shared libraries resolve, lets the runtime pass the display through
/// automatically, and execs the binary under hako's namespaces. Trades the
/// pristine versioned rootfs for "just run this downloaded app".
pub fn run_host(ctx: &Ctx<'_>, path: String, args: Vec<String>) -> io::Result<ExitCode> {
    use std::path::{Path, PathBuf};

    // Read-only binds of the host system, so a dynamically-linked binary finds
    // its interpreter (/lib64/ld-*.so) and libraries. Only existing dirs added.
    let mut volumes: Vec<VolumeMount> = ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/opt"]
        .iter()
        .filter(|d| Path::new(d).exists())
        .map(|d| VolumeMount {
            host: PathBuf::from(d),
            container: (*d).to_string(),
            readonly: true,
        })
        .collect();

    // The binary's own directory, in case it lives outside the dirs above
    // (e.g. a download under /home or /mnt/c). Mounted ro at the same path so
    // the in-container command path resolves.
    if let Some(dir) = Path::new(&path).parent() {
        let dir_s = dir.to_string_lossy().to_string();
        if !dir_s.is_empty() && !volumes.iter().any(|v| v.container == dir_s) {
            volumes.push(VolumeMount {
                host: dir.to_path_buf(),
                container: dir_s,
                readonly: true,
            });
        }
    }

    let repo = ctx.state.open_container(ctx.default_container)?;
    let branch = repo
        .current_branch()?
        .ok_or_else(|| io::Error::other("current container has no current branch"))?;

    let mut command = vec![path];
    command.extend(args);

    let code = hako_runtime::transform::run_container(&repo, &branch, command, &volumes)
        .map_err(runtime_to_io)?;
    Ok(exit_code_from(code))
}

/// `hako <unknown-subcommand> [args...]` — clap routes here when the first
/// token isn't a known command. Run those args as a command inside the
/// current identity's container, like the user typed `hako run <current>
/// <args...>`. The `is alpine; hako python ...` flow.
pub fn external(ctx: &Ctx<'_>, args: Vec<String>) -> io::Result<ExitCode> {
    if args.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "external dispatch with no args",
        ));
    }
    let volumes = build_volumes(ctx, &[], false)?;
    let repo = ctx.state.open_container(ctx.default_container)?;
    let branch = repo
        .current_branch()?
        .ok_or_else(|| io::Error::other("current container has no current branch"))?;
    let code = hako_runtime::transform::run_container(&repo, &branch, args, &volumes)
        .map_err(runtime_to_io)?;
    Ok(exit_code_from(code))
}

/// Parse user `-v` specs and prepend the implicit workspace mount unless:
///   - `--no-workspace` was passed, or
///   - the user already specified a mount targeting `/workspace`.
fn build_volumes(
    ctx: &Ctx<'_>,
    specs: &[String],
    no_workspace: bool,
) -> io::Result<Vec<VolumeMount>> {
    let mut user_volumes: Vec<VolumeMount> = specs
        .iter()
        .map(|s| VolumeMount::parse(s).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e)))
        .collect::<io::Result<_>>()?;

    if no_workspace {
        return Ok(user_volumes);
    }

    let user_already_mounted_workspace = user_volumes.iter().any(|v| v.container == "/workspace");
    if !user_already_mounted_workspace {
        let mut all = Vec::with_capacity(user_volumes.len() + 1);
        all.push(VolumeMount {
            host: ctx.workdir.to_path_buf(),
            container: "/workspace".into(),
            readonly: false,
        });
        all.append(&mut user_volumes);
        return Ok(all);
    }
    Ok(user_volumes)
}

pub fn ps(ctx: &Ctx<'_>, all: bool) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    let instances = hako_runtime::instances::list(&runtime_root).map_err(runtime_to_io)?;
    println!("{:<14} {:<20} {:<10} COMMAND", "ID", "BRANCH", "STATUS");
    for inst in instances {
        if !all && !inst.is_running() {
            continue;
        }
        let cmd = if inst.config.command.is_empty() {
            "(shell)".to_string()
        } else {
            inst.config.command.join(" ")
        };
        println!(
            "{:<14} {:<20} {:<10} {}",
            inst.id,
            inst.config.branch,
            inst.status(),
            cmd
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn logs(ctx: &Ctx<'_>, id: String, follow: bool) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    let (stdout_path, stderr_path) = hako_runtime::instances::log_paths(&runtime_root, &id);

    // Drain whatever is already in the log files.
    let mut stdout_pos: u64 = 0;
    let mut stderr_pos: u64 = 0;
    drain_from(&stdout_path, &mut stdout_pos, &mut io::stdout())?;
    drain_from(&stderr_path, &mut stderr_pos, &mut io::stderr())?;
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }

    // Poll until the instance exits. We check exit_code rather than
    // process liveness so that an instance whose process is gone but
    // hasn't yet recorded its exit is still followed to completion.
    loop {
        let drained_out = drain_from(&stdout_path, &mut stdout_pos, &mut io::stdout())?;
        let drained_err = drain_from(&stderr_path, &mut stderr_pos, &mut io::stderr())?;
        let inst = hako_runtime::instances::get(&runtime_root, &id);
        let done = matches!(&inst, Ok(i) if i.exit_code.is_some()) || inst.is_err();
        if done && !drained_out && !drained_err {
            // No more output coming; bail.
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Read any new bytes from `path` past `pos` and write them to `sink`.
/// Returns whether anything was drained. Updates `pos` to end-of-file.
fn drain_from<W: Write>(path: &std::path::Path, pos: &mut u64, sink: &mut W) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    if len <= *pos {
        // File may have been truncated (rotated, etc.); reset.
        if len < *pos {
            *pos = 0;
        }
        return Ok(false);
    }
    f.seek(SeekFrom::Start(*pos))?;
    let mut buf = Vec::with_capacity((len - *pos) as usize);
    f.read_to_end(&mut buf)?;
    sink.write_all(&buf)?;
    *pos = len;
    Ok(true)
}

pub fn exec(ctx: &Ctx<'_>, id: String, command: Vec<String>) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    let code = hako_runtime::transform::exec_in_instance(&runtime_root, &id, command)
        .map_err(runtime_to_io)?;
    Ok(exit_code_from(code))
}

pub fn stop(ctx: &Ctx<'_>, id: String) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    hako_runtime::instances::stop(&runtime_root, &id).map_err(runtime_to_io)?;
    Ok(ExitCode::SUCCESS)
}

pub fn reap(ctx: &Ctx<'_>, id: String, force: bool) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    hako_runtime::instances::remove(&runtime_root, &id, force).map_err(runtime_to_io)?;
    Ok(ExitCode::SUCCESS)
}

fn runtime_to_io(e: hako_runtime::RuntimeError) -> io::Error {
    let kind = match &e {
        hako_runtime::RuntimeError::UnsupportedPlatform { .. } => io::ErrorKind::Unsupported,
        hako_runtime::RuntimeError::BranchNotFound(_)
        | hako_runtime::RuntimeError::InstanceNotFound(_) => io::ErrorKind::NotFound,
        hako_runtime::RuntimeError::Io(io_err) => io_err.kind(),
        hako_runtime::RuntimeError::Other(_) => io::ErrorKind::Other,
    };
    io::Error::new(kind, e.to_string())
}

fn exit_code_from(code: i32) -> ExitCode {
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(u8::try_from(code).unwrap_or(1))
    }
}
