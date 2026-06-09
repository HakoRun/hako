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
  transferring only the chunks the remote is missing.

## Install / build

Requires a recent stable Rust toolchain (developed against 1.91).

```sh
# Native CLI + runtime (full runtime on Linux; CLI + host bridge on Win/Mac)
cargo build --workspace --release
# binary: target/release/hako
```

For the Windows/macOS auto-bootstrap build that embeds a cross-compiled Linux
binary, see [BUILD.md](BUILD.md).

## Command reference

| Area | Commands |
|------|----------|
| **Workspace** | `init` |
| **Files** | `write`, `cat`, `mkdir`, `del`, `cp`, `mv`, `import`, `export` |
| **Navigation** | `ls`, `pwd`, `cd`, `tree`, `status` |
| **Version control** | `commit`, `log`, `branch`, `checkout`, `merge`, `diff`, `tag` |
| **Containers** | `containers`, `new-container`, `del-container`, `is` |
| **OCI** | `pull` |
| **Runtime** (Linux / bridged) | `run`, `exec`, `ps`, `logs`, `stop`, `reap` |
| **Sync** | `fetch`, `push` |
| **Config** | `apply` (reads `hako.toml`) |
| **Maintenance** | `gc`, `fsck`, `mount`, `bootstrap` |

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

> **Isolation.** `hako run` runs the container rootless in user, mount, PID,
> IPC, UTS, and network namespaces, with a fresh procfs, a private `/tmp`, no
> host `$HOME`, private mount propagation, and a `pivot_root` rootfs (writable;
> writes are ephemeral) — similar in posture to rootless Podman. The workload
> runs under a minimal **PID-1 init** that reaps orphaned processes and forwards
> `SIGTERM`/`SIGINT`, so `hako stop` shuts it down cleanly; `hako exec` enters
> all of the container's namespaces. Network is **isolated by default for `run`**
> (opt in when a workload needs egress); `apply` keeps host networking so setup
> steps can install dependencies. The workload runs under a **seccomp filter** that
> blocks dangerous syscalls (module loading, kexec/reboot, mount, kernel keyring,
> bpf, …). It is not yet a hardened multi-tenant sandbox — no cgroup resource
> limits yet, and `/sys` is a host bind — so prefer trusted images for now.
> (`hako run` requires a Linux runtime; on Windows/macOS it is bridged into
> WSL2 / Lima.)

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

## Development

```sh
cargo test --workspace          # run the test suite
cargo fmt --all                 # format
cargo fmt --all -- --check      # verify formatting (CI gate)
cargo clippy --workspace --all-targets -- -D warnings   # lint (CI gate)
```

CI runs formatting, clippy, and tests on every push — see
[.github/workflows/ci.yml](.github/workflows/ci.yml).

## License

MIT — see [LICENSE](LICENSE).
