# Changelog

All notable changes to hako are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/) once it reaches a release.

## [Unreleased]

### Added
- **Remote process inspection (`--features cluster`):** the daemon now serves a
  container's live process tree over the control plane, so
  `hako cat /peers/<node>/containers/<name>/proc` lists the running pids and
  `…/proc/<pid>/{status,comm,cmdline}` reads them — the deploy operator's first
  question ("what's running on prod?"), answered remotely. Read-only and scoped
  to the container's PID namespace (host processes and `proc/<pid>/mem` are never
  exposed), served to any registered peer like `status`; per-peer scoping and the
  remote stop/signal + log-tail verbs come with the P2-1 capability work.
- **Push-to-deploy (`hako serve --allow-deploy`, `--features cluster`):** a node
  with a `[deploy]` table in its `hako.toml` reconciles a running workload when a
  push advances the tracked branch — it stops the old instance (graceful, then
  SIGKILL after `grace_secs`) and starts the new one at the just-pushed tree,
  supervised (`restart = always`) so it stays up. You declare the **entrypoint**
  receiver-side (`[deploy].run`); the pusher supplies the **filesystem** it runs
  against — so `--allow-deploy` is a code-execution grant (for an interpreted
  runtime the pushed tree is the code), like `--allow-remote-run`; enable it only
  for peers you'd let run code on the host. The deploy log rides back in the push response,
  so `hako peer push` prints a Heroku-style summary
  (`deploy app:main / stopping … / started …`). Off by default; needs both
  `--allow-deploy` and a `[deploy]` table. A restart re-launches the pinned tree,
  so `hako revert` + re-push rolls a deploy back. Config-only nodes are now
  first-class: a `hako.toml` that is *just* a `[deploy]` table no longer requires
  an `image`.
  - **Health-gate + auto-rollback.** After starting the new workload the daemon
    watches it for `grace_secs`; if it crash-loops (the supervisor respawns it
    within the window), the deploy **rolls the running workload back to the
    previous commit's tree** — the last-known-good, still in the store — while the
    ref stays at the new tip for the operator to fix-forward or `revert`. So a
    push of a tree that won't boot keeps the service serving the old version
    instead of thrashing. The deploy log reports `healthy` or
    `UNHEALTHY … rolled back to <commit>`.
  - Follow-up: `-p` port publishing (a `network = "host"` deploy serves on host
    ports meanwhile).

### Changed
- **The `hako serve` daemon is now concurrent (`--features cluster`):** one
  handler thread per connection (bounded by a semaphore, so a peer flood can't
  exhaust the node), replacing the serial accept loop where one connected — or
  stalled — peer monopolized the node up to the per-frame timeout. Daemon-side
  mutations serialize through a process-global mutex layered over the workspace
  flock (the flock guards other processes + `gc`; the mutex guards the daemon's
  own threads), while reads (status, fetch) run concurrently — so a slow push no
  longer blocks a ping. Verified: with a stalled connection held open, a
  concurrent `peer ping` still returns in ~20 ms.
- **Detached workloads (`hako run -d`) are now launched by fork + exec**, not a
  bare fork: the spawn creates the instance state, then re-execs the hako binary
  (a hidden `__run-detached <id>`) which reconstructs the run from the instance's
  persisted config and supervises the pinned root. Behavior is unchanged (`run
  -d` still returns the id immediately; ps/logs/stop/restart/revert-safety all
  hold), but the supervisor now shares no address space, locks, or fds with its
  launcher — the prerequisite for a concurrent `serve` daemon (push-to-deploy
  P0-3), and the same primitive a boot-time reconcile will reuse.

### Added
- **`hako run -d --restart no|on-failure|always`:** a detached workload can now
  be supervised — the background process re-launches it on exit per policy
  (`on-failure` = non-zero exits only, `always` = any exit), with bounded
  exponential backoff (1s→60s, reset after a run stays up ≥60s). This is
  push-to-deploy P0-2: "deploy = push" needs "stays running." A restart always
  re-launches the **tree root pinned at spawn**, never a re-resolution of the
  branch, so a `revert` (or any ref move) after spawn can't slip a different
  tree under a crash-restart. `hako stop` reaches the supervisor (SIGTERM →
  drain the workload → no respawn; `--force` SIGKILLs both). `ps`/`ps --json`
  show the policy, the respawn count, and the pinned root. Unsupervised
  (`restart = no`, the default) is unchanged. The instance config gained
  serde-defaulted fields (pinned root, policy, network, volumes, a reserved
  `start_on_boot`), so a running box's existing instances survive an in-place
  upgrade and a later boot-reconcile lands without another schema change.
- **`hako run --network none|host`:** a `run` workload can now opt into the
  host's network (`--network host`) so it can accept and make connections —
  the first slice of workload networking (push-to-deploy P0-1). `none`
  remains the default (fully isolated network namespace). Under `host` the
  process-isolation layers (namespaces, seccomp, cgroups) are unchanged, and
  the container takes the same shared-netns paths `apply` uses: host
  `resolv.conf`/`hosts` are bound in (DNS works) and `/sys` is a read-only
  bind of the host's sysfs rather than a fresh one — see
  `docs/runtime-isolation.md` before using `host` for untrusted workloads.
  The isolation CI check now asserts both modes. Rootless port publishing
  (`-p` via pasta/slirp4netns) is a follow-up.
- **Cluster foundations (opt-in `--features cluster`):** each node has a stable
  Ed25519 identity (`hako id`; secret seed at `.hako/identity`, 0600), and a
  static peer registry — `hako peer add|list|remove` over `.hako/peers.toml`
  (name → network address + public key). The first steps toward a private,
  trusted-fleet distributed hako (`docs/distributed.md`); gated so the base
  binary carries no crypto/transport weight.
- **Node daemon + remote meta-fs (`--features cluster`):** `hako serve` runs a
  node daemon that authenticates peers with a **mutual** Ed25519 handshake (each
  end proves it holds the key the other registered; the server serves only
  registered peers), then handles requests. Over that channel you **orchestrate
  a node by reading and writing its files**:
  `cat /peers/<node>/containers/<name>/status` reads a remote container's status,
  and `write /peers/<node>/containers/<name>/ctl "run …"` dispatches a control
  verb (run/commit/branch/tag) to it — its output (e.g. the new instance id) and
  errors come back over the wire. `hako peer ping <name>` checks reachability +
  identity. Trust is fail-closed: `serve` defaults to **loopback** (binding a
  routable address requires `--allow-remote`, intended for a trusted LAN/VPN),
  remote ref updates are **fast-forward-only**, and the `ctl "run …"` verb —
  which grants a peer command execution on this node — is refused unless the
  daemon was started with `--allow-remote-run`.
- **Encrypted cluster transport:** the peer channel is now a full **Noise IK**
  session (X25519 / ChaChaPoly / BLAKE2s, via `snow`), with the Noise static
  keys derived from each node's existing Ed25519 identity. Every message after
  the handshake is encrypted and authenticated — previously the channel proved
  peer identity but carried requests in plaintext.
- **Container replication over the cluster (`--features cluster`):** `hako peer
  push <node> [branch]` replicates a container branch to a peer over the
  encrypted channel — a content-addressed have/want sync that sends only the
  objects the peer is missing (a second push transfers nothing), then points the
  peer's ref at the commit (creating the container there if needed). With remote
  `ctl`, this closes the loop: ship a container to a node, then dispatch `run`.
  `hako peer fetch <node> [branch]` is the pull half: recovery from a refused
  (non-fast-forward) push becomes `fetch → merge → push`, git-style.
- **`hako revert <refspec>`:** roll back by committing an older tree on top of
  the current tip — the only rollback the fast-forward-only push protocol
  permits, and history records it. `refspec` is a branch, tag, or commit-hash
  prefix.
- **`[deploy]` table in `hako.toml` (parsed; reserved):** declares this node's
  push-to-deploy target — the container/branch it deploys and the run shape
  (command, network, ports, volumes, grace period) — per
  `docs/push-to-deploy.md`. Parsed and validated today; the deploy hook that
  consumes it lands separately. `[deploy]` is a reserved table name, no longer
  selectable as an `apply` profile.
- **hako-core feature gates:** the OCI registry client (`oci`) and the FUSE
  driver (`fuse`) are now default-on cargo features of the `hako` library.
  `--no-default-features` leaves the pure version-controlled-filesystem core
  with no network/TLS/FUSE dependency surface; CI builds each slice.
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

### Fixed
- `fetch`/`push` between two paths that resolve to the same workspace are
  refused, and the two workspaces are locked in a fixed global order —
  previously a self-sync could self-deadlock on the workspace lock.
- The serve daemon holds the workspace lock across a whole push session, so a
  concurrent `gc` can no longer collect objects while a peer is mid-push.
- Three-way merge raises a `FileDirectory` conflict when one side turns a file
  into a directory (or vice versa) instead of silently merging.
- FUSE serves file reads by window instead of reloading the whole file per read
  (large-file reads no longer re-materialize the file each syscall), plus
  rename/mtime correctness fixes.
- Host bridge: the `run-host` binary path is forwarded via env (not re-parsed
  from argv), and non-UTF-8 paths produce an error instead of being mangled.
- Chunk-store self-heal no longer races a concurrent writer, and ref/object
  writes fsync for durability.

### Security
- `hako exec` re-checks that a container's recorded init pid hasn't been
  recycled before `setns`, so a stale instance record can't enter an unrelated
  process's namespaces; the exec path also gained seccomp parity with `run`
  plus proper `/proc` and `/sys` mounts.
- OCI ingestion hardening: a recursion cap and a whiteout path guard on layer
  extraction, plus ref/error handling hardening on the wire.
- The bundle extraction cache verifies the cache directory is private and owned
  by the current user before trusting its contents (defeats local pre-seeding).

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
