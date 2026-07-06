use clap::Parser;
use hako::{Config, State, WorkspaceLock};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

mod args;
mod cmd;
mod diag;
mod helpers;
mod host_bridge;

#[cfg(feature = "cluster")]
use args::PeerCmd;
use args::{Cli, Cmd};
use cmd::Ctx;

pub const DOT_HAKO: &str = ".hako";

fn main() -> ExitCode {
    // If this binary is a self-contained bundle (a hako binary with a payload
    // appended), run the baked container command instead of the normal CLI.
    match cmd::bundle::maybe_run_as_bundle() {
        Ok(Some(code)) => return code,
        Ok(None) => {}
        Err(e) => {
            crate::diag!("bundle: {}", e);
            return ExitCode::FAILURE;
        }
    }
    match run() {
        Ok(code) => code,
        Err(e) => {
            crate::diag!("{}", e);
            ExitCode::FAILURE
        }
    }
}

/// Pull the leading `-w/-c` global flags off `args` (in any of `-w X`,
/// `--workdir X`, `--workdir=X` forms; same for `-c`/`--container`) and
/// return them alongside the unconsumed remainder. Used when retrying
/// dispatch after a clap UnknownArgument failure: we still want the
/// user's `-w` and `-c` to take effect, but the rest needs to flow into
/// External as the command to run.
fn strip_globals(args: &[String]) -> (Option<PathBuf>, Option<String>, Vec<String>) {
    let mut workdir: Option<PathBuf> = None;
    let mut container: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-w" || a == "--workdir" {
            if i + 1 >= args.len() {
                break;
            }
            workdir = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if a == "-c" || a == "--container" {
            if i + 1 >= args.len() {
                break;
            }
            container = Some(args[i + 1].clone());
            i += 2;
        } else if let Some(v) = a.strip_prefix("--workdir=") {
            workdir = Some(PathBuf::from(v));
            i += 1;
        } else if let Some(v) = a.strip_prefix("--container=") {
            container = Some(v.to_string());
            i += 1;
        } else if a == "--" {
            // Explicit end-of-options: everything after `--` is the guest
            // command, passed through verbatim. Drop the `--` itself.
            return (workdir, container, args[i + 1..].to_vec());
        } else {
            return (workdir, container, args[i..].to_vec());
        }
    }
    (workdir, container, vec![])
}

/// Rewrite `hako as <container> <args...>` into `hako -c <container> <args...>`
/// before clap sees it. `as` is a one-off identity prefix — it isn't a
/// real subcommand, just sugar for `-c`.
///
/// Conservatively only triggers when `as` is the first non-flag token; any
/// global flags (-w, etc.) before it are preserved.
fn rewrite_as(args: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    if let Some(prog) = iter.next() {
        out.push(prog);
    }
    let mut transformed = false;
    while let Some(a) = iter.next() {
        if !transformed && a == "as" {
            // Need a container name to follow.
            match iter.next() {
                Some(container) => {
                    out.push("-c".into());
                    out.push(container);
                    transformed = true;
                }
                None => {
                    // Bare `as` with no name — let clap surface the error.
                    out.push(a);
                }
            }
        } else {
            out.push(a);
        }
    }
    out
}

fn run() -> io::Result<ExitCode> {
    let raw: Vec<String> = std::env::args().collect();
    let rewritten = rewrite_as(raw);
    // If clap rejects a flag on a known subcommand (e.g. `hako ls -l /` —
    // hako's `ls` doesn't have `-l`), the user almost certainly meant the
    // container's binary with the natural flag set. Forward all the post-
    // global args to External so it runs inside the current container.
    // Reference (`hako-reference/src/main.rs:247`) does the same trick.
    let cli = match Cli::try_parse_from(&rewritten) {
        Ok(c) => c,
        Err(e) if e.kind() == clap::error::ErrorKind::UnknownArgument => {
            let (workdir, container, rest) = strip_globals(&rewritten[1..]);
            if rest.is_empty() {
                e.exit();
            }
            Cli {
                workdir,
                container,
                cmd: Cmd::External(rest),
            }
        }
        Err(e) => e.exit(),
    };
    let cwd = std::env::current_dir()?;

    if let Cmd::Init { path } = &cli.cmd {
        // For init, an explicit -w wins; otherwise initialize at cwd.
        // (Don't auto-discover here — the user is creating a new workspace.)
        let target = cli.workdir.clone().unwrap_or_else(|| cwd.clone());
        return init(&target, path.clone());
    }

    // Bootstrap doesn't touch a workspace — it's a host-platform setup
    // command. Run it before workspace discovery.
    if matches!(cli.cmd, Cmd::Bootstrap) {
        return host_bootstrap();
    }

    // Cross-platform host bridge: runtime ops on Win/Mac forward to the
    // user's Linux hako (inside WSL or Lima). Read-only commands stay
    // native here on the host. Skipped on Linux or when HAKO_NO_BRIDGE
    // is set.
    if cli.cmd.needs_linux_runtime() && host_bridge::should_bridge() {
        return host_bridge::forward();
    }

    // For every other command: if -w was passed, use it verbatim. Otherwise
    // fall back to $HAKO_WORKDIR (the host bridge sets this, via WSLENV path
    // translation, so a Windows workspace path with spaces survives the
    // wsl.exe boundary). Otherwise walk up from cwd looking for `.hako/`, like
    // git looks for `.git/`.
    let workdir = match cli.workdir.clone() {
        Some(w) => w,
        None => match std::env::var_os("HAKO_WORKDIR") {
            Some(w) if !w.is_empty() => PathBuf::from(w),
            _ => find_workspace_root(&cwd)?,
        },
    };

    let dot = workdir.join(DOT_HAKO);
    let state = State::open(&dot)?;
    let cfg = Config::load(&workdir)?;
    let session = state.read_session()?;
    // Precedence: -c flag > session container (only if SESSION file exists) > config default.
    let default_container = cli
        .container
        .clone()
        .or_else(|| {
            state
                .session_path_exists()
                .then(|| session.container.clone())
        })
        .unwrap_or_else(|| cfg.default_container.clone());

    // Auto-bootstrap on explicit container override (-c X / `as X cmd`):
    // if the named container doesn't exist, treat the name as an OCI image
    // ref and pull it. Same "do whatever it takes" intent as `hako is X`,
    // applied to the one-off form. Only fires when -c was explicitly given,
    // not when default_container is coming from session/config.
    //
    // This is a *mutating* operation (create container + write refs), so it
    // must be serialized even when the command itself is read-only or
    // long-running (which skip the command lock below). We take a short-lived
    // lock here, re-check existence under it (another process may have just
    // created the container), pull, then release before dispatch — so a
    // long-running `run`/`exec` doesn't hold the lock for its whole lifetime.
    if let Some(explicit) = &cli.container {
        if !state.list_containers()?.iter().any(|c| c == explicit) {
            let _pull_lock = WorkspaceLock::acquire(&dot)?;
            // Re-check under the lock to avoid a TOCTOU double-pull / create
            // race between concurrent invocations.
            if !state.list_containers()?.iter().any(|c| c == explicit) {
                let image_ref = hako::ImageRef::parse(explicit).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("no container {} and not a valid image ref: {}", explicit, e),
                    )
                })?;
                cmd::oci::pull_into(
                    &state,
                    &image_ref,
                    explicit,
                    "linux",
                    cmd::oci::host_oci_arch(),
                    false,
                )?;
            }
        }
    }

    // Acquire the workspace lock around any operation that mutates workspace
    // state. Skipped for long-running commands (mount, runtime is/as/spawn)
    // and for read-only inspection commands; those would either deadlock
    // other invocations for hours or don't need serialization at all.
    let _lock: Option<WorkspaceLock> = if cli.cmd.holds_workspace_lock() {
        Some(WorkspaceLock::acquire(&dot)?)
    } else {
        None
    };

    let ctx = Ctx {
        state: &state,
        session: &session,
        default_container: &default_container,
        workdir: &workdir,
        cfg: &cfg,
    };

    match cli.cmd {
        Cmd::Init { .. } => unreachable!(),

        // Containers (workspaces)
        Cmd::Containers { json } => {
            let names = state.list_containers()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&names)?);
            } else {
                for c in names {
                    println!("{}", c);
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        // Cluster (docs/distributed.md)
        #[cfg(feature = "cluster")]
        Cmd::Id => cmd::identity::show(&ctx),
        #[cfg(feature = "cluster")]
        Cmd::Peer { cmd } => match cmd {
            PeerCmd::Add {
                name,
                address,
                pubkey,
            } => cmd::peers::add(&ctx, name, address, pubkey),
            PeerCmd::List => cmd::peers::list(&ctx),
            PeerCmd::Remove { name } => cmd::peers::remove(&ctx, name),
            PeerCmd::Ping { name } => cmd::serve::ping(&ctx, &name),
            PeerCmd::Push { node, branch } => cmd::serve::remote_push(&ctx, &node, &branch),
            PeerCmd::Fetch { node, branch } => cmd::serve::remote_fetch(&ctx, &node, &branch),
        },
        #[cfg(feature = "cluster")]
        Cmd::Serve {
            addr,
            allow_remote,
            allow_remote_run,
        } => cmd::serve::serve(&ctx, &addr, allow_remote, allow_remote_run),
        Cmd::NewContainer { name } => {
            state.create_container(&name)?;
            println!("created container {}", name);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::DelContainer { name } => {
            // Refuse to delete the active default container — it would
            // leave the session pointing at a missing container, which
            // breaks every subsequent command with a confusing error.
            if name == default_container {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to delete the active container {} \
                         (cd into another container or pass -c <other> first)",
                        name
                    ),
                ));
            }
            // Also refuse to delete the only container.
            let containers = state.list_containers()?;
            if containers.len() == 1 && containers.iter().any(|c| c == &name) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to delete the only container in the workspace",
                ));
            }
            if !state.delete_container(&name)? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such container: {}", name),
                ));
            }
            println!("deleted container {}", name);
            Ok(ExitCode::SUCCESS)
        }

        // Files
        Cmd::Write {
            path,
            file,
            content,
        } => cmd::files::write(&ctx, path, file, content),
        Cmd::Cat { path } => cmd::files::cat(&ctx, path),
        Cmd::Mkdir { path } => cmd::files::mkdir(&ctx, path),
        Cmd::Del { path } => cmd::files::del(&ctx, path),
        Cmd::Cp { src, dst } => cmd::files::cp(&ctx, src, dst),
        Cmd::Mv { src, dst } => cmd::files::mv(&ctx, src, dst),
        Cmd::Import { src, dst, force } => cmd::files::import(&ctx, src, dst, force),
        Cmd::Export { src, dst, force } => cmd::files::export(&ctx, src, dst, force),

        // Navigation
        Cmd::Ls { path } => cmd::nav::ls(&ctx, path),
        Cmd::Pwd => cmd::nav::pwd(&ctx),
        Cmd::Cd { path } => cmd::nav::cd(&ctx, path),
        Cmd::Tree { path, depth } => cmd::nav::tree(&ctx, path, depth),
        Cmd::Status { json } => cmd::nav::status(&ctx, json),

        // VC
        Cmd::Commit { message, author } => cmd::vc::commit(&ctx, message, author),
        Cmd::Log { json } => cmd::vc::log(&ctx, json),
        Cmd::Branch {
            name,
            start,
            delete,
        } => cmd::vc::branch(&ctx, name, start, delete),
        Cmd::Checkout { branch, force } => cmd::vc::checkout(&ctx, branch, force),
        Cmd::Merge {
            branch,
            author,
            abort,
        } => cmd::vc::merge(&ctx, branch, author, abort),
        Cmd::Revert { refspec, author } => cmd::vc::revert(&ctx, refspec, author),
        Cmd::Diff { from, to } => cmd::vc::diff(&ctx, from, to),
        Cmd::Tag {
            name,
            start,
            delete,
        } => cmd::vc::tag(&ctx, name, start, delete),

        // Sync
        Cmd::Fetch {
            remote,
            branch,
            as_ref,
            from_container,
        } => cmd::sync::fetch(&ctx, remote, branch, as_ref, from_container),
        Cmd::Push {
            remote,
            branch,
            as_ref,
            to_container,
        } => cmd::sync::push(&ctx, remote, branch, as_ref, to_container),

        // OCI
        Cmd::Pull {
            image,
            into,
            per_layer,
            os,
            arch,
        } => cmd::oci::pull(&ctx, image, per_layer, os, arch, into),

        // Mount (Linux only — macOS/Windows bridge runtime ops into a Linux VM)
        #[cfg(target_os = "linux")]
        Cmd::Mount { mountpoint, from } => cmd::mount::mount(&ctx, mountpoint, from),
        #[cfg(not(target_os = "linux"))]
        Cmd::Mount { .. } => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hako mount uses FUSE and is only supported natively on Linux",
        )),

        // Identity
        Cmd::Is { branch } => cmd::nav::switch_identity(&ctx, branch),

        // Runtime
        Cmd::Run {
            branch,
            detach,
            volumes,
            network,
            restart,
            no_workspace,
            display,
            command,
        } => cmd::runtime::run(
            &ctx,
            branch,
            cmd::runtime::RunOpts {
                detach,
                volumes,
                network,
                restart,
                no_workspace,
                display,
            },
            command,
        ),
        Cmd::RunHost {
            in_container,
            display,
            command,
        } => cmd::runtime::run_host(&ctx, in_container, display, command),
        Cmd::Bundle {
            container,
            output,
            force,
            display,
            cmd,
        } => cmd::bundle::create(&ctx, container, cmd, output, force, display),
        Cmd::Ps { all, json } => cmd::runtime::ps(&ctx, all, json),
        Cmd::Logs { id, follow } => cmd::runtime::logs(&ctx, id, follow),
        Cmd::Exec { id, command } => cmd::runtime::exec(&ctx, id, command),
        Cmd::Stop { id, force } => cmd::runtime::stop(&ctx, id, force),
        Cmd::Reap { id, force } => cmd::runtime::reap(&ctx, id, force),

        // Maintenance
        Cmd::Gc { dry_run } => cmd::maintenance::gc(&ctx, dry_run),
        Cmd::Fsck => cmd::maintenance::fsck(&ctx),

        // Application config
        Cmd::Apply {
            profile,
            dry_run,
            force,
            user,
            workspace,
            env,
            env_pass,
            autocommit,
            no_autocommit,
        } => {
            // Re-load config with the user-supplied profile applied.
            let cfg = hako::Config::load_with_profile(&workdir, profile.as_deref())?;
            // Build CLI overrides.
            let workspace_mode = match workspace.as_deref() {
                None => None,
                Some("none") => Some(hako::WorkspaceMode::None),
                Some("ro") => Some(hako::WorkspaceMode::Ro),
                Some("rw") => Some(hako::WorkspaceMode::Rw),
                Some(other) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--workspace must be none|ro|rw, got {:?}", other),
                    ));
                }
            };
            let env_pairs: Result<Vec<_>, _> = env
                .iter()
                .map(|e| match e.split_once('=') {
                    Some((k, v)) => Ok((k.to_string(), v.to_string())),
                    None => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--env must be KEY=VALUE, got {:?}", e),
                    )),
                })
                .collect();
            let overrides = hako::AppOverrides {
                user,
                workspace: workspace_mode,
                env: env_pairs?,
                env_pass,
                autocommit: if autocommit {
                    Some(true)
                } else if no_autocommit {
                    Some(false)
                } else {
                    None
                },
            };
            cmd::apply::apply(&ctx, &cfg, &overrides, dry_run, force)
        }

        // Catch-all: route unknown subcommands to runtime exec in the
        // current container.
        Cmd::External(args) => cmd::runtime::external(&ctx, args),

        // Bootstrap is a host-platform op; doesn't touch the workspace.
        Cmd::Bootstrap => unreachable!("handled before workspace open"),
    }
}

/// Walk up from `start` looking for a directory containing `.hako/`. Returns
/// the deepest such directory (the workspace root). Errors with NotFound if
/// none is found anywhere up to the filesystem root.
fn find_workspace_root(start: &std::path::Path) -> io::Result<PathBuf> {
    let mut cursor: &std::path::Path = start;
    loop {
        if cursor.join(DOT_HAKO).is_dir() {
            return Ok(cursor.to_path_buf());
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no hako workspace at {} or any parent directory \
                         (run `hako init` first, or pass -w <path>)",
                        start.display()
                    ),
                ));
            }
        }
    }
}

fn host_bootstrap() -> io::Result<ExitCode> {
    if cfg!(target_os = "linux") {
        crate::diag!("nothing to bootstrap (already on Linux)");
        return Ok(ExitCode::SUCCESS);
    }
    host_bridge::ensure_runtime()?;
    if !host_bridge::has_embedded_binary() {
        crate::diag!(
            "bootstrap done (no embedded Linux binary; \
             expecting hako installed inside the WSL/Lima env)"
        );
    } else {
        crate::diag!("runtime ready");
    }
    Ok(ExitCode::SUCCESS)
}

fn init(workdir: &std::path::Path, path: Option<PathBuf>) -> io::Result<ExitCode> {
    let target = path.unwrap_or_else(|| workdir.to_path_buf());
    let dot = target.join(DOT_HAKO);
    let cfg = Config::load(&target)?;
    State::init(&dot)?;
    println!("initialized hako workspace at {}", target.display());
    if let Some(app) = &cfg.app {
        println!("found hako.toml: image={}, name={}", app.image, app.name);
        println!("run `hako apply` to pull the image and execute setup steps");
    }
    Ok(ExitCode::SUCCESS)
}
