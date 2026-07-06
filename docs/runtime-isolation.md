# Runtime isolation — current posture, verification, and history

Goal: make `hako run` a real security boundary so the same tool serves **dev
and production**. This document describes what the runtime isolates today, how
that is verified, what remains open — and, at the end, the design history
whose findings shaped the implementation (the fork ordering, the FUSE-in-userns
saga, why overlayfs is unused). Those history notes are load-bearing: read them
before restructuring `transform.rs`.

## What `hako run` isolates today

The container runs **rootless**, in user + mount + PID + IPC + UTS + network
namespaces, with:

- **PID** — the workload is PID 2 under a minimal in-container **PID-1 init**
  (`reap_as_init`) that reaps orphaned processes and forwards SIGTERM/SIGINT
  (so `hako stop` shuts the workload down gracefully and `exit 42` propagates
  as 42). A fresh procfs is mounted; host processes are never visible.
- **Filesystem** — the rootfs is the container's tree served over FUSE,
  mounted **read-write with ephemeral writes** (`docker run` semantics: the
  result is discarded, and writes never touch history). `pivot_root` detaches
  the host root; mount propagation is `private`.
- **No host `$HOME`** and a **private tmpfs `/tmp`**; the workspace is
  bind-mounted at `/workspace` honoring `rw`/`ro`/`none`. Host
  `/etc/resolv.conf` + `/etc/hosts` are bind-mounted **only** when the
  container has network (`apply`); an isolated `run` gets neither.
- **Network** — isolated by default for `run` (an empty network namespace: no
  connectivity, nothing can connect in). `run --network host` opts out of the
  network namespace only — the workload shares the host network and can
  listen/connect like a host process, with every other isolation layer
  unchanged (and the host's `resolv.conf`/`hosts` bound in so DNS works).
  Rootless port publishing (`-p` via pasta/slirp4netns) is the remaining
  P0-1 work in [push-to-deploy.md](push-to-deploy.md). `apply` keeps host
  networking so setup steps can install dependencies.
- **`/sys`** — for `run` (which owns its netns), a **fresh read-only sysfs**
  (`ro,nosuid,nodev,noexec`): no host cgroup/kernel internals exposed, and it
  reflects the container's own empty network. Where the kernel refuses a fresh
  sysfs (shared-netns cases like `apply`), a host bind with a best-effort
  read-only remount of the top mount is used instead.
- **Seccomp** — the workload (only — PID 1 stays unfiltered so it can reap)
  gets a seccomp-BPF denylist installed immediately before `exec`, returning
  `EPERM` for syscalls a container never legitimately needs: module loading,
  `kexec_load`/`reboot`, `swapon`/`swapoff`, the legacy **and** modern mount
  APIs (`mount`/`umount2`/`pivot_root`/`chroot`, `fsopen`/`fsconfig`/
  `fsmount`/`move_mount`/`open_tree`/`fspick`/`mount_setattr` — else a nested
  userns could mount despite the legacy block), host clock changes,
  `acct`/`quotactl`, the kernel keyring, `bpf`/`perf_event_open`, `io_uring_*`
  (the top recent kernel-LPE source), `userfaultfd`, and
  `open_by_handle_at`/`name_to_handle_at`. Everything else is allowed, so
  normal programs are unaffected. Built with the pure-Rust `seccompiler` crate
  (no libseccomp C dependency); installed via the userns `CAP_SYS_ADMIN`
  without `no_new_privs` (so in-container setuid still works).
  `HAKO_NO_SECCOMP` skips it. `hako exec` has seccomp parity with `run`.
- **Cgroup v2 limits (best-effort)** — the container subtree gets `pids.max`
  (default 1024 — the main fork-bomb DoS) and optional `memory.max`
  (`HAKO_MEMORY_MAX`; off by default since an over-tight cap OOM-kills
  legitimate workloads). Mirrors rootless Podman/Docker exactly: limits
  **require a delegated cgroup v2** (a systemd user session, or an explicit
  `HAKO_CGROUP_PARENT`) because the kernel grants an unprivileged process no
  cgroup powers over a subtree it doesn't own; with no delegation (default
  WSL2, hosted CI) it skips silently. The cgroup is removed on exit.
  `HAKO_PIDS_MAX`/`HAKO_MEMORY_MAX` tune it.
- **`hako exec`** enters ALL of the running instance's namespaces
  (user→ipc→uts→net→pid→mnt, then a fork for the PID ns), so an exec'd
  process sees only the container's processes and its isolated network. The
  recorded init pid is re-checked against recycling before `setns`.

`hako run-host` runs a host binary through the same namespaces + seccomp with
the host system directories bind-mounted read-only — a convenience sandbox,
not a versioned container.

**Honest limits:** this is similar in posture to rootless Podman — a strong
single-user boundary, **not yet a hardened multi-tenant sandbox**. Prefer
trusted images for now.

## Verification

`scripts/isolation-check.sh` runs a real container and asserts the properties
above: private PID view, no host `$HOME`, private `/tmp`, network isolation,
seccomp (a blocked `mount` returns `EPERM`), and read-only `/sys`. CI runs it
in the `isolation` job on **both x86_64 and native arm64** (seccomp-BPF
filters and syscall numbers are architecture-specific, and the release ships
an aarch64 binary). A runtime PR must keep it green — see
[CONTRIBUTING.md](../CONTRIBUTING.md).

Verify locally (Linux or WSL2):

```sh
HAKO=target/debug/hako bash scripts/isolation-check.sh
```

## Still open

- Hardening: recursive read-only for `:ro` volumes; a hako.toml `[safety]`
  knob for the seccomp/limits (currently env-var controlled).
- Ephemeral `run` writes create orphan store objects until `gc`; consider a
  scratch overlay or a dedicated ephemeral chunk area.
- Rootless port publishing for `run` (`-p` via `pasta`/`slirp4netns`) — the
  remaining half of P0-1 in [push-to-deploy.md](push-to-deploy.md)
  (`--network none|host` is done).

---

# Design history

Everything below is the investigation record, kept because its findings
constrain the implementation. It is historical: the "today" in these notes is
2026-06, and every increment described has since landed (see above).

## The original security gap and a real `run` bug were one defect

`hako run` originally used only user + mount namespaces and bind-mounted the
host `$HOME`, `/tmp`, `/proc`, and `/sys` into every container. It also
mounted the rootfs **read-only**, then `run_command_setup` wrote host paths
*into* that read-only tree:

- `setup_bind_mounts` did `create_dir_all(rootfs/home/$USER)` to bind the host
  `$HOME`. The toybox rootfs has **no `/home`**, so for any non-root user this
  `mkdir` into a read-only FS failed with **EROFS** and the whole run aborted.
- It also `fs::write`d `rootfs/etc/resolv.conf` / `hosts` — another RO write.

So the host-`$HOME` bind-mount (a credential-theft hole) was *also* what broke
`run` on a fresh Linux/WSL2 user. **Removing it for security fixed the
runtime.**

## The two store bugs that made every `run` fail

The "blocked on WSL2" conclusion reached mid-investigation was wrong: it was
**not** a WSL2/userns quirk but **two real, platform-independent bugs**:

1. **`resolve_branch` opened the chunk store at the wrong path** —
   `repo.root()/objects` (the per-container dir, which has no `objects/`)
   instead of the SHARED `<ws>/.hako/objects`. The store was empty → FUSE
   served an empty rootfs → every run failed. Fixed to mirror `cmd::mount`.
2. **`pivot_root` detached the store from the FUSE server.** `FsStore` reads
   objects by absolute path, and `command_setup` shared a mount namespace with
   the FUSE server; pivoting detached the old root, so the server could no
   longer read objects to serve (exec → ENOENT). Fixed by giving
   `command_setup` its **own** mount namespace (a copy that already has the
   FUSE mount) before `pivot_root`, leaving the server's namespace intact.

## Load-bearing constraints discovered along the way

- **overlayfs-over-FUSE is broken for exec** on the verification kernel (stat
  works, mmap/exec doesn't) — so the rootfs is the FUSE mount directly, with
  `pivot_root(".", ".")` (no writable `oldroot` needed). Writability comes
  from mounting the session read-write and discarding the result, not from an
  overlay.
- **`AllowOther` is invalid in a non-init userns** and **`AutoUnmount` forces
  the fusermount3 helper**; both are off in the runtime mounts so fuser mounts
  via `mount(2)` in-process.
- **`CLONE_NEWPID` stays out of the shared `run_inner` unshare** because the
  FUSE server there can't spawn its serve thread once a PID namespace is
  pending; `command_setup` unshares it separately and forks, making the child
  (`container_init`) PID 1 of the fresh namespace.
- The per-command **network namespace is created in `run_command_setup` after
  the FUSE mount** — creating it earlier broke `fusermount3`.
- Two detached-mode bugs to not reintroduce: instance state stored under the
  per-container dir but looked up at the workspace level (so
  `ps`/`exec`/`stop` never found it), and a detached supervisor inheriting the
  parent's stdio (so `id=$(hako run -d …)` blocked until the workload exited).

## Verified milestones (chronological)

All verified in WSL2 with `scripts/isolation-check.sh` against a real running
container, then gated in CI:

1. **Increment 1** — IPC/UTS/net namespaces, host-`$HOME` bind removed,
   private tmpfs `/tmp`, `private` mount propagation, no writes into the RO
   rootfs.
2. **Increment 2** — PID namespace: the command runs as PID 1 (later PID 2
   under the init), fresh procfs; all four core checks pass.
3. **Writable rootfs + `/workspace`** — rootfs mounted read-write, result
   discarded (ephemeral); container-created mountpoints and `/workspace`
   bind verified read-write.
4. **PID-1 init/reaper** — workload as PID 2 under `reap_as_init`; zombies
   reaped; exit codes propagate.
5. **`apply` end-to-end** — pulls a real image, runs setup steps in a
   read-write container, commits each; applied-step cache verified.
6. **`exec`/`stop`/signals** — exec enters all namespaces (7 pids / 0 routes
   vs host 54 / 3); stop delivers SIGTERM → exit 143.
7. **Seccomp** — in-container `mount` returns `EPERM` with the filter on,
   different errno with it off; isolation suite stays green.
8. **Read-only `/sys`** — writes fail with EROFS under `run`; `apply` still
   completes via the bind+remount fallback.
9. **Cgroup limits** — limit-writing unit-tested against a delegated parent;
   the no-delegation no-op path verified on real WSL2.
