# Changelog

All notable changes to hako are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/) once it reaches a release.

## [Unreleased]

Pre-release. No versioned releases have been published yet; this section tracks
the current state of `main`.

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

[Unreleased]: https://github.com/HakoRun/hako/commits/main
