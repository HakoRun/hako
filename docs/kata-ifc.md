# kata — infrastructure compiled from code, deployed as commits

**Status:** Draft / product design. kata is a separate product (own repo, own
binary) in the hako suite; this doc lives here while hako is the suite's home.
Depends on hako Milestone 1 (P0-1/2/3) and the P1-1 deploy hook
(`push-to-deploy.md`); nothing here blocks hako, but hako's `[deploy]` and
capability work should be shaped with this consumer in mind.

*(Working name: "kata" — a practiced form; the compiler derives the __shape__
of your infrastructure. Rename freely; the design doesn't care.)*

## Thesis

Infrastructure-from-Code (IfC) tools — Encore, Nitric, Wing, Ampt — read your
application code and derive the infrastructure it needs: you write a handler
that consumes a queue, the compiler concludes "one service, one queue, one
route." Every one of them then compiles that conclusion into **someone else's
cloud** (CloudFormation, Terraform-to-AWS). None of them own a runtime.

kata compiles it into **your own fleet**:

> Your code declares *what* it needs. A one-page fleet file declares *where*
> it runs. The compiler emits hako containers + `[deploy]` tables, and
> `kata apply` is a set of fast-forward pushes. The infrastructure plan is a
> content-addressed commit: same code → same infra hash, drift detection is
> hash comparison, and rollback is `hako revert`.

The suite thesis, restated for this layer: **the plan artifact is versioned
state, not a side effect.** Terraform's state file is the most feared file in
DevOps because it is mutable, external, and divergeable. kata's equivalent is
a commit in a hako container — diffable, pushable, revertable, and never out
of sync with what was deployed, because it *is* what was deployed.

## Non-goals

- **No cloud backend in v1.** The differentiator is compiling to a fleet you
  own. A `terraform`/cloud escape hatch can come later as just another
  backend; chasing AWS parity on day one is how every IfC startup drowned.
- **No auto-placement.** hako's rule holds: `replicas: 3` never appears.
  Placement is explicit in the fleet file; the compiler *errors* on an
  unplaced resource rather than choosing for you.
- **No new general-purpose language.** Wing proved the DSL tax is fatal.
  kata reads ordinary TypeScript (first) / Rust (second) with a small SDK.
- **No runtime magic.** The SDK call that declares a resource at compile time
  is the same call that returns its client at runtime. One artifact, two
  interpretations — nothing is injected, rewritten, or monkey-patched.

## How intent is expressed (the frontend)

The Encore/Nitric model, which is the right one: a resource is a **top-level
SDK call with static arguments**.

```ts
import { service, queue, kv, blob, cron, secret } from "kata";

const emails = queue("emails");            // compile-time: a queue exists
const sessions = kv("sessions");           // runtime: this is its client
const models = blob("models");             // hako-native: deduped, versioned
const stripeKey = secret("STRIPE_KEY");

export const api = service("api", {
  route: "/",
  handler: async (req) => {
    await emails.push({ to: req.body.email });
    ...
  },
});

cron("nightly-report", "0 3 * * *", async () => { ... });
```

Compilation is static analysis over the module graph: find the SDK calls,
require their arguments to be statically resolvable (string literals /
`const`s — enforced, with a compile error otherwise, exactly like Encore),
and emit the **resource graph**: nodes (services, queues, KV stores, cron
jobs, routes, secrets, static sites) and edges (this service *uses* that
queue — known because the handler's closure references it).

The edges are the quiet killer feature: **least-privilege wiring by
construction.** A service that never references the queue never receives its
address or credentials. Nobody writes an IAM policy; the dependency graph *is*
the policy.

**The runtime shim.** Each service compiles to a thin per-language harness
that kata owns: it serves HTTP around the handlers, hosts the SQLite-backed
`kv`, fires `cron` schedules, serves `static site` trees, and fails closed at
startup if a bound resource's wiring is missing. It is the only runtime code
kata ships besides the stdlib containers, and it runs under hako's ordinary
P0-2 supervision like any other workload.

## The IR

A small, boring, serialized graph — `kata.lock`-shaped, committed:

```
Resource   = Service | Queue | Kv | Cron | Secret | StaticSite | Route
Binding    = (consumer: ResourceId, capability: read|write|invoke, target: ResourceId)
Graph      = { resources, bindings, source_hash }
```

Three properties, non-negotiable:

1. **Deterministic** — same source → byte-identical IR. All derivation is
   pure; anything environmental (node names, addresses) lives in the fleet
   file, not the IR.
2. **Content-addressed** — the **plan hash** is the infra version, computed
   over IR + dependency lockfiles + base-image content hashes. Deliberately
   *not* over built container trees: a build step that runs `npm install`
   is pinned (lockfile + base hash) but not bit-reproducible, and promising
   otherwise would be a lie. `kata plan` against a fleet is "diff my plan
   hash against the one recorded on each node" before it is anything
   cleverer.
3. **Language-neutral** — frontends (TS, later Rust) compile *to* the IR;
   backends compile *from* it. The IR is the suite's HCL.

## The primitives problem (the honest hard part)

Encore compiles `queue()` to SQS. On your own fleet there is no SQS — **the
compiler must bring the implementations**. kata's answer: a standard library
of infrastructure, classified by **state temperature** — the same
"immutable tree, mutable volume" two-plane rule `push-to-deploy.md` already
enforces, used as the stdlib's design principle.

**Hot state (mutable, high-frequency, destructive-delivery) must NOT live in
hako trees.** A queue-as-container would turn every push/pop into a tree
write + commit + ref CAS under the workspace flock, with fsync: write
amplification (1k msg/s = 1k commits/s of permanent history, feeding the
P2-2 gc problem), lock serialization where Redis-class systems use an
in-memory event loop, and wrong semantics — version control *preserves*
history, so every "popped" message and deleted session lives forever in the
chunk store (the P2-4 secrets collision). Hot state is node-local mutable
state on a volume, exactly where the two-plane rule puts it.

**Cold/warm state (immutable or human-speed-mutable) is hako-native** —
that's the substrate's home turf, and where kata is differentiated:

| Primitive | Temperature | v1 implementation (one blessed choice, no menu) |
|---|---|---|
| `queue` (delivery) | hot | NATS (single container, tiny, static binary) — carrying **hashes only** |
| `queue` (payloads) | cold | claim-check into the hako chunk store: immutable, deduped, already replicated by `peer push` |
| `kv` (session/cache, service-scoped) | hot | SQLite on a volume, in the service's runtime shim — no Redis container at all |
| `config` (flags/config KV) | warm | **hako-native**: a container per store; `set` = write + commit; replicated by push; history, diff, and rollback for free (etcd/Consul-KV has none of these) |
| `blob` (artifacts, models, backups) | cold | **hako-native**: a hako container — dedup, versioning, diff-sized sync, undelete. A headline primitive, not a footnote |
| `db` | hot | SQLite-per-service on a volume first; Postgres container later |
| `cron` | — | no container and no crontab: the service **runtime shim** (which must exist anyway to serve HTTP around handlers) fires schedules itself, under P0-2 supervision |
| `static site` | cold | served by the runtime shim from the container tree |
| `secret` | — | never a container, never in a tree — see below |

Blob caveat, designed-in rather than discovered later: immutable history
means true deletion requires history rewrite + gc (`hako expunge`, P2-4's
"later" item). So `blob` v1 targets delete-rarely data (build artifacts, ML
models, backups) and explicitly not GDPR-shaped user uploads.

This makes kata *the vendor* of its stdlib: you pin versions, you test
upgrades, you ship the containers through the same content-addressed pipe as
user code. That is real ongoing cost — it is also exactly the moat. And the
temperature split keeps the vendored surface small: only NATS (and later
Postgres) is third-party; everything cold is hako itself.

Rule: **one implementation per primitive in v1.** A configurable
"queue-provider" interface is how the stdlib becomes an unmaintained plugin
zoo. Add a second provider only when a real user is blocked.

**Ingress (v1 rule):** one published port per service, no path routing.
Two HTTP services on one node = two ports. A reverse-proxy `ingress`
primitive (Caddy container) is a later addition, not an implicit behavior.

## Placement: the fleet file

The one piece of configuration, and deliberately dumb:

```toml
# fleet.toml — the only file kata reads besides your code
[peers]        # names must exist in .hako/peers.toml
vps     = {}
homelab = {}

[place]
api       = "vps"
emails    = "vps"       # the NATS container lands next to its consumer
sessions  = "homelab"
"*"       = "vps"       # optional default; omit it and unplaced = error
```

The compiler cross-checks placement against the binding graph and *warns* on
cross-node chatter (api on vps, its queue on homelab = every delivery
crosses the WAN — though claim-check payloads travel via chunk sync, so the
hot-path cost is the small delivery frame, not the payload). It warns — it
does not reroute. The operator knowing where things run is the product.

**Apply is per-node, never fleet-atomic.** Each node's plan commit is
independently consistent; a partially-applied fleet is two nodes at two
plan hashes, visible in `kata plan`, converged by re-running `apply`.
Fleet-wide atomicity would require the consensus hako deliberately doesn't
have; declining it here is the same choice.

## What the backend emits

For each node, from the IR + fleet file:

1. **Containers** — user services (built from source into a hako container:
   base image + app tree, each build step a commit) and stdlib containers.
2. **`[deploy]` tables** — one per placed workload: run command, network mode
   + ports (compiled from the route/binding graph — a service with no
   inbound route gets no published port), volumes, grace period.
3. **Wiring** — env vars carrying the addresses of bound resources
   (`KATA_QUEUE_EMAILS=nats://10.0.0.5:4222`), derived from placement.
   Unbound = unset = the SDK client fails closed at startup.
4. **The plan commit** — IR + fleet file + emitted config committed into a
   dedicated `kata` container on each target node. The node's infra version
   is `hako log` on that container.

`kata apply` is then: for each target node, `hako peer push` the containers +
plan (FF-only, diff-sized), and let hako's P1-1 deploy hook reconcile. The
apply log is the push response. **kata never talks a protocol of its own** —
it is a pure client of the hako wire. If kata is down, nothing breaks; if you
outgrow kata, your fleet is still plain hako.

## Secrets

`secret("NAME")` compiles to a *requirement*, never a value: the emitted
`[deploy]` lists it under `env_pass`/mounts, and `kata apply` **fails closed**
if a target node hasn't been provisioned with it (out-of-band:
`hako write`-able node-local file or env — P2-4's blessed channel). Secrets
never enter the tree, the IR, or the wire. The compile error for a service
referencing a secret with no placement-side value is the feature.

## CLI surface

```
kata build            # code → IR + containers, locally; prints the infra hash
kata plan [node...]   # diff desired IR vs each node's recorded plan commit
kata apply [node...]  # push + reconcile; prints the deploy log per node
kata dev              # run the whole graph locally (hako containers, local net)
kata graph            # render the resource/binding graph (dot/ascii)
```

`kata dev` is dev/prod parity for free: the same backend, targeting the local
workspace instead of peers. It is also the only part of kata usable before
hako Milestone 1 lands, which makes it Phase 1 rather than a nice-to-have.

## Phasing

- **Phase 0 — IR + TS frontend + `graph`/`build`.** Static extraction, the
  IR format, determinism tests (same source → same hash, across machines).
  No runtime. Immediately useful as "lint my architecture."
- **Phase 1 — `kata dev`.** Local backend: materialize the graph as local
  hako containers with host networking. Needs nothing from the hako roadmap
  that isn't already merged except basic P0-1 (`--network host`).
- **Phase 2 — `kata plan`/`apply` against a fleet.** Needs Milestone 1 +
  P1-1 (deploy hook) + P1-3 (remote status/logs, for the apply log). This is
  the demo: laptop → `kata apply` → two $5 boxes serving traffic, rollback
  by revert.
- **Phase 3 — stdlib depth + drift.** Postgres, static-site ergonomics,
  `plan` detecting out-of-band changes (trivially: the node's plan-commit
  hash ≠ any ancestor of desired), Rust frontend.

## Traps to decline

- **Analyzing arbitrary dynamic code.** If the resource argument isn't
  static, error. Every IfC tool that tried to be smarter shipped a
  half-language with undocumented rules.
- **A provider/plugin interface for the stdlib.** One blessed
  implementation per primitive until a paying user is blocked.
- **kata-side orchestration state.** The moment kata keeps a database of
  fleet state outside the plan commits, the Terraform state file has been
  reinvented. Fleet truth lives on the nodes, read via `/peers/`.
- **Auto-placement / cost optimization.** See Non-goals; it re-derives the
  scheduler hako already rejected.
- **Compiling to cloud early.** It converts the one differentiated product
  into the fifth-best Encore.

## Open questions

- **Durable streams (v2 candidate).** A Kafka-lite where *sealed* segments
  are immutable hako objects replicated by the existing chunk protocol, and
  only the active segment + consumer offsets are node-local mutable state.
  The temperature split says this is buildable on the substrate; the
  question is whether anyone at fleet-of-five scale needs it.
- **`hako expunge`.** The blob primitive makes P2-4's "later" item
  (history rewrite + gc + tombstone) load-bearing sooner: without it, blob
  deletion is forever-partial. When does it graduate from later to next?
- **Migrations.** A `db` primitive without a migration story is a demo.
  Ship `db` last for exactly this reason?
- **Frontend order.** TS first (audience) vs Rust first (dogfooding, and the
  compiler itself is Rust). Recommendation: TS first; the IR keeps the door
  open and the Rust frontend is a lockstep second.
- **Where does kata's own identity sit?** Probably nowhere — kata runs *as*
  the operator's hako identity (it is a client). Per-peer capabilities
  (P2-1) then bound what a stolen laptop can do, which is another reason
  P2-1 matters.
