//! Container transformation: is, as, spawn, ps, logs, stop, reap.

use super::Ctx;
use crate::DOT_HAKO;
use hako_runtime::{Network, RestartPolicy, VolumeMount};
use std::io::{self, Write};
use std::process::ExitCode;

/// `hako run`'s flag surface, grouped (the `AppOverrides` pattern) so the
/// handler stays under clippy's argument cap as run grows flags (network,
/// restart policy; later ports).
pub struct RunOpts {
    pub detach: bool,
    pub volumes: Vec<String>,
    pub network: String,
    pub restart: String,
    pub no_workspace: bool,
    pub display: bool,
}

/// `hako run [-d] [--network none|host] [--restart …] [--no-workspace] [-v ...] <branch> [cmd...]`.
///
/// Three modes, all dispatched here:
///   - foreground shell:    `hako run alpine`
///   - foreground command:  `hako run alpine ls /`
///   - detached:            `hako run -d alpine [cmd...]`
pub fn run(
    ctx: &Ctx<'_>,
    branch: String,
    opts: RunOpts,
    command: Vec<String>,
) -> io::Result<ExitCode> {
    // clap's value_parser restricts these, so parse can't fail from the CLI;
    // the error paths guard non-CLI callers.
    let network = Network::parse(&opts.network).map_err(io::Error::other)?;
    let restart = RestartPolicy::parse(&opts.restart).map_err(io::Error::other)?;
    if restart.is_supervised() && !opts.detach {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--restart requires -d (a restart policy only applies to a detached workload)",
        ));
    }
    set_display_env(ctx, opts.display);
    let volumes = build_volumes(ctx, &opts.volumes, opts.no_workspace)?;
    let repo = ctx.state.open_container(ctx.default_container)?;
    if opts.detach {
        let cmd = if command.is_empty() {
            None
        } else {
            Some(command)
        };
        let id = hako_runtime::transform::run_container_detached(
            &repo, &branch, cmd, &volumes, network, restart,
        )
        .map_err(runtime_to_io)?;
        println!("{}", id);
        Ok(ExitCode::SUCCESS)
    } else if command.is_empty() {
        let code = hako_runtime::transform::become_container(&repo, &branch, &volumes, network)
            .map_err(runtime_to_io)?;
        Ok(exit_code_from(code))
    } else {
        let code =
            hako_runtime::transform::run_container(&repo, &branch, command, &volumes, network)
                .map_err(runtime_to_io)?;
        Ok(exit_code_from(code))
    }
}

/// Opt display passthrough on for the runtime call about to be made. The
/// runtime reads `HAKO_DISPLAY`; setting it here (before the in-process fork
/// in `hako_runtime::transform`) propagates it to the container. Honors the
/// explicit `--display` flag OR `display = true` in the workspace's hako.toml.
/// Off otherwise — passthrough weakens isolation, so it is never the default.
fn set_display_env(ctx: &Ctx<'_>, flag: bool) {
    let from_cfg = ctx.cfg.app.as_ref().is_some_and(|a| a.display);
    if flag || from_cfg {
        std::env::set_var("HAKO_DISPLAY", "1");
    }
}

/// `hako run-host [--in <container>|auto] <path> [args...]` — run a Linux
/// binary from the host filesystem through hako, with display passthrough.
///
/// Three modes, differing in where the binary's *libraries* come from:
///   - default — the host system (bind-mounted read-only). Best for a binary
///     matching the host libc, a static binary, or an AppImage.
///   - `--in <container>` — that container's rootfs (only the binary is mounted
///     in). Lets a cross-distro binary (e.g. Alpine/musl) run against the
///     libraries it actually needs.
///   - `--in auto` — detect the binary's libc and pick (pulling if missing) a
///     base image: musl → alpine, glibc → debian.
pub fn run_host(
    ctx: &Ctx<'_>,
    in_container: Option<String>,
    display: bool,
    command: Vec<String>,
) -> io::Result<ExitCode> {
    set_display_env(ctx, display);
    // The Windows host bridge forwards the binary path via $HAKO_RUN_HOST_BIN
    // (WSLENV path-translated) instead of an argv token, so a spaced path survives
    // the wsl.exe boundary even when 8.3 short names are disabled (#79). When set,
    // it IS the binary path and `command` holds only the program's own args.
    let mut command = resolve_run_host_command(command, std::env::var("HAKO_RUN_HOST_BIN").ok());
    // command[0] is the host binary path; the rest are its arguments. Resolve
    // it to a canonical absolute path: this turns `./app`, `app`, `../app`,
    // and symlinks into a real absolute path with no `.`/`..` components — so
    // the mount target derived from it (push_bin_dir) can never become `/`,
    // the cwd, or a path above the container root. The in-container exec path
    // (command[0]) is rewritten to match, so it resolves to the mounted file.
    let raw = command
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "run-host needs a path"))?
        .clone();
    let abs = std::fs::canonicalize(&raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("run-host: cannot resolve binary {:?}: {}", raw, e),
        )
    })?;
    let path = abs.to_string_lossy().to_string();
    command[0] = path.clone();

    match in_container.as_deref() {
        None => run_host_on_host(ctx, &path, command),
        Some("auto") => {
            let container = resolve_auto_container(ctx, &path)?;
            run_host_in_container(ctx, &container, &path, command)
        }
        Some(name) => run_host_in_container(ctx, name, &path, command),
    }
}

/// Prepend an env-provided binary path (from `$HAKO_RUN_HOST_BIN`, set by the
/// Windows host bridge, #79) so `command[0]` is the path — matching the argv
/// form the guest otherwise expects. An unset/empty env leaves `command` as-is.
fn resolve_run_host_command(mut command: Vec<String>, env_bin: Option<String>) -> Vec<String> {
    if let Some(bin) = env_bin.filter(|b| !b.is_empty()) {
        command.insert(0, bin);
    }
    command
}

/// Tier 1: libraries from the host system.
fn run_host_on_host(ctx: &Ctx<'_>, path: &str, command: Vec<String>) -> io::Result<ExitCode> {
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
            mask: Vec::new(),
        })
        .collect();
    push_bin_dir(&mut volumes, path);

    let repo = ctx.state.open_container(ctx.default_container)?;
    let branch = repo
        .current_branch()?
        .ok_or_else(|| io::Error::other("current container has no current branch"))?;
    let code = hako_runtime::transform::run_container(
        &repo,
        &branch,
        command,
        &volumes,
        Network::Isolated,
    )
    .map_err(runtime_to_io)?;
    Ok(exit_code_from(code))
}

/// Tiers 2/3: libraries from `container`'s rootfs; only the binary is mounted in.
fn run_host_in_container(
    ctx: &Ctx<'_>,
    container: &str,
    path: &str,
    command: Vec<String>,
) -> io::Result<ExitCode> {
    let mut volumes: Vec<VolumeMount> = Vec::new();
    push_bin_dir(&mut volumes, path);

    let repo = ctx.state.open_container(container).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "container '{}' not found ({}). Pull it first (`hako pull {}`), or use `--in auto`.",
                container, e, container
            ),
        )
    })?;
    let branch = repo
        .current_branch()?
        .ok_or_else(|| io::Error::other("container has no current branch"))?;
    crate::diag!(
        "running {} against container '{}' (libraries from the container)",
        path,
        container
    );
    let code = hako_runtime::transform::run_container(
        &repo,
        &branch,
        command,
        &volumes,
        Network::Isolated,
    )
    .map_err(runtime_to_io)?;
    Ok(exit_code_from(code))
}

/// Mount the binary read-only so the in-container command path resolves to the
/// host file. `path` MUST be canonical+absolute (run_host guarantees this).
/// Normally we mount the binary's parent directory (so sibling resources
/// resolve); but for a root-level binary (`/app`, parent `/`) we mount just the
/// file — never the whole host root over the container rootfs.
fn push_bin_dir(volumes: &mut Vec<VolumeMount>, path: &str) {
    use std::path::{Path, PathBuf};
    let p = Path::new(path);
    let (host, container) = match p.parent() {
        // Root-level binary: mount the file itself, not all of `/`.
        Some(parent) if parent == Path::new("/") => (p.to_path_buf(), path.to_string()),
        // Normal case: mount the containing directory.
        Some(parent) if !parent.as_os_str().is_empty() => {
            (PathBuf::from(parent), parent.to_string_lossy().to_string())
        }
        // No usable parent — shouldn't happen for a canonical absolute path.
        _ => return,
    };
    if !volumes.iter().any(|v| v.container == container) {
        volumes.push(VolumeMount {
            host,
            container,
            readonly: true,
            mask: Vec::new(),
        });
    }
}

/// Tier 3: detect the binary's libc and resolve a container to run it against,
/// pulling a base image if no suitable container exists yet.
fn resolve_auto_container(ctx: &Ctx<'_>, path: &str) -> io::Result<String> {
    let (libc, distro, image) = match detect_libc(path)? {
        Libc::Musl => ("musl", "alpine", "alpine"),
        Libc::Glibc => ("glibc", "debian", "debian"),
    };
    crate::diag!("detected {} binary → base image '{}'", libc, image);

    // Reuse an existing container of that name if present (it may already have
    // the binary's other shared-lib deps installed); otherwise pull the base.
    if ctx.state.list_containers()?.iter().any(|c| c == distro) {
        crate::diag!("reusing existing container '{}'", distro);
    } else {
        let image_ref = hako::ImageRef::parse(image).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("bad image ref: {}", e))
        })?;
        crate::cmd::oci::pull_into(
            ctx.state,
            &image_ref,
            distro,
            "linux",
            crate::cmd::oci::host_oci_arch(),
            false,
        )?;
    }
    Ok(distro.to_string())
}

enum Libc {
    Musl,
    Glibc,
}

/// Detect a binary's libc from its ELF `PT_INTERP` program header (the dynamic
/// loader path): `ld-musl-*` → musl, `ld-linux*`/`ld.so` → glibc. Parses the
/// ELF/program-header structures and reads exactly the interpreter string —
/// not a substring scan (which would false-match the literal "ld-musl" sitting
/// in some glibc binary's `.rodata`). Reads only the header + interp bytes, so
/// a huge file isn't slurped into memory. Static binaries have no PT_INTERP and
/// are rejected with a hint to choose a container explicitly.
fn detect_libc(path: &str) -> io::Result<Libc> {
    let interp = read_elf_interp(path)?;
    if interp.contains("ld-musl") {
        Ok(Libc::Musl)
    } else if interp.contains("ld-linux") || interp.contains("ld.so") {
        Ok(Libc::Glibc)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "could not classify interpreter {:?} for {}; pass `--in <container>` explicitly",
                interp, path
            ),
        ))
    }
}

/// Read the `PT_INTERP` interpreter string from an ELF file. Errors if the file
/// isn't ELF or has no interpreter (static binary). Handles ELF32/64 and both
/// endiannesses; reads only the header, program-header table, and interp.
fn read_elf_interp(path: &str) -> io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
    let mut f = std::fs::File::open(path)?;

    let mut e = [0u8; 64];
    f.read_exact(&mut e)
        .map_err(|_| bad(format!("{} is too small to be an ELF", path)))?;
    if &e[..4] != b"\x7fELF" {
        return Err(bad(format!(
            "{} is not an ELF binary; `--in auto` needs an ELF",
            path
        )));
    }
    let is64 = e[4] == 2; // EI_CLASS: 1=32-bit, 2=64-bit
    let le = e[5] != 2; // EI_DATA: 1=little, 2=big
    let u16a = |b: &[u8]| {
        let a = [b[0], b[1]];
        if le {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    };
    let u32a = |b: &[u8]| {
        let a = [b[0], b[1], b[2], b[3]];
        if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };
    let u64a = |b: &[u8]| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[..8]);
        if le {
            u64::from_le_bytes(a)
        } else {
            u64::from_be_bytes(a)
        }
    };

    // Program-header table location, entry size, count (offsets differ by class).
    let (phoff, phentsize, phnum) = if is64 {
        (u64a(&e[32..40]), u16a(&e[54..56]), u16a(&e[56..58]))
    } else {
        (u32a(&e[28..32]) as u64, u16a(&e[42..44]), u16a(&e[44..46]))
    };
    if phnum == 0 || phnum > 4096 {
        return Err(bad(format!(
            "{}: implausible program-header count {}",
            path, phnum
        )));
    }

    let mut ph = vec![0u8; phentsize as usize];
    for i in 0..phnum {
        f.seek(SeekFrom::Start(phoff + i as u64 * phentsize as u64))?;
        f.read_exact(&mut ph)
            .map_err(|_| bad(format!("{}: truncated program header", path)))?;
        let p_type = u32a(&ph[0..4]);
        if p_type != 3 {
            continue; // PT_INTERP == 3
        }
        let (p_offset, p_filesz) = if is64 {
            (u64a(&ph[8..16]), u64a(&ph[32..40]))
        } else {
            (u32a(&ph[4..8]) as u64, u32a(&ph[16..20]) as u64)
        };
        if p_filesz == 0 || p_filesz > 4096 {
            return Err(bad(format!(
                "{}: implausible interpreter length {}",
                path, p_filesz
            )));
        }
        let mut buf = vec![0u8; p_filesz as usize];
        f.seek(SeekFrom::Start(p_offset))?;
        f.read_exact(&mut buf)
            .map_err(|_| bad(format!("{}: truncated interpreter string", path)))?;
        let s = buf.split(|b| *b == 0).next().unwrap_or(&[]);
        return Ok(String::from_utf8_lossy(s).into_owned());
    }
    Err(bad(format!(
        "{} has no PT_INTERP (likely a static binary); pass `--in <container>` explicitly",
        path
    )))
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
    let code =
        hako_runtime::transform::run_container(&repo, &branch, args, &volumes, Network::Isolated)
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
            // Hide the workspace's own .hako/ (store + refs + identity key) from
            // the workload — the implicit mount is the workspace root, so without
            // this the container could read the store/key and, since this is a
            // real host mount, delete or rewrite the host repo. See issue #39.
            mask: vec![DOT_HAKO.to_string()],
        });
        all.append(&mut user_volumes);
        return Ok(all);
    }
    Ok(user_volumes)
}

pub fn ps(ctx: &Ctx<'_>, all: bool, json: bool) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    let instances = hako_runtime::instances::list(&runtime_root).map_err(runtime_to_io)?;
    let shown: Vec<_> = instances
        .into_iter()
        .filter(|i| all || i.is_running())
        .collect();

    if json {
        let arr: Vec<serde_json::Value> = shown
            .iter()
            .map(|i| {
                serde_json::json!({
                    "id": i.id,
                    "container": i.config.container,
                    "branch": i.config.branch,
                    "command": i.config.command,
                    "status": i.status(),
                    "running": i.is_running(),
                    "pid": i.pid,
                    "exit_code": i.exit_code,
                    "started_unix": i.config.started_unix,
                    "restart": i.config.restart.as_str(),
                    "restart_count": i.restart_count,
                    // The tree pinned at spawn — distinct from the branch tip,
                    // which may have moved (revert) since the instance started.
                    "pinned_root": i.config.pinned_root,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(ExitCode::SUCCESS);
    }

    // Human table. Size the ID/BRANCH/STATUS columns to the widest value so a
    // long id or branch can't shove the later columns out of alignment (the old
    // fixed 14/20/10 widths broke on long names). A supervised instance shows
    // its policy and how many times it has respawned.
    let statuses: Vec<String> = shown
        .iter()
        .map(|i| {
            let base = i.status();
            if i.config.restart.is_supervised() {
                format!(
                    "{} [restart={} ×{}]",
                    base,
                    i.config.restart.as_str(),
                    i.restart_count
                )
            } else {
                base
            }
        })
        .collect();
    let id_w = shown
        .iter()
        .map(|i| i.id.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());
    let br_w = shown
        .iter()
        .map(|i| i.config.branch.len())
        .max()
        .unwrap_or(0)
        .max("BRANCH".len());
    let st_w = statuses
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("STATUS".len());
    println!(
        "{:<id_w$} {:<br_w$} {:<st_w$} COMMAND",
        "ID", "BRANCH", "STATUS"
    );
    for (i, st) in shown.iter().zip(&statuses) {
        let cmd = if i.config.command.is_empty() {
            "(shell)".to_string()
        } else {
            i.config.command.join(" ")
        };
        println!(
            "{:<id_w$} {:<br_w$} {:<st_w$} {}",
            i.id, i.config.branch, st, cmd
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

    // Poll until the instance is truly done: not running AND an exit code
    // recorded. Checking both matters two ways — a workload whose process is gone
    // but hasn't recorded its exit yet is still followed to completion, and a
    // SUPERVISED instance (whose supervisor stays alive across restarts, so
    // is_running() holds while a per-attempt exit_code is on disk) keeps being
    // followed across each restart instead of stopping after the first attempt.
    loop {
        let drained_out = drain_from(&stdout_path, &mut stdout_pos, &mut io::stdout())?;
        let drained_err = drain_from(&stderr_path, &mut stderr_pos, &mut io::stderr())?;
        let inst = hako_runtime::instances::get(&runtime_root, &id);
        let done = match &inst {
            Ok(i) => !i.is_running() && i.exit_code.is_some(),
            Err(_) => true,
        };
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

pub fn stop(ctx: &Ctx<'_>, id: String, force: bool) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    hako_runtime::instances::stop(&runtime_root, &id, force).map_err(runtime_to_io)?;
    Ok(ExitCode::SUCCESS)
}

pub fn reap(ctx: &Ctx<'_>, id: String, force: bool) -> io::Result<ExitCode> {
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    hako_runtime::instances::remove(&runtime_root, &id, force).map_err(runtime_to_io)?;
    Ok(ExitCode::SUCCESS)
}

pub fn runtime_to_io(e: hako_runtime::RuntimeError) -> io::Error {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_host_env_bin_becomes_command0() {
        // The Windows bridge sends the (already WSL-translated) binary path via
        // $HAKO_RUN_HOST_BIN; it must land as command[0], ahead of program args.
        let out = resolve_run_host_command(
            vec!["--flag".to_string(), "arg".to_string()],
            Some("/mnt/c/Users/First Last/app".to_string()),
        );
        assert_eq!(out, vec!["/mnt/c/Users/First Last/app", "--flag", "arg"]);
    }

    #[test]
    fn run_host_without_env_is_unchanged() {
        let argv = vec!["/bin/app".to_string(), "--flag".to_string()];
        // Unset and empty both leave the argv form untouched (the guest then uses
        // command[0] as the path, as before).
        assert_eq!(resolve_run_host_command(argv.clone(), None), argv);
        assert_eq!(
            resolve_run_host_command(argv.clone(), Some(String::new())),
            argv
        );
    }
}
