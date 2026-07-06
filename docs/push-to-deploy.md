# Push-to-deploy: hako as git-for-infrastructure

**Status:** Accepted / in progress. Supersedes the "auto-mesh" direction sketched
in the later phases of `distributed.md` (see Non-goals). Landed so far:
**P1-2** (`hako revert`), **P2-3** (`hako peer fetch` — network fetch), and the
**P1-1 prerequisite** (`[deploy]` parsed as a reserved node-local table). Each
landed item is marked in place below.

## Thesis

hako already *is* a git remote. `hako peer push <node> <branch>` is `git push`
(`serve/client.rs::remote_push`); `peers.toml` is `.git/config` remotes fused with
`known_hosts`; `sync_ref` is a fast-forward-only `receive-pack`. The product this
becomes, at personal/small-fleet scale, is:

> **Your infrastructure boxes are git remotes. Deploy is `push`. Rollback is a
> revert-commit. A node reacts to a ref advancing by reconciling the workload it
> runs.**

Two planes, kept strictly separate:

- **Data plane** — content-addressed chunk sync (`SyncHave/Put/Ref`). Each node
  holds a *full local replica*; there is never a network hop in a file-read path.
  This is git's pack protocol with BLAKE3. (It is **not** Venti — Venti was
  fetch-on-demand from a central server, the thing we are deliberately rejecting.)
- **Control plane** — file-named RPC over the wire (`MetaRead`/`MetaWrite` on
  `/peers/<node>/containers/<name>/{status,ctl,proc}`). Only small metadata
  crosses the network. This is the local "control the machine as files" idiom
  (`dispatch_ctl`, `proc_meta`) extended to remote nodes by path prefix. (It is a
  two-verb RPC, not 9P; that simplicity is a feature — it dodges 9P's hung-mount.)

"The fleet feels like one machine" is delivered by **federated addressing**
(`/peers/vps/...` and `/peers/homelab/...` are distinct subtrees, one authoritative
node each — no shared mutable cell, no consensus), **not** by shared state. The one
mutable thing that *is* replicated — a branch ref — is multi-master with FF-refusal
as the conflict detector and a human as the resolver. That is the git answer to
CAP, chosen deliberately.

## Non-goals (things this spec explicitly does NOT build)

- **No gossip / anti-entropy / consensus / membership CRDT.** Remotes are named in
  config, like `.git/config`. Deploys are explicit pushes.
- **No fetch-on-demand / network-backed FUSE.** A node's file reads are always
  local. `sync_ref` already refuses a ref whose object closure isn't fully present
  (`sync_ref_new_branch_requires_the_commit_present`); keep that invariant.
- **No "one machine" as a consistency contract** (single-system-image). We give
  the *navigable-and-controllable* feel, not linearizable shared state.
- **No transparent compute scheduling.** Placement stays explicit: `run app@vps`
  forever, never `replicas: 3`. Converge *state* automatically (via push); leave
  *placement* to the operator. An orchestrator, if ever built, is a thin client
  that pushes to N remotes and reads N statuses — a for-loop over the wire, not a
  new plane.
- **No mounting of `/peers/` inside the daemon**, and no serving `root/` file
  *content* over the control wire (both re-introduce the hung-mount / fetch-on-
  demand pathologies — see Traps).

## Design principles

1. **Two-plane discipline.** Never make the control plane strongly-consistent, and
   never let the control plane serve bulk file content. They meet at exactly one
   place — refs — which is governed below.
2. **Immutable tree, mutable volume.** The versioned tree is the *deployed
   artifact*. Anything a workload must persist (DB, uploads) and everything secret
   is a `VolumeMount` / node-local config, never committed or synced.
3. **Fail-closed trust.** A registered peer gets the least authority by default;
   deploy/commit/run are capabilities, not the ambient consequence of being in
   `peers.toml`.
4. **Explicit is better than magic.** The deploy "hook" is sugar over two visible
   operations (push + a control write). Keep the primitives usable by hand.

---

## Proposed changes

Ordered by leverage. P0 items are the difference between "clever demo" and
"serves traffic and stays up"; the elegant namespace work is deliberately P2+.

### P0-1 — Workload networking (the #1 blocker)

> **Partially landed:** `run --network none|host` shipped (#98) — `host`
> unblocks the acceptance test's `curl`. Remaining: `-p` port publishing and
> the rootless `pasta`/`slirp4netns` mode, plus the deploy hook consuming the
> receiver-side `network`/`ports` keys (the `[deploy]` parse already carries
> them).

**Problem.** `run` unshares `CLONE_NEWNET` with an empty network namespace and no
port publishing (`hako-runtime/src/transform.rs`, the `run_command_setup` /
`container_init` path). A deployed web service **cannot accept a single TCP
connection**. This is the biggest gap between the pitch and reality and no amount
of namespace design touches it.

**Change.** Add opt-in networking to `run`/`run -d`:
- `--network none|host|slirp` (default `none`, preserving today's isolation) and
  `-p <host>:<container>` port publishing, in `args.rs` (`Cmd::Run`), plumbed
  through `run_container` / `run_container_detached`.
- Ship `none|host` first — `host` alone unblocks the acceptance test's `curl`.
  Rootless port publishing via `pasta`/`slirp4netns` (userspace, no CAP_NET_ADMIN,
  keeps the static-musl story) is the long pole; land it second.
- Config surface: `network` / `ports` keys in the `[prod]`/`[deploy]` profile of
  `hako.toml` so a deploy target's networking is receiver-side declared.

**Touchpoints.** `hako-runtime/src/transform.rs` (netns setup), `args.rs`,
`hako-runtime/src/lib.rs` (a `NetworkSpec` alongside `VolumeMount`), `config.rs`.

**Risk.** Port collisions across instances; document last-writer / refuse-on-bound.
Depends on the `runtime-isolation.md` increment-3 work already scoped.

### P0-2 — Supervision & restart policy

**Problem.** `run -d` records an exit code (`instances.rs`) but has **no restart
policy and no start-on-boot**. A crashed service stays crashed until a human writes
a ctl file. "Deploy = push" is not credible without "stays running."

**Change.**
- `restart = no|on-failure|always` + optional `start_on_boot` in `InstanceConfig`
  (`instances.rs`), set from `hako.toml` or `run --restart`.
- The detached supervisor (`run_container_detached` in `transform.rs`) grows a
  loop: on workload exit, consult policy → re-spawn with backoff, recording each
  transition.
- Start-on-boot: `hako serve` (or a shipped systemd unit) reconciles instances
  marked `start_on_boot` on startup.
- **Record the resolved tree root hash in `InstanceConfig` at spawn.** Today it
  stores only the branch *name*, and `resolve_branch` runs once. Restart and
  start-on-boot must re-launch the **pinned root, never re-resolve the branch** —
  otherwise a crash or reboot silently re-boots a known-bad tip and undoes a
  rollback. (This is a *semantic prerequisite* of the restart policy and of P1-2
  rollback, so it lives here, not in Milestone 3; P2-2's gc merely consumes the
  same field. `status` must show both the ref tip and the running instance's root,
  or "branch at X, running Y" is invisible to the operator.)

**Touchpoints.** `hako-runtime/src/instances.rs`, `transform.rs`
(`run_container_detached`, `resolve_branch`), `cmd/runtime.rs`,
`hako-core/src/config.rs`.

**Risk.** Crash-loop storms — bound backoff; expose state in `status`/`ps`.

### P0-3 — Concurrent daemon

**Problem.** `serve` is a **serial accept loop** (`serve/server.rs::serve`):
`handle_peer` runs inline until the peer disconnects, so one connected peer
monopolizes the node and a stalled one blocks all others (up to the 30 s
`IO_TIMEOUT` per frame). The "control plane of the fleet" is single-tasking.

**Change.** Spawn a thread per accepted connection (a handful of lines around the
`for conn in listener.incoming()` loop). Keep the blocking model — resist tokio.

**This forces a companion change (do not skip).** The detached-spawn path
(`run_container_detached`, `transform.rs`) is today `fork()`-**without**-`exec`: the
child keeps running Rust (the FUSE server, the allocator). Forking from one thread
of a multithreaded daemon is unsafe — the child inherits any lock another thread
held at fork time (malloc arena, another connection's `NoiseChannel`) and wedges
forever, and it inherits the listener fd + every live connection fd (no `exec`, so
`CLOEXEC` never fires), so a crashed daemon's port stays bound and peers don't see
EOF while any workload lives. Convert detached spawn to **fork+exec** (re-exec
`hako run -d …` as a subprocess) so the runtime child shares no address space or
fds with the daemon. This is a prerequisite for both remote `ctl run` and the P1-1
hook under a threaded `serve`.

**Touchpoints.** `serve/server.rs::serve`; `hako-runtime/src/transform.rs`
(`run_container_detached` → fork+exec).

**Risk.** `WorkspaceLock` is a per-process `flock`; two concurrent sessions in one
daemon need in-process serialization on top of it (the #71/#75 self-deadlock
notes already flag this). Ref-mutating work must serialize; reads must not.

### P1-1 — The deploy hook (a remote that runs)

> **Partially landed:** the `[deploy]` table is parsed as a reserved node-local
> table in `config.rs` (the "reserve `[deploy]` as a real table" note below).
> The hook itself — reconciling on ref advance — is not built.

**Problem.** Today you can `push` and remotely `run`, but nothing ties a ref
advance to the running workload.

**Change.** On the *receiving* node, in `handle_peer` at the `TAG_SYNC_REF`
terminal **after the session lock is dropped** (the ref is durable + its closure
reachable there — #71's invariant — and spawning a container must not hold the
workspace lock — #78's lesson):
1. If the advanced `(container, branch)` matches the node's `[deploy]` config,
   find the live instance (`instances::list` by container).
2. Graceful stop: `instances::stop` (SIGTERM→nspid with the #72 recycle guard),
   wait `grace_secs`, SIGKILL.
3. Start: `run_container_detached(&repo, branch, run_spec, volumes)` at the
   **branch** (not `current_branch()` — the hook bypasses `dispatch_ctl`'s
   detached-HEAD refusal by targeting the ref explicitly).
4. Health-gate `grace_secs`; on boot failure, **auto-restart the previous
   commit's tree** (its root is still in the store, immutable).
5. Report the whole sequence in the `SYNC_REF` **response payload** so
   `hako push prod main` prints a Heroku-style deploy log for free.

**Config comes from the receiver's `hako.toml`, never the pushed tree** — otherwise
push == arbitrary RCE for any peer. Requires its own opt-in (config presence or a
`serve` flag), morally equivalent to `--allow-remote-run`.

```toml
# on the prod box only
[deploy]
container   = "app"
branch      = "main"
grace_secs  = 10
network     = "slirp"
ports       = ["8080:80"]
volumes     = ["/srv/app/data:/data"]
```

(Reserve `[deploy]` as a real table: today `AppRaw`'s `serde(flatten)` profile
catch-all in `config.rs` would swallow an unknown top-level table as a selectable
`--profile deploy` and silently drop its keys — a small breaking change to name.)

**Concurrency & lifetime — decide explicitly, don't leave it to the implementer.**
Under P0-3 the daemon is multithreaded, so:
1. **Serialize reconciles per `(container, branch)`** with a deploy mutex/queue that
   collapses to the latest — two pushes must not race two stop/start sequences
   (the session lock is already dropped here by design, so nothing else catches
   this).
2. **The reconcile must run to completion independent of the pusher's connection.**
   One-request/one-response has no progress frames, and stop→drain→start→health-
   gate→(maybe rollback) can exceed the 30 s `IO_TIMEOUT`; a pusher that times out
   or disconnects mid-reconcile must not leave the workload stopped-but-not-
   started. So: run the reconcile to completion and make the deploy log
   **best-effort in the response and always readable via `status`** — OR return the
   ref-advance immediately and observe the deploy on the control plane. Pick one.
3. **Gate the hook on `sync_ref` success** — the session-lock clear at the
   `TAG_SYNC_REF` terminal currently fires regardless of outcome.

**Touchpoints.** `serve/server.rs` (`handle_peer` terminal, a new `reconcile`
fn), `hako-core/src/config.rs` (`[deploy]`), `instances.rs`, `transform.rs`.

**Risk.** RCE-on-ref-advance — mitigated by receiver-side config + opt-in + P2-1
capabilities. In-flight requests: SIGTERM + drain window, same contract as Docker.

### P1-2 — `hako revert` ✅ LANDED

**Problem.** The wire is FF-only, so rollback **cannot** be a backward ref move.
The only rollback the protocol permits is a *revert-commit* (a new commit whose
tree equals an old one) — and `hako revert` does not exist (`vc.rs` has
commit/branch/tag/checkout/merge, no revert).

**Change.** `hako revert <ref>`: commit the target's tree with `parents = [tip]`.
Then `hako push prod main` rolls prod back FF-safely, and history records it.

**Touchpoints.** `hako-cli/src/cmd/vc.rs`, `args.rs` (`Cmd::Revert`).

**Risk.** None material; it's a thin `commit` variant.

### P1-3 — Remote stop / proc / logs (the deploy-loop sliver)

**Problem.** You can remotely *start* but not *stop* or *observe*. `meta_read`
serves only `status`; `meta_write` only container `ctl`; the stop path is
instance-addressed and local-only; you can't even find a remote pid. Logs live at
host paths (`instances::log_paths`), addressed by `hako logs <id>` — **not in the
`/containers/<name>/...` namespace at all**, a hole exactly where a deploy operator
looks first.

**Change.** Complete the *minimal* remote namespace:
- Widen `meta_read` to serve `proc/` (reuse `proc_meta::cat`/`ls` after the
  `out: &mut dyn Write` refactor `dispatch_ctl` already took).
- Widen `meta_write` to `proc/<pid>/ctl` (remote stop/signal).
- Serve instance logs over the control plane as a **bounded tail window** — log
  content exceeds `MAX_FRAME` (1 MiB); never stream a whole log in one frame.

Explicitly **not**: serving `root/` file content over the wire, remote `ls`/`tree`
for its own sake, or a FUSE `/peers/` mount (see Traps).

**Touchpoints.** `serve/server.rs` (`meta_read`/`meta_write` match arms),
`cmd/proc_meta.rs`, `cmd/runtime.rs` (`logs`).

**Risk.** proc cmdlines can leak secrets; ship this *with* P2-1 capabilities.

### P2-1 — Per-peer capabilities & ref-mutation gating

**Problem.** Trust is flat: any registered peer can push objects, move refs (FF),
**commit/branch/tag on your node ungated** (only `run` is gated, node-wide via
`--allow-remote-run`), and read any status. A remote `ctl commit` on a deploy
target advances its ref past your history → your next push is refused non-FF, and
there is **no network fetch to recover** (see P2-3). This is the concrete collision
where the two planes meet.

**Change.**
- Add capabilities to the `Peer` record (`peers.toml`): `role = read | sync |
  deploy` and optional per-container/branch scoping.
- Gate ref-mutating `ctl` verbs (`commit`/`branch`/`tag`) and `sync_ref` by
  capability. **Deploy targets hold refs passively** — a peer without `deploy`
  cannot move a tracked branch; a local `ctl commit` on a deploy box writes to a
  box-local branch, never the tracked one.
- Replace the node-wide `allow_remote_run` boolean with per-peer `deploy`.

**Touchpoints.** `hako-cli/src/cmd/peers.rs`, `identity.rs`, `serve/server.rs`
(`meta_write`, `sync_ref`).

**Risk.** This is the factotum-shaped hole `distributed.md` §Identity already flags;
it graduates from "open question" to prerequisite for the deploy framing.

### P2-2 — GC that works on a live prod box

**Problem.** `gc` (CLI handler in `cmd/maintenance.rs`) **refuses while any instance
is running**, because a live FUSE session writes uncommitted chunks into the shared
store. A prod box runs a workload 24/7, so **gc is permanently refused** as written.

**Change.** Two protections are needed — root-pinning alone is *insufficient*:
- Union each live instance's *spawn* tree root (recorded by **P0-2**) into the
  reachable set — this protects the committed base the instance runs on.
- **Plus a grace period** (`gc` skips objects whose store mtime is within the last
  N minutes, à la git's `gc.pruneExpire`). A live RW mount writes *new* chunks
  continuously and reads them back; its advancing root lives only in the
  supervisor's memory (`RwSession::current_root`), so no persisted root covers
  them. The grace window protects those uncommitted-but-in-use chunks (and, as a
  bonus, objects a concurrent push just wrote) without gc needing the moving root.

Then a running box can gc everything that is neither reachable from a ref / a live
instance's base, nor recently written.

**Touchpoints.** `hako-core/src/maintenance.rs` (`gc` reachable set),
`hako-cli/src/cmd/maintenance.rs` (the running-instance check), `instances.rs`.

**Risk.** Small; mirrors the existing reachable-roots logic.

### P2-3 — Network fetch (recovery + hub topology) ✅ LANDED

(Shipped as `hako peer fetch <node> [branch]`.)

**Problem.** The wire was **push-only** (`HAVE`/`PUT`/`REF`, no `WANT`/`GET`);
`cmd/sync.rs` fetch is path-local. A peer whose push is refused (non-FF) has **no
way to reconcile over the network** — git says "fetch, integrate, re-push"; hako
can't fetch. This is needed regardless of topology and is the other half of a real
protocol.

**Change.** Add `TAG_SYNC_WANT` (a hash list → streamed objects), a client
`remote_fetch`, and network-backed `hako fetch <remote> <container>@<branch>`.
Recovery becomes `fetch → merge (three_way_merge) → push`.

**Touchpoints.** `serve/proto.rs` (new tag), `serve/server.rs` (serve WANT),
`serve/client.rs` (`remote_fetch`), `cmd/sync.rs`.

**Risk.** Object streaming must respect `MAX_FRAME` (stream, don't single-frame
large objects — also fixes the existing HAVE ceiling for push).

### P2-4 — Secrets guardrails

**Problem.** A versioned, replicated tree makes committed secrets a fleet-wide,
permanent leak (delete removes them from the *tree*, not the *store* — chunks
survive until history rewrite + gc), and the FF-only wire makes remote cleanup
impossible by design.

**Change.**
- **The load-bearing half:** bless the unversioned channels as the *first*
  documented path — `env`/`env_pass` in the **receiver's** `hako.toml` (node-local)
  + `-v /run/secrets:...:ro` bind mounts. Never a tree write. This alone is worth
  shipping.
- **Stretch:** a commit-time entropy/pattern warning (`push` refuses without
  `--i-know` on a hit, reusing `sync_ref`'s firm-refusal style). High false-positive
  surface, modest real protection — do it only after the blessing above.
- Later: `hako expunge <path>` (local history rewrite + gc + tombstone), because
  someone will do it anyway.

**Touchpoints.** `cmd/vc.rs` (commit warning), `config.rs` (`env`/`env_pass`),
docs.

---

## Reference deploy loop (acceptance test)

The whole spec is "done enough" when this works, using only the two planes:

```
hako push prod main                       # data plane: FF push, diff-sized
# → deploy hook: drains old instance, boots new tree, health-confirms
# → push reply prints: "updated app:main → a3f81c2 / stopped i-8c / started i-df (healthy 3s)"

cat  /peers/prod/containers/app/status    # control plane: observe
cat  /peers/prod/containers/app/proc/1/status
curl http://prod:8080/                    # P0-1: it actually serves

# break it, redeploy, watch auto-rollback of the runtime to the old tree
hako commit -m "v2 (broken)"; hako push prod main
# → hook boots v2, health fails, auto-restarts the previous commit; reply says so

# roll the ref back, properly
hako revert v1 && hako push prod main
```

If, running two boxes through push + a few file verbs + a for-loop, it *feels* like
one machine — the framing is proven in its honest form. If what's missing is a port
to `curl` and a process that restarts itself (P0-1/P0-2), those are the blockers,
not more namespace.

---

## Traps to decline (write them down so they're not re-litigated)

- **FUSE-mounting `/peers/`.** `HakoFs` serves a *content tree* from
  `(ChunkStore, root_hash)`; a `/peers/` mount would be a from-scratch synthetic FS
  doing network RPC in kernel FUSE callbacks — `cat /peers/dead/status` blocks in
  the kernel, `ls` hangs the shell, the mount outlives the failure. hako's one-shot
  timeout-bounded RPC is the *better* alternative. If ever wanted: a disposable
  external client, read-only, aggressive per-op deadlines — a debugging toy, not
  plumbing.
- **Serving `root/` content over the control wire.** `MAX_FRAME` (1 MiB) is the
  tripwire; naive widening breaks on the first binary. Rule: refuse ("pull it
  first") or replicate-then-read at commit granularity — never byte-fetch-on-demand
  (that re-derives the auto-mesh, one convenience read at a time). `ChunkStore::
  read_at` exists for FUSE paging; the temptation to point it at a remote store impl
  will arrive — decline it.
- **Transparent scheduling / `replicas: N`.** The moment placement is implicit, the
  operator loses the property that makes personal infra tractable: knowing where
  things run.

## Open questions

- **Shallow prod stores vs FF verification.** FF-only makes prod history
  append-only forever, so a deploy box's store grows monotonically (CAS keeps the
  increments small, but nothing is collectible beyond P2-2). Shallow remotes would
  bound it — but conflict with `common_ancestor`, which needs the old tip's
  ancestry to prove descent. Real, unsolved; park it.
- **Ref-divergence UX.** Once P2-3 (fetch) exists, is the recovery `fetch → merge →
  push` manual, or does a `deploy` target simply never diverge (P2-1 keeps its
  tracked branch peer-writable only)? Recommend the latter as default.

## Sequencing

**Milestone 1 (makes it real):** P0-1 networking, P0-2 supervision, P0-3 concurrent
daemon. Without these there is no production and no demo.

**Milestone 2 (makes it push-to-deploy):** P1-1 hook, P1-2 `hako revert` ✅,
P1-3 remote stop/proc/logs, P2-1 capabilities (ship with P1-3).

**Milestone 3 (makes it safe & operable):** P2-2 gc-on-prod, P2-3 fetch ✅,
P2-4 secrets.

The auto-mesh (gossip/converged namespace) is **not** a milestone; it becomes an
optional convergence layer only if node count ever outgrows explicit push — and the
reconciler built in P1-1, made trigger-agnostic, is reused verbatim if it does.
