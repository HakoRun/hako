//! Server side of the cluster protocol: the `hako serve` daemon — bind + safety
//! gate, per-connection handshake, and per-request dispatch (meta-fs reads/writes
//! and the sync data plane), plus the two-node integration tests.

use super::channel::*;
use super::proto::*;
use crate::cmd::{identity, peers, Ctx};
use hako::{ChunkStore, Hash, WorkspaceLock};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::{Condvar, Mutex, MutexGuard};

/// Ceiling on concurrent connection handlers. A bound is the point of P0-3's
/// threading: a peer flood must not spawn unbounded threads and OOM the node.
/// The accept loop applies backpressure at the cap (new connections wait in the
/// kernel backlog), rather than accepting work it can't bound.
const MAX_CONNECTIONS: usize = 64;

/// Process-global serialization for daemon-side workspace **mutations**. The
/// `WorkspaceLock` flock serializes against other *processes* (the local CLI,
/// `gc`); this mutex serializes the daemon's own connection threads against each
/// other. Relying on flock's same-process, cross-fd behaviour for that would be
/// a footgun (#75), so intra-daemon serialization is made explicit: a thread
/// takes this mutex *then* the flock (always that order — no deadlock), so at
/// most one thread ever contends the flock. Held for a mutation's whole span (a
/// push cycle, #71; a single `ctl` verb). Reads never take it — the concurrency
/// win is that a slow push no longer blocks pings/status/fetch.
static DAEMON_MUTATION_LOCK: Mutex<()> = Mutex::new(());

/// Reject binding a routable (non-loopback) address unless the operator opts in.
/// The channel is now encrypted and mutually authenticated (Noise IK), so this is
/// no longer about plaintext exposure — it's that making a node reachable off-host
/// should be a deliberate choice (trusted LAN/VPN), not a surprise default.
/// Returns whether the chosen address exposes the node off-host.
fn check_bind_safety(addr: &str, allow_remote: bool) -> io::Result<bool> {
    use std::net::ToSocketAddrs;
    let exposes_remote = addr.to_socket_addrs()?.any(|sa| !sa.ip().is_loopback());
    if exposes_remote && !allow_remote {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to bind {addr}: making this node reachable off-host should be a \
                 deliberate choice. Re-run with --allow-remote to bind a routable address \
                 (the channel is encrypted + peer-authenticated, but expose it only on a \
                 trusted LAN/VPN)."
            ),
        ));
    }
    Ok(exposes_remote)
}

/// `hako serve [--addr ...]` — listen, authenticate peers, serve requests.
///
/// `allow_remote_run` gates the one request that grants command execution on
/// this node (the `ctl run` verb); it is off unless the operator opts in.
pub fn serve(
    ctx: &Ctx<'_>,
    addr: &str,
    allow_remote: bool,
    allow_remote_run: bool,
    allow_deploy: bool,
) -> io::Result<ExitCode> {
    let id = identity::load_or_create(ctx)?;
    let exposes_remote = check_bind_safety(addr, allow_remote)?;
    let listener = TcpListener::bind(addr)?;
    println!(
        "hako: serve: listening on {} as {}",
        listener.local_addr()?,
        id.node_id()
    );
    if exposes_remote {
        crate::diag!(
            "serve: warning: bound a routable address; the channel is encrypted and \
             peer-authenticated, but expose it only on a trusted LAN/VPN."
        );
    }
    if allow_remote_run {
        crate::diag!(
            "serve: warning: remote `ctl run` is enabled; any registered peer can \
             execute commands in a container on this node."
        );
    }
    // Push-to-deploy is on only when BOTH the operator opted in (`--allow-deploy`)
    // AND a `[deploy]` target is configured. Either alone is inert.
    let deploy_on = allow_deploy && ctx.cfg.deploy.is_some();
    if allow_deploy {
        match &ctx.cfg.deploy {
            Some(d) => crate::diag!(
                "serve: push-to-deploy enabled for {}:{} (a push that advances it \
                 (re)launches the workload here)",
                d.container,
                d.branch
            ),
            None => crate::diag!(
                "serve: --allow-deploy set but no [deploy] table in hako.toml; \
                 push-to-deploy is inert"
            ),
        }
    }
    // One handler thread per connection so a slow or stalled peer can't block the
    // others (previously the serial accept loop let one connected peer monopolize
    // the node up to IO_TIMEOUT per frame). `thread::scope` lets the handlers
    // borrow `id`/`ctx` directly — no `'static`/Arc restructuring — and joins any
    // still-running handlers if the loop ever exits. A semaphore bounds the fan-out.
    let slots = Semaphore::new(MAX_CONNECTIONS);
    std::thread::scope(|scope| {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    // Backpressure at the cap: block before accepting more work.
                    let permit = slots.acquire();
                    scope.spawn(|| {
                        // Released when this handler returns (or unwinds).
                        let _permit = permit;
                        // Contain a handler panic to its own connection: catch and
                        // log rather than let it unwind. The scope never joins (the
                        // accept loop is infinite), so an uncaught panic would just
                        // be dropped silently — and one malformed request must not
                        // take down the whole daemon. The permit still releases (its
                        // drop runs during unwind); the mutation mutex is
                        // poison-tolerant (see `lock_daemon`).
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                handle_peer(stream, &id, ctx, allow_remote_run, deploy_on)
                            }));
                        match outcome {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => crate::diag!("serve: connection error: {e}"),
                            Err(_) => crate::diag!("serve: connection handler panicked"),
                        }
                    });
                }
                Err(e) => crate::diag!("serve: accept error: {e}"),
            }
        }
    });
    Ok(ExitCode::SUCCESS)
}

/// A minimal counting semaphore (std has none) bounding concurrent connection
/// handlers. `acquire` blocks the accept loop when no permit is free, so excess
/// connections wait in the kernel backlog instead of spawning threads.
struct Semaphore {
    permits: Mutex<usize>,
    available: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Semaphore {
            permits: Mutex::new(n),
            available: Condvar::new(),
        }
    }

    fn acquire(&self) -> SemaphorePermit<'_> {
        let mut permits = self.permits.lock().unwrap_or_else(|e| e.into_inner());
        while *permits == 0 {
            permits = self
                .available
                .wait(permits)
                .unwrap_or_else(|e| e.into_inner());
        }
        *permits -= 1;
        SemaphorePermit { sem: self }
    }
}

/// Releases its permit back to the semaphore on drop (when a handler finishes).
struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        let mut permits = self.sem.permits.lock().unwrap_or_else(|e| e.into_inner());
        *permits += 1;
        self.sem.available.notify_one();
    }
}

fn handle_peer(
    stream: TcpStream,
    id: &identity::Identity,
    ctx: &Ctx<'_>,
    allow_remote_run: bool,
    deploy_on: bool,
) -> io::Result<()> {
    set_io_timeouts(&stream)?;
    // Authorize the initiator's Noise (X25519) static against the registry, which
    // stores Ed25519 — compare against the converted form.
    let mut ch = handshake_as_server(stream, id, |x| {
        peers::registered_x25519(ctx)
            .map(|ks| ks.contains(x))
            .unwrap_or(false)
    })?;
    // A push (SYNC_HAVE -> PUT... -> REF) holds the daemon mutation lock (in-process
    // mutex + workspace flock, see `lock_daemon`) across the whole unit, so a
    // concurrent `gc` can't sweep objects between the HAVE that vouches they are
    // present and the REF that makes them reachable — the HAVE reply is a
    // reachability claim gc would otherwise be free to invalidate (#71). Acquired on
    // the first request of a push and released at the terminal REF, so a peer that
    // keeps the connection open between pushes doesn't hold the lock idle and starve
    // other work. Read-only reads never lock; ctl meta-writes lock themselves,
    // scoped per-verb (see `meta_write`), so a long remote `run` doesn't hold it.
    let mut session_lock: Option<DaemonLock> = None;
    loop {
        let req = match ch.recv() {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some((&tag, payload)) = req.split_first() else {
            return Ok(());
        };
        // While a push session holds the mutation lock, only that push's own
        // frames (HAVE/PUT/REF) are legal. Any other frame is a protocol
        // violation — and a `META_WRITE` ctl verb here would re-enter
        // `lock_daemon` on THIS thread and self-deadlock the non-reentrant global
        // mutation mutex, wedging every connection's mutations. Refuse the
        // interleave and close (dropping `session_lock` releases the locks). A
        // well-behaved client never interleaves; the push and ctl planes are
        // separate connections.
        if session_lock.is_some() && !matches!(tag, TAG_SYNC_HAVE | TAG_SYNC_PUT | TAG_SYNC_REF) {
            let mut r = vec![RESP_ERR];
            r.extend_from_slice(b"illegal request interleaved with an in-progress push");
            let _ = ch.send(&r);
            return Ok(());
        }
        if session_lock.is_none() && matches!(tag, TAG_SYNC_HAVE | TAG_SYNC_PUT | TAG_SYNC_REF) {
            session_lock = Some(lock_daemon(ctx)?);
        }
        // `ref_advanced` distinguishes a real branch advance from a no-op re-push
        // (same tip) so the deploy hook doesn't restart a healthy workload for a
        // duplicate/retry push; `ref_prev` is the pre-push commit — the deploy's
        // rollback target if the new tree fails to boot.
        let mut ref_advanced = false;
        let mut ref_prev: Option<Hash> = None;
        let mut result: io::Result<Vec<u8>> = match tag {
            TAG_META_READ => std::str::from_utf8(payload)
                .map_err(|_| invalid("request path is not UTF-8"))
                .and_then(|path| meta_read(ctx, path)),
            TAG_META_WRITE => meta_write(ctx, payload, allow_remote_run),
            TAG_SYNC_HAVE => sync_have(ctx, payload),
            TAG_SYNC_PUT => sync_put(ctx, payload),
            TAG_SYNC_REF => sync_ref(ctx, payload).map(|u| {
                ref_advanced = u.advanced;
                ref_prev = u.prev;
                u.message
            }),
            // Fetch (pull): reads only. The objects served are reachable from a
            // ref, which `gc` preserves, so no session lock is needed.
            TAG_SYNC_WANT => sync_want(ctx, payload),
            TAG_SYNC_GET => sync_get(ctx, payload),
            _ => Err(invalid("unknown request")),
        };
        // Terminal request of a push cycle: the ref is now durable and its object
        // closure reachable, so gc is safe and the lock can be dropped rather than
        // held idle until the peer disconnects (#71, liveness).
        if matches!(tag, TAG_SYNC_REF) {
            session_lock = None;
            // Push-to-deploy (P1-1): with the mutation lock dropped (spawning a
            // container must not hold it, #78) and the ref durable, if this update
            // actually ADVANCED the node's deploy target, reconcile the workload
            // and append the deploy log to the push response.
            if deploy_on && ref_advanced {
                if let (Ok(bytes), Some(deploy)) = (&mut result, &ctx.cfg.deploy) {
                    if let Ok((container, branch)) = parse_ref_target(payload) {
                        if super::deploy::matches(deploy, container, branch) {
                            let log = super::deploy::reconcile(ctx, deploy, ref_prev);
                            bytes.extend_from_slice(log.as_bytes());
                        }
                    }
                }
            }
        }
        let resp = match &result {
            Ok(bytes) => {
                let mut r = Vec::with_capacity(1 + bytes.len());
                r.push(RESP_OK);
                r.extend_from_slice(bytes);
                r
            }
            Err(e) => {
                // Log the full error locally, but only echo our own intentional
                // application errors (PermissionDenied / InvalidData — FF refusal,
                // the run gate, malformed requests, missing objects) to the peer.
                // Other kinds are raw filesystem errors that can embed local paths,
                // so send a generic reply rather than leak host detail (#63).
                crate::diag!("serve: request error: {e}");
                let detail = match e.kind() {
                    io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidData => e.to_string(),
                    _ => "request failed on the remote node".to_string(),
                };
                let mut r = vec![RESP_ERR];
                r.extend_from_slice(detail.as_bytes());
                r
            }
        };
        ch.send(&resp)?;
    }
}

/// Serve a meta-fs read. For now: a container's `status` readout (the bytes
/// `cat /containers/<name>/status` would print locally).
fn meta_read(ctx: &Ctx<'_>, path: &str) -> io::Result<Vec<u8>> {
    use hako::RouteTarget;
    match RouteTarget::parse(path) {
        RouteTarget::Container { name, path: sub } if sub.is_empty() || sub == "status" => {
            let repo = ctx.state.open_container(&name)?;
            crate::helpers::render_container_status(&repo, &name)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot serve {path} remotely yet (only container status)"),
        )),
    }
}

/// A held daemon mutation lock: the in-process [`DAEMON_MUTATION_LOCK`] AND the
/// workspace flock, together. Dropping it releases the flock first, then the
/// mutex (reverse of acquire order).
struct DaemonLock {
    _flock: WorkspaceLock,
    _mutex: MutexGuard<'static, ()>,
}

/// Acquire the daemon mutation lock: the process-global mutex FIRST (serializes
/// the daemon's own connection threads), then the workspace flock (serializes
/// against local commands + `gc`). Always this order so the two locks can't
/// deadlock; the mutex ensures at most one thread contends the flock.
///
/// Two callers hold it: `handle_peer` for a whole push cycle (SYNC_HAVE..REF), so
/// the object closure a push depends on can't be swept between the HAVE and the
/// REF (#71); and `meta_write` for a single ref-mutating ctl verb
/// (commit/branch/tag), but NOT across `run` (#78). A connection is a push XOR a
/// ctl, so these never nest — do not add a nested acquire on any path reachable
/// while one is already held (it would self-deadlock on the mutex).
fn lock_daemon(ctx: &Ctx<'_>) -> io::Result<DaemonLock> {
    // Poison-tolerant: a handler that panicked mid-mutation must not wedge the
    // whole daemon (the flock + on-disk state stay consistent either way).
    let _mutex = DAEMON_MUTATION_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _flock = WorkspaceLock::acquire(&ctx.workdir.join(crate::DOT_HAKO))?;
    Ok(DaemonLock { _flock, _mutex })
}

/// Serve a meta-fs write. Payload is `[path_len: u32 BE][path][body]`. For now:
/// a container `ctl` verb (run/commit/branch/tag), dispatched on this node with
/// its output captured and returned.
fn meta_write(ctx: &Ctx<'_>, payload: &[u8], allow_remote_run: bool) -> io::Result<Vec<u8>> {
    use hako::RouteTarget;
    let plen = u32::from_be_bytes(first_array::<4>(payload, "malformed write request")?) as usize;
    let rest = &payload[4..];
    if rest.len() < plen {
        return Err(invalid("malformed write request"));
    }
    let path =
        std::str::from_utf8(&rest[..plen]).map_err(|_| invalid("write path is not UTF-8"))?;
    let body = &rest[plen..];
    match RouteTarget::parse(path) {
        RouteTarget::Container { name, path: sub } if sub == "ctl" => {
            // Gate remote command execution. The `run` verb spawns an arbitrary
            // command in a container on THIS node, so it is refused unless the
            // operator opted in with `hako serve --allow-remote-run` — otherwise a
            // registered peer would get code execution here by default (issue #40).
            // The other ctl verbs (commit/branch/tag) only touch this node's own
            // version-control state and stay available.
            let verb = std::str::from_utf8(body)
                .ok()
                .and_then(|s| s.split_whitespace().next())
                .unwrap_or("");
            if verb == "run" && !allow_remote_run {
                return Err(denied(
                    "remote `ctl run` is disabled on this node; \
                     start it with `hako serve --allow-remote-run` to permit it",
                ));
            }
            // Serialize ref-mutating verbs (commit/branch/tag) against local
            // commands and other daemon threads with the daemon mutation lock, but
            // do NOT hold it across `run`: that spawns a possibly-long container,
            // and holding the lock for its lifetime would block every mutator
            // (#78). `run` doesn't touch
            // refs, and `gc` already refuses while an instance is live.
            let mut buf = Vec::new();
            if verb == "run" {
                crate::cmd::files::dispatch_ctl(ctx, &name, body, &mut buf)?;
            } else {
                let _lock = lock_daemon(ctx)?;
                crate::cmd::files::dispatch_ctl(ctx, &name, body, &mut buf)?;
            }
            Ok(buf)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot write {path} remotely yet (only container ctl)"),
        )),
    }
}

/// Data plane: report which of the offered object hashes we are missing.
fn sync_have(ctx: &Ctx<'_>, payload: &[u8]) -> io::Result<Vec<u8>> {
    let store = ctx.state.store();
    let mut missing = Vec::new();
    for h in decode_hashes(payload)? {
        if !store.has(&h)? {
            missing.extend_from_slice(&h.0);
        }
    }
    Ok(missing)
}

/// Fetch, step 1: resolve `container:branch`'s tip and reply `[tip: 32]` followed
/// by every object hash reachable from it (the client then requests the subset it
/// lacks via SyncGet).
fn sync_want(ctx: &Ctx<'_>, payload: &[u8]) -> io::Result<Vec<u8>> {
    let (container, rest) = take_lenprefixed_str(payload)?;
    let (branch, _rest) = take_lenprefixed_str(rest)?;
    if !ctx.state.list_containers()?.iter().any(|c| c == container) {
        return Err(invalid("no such container on this node"));
    }
    let repo = ctx.state.open_container(container)?;
    let tip = repo
        .read_ref(branch)?
        .ok_or_else(|| invalid("no such branch on this node"))?;
    let reachable = repo.reachable_objects(tip)?;
    let mut out = Vec::with_capacity(HASH_LEN * (1 + reachable.len()));
    out.extend_from_slice(&tip.0);
    for h in &reachable {
        out.extend_from_slice(&h.0);
    }
    Ok(out)
}

/// Fetch, step 2: return the requested objects as `[obj_len: u32][obj]...`, in the
/// order asked, for the longest prefix that fits under `PUT_BATCH_LIMIT` (the
/// client re-requests the rest). A single object is always included even if it
/// alone exceeds the cap. A requested object that isn't present is an error.
fn sync_get(ctx: &Ctx<'_>, payload: &[u8]) -> io::Result<Vec<u8>> {
    let store = ctx.state.store();
    let mut out = Vec::new();
    for h in decode_hashes(payload)? {
        let obj = store
            .get(&h)?
            .ok_or_else(|| invalid("requested object is not present on this node"))?;
        if !out.is_empty() && out.len() + 4 + obj.len() > PUT_BATCH_LIMIT {
            break;
        }
        out.extend_from_slice(&(obj.len() as u32).to_be_bytes());
        out.extend_from_slice(&obj);
    }
    Ok(out)
}

/// Data plane: store a batch of objects (`[obj_len: u32][obj]...`).
fn sync_put(ctx: &Ctx<'_>, mut payload: &[u8]) -> io::Result<Vec<u8>> {
    let store = ctx.state.store();
    while !payload.is_empty() {
        let len = u32::from_be_bytes(first_array::<4>(payload, "malformed put batch")?) as usize;
        payload = &payload[4..];
        if payload.len() < len {
            return Err(invalid("malformed put batch"));
        }
        let (obj, rest) = payload.split_at(len);
        store.put(obj)?;
        payload = rest;
    }
    Ok(Vec::new())
}

/// Extract the `(container, branch)` a SYNC_REF payload targets, for the
/// push-to-deploy match. (The commit tail is `sync_ref`'s concern.)
fn parse_ref_target(payload: &[u8]) -> io::Result<(&str, &str)> {
    let (container, rest) = take_lenprefixed_str(payload)?;
    let (branch, _) = take_lenprefixed_str(rest)?;
    Ok((container, branch))
}

/// Outcome of a `sync_ref`: whether the ref moved, the commit it pointed at
/// BEFORE (the push-to-deploy rollback target), and the human message.
#[derive(Debug)]
struct RefUpdate {
    /// False for a no-op re-push of the current tip — the deploy hook skips it.
    advanced: bool,
    /// The pre-update commit, if the branch existed. On a health-gate failure the
    /// deploy re-launches this commit's tree (the last-known-good).
    prev: Option<Hash>,
    message: Vec<u8>,
}

/// Data plane: point a container's branch at a (now-present) commit, creating
/// the container if the node doesn't have it yet.
fn sync_ref(ctx: &Ctx<'_>, payload: &[u8]) -> io::Result<RefUpdate> {
    let (container, rest) = take_lenprefixed_str(payload)?;
    let (branch, rest) = take_lenprefixed_str(rest)?;
    let commit =
        Hash(<[u8; HASH_LEN]>::try_from(rest).map_err(|_| invalid("malformed ref request"))?);
    // The daemon mutation lock (mutex + flock) is held by the caller
    // (`handle_peer`) for the whole push session, so the create-container + ref
    // update here is serialized against local commands, other daemon threads, and
    // a concurrent `gc` without re-locking (#71).
    if !ctx.state.list_containers()?.iter().any(|c| c == container) {
        ctx.state.create_container(container)?;
    }
    let repo = ctx.state.open_container(container)?;
    let existing = repo.read_ref(branch)?;
    // Whether this update moves the ref at all — false for a no-op re-push of the
    // same commit. Push-to-deploy keys off this so a retry / duplicate push
    // doesn't needlessly stop-and-restart a healthy workload.
    let advanced = existing != Some(commit);
    // Fast-forward-only: a peer may only advance an existing branch to a commit
    // that descends from its current tip. Without this, any registered peer could
    // force-overwrite `main` (or any ref) to an arbitrary commit and rewrite the
    // node's history. A brand-new branch (no current tip) is always allowed, as is
    // a no-op re-push of the same commit. See issue #40.
    if let Some(existing) = existing {
        if existing != commit {
            // The pushed commit and its history must already be present — SyncPut
            // runs before SyncRef — for the ancestry walk to resolve. If they are
            // not, surface a clear "push objects first" instead of the opaque
            // "missing commit" the walk would otherwise raise.
            let base = repo.common_ancestor(existing, commit).map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    invalid(
                        "ref target commit or its history is missing on this node; \
                         push its objects before the ref",
                    )
                } else {
                    e
                }
            })?;
            if base != Some(existing) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing non-fast-forward update to {container}:{branch} \
                         (current tip {} is not an ancestor of {})",
                        &hex(&existing.0)[..12],
                        &hex(&commit.0)[..12]
                    ),
                ));
            }
        }
    }
    // A brand-new branch (or a no-op re-push) skips the ancestry walk above, so
    // verify the commit object is actually present before pointing a ref at it —
    // otherwise a peer could create a ref dangling at a commit it never pushed, a
    // self-inflicted broken ref that later reads/GC trip over (#63).
    if !ctx.state.store().has(&commit)? {
        return Err(invalid(
            "ref target commit is missing on this node; push its objects before the ref",
        ));
    }
    repo.write_ref(branch, commit)?;
    Ok(RefUpdate {
        advanced,
        prev: existing,
        message: format!("updated {container}:{branch} -> {}", &hex(&commit.0)[..12]).into_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // The two-node integration tests drive the client verbs against this server.
    use super::super::client::connect_and_handshake;
    use crate::cmd::serve::{remote_fetch, remote_push, remote_write};

    #[test]
    fn loopback_bind_needs_no_optin() {
        // literal IPs only — no DNS resolution in the test; loopback never exposes
        assert!(!check_bind_safety("127.0.0.1:7777", false).unwrap());
        assert!(!check_bind_safety("[::1]:7777", false).unwrap());
    }

    #[test]
    fn routable_bind_requires_optin() {
        // all-interfaces / specific routable address is refused without the flag
        assert!(check_bind_safety("0.0.0.0:7777", false).is_err());
        assert!(check_bind_safety("192.168.1.5:7777", false).is_err());
        // ...and allowed (reported as remote-exposing) with it
        assert!(check_bind_safety("0.0.0.0:7777", true).unwrap());
    }

    #[test]
    fn semaphore_bounds_concurrency_and_releases() {
        use std::sync::mpsc;
        use std::time::Duration;
        // Two permits: the third acquire must block until one is released.
        let sem = Semaphore::new(2);
        let p1 = sem.acquire();
        let _p2 = sem.acquire();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let _p3 = sem.acquire(); // blocks at the cap
                tx.send(()).unwrap();
            });
            // At the cap, the third acquire has not completed.
            assert!(
                rx.recv_timeout(Duration::from_millis(150)).is_err(),
                "third acquire should block while the semaphore is at its cap"
            );
            // Freeing a permit lets it through.
            drop(p1);
            assert!(
                rx.recv_timeout(Duration::from_secs(2)).is_ok(),
                "third acquire should proceed once a permit is released"
            );
        });
    }

    #[test]
    fn daemon_lock_acquires_and_releases() {
        use hako::{Config, Session};
        // The mutation lock must acquire, release on drop, and be re-acquirable
        // (no self-deadlock, no leaked flock). The two-node tests exercise it
        // under real push/ctl traffic; this pins the release contract directly.
        let (dir, state, _id) = setup_node();
        let (sess, cfg) = (Session::default(), Config::default());
        let c = ctx(&state, &sess, &cfg, "hako", dir.path());
        {
            let _l = lock_daemon(&c).expect("first acquire");
        }
        {
            let _l = lock_daemon(&c).expect("re-acquire after release");
        }
    }

    #[test]
    fn sync_ref_new_branch_requires_the_commit_present() {
        use hako::{Config, Hash, Session, State};

        let d = tempfile::tempdir().unwrap();
        let state = State::init(&d.path().join(crate::DOT_HAKO)).unwrap();
        let session = Session::default();
        let cfg = Config::default();
        let ctx = Ctx {
            state: &state,
            session: &session,
            default_container: "hako",
            workdir: d.path(),
            cfg: &cfg,
        };

        // Creating a BRAND-NEW branch pointing at a commit whose objects were
        // never pushed must be refused, not left as a dangling ref (#63). This
        // path skips the fast-forward ancestry walk, so it needs its own check.
        let ghost = Hash([0x7c; 32]);
        let mut p = Vec::new();
        p.extend_from_slice(&4u32.to_be_bytes());
        p.extend_from_slice(b"hako");
        p.extend_from_slice(&5u32.to_be_bytes());
        p.extend_from_slice(b"feat1");
        p.extend_from_slice(&ghost.0);

        let err = sync_ref(&ctx, &p).unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "expected a missing-commit error, got: {err}"
        );
        // No dangling ref was created.
        assert!(state
            .open_container("hako")
            .unwrap()
            .read_ref("feat1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn sync_ref_is_fast_forward_only() {
        use hako::{Config, Hash, Session, State};

        let d = tempfile::tempdir().unwrap();
        let state = State::init(&d.path().join(crate::DOT_HAKO)).unwrap();
        // Build a base commit, a fast-forward descendant, and an unrelated commit.
        let repo = state.open_container("hako").unwrap();
        let t = repo.working_tree().unwrap();
        let base = repo.commit(t, vec![], "u", "base", 1).unwrap();
        let ff = repo.commit(t, vec![base], "u", "ff", 2).unwrap();
        let diverged = repo.commit(t, vec![], "u", "x", 3).unwrap();
        repo.write_ref("main", base).unwrap();
        drop(repo);

        let session = Session::default();
        let cfg = Config::default();
        let ctx = Ctx {
            state: &state,
            session: &session,
            default_container: "hako",
            workdir: d.path(),
            cfg: &cfg,
        };
        let enc = |commit: Hash| {
            let mut p = Vec::new();
            p.extend_from_slice(&4u32.to_be_bytes());
            p.extend_from_slice(b"hako");
            p.extend_from_slice(&4u32.to_be_bytes());
            p.extend_from_slice(b"main");
            p.extend_from_slice(&commit.0);
            p
        };
        let tip = || {
            state
                .open_container("hako")
                .unwrap()
                .read_ref("main")
                .unwrap()
        };

        // A non-fast-forward (unrelated) update is refused and leaves the ref put.
        let err = sync_ref(&ctx, &enc(diverged)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(tip(), Some(base), "ref must not move on a rejected update");

        // A genuine fast-forward is accepted, advances the ref, reports the
        // PREVIOUS tip (the rollback target), and `advanced = true`.
        let u = sync_ref(&ctx, &enc(ff)).unwrap();
        assert!(u.advanced, "a real fast-forward must report advanced");
        assert_eq!(u.prev, Some(base), "prev must be the pre-push tip");
        assert_eq!(tip(), Some(ff));

        // A no-op re-push of the current tip succeeds but reports `advanced =
        // false`, so a duplicate/retry push does NOT trigger a redeploy.
        let u = sync_ref(&ctx, &enc(ff)).unwrap();
        assert!(!u.advanced, "a no-op re-push must not report advanced");
        assert_eq!(tip(), Some(ff));
    }

    #[test]
    fn sync_ref_missing_target_gives_clear_error() {
        use hako::{Config, Hash, Session, State};

        let d = tempfile::tempdir().unwrap();
        let state = State::init(&d.path().join(crate::DOT_HAKO)).unwrap();
        let repo = state.open_container("hako").unwrap();
        let base = repo
            .commit(repo.working_tree().unwrap(), vec![], "u", "base", 1)
            .unwrap();
        repo.write_ref("main", base).unwrap();
        drop(repo);

        let session = Session::default();
        let cfg = Config::default();
        let ctx = Ctx {
            state: &state,
            session: &session,
            default_container: "hako",
            workdir: d.path(),
            cfg: &cfg,
        };

        // A ref update whose target commit was never pushed: the ancestry walk
        // can't resolve it, so the error must clearly say "push objects first"
        // rather than the opaque "missing commit".
        let ghost = Hash([0x42; 32]);
        let mut p = Vec::new();
        p.extend_from_slice(&4u32.to_be_bytes());
        p.extend_from_slice(b"hako");
        p.extend_from_slice(&4u32.to_be_bytes());
        p.extend_from_slice(b"main");
        p.extend_from_slice(&ghost.0);

        let err = sync_ref(&ctx, &p).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing") && msg.contains("push"),
            "expected a clear push-objects-first error, got: {msg}"
        );
        // The ref must be untouched.
        assert_eq!(
            state
                .open_container("hako")
                .unwrap()
                .read_ref("main")
                .unwrap(),
            Some(base)
        );
    }

    #[test]
    fn meta_write_gates_remote_run() {
        use hako::{Config, Session, State};

        let d = tempfile::tempdir().unwrap();
        let state = State::init(&d.path().join(crate::DOT_HAKO)).unwrap();
        let session = Session::default();
        let cfg = Config::default();
        let ctx = Ctx {
            state: &state,
            session: &session,
            default_container: "hako",
            workdir: d.path(),
            cfg: &cfg,
        };
        let enc = |path: &str, body: &str| {
            let mut p = Vec::new();
            p.extend_from_slice(&(path.len() as u32).to_be_bytes());
            p.extend_from_slice(path.as_bytes());
            p.extend_from_slice(body.as_bytes());
            p
        };

        // `run` is refused when remote-run is disabled (the default), before any
        // spawn is attempted.
        let err = meta_write(&ctx, &enc("/containers/hako/ctl", "run echo hi"), false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // A non-`run` verb is not gated — it only touches this node's local VC
        // state (a clean tree yields "nothing to commit", still a non-error
        // dispatch, so the request itself is served).
        assert!(meta_write(&ctx, &enc("/containers/hako/ctl", "commit msg"), false).is_ok());
    }

    // ---------------------------------------------------------------------
    // Two-node integration: real loopback TCP through the full handshake +
    // wire protocol (docs/distributed.md flagged this as the missing coverage
    // for phases 2–3). Each test wires a server node and a client node, mutually
    // registered, and drives one connection. `handle_peer` returns when the
    // client disconnects, so a single `accept()` serves a whole exchange.
    // ---------------------------------------------------------------------

    /// A test node: an initialized workspace plus its persisted identity.
    fn setup_node() -> (tempfile::TempDir, hako::State, identity::Identity) {
        let d = tempfile::tempdir().unwrap();
        let state = hako::State::init(&d.path().join(crate::DOT_HAKO)).unwrap();
        let id =
            identity::load_or_create_at(&d.path().join(crate::DOT_HAKO).join("identity")).unwrap();
        (d, state, id)
    }

    fn ctx<'a>(
        state: &'a hako::State,
        session: &'a hako::Session,
        cfg: &'a hako::Config,
        container: &'a str,
        workdir: &'a std::path::Path,
    ) -> Ctx<'a> {
        Ctx {
            state,
            session,
            default_container: container,
            workdir,
            cfg,
        }
    }

    #[test]
    fn two_node_push_replicates_a_branch() {
        use hako::{Config, ScopedFs, Session};

        let (a_dir, a_state, a_id) = setup_node(); // server
        let (b_dir, b_state, b_id) = setup_node(); // client

        // Client builds a fresh container with a commit to replicate. A new
        // container name the server lacks avoids any ref divergence — the push
        // creates it fresh on the server.
        let repo = b_state.create_container("app").unwrap();
        let base = repo.working_tree().unwrap();
        let tree = ScopedFs::new(repo.store())
            .write_file(&base, "hello.txt", b"from client")
            .unwrap();
        let commit = repo.commit(tree, vec![], "b", "add hello", 1).unwrap();
        repo.write_ref("main", commit).unwrap();
        drop(repo);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (a_sess, a_cfg) = (Session::default(), Config::default());
        let a_ctx = ctx(&a_state, &a_sess, &a_cfg, "hako", a_dir.path());
        let (b_sess, b_cfg) = (Session::default(), Config::default());
        let b_ctx = ctx(&b_state, &b_sess, &b_cfg, "app", b_dir.path());

        // Mutual registration: server authorizes the client's key; client knows
        // the server's address + key.
        peers::add(&a_ctx, "client".into(), "unused".into(), b_id.node_id()).unwrap();
        peers::add(&b_ctx, "server".into(), addr, a_id.node_id()).unwrap();

        std::thread::scope(|s| {
            let server = s.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                handle_peer(stream, &a_id, &a_ctx, false, false)
            });
            let rc = remote_push(&b_ctx, "server", "main");
            assert!(rc.is_ok(), "push failed: {rc:?}");
            server.join().unwrap().expect("server handled the peer");
        });

        // The server now has the replicated container, ref, and objects.
        let a_repo = a_state.open_container("app").expect("server created 'app'");
        assert_eq!(a_repo.read_ref("main").unwrap(), Some(commit));
        let t = a_repo.load_commit(&commit).unwrap().tree;
        assert_eq!(
            ScopedFs::new(a_repo.store())
                .read_file(&t, "hello.txt")
                .unwrap(),
            b"from client"
        );
    }

    #[test]
    fn two_node_fetch_pulls_a_branch() {
        use hako::{Config, ScopedFs, Session};

        let (a_dir, a_state, a_id) = setup_node(); // server (has the branch)
        let (b_dir, b_state, b_id) = setup_node(); // client (fetches)

        // The server builds a container with a commit for the client to pull.
        let repo = a_state.create_container("app").unwrap();
        let base = repo.working_tree().unwrap();
        let tree = ScopedFs::new(repo.store())
            .write_file(&base, "hello.txt", b"from server")
            .unwrap();
        let commit = repo.commit(tree, vec![], "a", "add hello", 1).unwrap();
        repo.write_ref("main", commit).unwrap();
        drop(repo);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (a_sess, a_cfg) = (Session::default(), Config::default());
        let a_ctx = ctx(&a_state, &a_sess, &a_cfg, "app", a_dir.path());
        let (b_sess, b_cfg) = (Session::default(), Config::default());
        let b_ctx = ctx(&b_state, &b_sess, &b_cfg, "app", b_dir.path());

        peers::add(&a_ctx, "client".into(), "unused".into(), b_id.node_id()).unwrap();
        peers::add(&b_ctx, "server".into(), addr, a_id.node_id()).unwrap();

        std::thread::scope(|s| {
            let server = s.spawn(|| {
                // A fetch is two requests (WANT + GET), so one accept serves the
                // whole connection until the client disconnects.
                let (stream, _) = listener.accept().unwrap();
                handle_peer(stream, &a_id, &a_ctx, false, false)
            });
            let rc = remote_fetch(&b_ctx, "server", "main");
            assert!(rc.is_ok(), "fetch failed: {rc:?}");
            server.join().unwrap().expect("server handled the peer");
        });

        // The client now has the pulled container, ref, and objects.
        let b_repo = b_state.open_container("app").expect("client created 'app'");
        assert_eq!(b_repo.read_ref("main").unwrap(), Some(commit));
        let t = b_repo.load_commit(&commit).unwrap().tree;
        assert_eq!(
            ScopedFs::new(b_repo.store())
                .read_file(&t, "hello.txt")
                .unwrap(),
            b"from server"
        );
    }

    #[test]
    fn two_node_meta_read_returns_status_content() {
        use hako::{Config, Session};

        let (a_dir, a_state, a_id) = setup_node(); // server (has the seeded "hako")
        let (b_dir, b_state, b_id) = setup_node(); // client

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (a_sess, a_cfg) = (Session::default(), Config::default());
        let a_ctx = ctx(&a_state, &a_sess, &a_cfg, "hako", a_dir.path());
        let (b_sess, b_cfg) = (Session::default(), Config::default());
        let b_ctx = ctx(&b_state, &b_sess, &b_cfg, "hako", b_dir.path());

        peers::add(&a_ctx, "client".into(), "unused".into(), b_id.node_id()).unwrap();
        peers::add(&b_ctx, "server".into(), addr, a_id.node_id()).unwrap();

        std::thread::scope(|s| {
            let server = s.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                handle_peer(stream, &a_id, &a_ctx, false, false)
            });
            // Drive the client side directly so we can assert the RETURNED BYTES,
            // not merely that the round-trip completed: read the server's own
            // container status and verify its content came back over the wire.
            let peer = peers::lookup(&b_ctx, "server")
                .unwrap()
                .expect("peer registered");
            let mut ch = connect_and_handshake(&b_ctx, &peer).unwrap();
            let mut req = vec![TAG_META_READ];
            req.extend_from_slice(b"/containers/hako/status");
            ch.send(&req).unwrap();

            let resp = ch.recv().unwrap();
            assert_eq!(
                resp.first().copied(),
                Some(RESP_OK),
                "expected an OK status response"
            );
            let body = String::from_utf8_lossy(&resp[1..]);
            assert!(body.contains("container: hako"), "status body: {body:?}");
            assert!(body.contains("branch:"), "status body: {body:?}");

            drop(ch); // client hangs up → server's read loop hits EOF, returns
            server.join().unwrap().expect("server handled the peer");
        });
    }

    #[test]
    fn two_node_remote_run_refused_when_gate_off() {
        use hako::{Config, Session};

        let (a_dir, a_state, a_id) = setup_node(); // server
        let (b_dir, b_state, b_id) = setup_node(); // client

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (a_sess, a_cfg) = (Session::default(), Config::default());
        let a_ctx = ctx(&a_state, &a_sess, &a_cfg, "hako", a_dir.path());
        let (b_sess, b_cfg) = (Session::default(), Config::default());
        let b_ctx = ctx(&b_state, &b_sess, &b_cfg, "hako", b_dir.path());

        peers::add(&a_ctx, "client".into(), "unused".into(), b_id.node_id()).unwrap();
        peers::add(&b_ctx, "server".into(), addr, a_id.node_id()).unwrap();

        std::thread::scope(|s| {
            // Server started WITHOUT --allow-remote-run (the false arg).
            let server = s.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                handle_peer(stream, &a_id, &a_ctx, false, false)
            });
            let rc = remote_write(&b_ctx, "server/containers/hako/ctl", b"run echo hi");
            let err = rc.expect_err("remote `ctl run` must be refused when the gate is off");
            let msg = err.to_string();
            assert!(
                msg.contains("disabled") || msg.contains("allow-remote-run"),
                "unexpected refusal message: {msg}"
            );
            server.join().unwrap().expect("server handled the peer");
        });
    }

    #[test]
    fn interleaving_a_ctl_write_into_a_push_is_refused_not_deadlocked() {
        use hako::{Config, Session};

        // A peer that opens a push (taking the daemon mutation lock) and then
        // interleaves a `ctl` write on the SAME connection must be refused — the
        // ctl path re-enters `lock_daemon`, and before this guard that
        // self-deadlocked the non-reentrant global mutex, wedging every
        // connection's mutations daemon-wide. The response must come back (a fast
        // error), not hang until the connection times out.
        let (a_dir, a_state, a_id) = setup_node(); // server
        let (b_dir, b_state, b_id) = setup_node(); // client

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (a_sess, a_cfg) = (Session::default(), Config::default());
        let a_ctx = ctx(&a_state, &a_sess, &a_cfg, "hako", a_dir.path());
        let (b_sess, b_cfg) = (Session::default(), Config::default());
        let b_ctx = ctx(&b_state, &b_sess, &b_cfg, "hako", b_dir.path());

        peers::add(&a_ctx, "client".into(), "unused".into(), b_id.node_id()).unwrap();
        peers::add(&b_ctx, "server".into(), addr, a_id.node_id()).unwrap();

        std::thread::scope(|s| {
            let server = s.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                handle_peer(stream, &a_id, &a_ctx, false, false)
            });
            let peer = peers::lookup(&b_ctx, "server")
                .unwrap()
                .expect("registered");
            let mut ch = connect_and_handshake(&b_ctx, &peer).unwrap();

            // Open a push session: an empty HAVE takes the mutation lock (the lock
            // is acquired before dispatch, regardless of the HAVE's own result).
            ch.send(&[TAG_SYNC_HAVE]).unwrap();
            let _ = ch.recv().unwrap(); // drain the HAVE reply

            // Now illegally interleave a ctl commit on the same connection.
            let path = b"/containers/hako/ctl";
            let mut w = vec![TAG_META_WRITE];
            w.extend_from_slice(&(path.len() as u32).to_be_bytes());
            w.extend_from_slice(path);
            w.extend_from_slice(b"commit x");
            ch.send(&w).unwrap();

            // The server refuses and closes — a returned error frame, not a hang.
            let resp = ch.recv().unwrap();
            assert_eq!(
                resp.first().copied(),
                Some(RESP_ERR),
                "interleaving a ctl write into a push must be refused"
            );
            drop(ch);
            server.join().unwrap().expect("server handled the peer");
        });
    }
}
