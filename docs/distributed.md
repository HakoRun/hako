# Distributed hako — design notes

Status: **largely landed.** Phases 0–3 below are built (`ctl` lifecycle, node
identity + peers, the `hako serve` daemon with a Noise-encrypted wire protocol,
and the `/peers/` client surface). The successor spec —
[push-to-deploy.md](push-to-deploy.md) — supersedes the auto-mesh/orchestrator
direction sketched in the later phases and is where new work is scoped. This
document remains the architectural rationale.

The goal is a private, trusted-fleet distributed runtime — "compose N hako
nodes into one logical computer." It is *not* a public, untrusted marketplace
(that needs a microVM isolation tier we deliberately keep off the critical
path).

## Goal & posture

- **Private / trusted fleet.** Peers are nodes you control or explicitly add
  (with their public key). Peer-to-peer trust, not running-strangers'-code.
- **Orchestrator over N unmodified node agents**, not a monolith. Each hako node
  exposes a uniform file surface; a thin coordinator composes them. (kubelet vs.
  control-plane.)
- **Keep the base lean.** The single-machine FS/dev user pays nothing — all of
  this is feature-gated (`--features cluster`) and ideally the orchestrator is a
  separate binary.

## The foundation we already have (`main`)

- **The namespace is the right shape.** `RouteTarget = Local | ContainersList |
  Container | Workspace | Peers`. `/peers/<node>/…` was pre-carved as an
  extension point and is now wired to the daemon.
- **Control is a file write.** `ctl` (Plan 9 control-file model) already turns
  "act on a container" into "write a verb." Remoting it is transport, not
  redesign.
- **The data-plane protocol exists.** `sync_objects` = `reachable(commit)` minus
  `dst.has()`, copying via the `ChunkStore` trait. Content-addressed, dedup.
- **Observability is files.** `ls .../proc`, `cat .../status`.
- **Host-scoping separates the planes.** `/containers` + `/peers` are host-only;
  guests are pure — exactly the orchestrator/workload split a cluster wants.

## The keystone: the node daemon (built)

hako was a one-shot CLI with no listener anywhere; for a peer to receive a
remote op, a process must be *listening on the node*. That component now
exists: **`hako serve`** (`hako-cli/src/cmd/serve/`) — bind + safety gate,
per-connection Noise handshake, then per-request dispatch of the control and
data planes described below. It is currently a serial accept loop; making it
concurrent (and the fork-safety work that forces) is scoped as P0-3 in
[push-to-deploy.md](push-to-deploy.md).

## Architecture: `/peers/<node>/…` is the same op, run *there*

The unifying rule: `/peers/<node>/containers/<name>/…` ≡
`/containers/<name>/…` executed on `<node>`. The node's `hako serve` runs its own
meta-fs logic and returns the result. New *destination*, not new semantics.

### Two planes, matched to the data shape

| Plane | Paths | Shape | Wire |
| --- | --- | --- | --- |
| Control / observability | `ctl` (write), `status`, `proc/` | small, live | request/response RPC: peer runs the op, returns bytes |
| Data | `root/…` | bulk, content-addressed, immutable | **batched** have/want chunk-sync (generalized `sync_objects`) |

One transport for both would be wrong (9P-style streaming for `root/` discards
dedup; chunk-sync for `ctl` is nonsense). The meta-fs already sorts its nodes
into these two shapes — `container_fs_path` / `proc_subpath` classify which.

### The protocols

**Data (store) protocol** — the `ChunkStore` verbs, *batched* (the per-object
`has()` in today's `sync_objects` is a round-trip-per-object disaster over a
network — this is the most underestimated piece, git's hardest problem):
`Have(container, {hash…}) → {missing…}` · `Get({hash…}) → stream<bytes>` ·
`Put(stream<bytes>)` · `ReadRef`/`WriteRef` · `Reachable(commit) → {hash…}`.

**Control protocol** — two verbs the node executes against its own meta-fs:
`MetaRead(container, path) → bytes` (status, `cat` of proc) ·
`MetaWrite(container, path, bytes) → result` (`ctl` verbs).

## Identity & trust

- **Ed25519 identity per node** (built: `hako id`, seed at `.hako/identity`).
  A peer is `{ name, address, pubkey }`, in a static `peers.toml` (discovery
  evolves static → mDNS → DHT; do not build the DHT first).
- **Mutual auth + encryption** via a Noise IK handshake (built — see "Current
  posture" below).
- **Per-peer capability (built — P2-1).** Each peer has a `role` in `peers.toml`:
  `read` (telemetry: status/proc/fetch) < `sync` (+ push/replicate + VC ctl) <
  `deploy` (+ run code). The handshake learns the connecting peer's role and the
  daemon gates every request by it; revocation is editing `peers.toml`. A
  `deploy`-capable peer can run code on you — a real trust grant, now explicit
  per peer rather than node-wide.
- **Bootstrapping** (how two nodes first trust each other — out-of-band key
  exchange vs. TOFU) is a genuine security+UX problem, not an afterthought.

## Transport — the bloat-defining choice

Feature-gate everything (`--features cluster`) so the base stays ~4.9 MB and
dependency-clean. Prefer a **lean blocking stack**: framed TCP (or QUIC via
`quinn` for NAT-friendliness) + Noise (`snow`), **no tokio** (~+1–3 MB), honoring
hako's synchronous discipline. libp2p is the heavyweight alternative (mesh's
choice; the donor if you later want DHT/relay) — defer.

Concurrency split: a **node serving** requests is fine blocking; an
**orchestrator fanning out** to N nodes wants async/threads — so the orchestrator
is a separate binary that can afford a heavier stack without bloating the agent.

**Current posture (Noise wired).** The shipped channel is a full **Noise IK**
session (`Noise_IK_25519_ChaChaPoly_BLAKE2s` via `snow`, `serve/channel.rs`),
with the Noise static keys derived from each node's Ed25519 identity — every
message after the mutual handshake is encrypted and authenticated. `hako serve`
still **defaults to loopback** and refuses a routable bind without
`--allow-remote`: no longer about plaintext exposure, but because making a node
reachable off-host should be a deliberate choice (trusted LAN/VPN), not a
surprise default.

Two further gates shrink the per-peer blast radius until real per-peer
capabilities land (P2-1 in [push-to-deploy.md](push-to-deploy.md)):
peer-triggered command execution (`ctl "run …"`) is refused unless the node is
started with `--allow-remote-run` (off by default), and remote ref updates
(`sync_ref`) are **fast-forward-only**, so a registered peer cannot
force-rewrite a branch's history. Replication and the version-control `ctl`
verbs (commit/branch/tag) stay available without the flag.

## Known wrinkles (found while reviewing the code)

- **`ctl` runtime verbs ride inside `write`.** `run`/`stop` via `ctl` are runtime
  ops, but `write` isn't bridge-classified, and the verb is in the write *body*
  (harder to detect than the path-based `proc` bridge). Phase 0 ships them
  Linux-native; Windows→WSL bridging for write-borne runtime verbs is a follow-up.
- **Lifecycle is instance-id-addressed; `ctl` is container-addressed.** The clean
  model: `ctl "run …"` = spawn (returns an id); per-process control sits at
  `proc/<pid>/ctl "stop|kill"` (Plan 9 `/proc/n/ctl`), reusing the existing
  start-time recycle guard + the `proc/` namespace-scoping. `exec` (interactive,
  needs a tty) is a bad fit for a file write — defer it.
- **`WorkspaceLock` is a local `flock`.** The network model flips: the remote owns
  and serializes its own writes; the client negotiates, never reaches in.

## Phasing (corrected critical path)

0. ✅ **`ctl` lifecycle (local, no network).** `ctl "run"` (spawn) + per-process
   `proc/<pid>/ctl` signal. Reuses `run_container_detached` + the runtime's
   guarded stop.
1. ✅ **Identity + static peers.** Ed25519 keypair (`hako id`); `peers.toml`
   (`hako peer add|list|remove`).
2. ✅ **The node daemon (`hako serve`) + wire protocol.** Control RPC + batched
   data sync (`peer push`/`peer fetch`) over Noise.
3. ✅ **Wire `/peers/` (client).** `RouteTarget::Peers` resolves against the
   daemon (`cat`/`write` on `/peers/<node>/containers/<name>/{status,ctl}`).
4. **Orchestrator — superseded.** [push-to-deploy.md](push-to-deploy.md)
   deliberately rejects a scheduler: placement stays explicit, and an
   "orchestrator" is a thin client that pushes to N remotes and reads N
   statuses. Remaining work (networking, supervision, deploy hook,
   capabilities, gc-on-prod) is phased there.

## Open questions

- Batched-sync negotiation: how history-aware? (git haves/wants vs. a simpler
  reachable-set diff.)
- Capability model: where expressed, how enforced per-op, how revoked.
- QUIC vs TCP for the lean transport (NAT traversal vs. dependency weight).
- Does the orchestrator address nodes by `/peers/<node>` only, or mount a unioned
  view (Plan 9 union dirs) so a job sees `node-a:/data` + `node-b:/models` as one?
