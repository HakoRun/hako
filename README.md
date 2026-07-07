# hako

**Content-addressed, version-controlled filesystem with a built-in container runtime.**

Every container in hako has a [prolly-tree](https://docs.dolthub.com/architecture/storage-engine/prolly-tree)
filesystem backed by a shared, content-addressed chunk store. All changes are
version-controlled automatically — you `commit`, `branch`, `merge`, `diff`, and
roll back container state exactly like source code. Pull real OCI images, switch
between them with `hako is`, and run them with namespace isolation on Linux (or
through a WSL2 / Lima bridge on Windows and macOS).

```sh
hako init
hako pull alpine                 # pull a real OCI image into a container
hako is alpine                   # switch the workspace identity to alpine
hako ls /                        # browse alpine's filesystem — no mount, instant
hako write /etc/motd "hello"     # edit the versioned filesystem
hako commit -m "set motd"        # snapshot it
hako run alpine sh               # run it for real (Linux / WSL2 / Lima)
```

## Why hako

- **Content-addressed storage** — BLAKE3-hashed chunks with structural sharing
  across containers and history. Identical data is stored once.
- **Real version control for filesystems** — commits, branches, three-way
  `merge` with conflict detection, `diff`, `tag`, and history for *every*
  container, powered by a deterministic prolly tree.
- **OCI images** — `hako pull` fetches from Docker Hub and other registries and
  commits the image as the container's first snapshot.
- **Instant, cross-platform inspection** — `ls`, `cat`, `tree`, `write`, and
  friends read and write the prolly tree directly. No FUSE mount, no isolation,
  no platform-specific code — they work identically on Linux, Windows, and macOS.
- **Container runtime** — `hako run` is the execution boundary: FUSE serves the
  tree as a real filesystem, namespaces provide isolation. Native on Linux;
  transparently bridged into WSL2 (Windows) or Lima (macOS).
- **Declarative config** — a `hako.toml` defines the image, setup steps, run
  command, workspace mode, env, and named profiles; `hako apply` materializes
  it, each setup step becoming a commit.
- **Distributed** — `fetch` and `push` copy branches between workspaces,
  transferring only the chunks the remote is missing. With the opt-in cluster
  build, `hako serve` turns a node into a network peer: replicate containers
  with `peer push`/`peer fetch` and control remote containers by reading and
  writing their files, over a Noise-encrypted, mutually-authenticated channel.

## Install / build

The quickest way to get hako is the install script (Linux / macOS):

```sh
curl -fsSL https://hako.run/install.sh | sh
```

Or grab a binary from the [latest release](https://github.com/HakoRun/hako/releases/latest).
On Windows/macOS the binary auto-bootstraps a WSL2 distro / Lima VM on first
runtime command.

### From source

Requires a recent stable Rust toolchain (developed against 1.96). No system
libraries are needed — hako mounts FUSE via `mount(2)` directly, so there's no
libfuse/pkg-config build dependency.

```sh
# Native CLI + runtime (full runtime on Linux; CLI + host bridge on Win/Mac)
cargo build --workspace --release
# binary: target/release/hako
```

(The `hako mount` browse command shells out to `fusermount3` at runtime for an
unprivileged mount; the core `run`/`apply` runtime needs nothing extra.)

For the Windows/macOS auto-bootstrap build that embeds a cross-compiled Linux
binary, see [BUILD.md](BUILD.md).

## Command reference

| Area | Commands |
|------|----------|
| **Workspace** | `init` |
| **Files** | `write`, `cat`, `mkdir`, `del`, `cp`, `mv`, `import`, `export` |
| **Navigation** | `ls`, `pwd`, `cd`, `tree`, `status`, `mount` (FUSE browse) |
| **Version control** | `commit`, `log`, `branch`, `checkout`, `merge`, `revert`, `diff`, `tag` |
| **Containers** | `containers`, `new-container`, `del-container`, `is`, `as` |
| **OCI** | `pull` |
| **Runtime** (Linux / bridged) | `run`, `run-host`, `exec`, `ps`, `logs`, `stop`, `reap` |
| **Packaging** | `bundle` (container → single self-contained executable) |
| **Sync** | `fetch`, `push` |
| **Cluster** (`--features cluster`) | `id`, `peer add\|list\|remove\|ping\|push\|fetch`, `serve` |
| **Config** | `apply` (reads `hako.toml`) |
| **Maintenance** | `gc`, `fsck`, `bootstrap` |

Any *unknown* subcommand runs inside the current container's runtime:
`hako python app.py` is `hako run <current> python app.py` (with the workspace
mounted at `/workspace`). `hako as <container> <cmd>…` does the same in another
container without switching to it.

`hako run -d` detaches a workload and prints its instance id; `ps`, `logs`,
`exec`, and `stop` manage it. Add `--restart on-failure|always` to have the
supervising process re-launch it on exit (bounded backoff) — it re-launches the
**tree pinned at spawn**, so a later `revert` can't quietly swap the running
tree. `--network host` gives the workload the host network (default is isolated).

`run-host <path>` runs a Linux executable straight from the host filesystem
through hako's sandbox (add `--display` for a GUI app); `bundle <container>`
packages a container plus its command into one runnable file that needs no
prior hako install on the target.

`cat`, `ls`, `export`, and `tree` accept a `<ref>:<path>` argument to read from
any commit, branch, or tag (e.g. `hako cat main:/etc/hosts`). Run
`hako <command> --help` for full details.

### Identity: `hako is`

`hako is <container>` switches the workspace's active identity. Subsequent
commands (`ls`, `cat`, `commit`, …) operate on that container's filesystem until
you switch again. It is a metadata operation — instant, no shell, no mount — so
`hako is alpine; hako ls /` shows alpine's root filesystem immediately.

## hako.toml

```toml
image = "python:3.12-slim"
name  = "myproject"

setup = ["pip install -r /workspace/requirements.txt"]
run   = "python -m myapp"

# Workspace bind-mount mode: "rw" (default), "ro", or "none".
workspace  = "rw"
env        = { LOG_LEVEL = "info" }
env_pass   = ["OPENAI_API_KEY"]   # host env vars to forward in
autocommit = false                # snapshot the tree after each exec

# Named profiles overlay the base config: `hako apply --profile prod`.
[prod]
workspace = "none"

[ci]
autocommit = false
```

`hako apply` ensures the container exists (pulling the image if needed) and runs
each not-yet-applied setup step, recording a commit per step. Re-running is fast:
already-applied steps are skipped via a hash recorded in `.hako/applied`.

Any other top-level table (`[prod]`, `[ci]`, …) is a profile — except `[deploy]`,
which is reserved: it declares this node's push-to-deploy target (see
[docs/push-to-deploy.md](docs/push-to-deploy.md)) and is never a profile.

> **Isolation.** `hako run` runs the container rootless in user, mount, PID,
> IPC, UTS, and network namespaces, with a fresh procfs, a private `/tmp`, no
> host `$HOME`, private mount propagation, and a `pivot_root` rootfs (writable;
> writes are ephemeral) — similar in posture to rootless Podman. The workload
> runs under a minimal **PID-1 init** that reaps orphaned processes and forwards
> `SIGTERM`/`SIGINT`, so `hako stop` shuts it down cleanly; `hako exec` enters
> all of the container's namespaces. Network is **isolated by default for `run`**
> (`--network host` opts into the host network so a workload can serve or make
> connections; rootless port publishing is planned); `apply` keeps host
> networking so setup steps can install dependencies. The workload runs under a **seccomp filter** that
> blocks dangerous syscalls (module loading, kexec/reboot, mount, kernel keyring,
> bpf, …), `/sys` is a fresh read-only sysfs, and — where a delegated cgroup v2
> is available (systemd host) — a **cgroup `pids`/`memory` limit** (best-effort,
> like rootless Podman). It is not yet a hardened multi-tenant sandbox, so prefer
> trusted images for now. (`hako run` requires a Linux runtime; on Windows/macOS
> it is bridged into WSL2 / Lima.)

## Cluster (experimental)

Built with `--features cluster`, hako nodes can talk to each other over the
network. Each node has a stable Ed25519 identity (`hako id`); peers are
registered explicitly by name, address, and public key; and all traffic runs
over a Noise-encrypted, mutually-authenticated channel:

```sh
hako serve                        # on the node: run the daemon (loopback by default)
hako peer add prod 10.0.0.5:7777 <pubkey>
hako peer ping prod               # reachability + identity check
hako peer push prod main          # replicate a branch (only missing chunks travel)
hako peer fetch prod main         # ...and the pull half
hako cat /peers/prod/containers/app/status    # read a remote container's status
hako write /peers/prod/containers/app/ctl "commit -m nightly"   # remote control verb
```

Trust is deliberately conservative: `serve` binds loopback unless you pass
`--allow-remote` (intended for a trusted LAN/VPN), remote ref updates are
fast-forward-only, and peer-triggered command execution requires
`--allow-remote-run`. Design notes: [docs/distributed.md](docs/distributed.md)
and [docs/push-to-deploy.md](docs/push-to-deploy.md).

**Push-to-deploy.** A node with a `[deploy]` table and `hako serve
--allow-deploy` reacts to a push that advances the tracked branch by
reconciling the workload — stop the old instance, start the new one at the
just-pushed tree, kept up with `restart = always`. You declare the *entrypoint*
on the receiver (`[deploy].run`); the pusher supplies the *filesystem* it runs
against — so `--allow-deploy` is a code-execution grant (for an interpreted
runtime the pushed tree is the code), on par with `--allow-remote-run`. Enable
it only for peers you'd let run code on the host. `hako peer push` prints the
deploy log; `hako revert` + re-push rolls back.

```toml
# on the deploy node's hako.toml
[deploy]
container = "app"
branch    = "main"
run       = "python -m myapp"
network   = "host"       # serve on host ports (rootless -p publishing is WIP)
```

## Architecture

A Cargo workspace of four crates:

| Crate | Responsibility |
|-------|----------------|
| **hako-core** | The engine: `store` (BLAKE3 chunk store), `tree` (prolly tree: ops, cursor, diff, merge, set-ops), `fs` (ScopedFs — directories as prolly trees), `repo` (commit DAG), `oci` (registry pull + layer apply), `rootfs`, `fuse`, `state`, `config`, `maintenance` (gc/fsck) |
| **hako-cli** | The `hako` binary: command dispatch, `host_bridge` (WSL2 / Lima forwarding), and the `cmd/*` handlers |
| **hako-runtime** | Linux container instances: namespace setup, supervision, lifecycle |
| **xtask** | Build automation (cross-compiles the static Linux binary the host wrappers embed) |

### The `.hako` workspace

`hako init` creates a `.hako/` directory at the workspace root (discovered by
walking up from the cwd, like `.git/`). It holds the content-addressed chunk
store, container/branch refs, and session state. A fresh workspace is seeded with
a working toybox-based rootfs, so `hako ls /` shows a usable Linux-like
filesystem from the first commit.

### Cross-platform runtime

Read/write and version-control commands are pure prolly-tree operations and run
natively on every platform. Only `run`/`exec` need a Linux kernel:

- **Linux** — native FUSE + user/mount namespaces + `pivot_root`, rootless.
- **Windows** — runtime ops forward into a private WSL2 distro via the host bridge.
- **macOS** — same pattern via a Lima VM.

See [BUILD.md](BUILD.md) for the bridge and embedded-binary details, and the
`HAKO_DISTRO` / `HAKO_LIMA_VM` / `HAKO_NO_BRIDGE` environment knobs.

### Runtime tuning (env)

| Variable | Effect |
| --- | --- |
| `HAKO_PIDS_MAX` | Container process cap (`pids.max`). Default `1024`; `0`/`max` = unlimited. Requires a delegated cgroup v2. |
| `HAKO_MEMORY_MAX` | Container memory cap (`memory.max`), e.g. `512M`, `2G`. Off by default. |
| `HAKO_CGROUP_PARENT` | Explicit delegated cgroup to create the container cgroup under (otherwise auto-detected). |
| `HAKO_NO_SECCOMP` | Skip the workload seccomp filter (debugging, or a workload that needs a blocked syscall). |

Cgroup limits are best-effort: they apply only where a delegated cgroup v2 is
available (a systemd user session, or `HAKO_CGROUP_PARENT`), and are skipped
silently otherwise — the same posture as rootless Podman/Docker.

## Development

```sh
cargo test --workspace          # run the test suite
cargo fmt --all                 # format
cargo fmt --all -- --check      # verify formatting (CI gate)
cargo clippy --workspace --all-targets -- -D warnings   # lint (CI gate)

# The opt-in cluster surface isn't in the default build — lint/test it too:
cargo clippy -p hako-cli --all-targets --features cluster -- -D warnings
cargo test -p hako-cli --features cluster
```

CI runs formatting, clippy (including the cluster feature), a hako-core
feature matrix, tests on Linux + Windows, `cargo-deny`, and a real-runtime
isolation check on x86_64 + arm64 — see
[.github/workflows/ci.yml](.github/workflows/ci.yml) and
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).
