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

## Status (2026-06-08) — Increment 1 written, NOT YET VERIFIED

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

### Honest conclusion

Production-grade isolation here is a multi-part **runtime** project, not a single
change: (a) make `hako run` work portably (the FUSE/empty-mount bug), (b) add an
overlay writable rootfs, (c) the namespace/mount isolation on this branch,
(d) PID-ns fresh-procfs, (e) opt-in networking. The isolation code on this
branch is staged but must not be merged until it can be run and the
`scripts/isolation-check.sh` properties verified.
