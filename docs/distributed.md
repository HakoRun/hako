# Distributed hako — design notes

Status: **conceptual / in progress.** This is the north star for turning hako
into a private, trusted-fleet distributed runtime — "compose N hako nodes into
one logical computer." It is *not* a public, untrusted marketplace (that needs a
microVM isolation tier we deliberately keep off the critical path).

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
  Container | Workspace | Peers`. `/peers/<node>/…` is a pre-carved (stubbed)
  extension point.
- **Control is a file write.** `ctl` (Plan 9 control-file model) already turns
  "act on a container" into "write a verb." Remoting it is transport, not
  redesign.
- **The data-plane protocol exists.** `sync_objects` = `reachable(commit)` minus
  `dst.has()`, copying via the `ChunkStore` trait. Content-addressed, dedup.
- **Observability is files.** `ls .../proc`, `cat .../status`.
- **Host-scoping separates the planes.** `/containers` + `/peers` are host-only;
  guests are pure — exactly the orchestrator/workload split a cluster wants.

## The keystone gap: there is no daemon

hako is a one-shot CLI; there is **no listener/socket/service loop** anywhere.
For a peer to receive a remote op, a process must be *listening on the node* —
a supervised, long-lived **`hako serve`**. This is the largest unbuilt component
and the honest center of gravity for distribution.

Good news: it is not built on nothing. `hako run -d` already spawns a supervised,
shell-surviving process per instance (`instances.rs` + `transform.rs`: spawn,
pidfile, start-time liveness, graceful SIGTERM teardown). `hako serve` reuses
that lifecycle — it is net-new only as a **socket + request loop**, not as
"invent persistent processes."

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

- **Ed25519 identity per node** (hako lacks this today; mesh has it). A peer is
  `{ name, address, pubkey }`, in a static `peers.toml` first (discovery evolves
  static → mDNS → DHT; do not build the DHT first).
- **Mutual auth** via a Noise handshake (both ends verify pubkeys).
- **Per-peer capability** — read-only telemetry vs. `ctl`-control vs. data-sync.
  A `ctl`-capable peer can run code on you; this is a real trust grant and needs
  a real (not sketched) capability + revocation model.
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

**Current posture (Noise not yet wired).** The shipped channel does the framed
TCP + the mutual Ed25519 handshake, but **not** the `snow` session yet — so the
channel is *authenticated* (you know which registered peer you're talking to) but
**not encrypted or per-message-authenticated**. On a network where an attacker can
inject into the TCP session that is exploitable (e.g. a forged `ctl` write). Until
the Noise layer lands, `hako serve` therefore **defaults to loopback** and refuses
a routable bind unless you pass `--allow-remote` (intended for a trusted LAN/VPN,
and it warns). Treat `--allow-remote` over the open internet as unsafe.

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

0. **`ctl` lifecycle (local, no network).** `ctl "run"` (spawn) + per-process
   `proc/<pid>/ctl` signal. Reuses `run_container_detached` + the runtime's
   guarded stop. Fully testable on the existing harness. *This branch.*
1. **Identity + static peers.** Ed25519 keypair; `peers.toml`; bootstrapping UX.
2. **The node daemon (`hako serve`) + wire protocol.** Control RPC + batched data
   sync. The real lift.
3. **Wire `/peers/` (client).** `RouteTarget::Peers` resolves against the daemon.
4. **Orchestrator.** Thin scheduler over the `/peers/` file interface; separate
   binary; async.

Phases 0–1 are local and land on the current tests. 2–3 want a two-node
integration test (two WSL distros, like the proc test used `unshare`). 4 is the
cloud.

## Open questions

- Batched-sync negotiation: how history-aware? (git haves/wants vs. a simpler
  reachable-set diff.)
- Capability model: where expressed, how enforced per-op, how revoked.
- QUIC vs TCP for the lean transport (NAT traversal vs. dependency weight).
- Does the orchestrator address nodes by `/peers/<node>` only, or mount a unioned
  view (Plan 9 union dirs) so a job sees `node-a:/data` + `node-b:/models` as one?
