# Changelog

All notable changes to hako are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/) once it reaches a release.

## [Unreleased]

### Added
- **Cluster foundations (opt-in `--features cluster`):** each node has a stable
  Ed25519 identity (`hako id`; secret seed at `.hako/identity`, 0600), and a
  static peer registry — `hako peer add|list|remove` over `.hako/peers.toml`
  (name → network address + public key). The first steps toward a private,
  trusted-fleet distributed hako (`docs/distributed.md`); gated so the base
  binary carries no crypto/transport weight.
- **Node daemon + authenticated handshake (`--features cluster`):** `hako serve`
  listens for peers, and `hako peer ping <name>` connects and verifies the peer
  cryptographically proves the Ed25519 identity registered for it — it signs a
  fresh challenge nonce, checked against the registered public key. The first
  wire exchange the control/data protocols will build on.
- **Container meta-fs:** from the host (`hako`) context, each container is
  addressable as a tree under `/containers/<name>/` — `root/` for its
  filesystem, plus meta nodes: `status` (read a snapshot of branch/HEAD/dirty),
  `ctl` (write a verb — `commit`/`branch`/`tag` — the Plan 9 control-file model),
  and `proc/` for the container's live process tree. `proc/` reads the host
  kernel's `/proc` scoped to the container's PID namespace (host processes are
  never exposed; `mem` is not exposed); on Windows/macOS it bridges into
  WSL2/Lima like `run`/`exec`.
- **Container lifecycle through the control plane:** `ctl "run [command]"`
  dispatches a detached workload (the container's current branch, like
  `run -d`), and `proc/<pid>/ctl "stop|kill|int|hup|<number>"` signals a process
  — both scoped to the container's PID namespace (a host or out-of-container pid
  is never signaled, re-checked immediately before the kill). The first steps of
  the distributed roadmap (`docs/distributed.md`); Linux-native for now.

### Changed
- The `/containers`, `/workspace`, and `/peers` prefixes are recognized only
  from the **host** (`hako`) container; from a guest image they are ordinary
  paths in the guest's own filesystem, so a guest is never shadowed by hako's
  namespace.
- A fresh workspace's session now defaults to the **`hako`** container instead of
  `"main"`, matching the seeded default container. This only changes behavior for
  a workspace with no `SESSION` file that had a container explicitly named `main`
  (it would now default to `hako`).

## [0.1.1] — 2026-06-10

### Added
- **Release hardening:** the published release now includes a `SHA256SUMS.txt`
  (with a keyless cosign signature) so downloads can be verified.
- `--help` documents the `hako is` / `hako as` / unknown-command-runs-in-container
  ergonomics, and bare `hako` now prints help.

### Changed
- CI gained a native **arm64** isolation run (the aarch64 binary is now exercised,
  not just cross-compiled) and a PR-time build of the Windows/macOS embedded
  wrappers (so embedded/cross breakage is caught before a release tag). Release
  and CI builds use `--locked`; a tag/version guard prevents mislabeled releases.

### Fixed
- Removed a stale `build.sh` and corrected "Linux + macOS only" doc/help strings
  to "Linux only" (FUSE is Linux-only since the libfuse drop).

## [0.1.0] — 2026-06-10

First release. Static Linux binaries (x86_64 / aarch64) plus Windows/macOS
wrappers that embed the Linux runtime and auto-bootstrap WSL2 / Lima.

### Added
- Content-addressed, version-controlled filesystem: BLAKE3 prolly-tree store
  with commits, branches, three-way merge, diff, tags, and `fetch`/`push`.
- OCI image support: `hako pull` from Docker Hub and other registries.
- Declarative `hako.toml` + `hako apply` (image, setup steps as commits, run,
  workspace mode, env, profiles), with an applied-step cache.
- **Container runtime** (`hako run`/`exec`/`ps`/`stop`/`logs`/`reap`): rootless,
  isolated in user + mount + PID + IPC + UTS + network namespaces, with a fresh
  procfs, private `/tmp`, no host `$HOME`, private mount propagation, a writable
  (ephemeral) rootfs, a `/workspace` bind, and a minimal PID-1 init that reaps
  orphans and forwards SIGTERM/SIGINT. Network is isolated for `run`, connected
  for `apply`. Linux-native; bridged into WSL2 / Lima on Windows / macOS.
- `gc` / `fsck` for the object store; `mount` (FUSE) for browsing trees.
- **Workload hardening:** a seccomp-BPF syscall filter (blocks module loading,
  kexec/reboot, the mount API, kernel keyring, bpf, io_uring, …; `HAKO_NO_SECCOMP`
  to skip), a fresh read-only `/sys` for `run`, and best-effort cgroup v2
  `pids`/`memory` limits where a delegated cgroup is available
  (`HAKO_PIDS_MAX` / `HAKO_MEMORY_MAX` / `HAKO_CGROUP_PARENT`).
- CI: fmt, clippy (`-D warnings`), tests on Linux + Windows, `cargo-deny`, and an
  `isolation` job that runs the real runtime and asserts its isolation
  properties.

### Fixed (notable, pre-release)
- `gc`/`fsck` now treat tags as roots (previously could delete tag-only data).
- OCI layer extraction bounds (gzip-bomb / oversized-header guards).
- Container runtime store-path + `pivot_root` bugs that made `hako run` mount an
  empty rootfs; `hako exec` now enters all container namespaces; `hako stop`
  terminates the workload cleanly.
- `common_ancestor` now returns the true lowest common ancestor (a naive
  breadth-first search picked the wrong merge base on non-linear history).
- FUSE `write`/`setattr` reject absurd sizes (4 GiB cap) instead of attempting a
  multi-gigabyte allocation; FUSE locks are poison-tolerant.

### Known limitations
See [`docs/runtime-isolation.md`](docs/runtime-isolation.md): not yet a hardened
multi-tenant sandbox — cgroup limits require a delegated cgroup v2 (skipped
otherwise), `:ro` volumes aren't recursively read-only, and ephemeral `run`
writes create orphan store objects until `gc`.

[0.1.1]: https://github.com/HakoRun/hako/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/HakoRun/hako/releases/tag/v0.1.0
