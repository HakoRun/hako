//! The CLI argument surface: the clap model (`Cli` / `Cmd` / `PeerCmd`), command
//! classification (`needs_linux_runtime`, `holds_workspace_lock`), and the
//! container-`proc` path check. `main.rs` owns dispatch; this owns "what the user
//! typed".

use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
pub(crate) struct Cli {
    /// Workspace directory containing .hako (defaults to current dir).
    /// A context flag: must appear before the subcommand (e.g.
    /// `hako -w /path run ...`), so it never collides with a guest program's
    /// own `-w`/`--workdir` in `run`/`run-host`/`exec`.
    #[arg(short = 'w', long)]
    pub(crate) workdir: Option<PathBuf>,

    /// Default container, overriding the session identity for this one command.
    /// A context flag: must appear before the subcommand (e.g.
    /// `hako -c ubuntu ls /`), so it never collides with a guest program's own
    /// `-c` (use `hako is`/`as` for the durable/sugared forms).
    #[arg(short = 'c', long)]
    pub(crate) container: Option<String>,

    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

/// Subcommands of `hako peer` — the static cluster registry (docs/distributed.md).
#[cfg(feature = "cluster")]
#[derive(Subcommand)]
pub(crate) enum PeerCmd {
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
    /// Fetch a container branch from a peer (pull it over the network)
    Fetch {
        node: String,
        #[arg(default_value = "main")]
        branch: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
    /// Roll back: commit an older tree on top of the current tip. This is the only
    /// rollback the fast-forward-only push protocol permits, and history records it.
    /// `refspec` is a branch, tag, or commit-hash prefix.
    Revert {
        refspec: String,
        /// Commit author (default: $HAKO_AUTHOR, else "user")
        #[arg(short = 'a', long)]
        author: Option<String>,
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
        /// is encrypted and peer-authenticated (Noise IK), but making this node
        /// reachable off-host must be a deliberate choice — use only on a
        /// trusted LAN/VPN.
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
        #[arg(long, default_value_t = crate::cmd::oci::host_oci_arch().to_string())]
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
        /// Networking: `none` (default — fully isolated, the workload has no
        /// connectivity) or `host` (share the host network: the workload can
        /// listen on and connect from host ports, at the cost of network
        /// isolation). Rootless port publishing (`-p`) is a planned follow-up.
        #[arg(long, value_parser = ["none", "host"], default_value = "none")]
        network: String,
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
        /// own flags (`-d`, `-v`, `--network`, `--no-workspace`, `--display`)
        /// is taken by hako — put `--` before the command to force everything
        /// through (e.g. `hako run alpine -- top -d`).
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
        /// Overlay this profile (a top-level `[<name>]` table in hako.toml)
        /// on top of the base config. Errors if no such profile exists.
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
    pub(crate) fn needs_linux_runtime(&self) -> bool {
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
    pub(crate) fn holds_workspace_lock(&self) -> bool {
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

    #[test]
    fn run_network_flag_defaults_parses_and_rejects() {
        // Default: isolated ("none").
        let cli = Cli::try_parse_from(["hako", "run", "alpine"]).unwrap();
        let Cmd::Run { network, .. } = cli.cmd else {
            panic!("expected Run");
        };
        assert_eq!(network, "none");
        // Explicit host mode (context flag: before the branch positional).
        let cli =
            Cli::try_parse_from(["hako", "run", "--network", "host", "alpine", "true"]).unwrap();
        let Cmd::Run {
            network, command, ..
        } = cli.cmd
        else {
            panic!("expected Run");
        };
        assert_eq!(network, "host");
        assert_eq!(command, vec!["true"]);
        // Anything else is rejected at parse time.
        assert!(Cli::try_parse_from(["hako", "run", "--network", "bridge", "alpine"]).is_err());
    }
}
