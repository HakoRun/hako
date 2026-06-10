# Runtime isolation — design & plan

Goal: make `hako run` a real security boundary so the same tool serves **dev
and production**. Today it is not: it uses only user + mount namespaces and
bind-mounts the host `$HOME`, `/tmp`, `/proc`, and `/sys` into every container.

## Key finding: the security gap and a real `run` bug are one defect

`hako run` mounts the rootfs **read-only** (`mount_session`, not `_rw`), then
`run_command_setup` writes host paths *into* that read-only tree:

- `setup_bind_mounts` does `create_dir_all(rootfs/home/$USER)` to bind the host
  `$HOME`. The toybox rootfs has **no `/home`**, so for any non-root user this
  `mkdir` into a read-only FS fails with **EROFS** and the whole run aborts.
- It also `fs::write`s `rootfs/etc/resolv.conf` / `hosts` — another RO write.

So the host-`$HOME` bind-mount (a credential-theft hole) is *also* what breaks
`run` on a fresh Linux/WSL2 user. **Removing it for security fixes the runtime.**

Reproduced in WSL2 Ubuntu (user `ew_uy`): `hako run main …` →
`io error: Read-only file system (os error 30)`.

## Verification environment

WSL2 Ubuntu builds the Linux runtime and supports rootless **user+net+pid**
namespaces (`unshare -Urn` works), `/dev/fuse` + setuid `fusermount3`.
`scripts/isolation-check.sh` asserts the four properties below.

## Target properties (the checks)

1. **PID** — container cannot see host processes (private PID ns + fresh procfs).
2. **Home** — host `$HOME` is never mounted in.
3. **/tmp** — private tmpfs, not the host `/tmp`.
4. **Network** — isolated by default; opt-in connectivity for workloads.

Plus: mount propagation `private`; `/sys` mounted safely; workspace honored
(`rw`/`ro`/`none`); IPC/UTS namespaces.

## Staged plan

- **Increment 1** (no fork restructure): thread a policy; add
  `NEWNET|NEWIPC|NEWUTS`; drop host `$HOME`; private tmpfs `/tmp`; propagation
  `private`; stop writing into the RO rootfs. → home/tmp/network checks pass and
  `run` works on WSL2.
- **Increment 2**: restructure the double-fork so the command is **PID 1** in a
  new PID namespace + mount fresh procfs. → PID check passes.
- **Increment 3**: opt-in outbound networking via `pasta`/`slirp4netns`.

Each increment is gated on `scripts/isolation-check.sh` in WSL2, then CI, then
the README's `[safety]` section is restored honestly.

## RESOLVED (2026-06-09) — `hako run` works in WSL2; Increment 1 verified

The "blocked" conclusion below was wrong: it was **not** a WSL2/userns quirk but
**two real, platform-independent bugs** that made `hako run` fail everywhere:

1. **`resolve_branch` opened the chunk store at the wrong path** —
   `repo.root()/objects` (the per-container dir, which has no `objects/`) instead
   of the SHARED `<ws>/.hako/objects`. The store was empty → FUSE served an empty
   rootfs → every run failed. Fixed to mirror `cmd::mount`.
2. **`pivot_root` detached the store from the FUSE server.** `FsStore` reads
   objects by absolute path, and `command_setup` shared a mount namespace with
   the FUSE server; pivoting detached the old root, so the server could no longer
   read objects to serve (exec → ENOENT). Fixed by giving `command_setup` its
   **own** mount namespace (a copy that already has the FUSE mount) before
   `pivot_root`, leaving the server's namespace intact.

Also: `overlayfs`-over-FUSE is broken for exec on this kernel (stat works,
mmap/exec doesn't) — so the rootfs is the RO FUSE directly, with
`pivot_root(".", ".")` (no writable `oldroot` needed). `AllowOther` is invalid in
a non-init userns and `AutoUnmount` forces the fusermount3 helper; both removed
from the runtime mounts so fuser mounts via `mount(2)` in-process.

**Verified in WSL2 (`scripts/isolation-check.sh`, real running container) — ALL
PASS as of Increment 2:** host `$HOME` not exposed ✅, private `/tmp` ✅, network
isolated ✅, **PID namespace ✅** (the command is PID 1 and sees only its own
processes).

### Increment 2 — DONE (PID namespace)

`command_setup` unshares `CLONE_NEWPID` and forks; the child (`container_init`)
is PID 1 of a fresh PID namespace, mounts a fresh procfs, isolates the network,
`pivot_root`s, and execs. The parent waits and propagates the exit code.
`CLONE_NEWPID` stays out of the shared `run_inner` unshare because the FUSE
server there can't spawn its serve thread once a PID namespace is pending.

### Writable rootfs + `/workspace` — DONE

`run` now mounts the rootfs **read-write** (`mount_session_rw`) and discards the
result (never reads `current_root()`), giving `docker run`-style ephemeral
writability. This lets the container create mountpoints (e.g. `/workspace`) and
write scratch. Verified: the implicit `/workspace` bind to the host workdir works
read-write (a container write appears on the host), `/tmp`/scratch writes work,
and all isolation checks still pass. (overlayfs-over-FUSE stays unused — broken
for exec on this kernel.)

### PID-1 init/reaper — DONE

`container_init` (PID 1) no longer execs the workload directly; it forks (the
workload runs as PID 2) and becomes a minimal init (`reap_as_init`) that reaps
zombies of orphaned processes while the workload runs and returns the workload's
exit code as soon as it exits (remaining background processes are killed by PID-
namespace teardown — `docker run` semantics). Verified: workload is PID 2,
zombies reaped, exit codes propagate (e.g. `exit 42` → 42).

### `hako apply` — VERIFIED end-to-end

`apply` was fixed by the same store-path repair and works through the new
PID-fork/reaper path: it pulls a real image (`alpine:3.19`), runs each setup step
in a read-write container, and commits the result. Verified the committed tree
(`/etc/hako-marker` has both setup lines) and the applied-step cache (re-run =
"0 ran, 2 cached").

### `exec` / `stop` / init-reaper signals — DONE

- The workload runs under a PID-1 init (`reap_as_init`) that installs a
  SIGTERM/SIGINT handler forwarding to the workload (PID 1 ignores un-handled
  signals from an ancestor namespace), so `hako stop` shuts the container down
  gracefully (workload sees SIGTERM → exit 143).
- The container's PID-1 host pid is recorded (`nspid`); `hako exec` setns into
  ALL its namespaces (user→ipc→uts→net→pid→mnt, then fork for the PID ns), so an
  exec'd process sees only the container's processes and its isolated network —
  not the host's. Verified: exec shows 7 pids / 0 routes vs host 54 / 3.
- Fixed two pre-existing detached bugs: instance state was stored under the
  per-container dir but looked up at the workspace level (so `ps`/`exec`/`stop`
  never found it); and the detached supervisor inherited the parent's stdio, so
  `id=$(hako run -d …)` blocked until the workload exited. The supervisor now
  detaches its stdio.

### Seccomp syscall filter — DONE

The workload (only — PID 1 stays unfiltered so it can reap) gets a seccomp-BPF
filter installed immediately before `exec`, after all mounts/pivot. It's a
denylist returning `EPERM` for syscalls a container never legitimately needs and
that widen the host kernel attack surface: module loading
(`init_module`/`finit_module`/`delete_module`), `kexec_load`/`reboot`,
`swapon`/`swapoff`, `mount`/`umount2`/`pivot_root`/`chroot`, host clock changes
(`settimeofday`/`clock_settime`/`adjtimex`/`clock_adjtime`), `acct`/`quotactl`,
the kernel keyring (`add_key`/`request_key`/`keyctl`), and `bpf`/`perf_event_open`.
Everything else is allowed, so normal programs are unaffected. Built with the
pure-Rust `seccompiler` crate (no libseccomp C dependency); installs via the
userns `CAP_SYS_ADMIN` without `no_new_privs` (so in-container setuid still
works). `HAKO_NO_SECCOMP` skips it. Verified: `mount` inside a container returns
`EPERM` with the filter on and runs (different errno) with it off, while the full
isolation check still passes.

### Read-only `/sys` — DONE

`/sys` is now mounted after the netns unshare. For `run` (owns its netns) it's a
**fresh read-only sysfs** (`ro,nosuid,nodev,noexec`) — no host sysfs exposure
(cgroup/kernel internals) and it reflects the container's own empty network. For
shared-netns cases (`apply`) the kernel refuses a fresh sysfs mount, so it falls
back to a host `/sys` bind with a best-effort read-only remount of the top mount
(a recursive RO remount is refused for submounts we don't own; the bind stays rw
if even the top can't be remounted, matching prior behavior). Verified: writes to
`/sys` in a `run` container fail with EROFS, and `apply` still completes.

### Cgroup v2 resource limits — DONE (best-effort)

The container's whole subtree is placed in a cgroup v2 with `pids.max` (default
1024 — the main fork-bomb DoS) and optional `memory.max` (`HAKO_MEMORY_MAX`,
off by default since an over-tight cap OOM-kills legitimate workloads).

This mirrors rootless Podman/Docker exactly: cgroup limits **require a delegated
cgroup v2** (systemd user session, or an explicit `HAKO_CGROUP_PARENT`), because
the kernel grants an unprivileged process no cgroup powers over a subtree it
doesn't own — and a container can't self-provision that. So the limiter is
best-effort: it finds the delegation boundary (the highest writable ancestor of
its own cgroup, e.g. `…/user@1000.service`), creates `…/hako-<pid>`, enables the
controllers, writes the limits, and moves the container in; if nothing is
delegated (rootless without systemd — e.g. default WSL2, hosted CI) it skips
silently. The cgroup is removed when the container exits.

Verified: the limit-writing logic is unit-tested against a delegated parent (the
right values land in `pids.max`/`memory.max`/`cgroup.procs`); the no-op path is
verified on real WSL2 (no delegation → run is unaffected). Kernel *enforcement*
of those values is the kernel's job. `HAKO_PIDS_MAX`/`HAKO_MEMORY_MAX` tune it.

### Still open
- Hardening: recursive read-only for `:ro` volumes; a hako.toml `[safety]` knob
  for the seccomp/limits (currently env-var controlled).
- Ephemeral `run` writes create orphan store objects until `gc`; consider a
  scratch overlay or a dedicated ephemeral chunk area.
- CI runs `scripts/isolation-check.sh` on a Linux runner (the `isolation` job)
  as the automated gate for the runtime.

---

## Status (2026-06-08) — Increment 1 written, NOT YET VERIFIED (superseded above)

Increment 1 isolation code is implemented (`transform.rs`): IPC+UTS namespaces
for `run`/`apply`, deferred per-command network namespace for `run` (created in
`run_command_setup` after the FUSE mount — creating it earlier breaks
`fusermount3`), host-`$HOME` bind removed, private tmpfs `/tmp`, `make_rprivate`
mount-propagation. It compiles. **It is not runtime-verified.**

### Blockers found during verification (in WSL2 Ubuntu)

1. **`hako run` does not work in WSL2 Ubuntu at all — on `main`, before any of
   this branch's changes.** The FUSE rootfs mounts empty / `fusermount3:
   not mounted`, so every `run` fails (`EROFS` on `main`, `ENOENT` here). Root
   cause not yet identified (`/etc/fuse.conf` already has `user_allow_other`, so
   it is not that). Until `hako run` works in a reachable Linux env, the
   isolation cannot be empirically checked here. The dedicated `hako-runtime`
   WSL distro (hako's real runtime target) may behave differently and is the
   next place to test.
2. **Writable rootfs is required.** The `run` rootfs FUSE is read-only and the
   base image lacks mountpoints (`/home`, `/workspace`, possibly others), so
   bind/volume mountpoints can't be created. The fix is a rootless **overlayfs**
   rootfs (RO FUSE lower + tmpfs upper) — confirmed working rootless in this
   WSL2. This is a prerequisite increment, not optional.

### Root cause narrowed (2026-06-09)

`hako mount <dir>` (the simple FUSE path, no fork/namespaces) **works** in WSL2
Ubuntu — the mountpoint shows the full tree and is a real mountpoint. So FUSE
itself is fine. The failure is specific to `hako run`'s architecture: the FUSE
mount established by the **fuse_server** process is **not visible** in its
sibling **command_setup** process (which sees an empty `/tmp/hako-transform`),
so the subsequent mount/exec fail. Confirmed identical on `main`, as **root and
rootless**, and the `hako-runtime` distro is the same Ubuntu 24.04 (not a
different env). So the bug is the FUSE-mount visibility across the
fuse_server/command_setup fork — likely a mount-namespace/propagation issue in
the run path (`run_inner` → inner fork → `run_fuse_server` vs
`run_command_setup`). Next step: inspect `/proc/<pid>/ns/mnt` of both processes
during a run, and/or `hako-core/src/fuse.rs::mount_session`. This is the
prerequisite bug to fix before isolation can be verified.

### Final mechanism (2026-06-09)

Narrowed to the exact trigger: **`fuse::mount_session` (background `Session::
spawn`) serves an EMPTY tree when called inside `unshare(CLONE_NEWUSER)`** — even
as real root (0→0 map) and even after removing `AllowOther`. The mounting
process itself sees 0 entries. By contrast `hako mount` (foreground `mount2`, no
userns) serves the identical tree correctly. So the run path's FUSE-in-userns is
the blocker, not permissions, not the tree, not the fork.

Two plausible explanations, in priority order to test next:
1. **WSL2-specific FUSE/userns limitation.** Background FUSE serving inside a
   user namespace may simply not work on the WSL2 kernel. If so, `hako run` is
   fine on native Linux and the entire verification should move to a real Linux
   host (cloud VM). **Test this first — it may mean nothing is actually broken.**
2. **A real architectural constraint:** background FUSE must be mounted/served
   from a process OUTSIDE the user namespace (mount before `unshare(NEWUSER)` and
   pass the fd/mount in), then run the command in the userns against it. That's a
   focused run-path restructuring.

`AllowOther` was removed from the runtime mounts (`fuse.rs`) — correct on its own
merits (allow_other is invalid in a non-init userns) but it did NOT resolve the
empty mount, so the deeper userns issue above remains.

### Honest conclusion

Production-grade isolation here is a multi-part **runtime** project, not a single
change: (a) make `hako run` work portably (the FUSE/empty-mount bug), (b) add an
overlay writable rootfs, (c) the namespace/mount isolation on this branch,
(d) PID-ns fresh-procfs, (e) opt-in networking. The isolation code on this
branch is staged but must not be merged until it can be run and the
`scripts/isolation-check.sh` properties verified.
