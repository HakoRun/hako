use clap::{Parser, Subcommand};
use hako::{Config, State, WorkspaceLock};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

mod cmd;
mod diag;
mod helpers;
mod host_bridge;

use cmd::Ctx;

pub const DOT_HAKO: &str = ".hako";

#[derive(Parser)]
#[command(
    name = "hako",
    about = "Content-addressed version-controlled filesystem",
    version,
    arg_required_else_help = true,
    after_help = "EXTRAS:\n  \
        hako is <image>          Switch the workspace's identity to an image (e.g. `hako is alpine`).\n  \
        hako as <ctr> <cmd>...   Run a one-off command inside another container without switching.\n  \
        hako <anything> ...      Any unknown command runs inside the current container\n                           \
        (e.g. `hako python app.py`, `hako npm install`).\n\n\
        Run/exec need a Linux runtime; on Windows/macOS they bridge into a WSL2 distro / Lima VM."
)]
struct Cli {
    /// Workspace directory containing .hako (defaults to current dir).
    /// A context flag: must appear before the subcommand (e.g.
    /// `hako -w /path run ...`), so it never collides with a guest program's
    /// own `-w`/`--workdir` in `run`/`run-host`/`exec`.
    #[arg(short = 'w', long)]
    workdir: Option<PathBuf>,

    /// Default container, overriding the session identity for this one command.
    /// A context flag: must appear before the subcommand (e.g.
    /// `hako -c ubuntu ls /`), so it never collides with a guest program's own
    /// `-c` (use `hako is`/`as` for the durable/sugared forms).
    #[arg(short = 'c', long)]
    container: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

/// Subcommands of `hako peer` — the static cluster registry (docs/distributed.md).
#[cfg(feature = "cluster")]
#[derive(Subcommand)]
enum PeerCmd {
    /// Add (or update) a peer: its network address and Ed25519 public key
    Add {
        name: String,
        address: String,
        pubkey: String,
    },
    /// List configured peers
    List,
    /// Remove a peer
    Remove { name: String },
    /// Connect to a peer and verify it proves its registered identity
    Ping { name: String },
    /// Push a container branch to a peer (replicate it over the network)
    Push {
        node: String,
        #[arg(default_value = "main")]
        branch: String,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a new hako workspace
    Init { path: Option<PathBuf> },

    // ------------------------------------------------------------ Files
    /// Write a file from a source (- for stdin, --file for a path, otherwise inline arg)
    Write {
        path: String,
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        content: Option<String>,
    },
    /// Read a file to stdout. `path` may be `<ref>:<path>` to read from a commit/branch.
    Cat { path: String },
    /// Create a directory marker
    Mkdir { path: String },
    /// Delete a file or directory (recursive)
    Del { path: String },
    /// Copy
    Cp { src: String, dst: String },
    /// Move/rename
    Mv { src: String, dst: String },
    /// Import a host file or directory into the vfs.
    Import {
        /// Source path on the host filesystem
        src: PathBuf,
        /// Destination path in the vfs
        dst: String,
        /// Overwrite an existing file at the destination.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Export a vfs file or directory to the host. `src` may be `<ref>:<path>`.
    Export {
        src: String,
        dst: PathBuf,
        #[arg(short = 'f', long)]
        force: bool,
    },

    // ------------------------------------------------------------ Navigation
    /// List a directory. `path` may be `<ref>:<path>` to list from a commit/branch.
    Ls { path: Option<String> },
    /// Print the current container and working directory.
    Pwd,
    /// Change the working directory (and optionally the container via /containers/<name>/...).
    Cd { path: String },
    /// Recursive ASCII tree of a directory in WORKING (or another ref via `<ref>:<path>`).
    Tree {
        path: Option<String>,
        #[arg(short = 'd', long)]
        depth: Option<usize>,
    },
    /// Show working tree status
    Status {
        /// Emit machine-readable JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },

    // ------------------------------------------------------------ Version control
    /// Commit the working tree
    Commit {
        #[arg(short = 'm', long)]
        message: String,
        /// Commit author (default: $HAKO_AUTHOR, else "user")
        #[arg(short = 'a', long)]
        author: Option<String>,
    },
    /// Show commit history
    Log {
        /// Emit machine-readable JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// List, create, or delete branches. `start` may be a branch name or hash prefix.
    Branch {
        name: Option<String>,
        start: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
    },
    /// Switch to another branch (refuses if working tree differs from HEAD; use --force)
    Checkout {
        branch: String,
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Merge another branch into the current one (or --abort to reset working to HEAD)
    Merge {
        branch: Option<String>,
        /// Commit author (default: $HAKO_AUTHOR, else "user")
        #[arg(short = 'a', long)]
        author: Option<String>,
        #[arg(long)]
        abort: bool,
    },
    /// Diff working tree against HEAD (or specified refs). Refs may be branches or hash prefixes.
    Diff {
        from: Option<String>,
        to: Option<String>,
    },
    /// List, create, or delete tags. With no args, lists all tags.
    /// `start` defaults to HEAD.
    Tag {
        name: Option<String>,
        start: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
    },

    // ------------------------------------------------------------ Sync
    /// Copy a remote workspace's branch into the local store, updating a local ref.
    Fetch {
        remote: PathBuf,
        branch: String,
        /// Name for the local ref to create/update (default: the branch name)
        #[arg(long)]
        as_ref: Option<String>,
        /// Remote container to fetch the branch from (default: the local default container)
        #[arg(long)]
        from_container: Option<String>,
    },
    /// Push a local branch to a remote workspace, copying objects and updating its ref.
    Push {
        remote: PathBuf,
        branch: String,
        /// Name of the ref to update on the remote (default: the branch name)
        #[arg(long)]
        as_ref: Option<String>,
        /// Remote container to push into (default: the local default container)
        #[arg(long)]
        to_container: Option<String>,
    },

    // ------------------------------------------------------------ Containers (workspaces)
    /// List containers
    Containers {
        /// Emit machine-readable JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Create a new container
    NewContainer { name: String },
    /// Delete a container
    DelContainer { name: String },

    // ------------------------------------------------------------ Cluster (docs/distributed.md)
    /// Show this node's cluster identity (its Ed25519 public key)
    #[cfg(feature = "cluster")]
    Id,
    /// Manage cluster peers (the static .hako/peers.toml registry)
    #[cfg(feature = "cluster")]
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Run the node daemon: serve this node's cluster surface to peers
    #[cfg(feature = "cluster")]
    Serve {
        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1:7777")]
        addr: String,
        /// Allow binding a routable (non-loopback) address. The cluster channel
        /// is authenticated but not yet encrypted, so a remote bind must be a
        /// deliberate choice — use only on a trusted LAN/VPN.
        #[arg(long)]
        allow_remote: bool,
        /// Allow peers to trigger command execution on this node via
        /// `write /peers/<node>/containers/<name>/ctl "run …"`. Off by default:
        /// it grants any registered peer arbitrary command execution inside a
        /// container on this host. The version-control ctl verbs
        /// (commit/branch/tag) and replication stay available without it.
        #[arg(long)]
        allow_remote_run: bool,
    },

    // ------------------------------------------------------------ Mount
    /// Mount a tree (ref or working) as a read-only filesystem at `mountpoint`.
    /// Linux only. Blocks until unmounted.
    Mount {
        mountpoint: PathBuf,
        #[arg(long, default_value = "working")]
        from: String,
    },

    // ------------------------------------------------------------ OCI
    /// Pull an OCI image into a container and commit it. By default, the
    /// container is named after the image's repo basename (`alpine` →
    /// `alpine`, `ghcr.io/foo/bar` → `bar`); pass `--into <name>` to
    /// override. Creates the container if it doesn't exist.
    /// Examples: `hako pull busybox`, `hako pull ghcr.io/foo/bar:v1`.
    Pull {
        image: String,
        /// Container to pull into (defaults to image repo basename).
        #[arg(long)]
        into: Option<String>,
        #[arg(long)]
        per_layer: bool,
        #[arg(long, default_value = "linux")]
        os: String,
        /// OCI architecture (default: the host's — amd64 on x86_64, arm64 on aarch64)
        #[arg(long, default_value_t = cmd::oci::host_oci_arch().to_string())]
        arch: String,
    },

    // ------------------------------------------------------------ Identity
    /// Switch the workspace's identity to <container>: subsequent commands
    /// (`hako ls`, `hako cat`, etc.) operate on that container's filesystem
    /// until you switch again. Resets cwd to `/`. The headline pitch:
    /// `hako is alpine; hako ls /` shows alpine's filesystem.
    Is { branch: String },

    // ------------------------------------------------------------ Runtime
    /// Run a Linux container with <branch>'s tree as its rootfs.
    /// With no command: drops you into an interactive shell.
    /// With `-d`: detaches and prints the instance id.
    /// Examples:
    ///   `hako run alpine`              — interactive alpine shell
    ///   `hako run alpine ls /`         — run `ls /` once and exit
    ///   `hako run -d alpine sleep 100` — start in background
    /// Linux only. The workspace auto-mounts at /workspace.
    Run {
        branch: String,
        /// Detach and run in the background. Returns the instance id.
        #[arg(short = 'd', long)]
        detach: bool,
        /// Bind-mount HOST:CONTAINER[:ro]. Repeatable.
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// Skip the implicit workspace bind-mount at /workspace.
        #[arg(long)]
        no_workspace: bool,
        /// Pass the host display (X11/Wayland) into the container so a GUI app
        /// renders on the host desktop. Off by default — it exposes the host
        /// display socket to the workload, weakening isolation.
        #[arg(long)]
        display: bool,
        /// Command + args to run, passed through verbatim. Most guest flags
        /// pass through, but a first token that collides with one of hako run's
        /// own flags (`-d`, `-v`, `--no-workspace`, `--display`) is taken by
        /// hako — put `--` before the command to force everything through
        /// (e.g. `hako run alpine -- top -d`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Run a Linux executable from the HOST filesystem through hako.
    ///
    /// The host system directories are bind-mounted read-only (so a
    /// dynamically-linked binary resolves its loader + libraries) and the
    /// process runs under hako's namespaces + seccomp. Pass `--display` to
    /// render a GUI app on the host desktop. This is the "I downloaded a Linux
    /// app, just run it" path — a convenience sandbox, not a reproducible
    /// versioned container.
    /// Examples:
    ///   `hako run-host --display /usr/bin/xeyes`  — render a GUI app
    ///   `hako run-host ~/Downloads/app.bin`       — run a downloaded binary
    /// Network is isolated. Linux only (bridged from Windows/macOS).
    RunHost {
        /// Run the binary against a CONTAINER's filesystem instead of the
        /// host's — its libraries come from the container, so an Alpine/musl
        /// or other cross-distro binary works. Pass a container name, or
        /// `auto` to pick (and pull if missing) a base image from the binary's
        /// libc (musl → alpine, glibc → debian). Omit for host libraries.
        #[arg(long = "in", value_name = "CONTAINER")]
        in_container: Option<String>,
        /// Pass the host display (X11/Wayland) through so a GUI app renders on
        /// the host desktop. Off by default (weakens isolation).
        #[arg(long)]
        display: bool,
        /// The host executable to run, followed by its arguments. Everything
        /// here is passed through verbatim (the first token is the binary
        /// path, absolute or relative to cwd). Use `--` if the binary's own
        /// flags would otherwise look like hako flags.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// List runtime instances (running and exited).
    Ps {
        #[arg(short = 'a', long)]
        all: bool,
        /// Emit machine-readable JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Show the captured stdout/stderr of a runtime instance.
    Logs {
        id: String,
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Run a command inside an already-running instance's namespaces.
    /// Like `docker exec`. Linux only.
    Exec {
        /// Instance id (or unique prefix)
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Stop a runtime instance: SIGTERM by default (graceful), or `--force`
    /// for SIGKILL when a workload ignores SIGTERM. Unix only.
    Stop {
        id: String,
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Remove a runtime instance's state. Refuses if running unless --force.
    Reap {
        id: String,
        #[arg(long)]
        force: bool,
    },

    // ------------------------------------------------------------ Maintenance
    /// Garbage-collect unreachable objects from the chunk store.
    Gc {
        /// Report what would be deleted without touching disk.
        #[arg(long)]
        dry_run: bool,
    },
    /// Verify the object graph is intact. Exit 1 if problems found.
    Fsck,

    /// Package a container + command into a single self-contained executable
    /// that runs the app through hako — no prior hako install or workspace
    /// needed on the target. (First cut: a Unix self-extracting bundle; the
    /// native Windows `.exe` stub is WIP.)
    Bundle {
        /// Container to package.
        container: String,
        /// Output path for the bundle executable.
        #[arg(short = 'o', long, default_value = "app.hako")]
        output: PathBuf,
        /// Overwrite the output file if it already exists.
        #[arg(short = 'f', long)]
        force: bool,
        /// Record that the bundled app wants display passthrough (a GUI). This
        /// is only a request — whoever RUNS the bundle must consent by setting
        /// HAKO_DISPLAY=1; it never auto-grants display access to the host.
        #[arg(long)]
        display: bool,
        /// Command to run inside it (default: the container's interactive
        /// shell). Must come last; passed through verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },

    /// Pre-warm the host runtime: idempotently set up the WSL distro
    /// (Windows) / Lima VM (macOS) and inject the embedded Linux hako
    /// binary. No-op on Linux. Useful to do this once explicitly so the
    /// first real `hako run` / `hako apply` doesn't pay the setup cost.
    Bootstrap,

    /// Read hako.toml, ensure the configured container exists (pulling its
    /// image if needed), and execute any setup steps that haven't been run
    /// before. Each successful setup step becomes a commit on the
    /// container's branch. Re-running is fast — already-applied steps are
    /// skipped via a hash recorded in `.hako/applied`.
    Apply {
        /// Overlay this profile from `[profiles.<name>]` in hako.toml on
        /// top of the base config. Errors if no such profile exists.
        #[arg(short = 'p', long)]
        profile: Option<String>,
        /// Print what would happen without pulling, executing, or committing.
        #[arg(long)]
        dry_run: bool,
        /// Re-run all setup steps even if their hashes are recorded as applied.
        #[arg(long)]
        force: bool,
        /// Override the user inside the container.
        #[arg(long)]
        user: Option<String>,
        /// Override the workspace mount mode (none|ro|rw).
        #[arg(long)]
        workspace: Option<String>,
        /// Add or override env vars: `-e KEY=VALUE` (repeatable).
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Additional host env vars to forward into the container.
        #[arg(long = "env-pass")]
        env_pass: Vec<String>,
        /// Force autocommit on, overriding hako.toml.
        #[arg(long)]
        autocommit: bool,
        /// Force autocommit off, overriding hako.toml.
        #[arg(long, conflicts_with = "autocommit")]
        no_autocommit: bool,
    },

    /// Catch-all: any unknown subcommand becomes "run this command inside
    /// the current container's runtime." So `hako python script.py` (when
    /// the current container has python) is equivalent to
    /// `hako run <current> python script.py`. Auto-mounts the workspace
    /// at /workspace like the explicit `run` does. Linux only.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Whether a `cat`/`ls`/`tree` path targets a container's `proc/` meta surface
/// (`/containers/<name>/proc[/...]`), which reads live process state and so must
/// run on the Linux runtime. Only the absolute form is detected here — the
/// bridge decision runs before the session cwd is loaded, so a relative
/// `proc/...` while cd'd into a container isn't auto-bridged (a v1 limitation).
fn is_container_proc_path(path: &str) -> bool {
    matches!(
        hako::RouteTarget::parse(path),
        hako::RouteTarget::Container { path: sub, .. }
            if crate::cmd::proc_meta::proc_subpath(&sub).is_some()
    )
}

impl Cmd {
    /// Whether this command requires the Linux runtime (namespaces, FUSE,
    /// pivot_root). On non-Linux hosts these are auto-forwarded into the
    /// user's WSL/Lima env via `host_bridge`. Read-only commands stay native.
    fn needs_linux_runtime(&self) -> bool {
        match self {
            Cmd::Run { .. } | Cmd::RunHost { .. } | Cmd::Exec { .. } | Cmd::External(_) => true,
            // `apply --dry-run` parses hako.toml and prints the plan
            // without touching the runtime — no reason to pay the WSL/Lima
            // round-trip just to print 4 lines.
            Cmd::Apply { dry_run, .. } => !dry_run,
            // Reading a container's live processes (`/containers/<name>/proc/...`)
            // reads /proc on the kernel the container runs on, so it must bridge
            // like run/exec to work from Windows/macOS via WSL2/Lima.
            Cmd::Cat { path } => is_container_proc_path(path),
            Cmd::Ls { path: Some(path) } => is_container_proc_path(path),
            Cmd::Tree {
                path: Some(path), ..
            } => is_container_proc_path(path),
            _ => false,
        }
    }

    /// Whether this command needs the exclusive workspace lock around its
    /// dispatch. False for: read-only commands, runtime commands that fork/
    /// exec into long-running processes, and inspection of detached state.
    fn holds_workspace_lock(&self) -> bool {
        match self {
            // Read-only inspection — no lock needed.
            Cmd::Containers { .. }
            | Cmd::Cat { .. }
            | Cmd::Ls { .. }
            | Cmd::Pwd
            | Cmd::Tree { .. }
            | Cmd::Status { .. }
            | Cmd::Log { .. }
            | Cmd::Diff { .. }
            | Cmd::Export { .. }
            | Cmd::Fsck
            | Cmd::Ps { .. }
            | Cmd::Logs { .. }
            | Cmd::Stop { .. } => false,
            // Long-running: holding the lock would block every other CLI
            // invocation for the lifetime of the container/mount.
            Cmd::Mount { .. }
            | Cmd::Run { .. }
            | Cmd::RunHost { .. }
            | Cmd::Exec { .. }
            | Cmd::External(_) => false,
            // Branch / tag list mode (no name) is read-only.
            Cmd::Branch { name: None, .. } => false,
            Cmd::Tag { name: None, .. } => false,
            // Gc dry-run reports without mutating.
            Cmd::Gc { dry_run: true } => false,
            // Init was already handled before locking.
            Cmd::Init { .. } => false,
            // Cluster: identity/peers act on their own files (.hako/identity,
            // .hako/peers.toml), not the workspace refs this lock protects — and
            // `serve` is a long-running daemon, so holding the lock for its
            // lifetime would deadlock every other command (including `peer ping`
            // against it).
            #[cfg(feature = "cluster")]
            Cmd::Id | Cmd::Serve { .. } | Cmd::Peer { .. } => false,
            // fetch/push lock BOTH the local and remote workspaces themselves, in
            // a global order that refuses a self-sync. main must NOT pre-lock the
            // local workspace here, or the second acquire would self-deadlock when
            // local and remote resolve to the same path (#75).
            Cmd::Fetch { .. } | Cmd::Push { .. } => false,
            // Everything else is RMW on workspace state.
            _ => true,
        }
    }
}

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
            no_workspace,
            display,
            command,
        } => cmd::runtime::run(
            &ctx,
            branch,
            detach,
            volumes,
            no_workspace,
            display,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_paths_need_the_linux_runtime() {
        // A container `proc/` read is runtime-backed → must bridge to Linux.
        assert!(is_container_proc_path("/containers/alpine/proc"));
        assert!(is_container_proc_path("/containers/alpine/proc/1"));
        assert!(is_container_proc_path("/containers/alpine/proc/1/status"));
    }

    #[test]
    fn non_proc_paths_stay_native() {
        // The filesystem and the store-backed meta nodes are not bridged.
        assert!(!is_container_proc_path("/containers/alpine/root/etc/hosts"));
        assert!(!is_container_proc_path("/containers/alpine/status"));
        assert!(!is_container_proc_path("/containers/alpine/ctl"));
        assert!(!is_container_proc_path("/containers/alpine")); // the container dir
        assert!(!is_container_proc_path("/containers/alpine/procfs")); // not the proc node
        assert!(!is_container_proc_path("/etc/hosts")); // active-container fs path
        assert!(!is_container_proc_path("/containers")); // the container list
    }

    #[test]
    fn cmd_classification_routes_proc_reads() {
        // Cat/Ls/Tree pick up the proc-path classification; nothing else does.
        assert!(Cmd::Cat {
            path: "/containers/alpine/proc/1/status".into()
        }
        .needs_linux_runtime());
        assert!(!Cmd::Cat {
            path: "/containers/alpine/status".into()
        }
        .needs_linux_runtime());
        assert!(Cmd::Ls {
            path: Some("/containers/alpine/proc".into())
        }
        .needs_linux_runtime());
        assert!(!Cmd::Ls { path: None }.needs_linux_runtime());
    }
}
