//! The node daemon (`hako serve`) and the cluster wire protocol (Phase 2 of
//! `docs/distributed.md`).
//!
//! A connection has two phases:
//!
//! 1. **Noise handshake** — a mutually-authenticated
//!    `Noise_IK_25519_ChaChaPoly_BLAKE2s` handshake (see [`NOISE_PARAMS`]). The
//!    initiator knows the responder's static key ahead of time from `peers.toml`;
//!    the responder learns the initiator's static during the handshake and
//!    authorizes it against *its* registry before serving anything. Both static
//!    keys are X25519 keys derived from the node's existing Ed25519 identity. The
//!    result is an encrypted, integrity-protected, forward-secret channel — every
//!    request/response below rides inside it (a [`NoiseChannel`]).
//! 2. **Requests** — e.g. `MetaRead(path)`, which runs the node's own meta-fs read
//!    (a container `status`) and returns the bytes, or the push data plane
//!    (`SyncHave`/`SyncPut`/`SyncRef`). This is what makes
//!    `cat /peers/<node>/containers/<name>/status` work remotely.
//!
//! `hako peer ping <name>` does the handshake and stops (a reachability +
//! identity check); `cat /peers/...` does the handshake then a `MetaRead`.

use super::Ctx;
use crate::cmd::{identity, peers};
use hako::{ChunkStore, Hash, WorkspaceLock};
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;

mod channel;
mod proto;
use channel::*;
use proto::*;

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

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
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle_peer(stream, &id, ctx, allow_remote_run) {
                    crate::diag!("serve: connection error: {e}");
                }
            }
            Err(e) => crate::diag!("serve: accept error: {e}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn handle_peer(
    stream: TcpStream,
    id: &identity::Identity,
    ctx: &Ctx<'_>,
    allow_remote_run: bool,
) -> io::Result<()> {
    set_io_timeouts(&stream)?;
    // Authorize the initiator's Noise (X25519) static against the registry, which
    // stores Ed25519 — compare against the converted form.
    let mut ch = handshake_as_server(stream, id, |x| {
        peers::registered_x25519(ctx)
            .map(|ks| ks.contains(x))
            .unwrap_or(false)
    })?;
    // A push (SYNC_HAVE -> PUT... -> REF) holds the workspace lock across the whole
    // unit, so a concurrent `gc` can't sweep objects between the HAVE that vouches
    // they are present and the REF that makes them reachable — the HAVE reply is a
    // reachability claim gc would otherwise be free to invalidate (#71). Acquired on
    // the first request of a push and released at the terminal REF, so a peer that
    // keeps the connection open between pushes doesn't hold the lock idle and starve
    // local commands. Read-only reads never lock; ctl meta-writes lock themselves,
    // scoped per-verb (see `meta_write`), so a long remote `run` doesn't hold it.
    let mut session_lock: Option<WorkspaceLock> = None;
    loop {
        let req = match ch.recv() {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some((&tag, payload)) = req.split_first() else {
            return Ok(());
        };
        if session_lock.is_none() && matches!(tag, TAG_SYNC_HAVE | TAG_SYNC_PUT | TAG_SYNC_REF) {
            session_lock = Some(lock_workspace(ctx)?);
        }
        let result: io::Result<Vec<u8>> = match tag {
            TAG_META_READ => std::str::from_utf8(payload)
                .map_err(|_| invalid("request path is not UTF-8"))
                .and_then(|path| meta_read(ctx, path)),
            TAG_META_WRITE => meta_write(ctx, payload, allow_remote_run),
            TAG_SYNC_HAVE => sync_have(ctx, payload),
            TAG_SYNC_PUT => sync_put(ctx, payload),
            TAG_SYNC_REF => sync_ref(ctx, payload),
            _ => Err(invalid("unknown request")),
        };
        // Terminal request of a push cycle: the ref is now durable and its object
        // closure reachable, so gc is safe and the lock can be dropped rather than
        // held idle until the peer disconnects (#71, liveness).
        if matches!(tag, TAG_SYNC_REF) {
            session_lock = None;
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

/// Acquire the workspace lock, so a daemon-side mutation serializes against
/// concurrent *local* commands (which hold the same lock) and a concurrent `gc`.
///
/// Two callers hold the returned guard: `handle_peer` for a whole push cycle
/// (SYNC_HAVE..REF), so the object closure a push depends on can't be swept
/// between the HAVE and the REF (#71); and `meta_write` for a single ref-mutating
/// ctl verb (commit/branch/tag), but NOT across `run` (#78). A connection is a
/// push XOR a ctl, so these never nest — and `WorkspaceLock` is a fresh flock per
/// acquire that would hang against itself, so do not add a nested acquire on any
/// path reachable while one is already held.
fn lock_workspace(ctx: &Ctx<'_>) -> io::Result<WorkspaceLock> {
    WorkspaceLock::acquire(&ctx.workdir.join(crate::DOT_HAKO))
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
            // commands with the workspace lock, but do NOT hold it across `run`:
            // that spawns a possibly-long container, and holding the lock for its
            // lifetime would block every local mutator (#78). `run` doesn't touch
            // refs, and `gc` already refuses while an instance is live.
            let mut buf = Vec::new();
            if verb == "run" {
                crate::cmd::files::dispatch_ctl(ctx, &name, body, &mut buf)?;
            } else {
                let _lock = lock_workspace(ctx)?;
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

/// Data plane: point a container's branch at a (now-present) commit, creating
/// the container if the node doesn't have it yet.
fn sync_ref(ctx: &Ctx<'_>, payload: &[u8]) -> io::Result<Vec<u8>> {
    let (container, rest) = take_lenprefixed_str(payload)?;
    let (branch, rest) = take_lenprefixed_str(rest)?;
    let commit =
        Hash(<[u8; HASH_LEN]>::try_from(rest).map_err(|_| invalid("malformed ref request"))?);
    // The workspace lock is held by the caller (`handle_peer`) for the whole
    // push session, so the create-container + ref update here is serialized
    // against local commands and a concurrent `gc` without re-locking (#71).
    if !ctx.state.list_containers()?.iter().any(|c| c == container) {
        ctx.state.create_container(container)?;
    }
    let repo = ctx.state.open_container(container)?;
    // Fast-forward-only: a peer may only advance an existing branch to a commit
    // that descends from its current tip. Without this, any registered peer could
    // force-overwrite `main` (or any ref) to an arbitrary commit and rewrite the
    // node's history. A brand-new branch (no current tip) is always allowed, as is
    // a no-op re-push of the same commit. See issue #40.
    if let Some(existing) = repo.read_ref(branch)? {
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
    Ok(format!("updated {container}:{branch} -> {}", &hex(&commit.0)[..12]).into_bytes())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// `hako peer ping <name>` — handshake with a peer and report success.
pub fn ping(ctx: &Ctx<'_>, name: &str) -> io::Result<ExitCode> {
    let peer = peers::lookup(ctx, name)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {name}")))?;
    let _ch = connect_and_handshake(ctx, &peer)?;
    println!("peer {name} ({}) verified", peer.address);
    Ok(ExitCode::SUCCESS)
}

/// `cat /peers/<node>/<subpath>` — handshake, then `MetaRead(subpath)`.
pub fn remote_cat(ctx: &Ctx<'_>, peer_rest: &str) -> io::Result<ExitCode> {
    let (node, subpath) = peer_rest.split_once('/').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "address a peer path as /peers/<node>/containers/<name>/status",
        )
    })?;
    let peer = peers::lookup(ctx, node)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {node}")))?;
    let mut ch = connect_and_handshake(ctx, &peer)?;
    let mut req = vec![TAG_META_READ];
    req.extend_from_slice(format!("/{subpath}").as_bytes());
    ch.send(&req)?;
    read_response(&mut ch, node)
}

/// `write /peers/<node>/<subpath>` — handshake, then `MetaWrite(subpath, body)`.
/// Dispatches a `ctl` verb (e.g. `run …`) to a remote node and prints its reply.
pub fn remote_write(ctx: &Ctx<'_>, peer_rest: &str, body: &[u8]) -> io::Result<ExitCode> {
    let (node, subpath) = peer_rest.split_once('/').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "address a peer path as /peers/<node>/containers/<name>/ctl",
        )
    })?;
    let peer = peers::lookup(ctx, node)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {node}")))?;
    let mut ch = connect_and_handshake(ctx, &peer)?;
    let path = format!("/{subpath}");
    let mut req = vec![TAG_META_WRITE];
    req.extend_from_slice(&(path.len() as u32).to_be_bytes());
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(body);
    ch.send(&req)?;
    read_response(&mut ch, node)
}

/// `hako peer push <node> [branch]` — replicate the local container's branch to
/// a peer over the authenticated channel: offer the reachable object hashes,
/// send only the ones it lacks, then point its ref at the commit.
pub fn remote_push(ctx: &Ctx<'_>, node: &str, branch: &str) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    let commit = repo.read_ref(branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "local branch {branch} not found in {}",
                ctx.default_container
            ),
        )
    })?;
    let reachable: Vec<Hash> = repo.reachable_objects(commit)?.into_iter().collect();
    let peer = peers::lookup(ctx, node)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {node}")))?;
    let mut ch = connect_and_handshake(ctx, &peer)?;

    // Offer every reachable hash; the peer replies with the subset it lacks.
    let mut have = Vec::with_capacity(1 + reachable.len() * HASH_LEN);
    have.push(TAG_SYNC_HAVE);
    for h in &reachable {
        have.extend_from_slice(&h.0);
    }
    ch.send(&have)?;
    let missing = decode_hashes(&read_ok_payload(&mut ch, node)?)?;

    // Send the missing objects, batched to stay well under MAX_FRAME.
    let store = ctx.state.store();
    let mut batch = vec![TAG_SYNC_PUT];
    let mut sent = 0usize;
    for h in &missing {
        let bytes = store
            .get(h)?
            .ok_or_else(|| invalid("a reachable object is missing locally"))?;
        if batch.len() > 1 && batch.len() + 4 + bytes.len() > PUT_BATCH_LIMIT {
            ch.send(&batch)?;
            read_ok_payload(&mut ch, node)?;
            batch = vec![TAG_SYNC_PUT];
        }
        batch.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        batch.extend_from_slice(&bytes);
        sent += 1;
    }
    if batch.len() > 1 {
        ch.send(&batch)?;
        read_ok_payload(&mut ch, node)?;
    }

    // Point the peer's ref at the commit (creating the container if needed).
    let mut req = vec![TAG_SYNC_REF];
    let container = ctx.default_container;
    req.extend_from_slice(&(container.len() as u32).to_be_bytes());
    req.extend_from_slice(container.as_bytes());
    req.extend_from_slice(&(branch.len() as u32).to_be_bytes());
    req.extend_from_slice(branch.as_bytes());
    req.extend_from_slice(&commit.0);
    ch.send(&req)?;
    let confirm = read_ok_payload(&mut ch, node)?;
    println!(
        "pushed {sent} objects to {node}; {}",
        String::from_utf8_lossy(&confirm)
    );
    Ok(ExitCode::SUCCESS)
}

/// Read a response message; return its payload on success, an error otherwise.
fn read_ok_payload(ch: &mut NoiseChannel, node: &str) -> io::Result<Vec<u8>> {
    let resp = ch.recv()?;
    let (&status, payload) = resp
        .split_first()
        .ok_or_else(|| invalid("empty response"))?;
    if status == RESP_OK {
        Ok(payload.to_vec())
    } else {
        Err(io::Error::other(format!(
            "peer {node}: {}",
            String::from_utf8_lossy(payload)
        )))
    }
}

/// Read a response and write its payload to stdout.
fn read_response(ch: &mut NoiseChannel, node: &str) -> io::Result<ExitCode> {
    let payload = read_ok_payload(ch, node)?;
    io::stdout().write_all(&payload)?;
    Ok(ExitCode::SUCCESS)
}

fn connect_and_handshake(ctx: &Ctx<'_>, peer: &peers::Peer) -> io::Result<NoiseChannel> {
    let expected = peer.verifying_key()?;
    let id = identity::load_or_create(ctx)?;
    let stream = TcpStream::connect(&peer.address)?;
    set_io_timeouts(&stream)?;
    handshake_as_client(stream, &id, &expected)
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // A genuine fast-forward is accepted and advances the ref.
        sync_ref(&ctx, &enc(ff)).unwrap();
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
                handle_peer(stream, &a_id, &a_ctx, false)
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
                handle_peer(stream, &a_id, &a_ctx, false)
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
                handle_peer(stream, &a_id, &a_ctx, false)
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
}
