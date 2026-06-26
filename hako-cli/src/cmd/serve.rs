//! The node daemon (`hako serve`) and the cluster wire protocol (Phase 2 of
//! `docs/distributed.md`).
//!
//! A connection has two phases:
//!
//! 1. **Mutual handshake** — both ends prove they hold the Ed25519 key the other
//!    has registered. The client verifies the server is the node it dialed; the
//!    server verifies the client is a peer in *its* registry, before serving
//!    anything. (Station-to-station style: exchange pubkey+nonce, sign the
//!    other's nonce, verify.)
//! 2. **Requests** — currently one: `MetaRead(path)`, which runs the node's own
//!    meta-fs read (e.g. a container `status`) and returns the bytes. This is
//!    what makes `cat /peers/<node>/containers/<name>/status` work remotely.
//!
//! `hako peer ping <name>` does the handshake and stops (a reachability +
//! identity check); `cat /peers/...` does the handshake then a `MetaRead`.

use super::Ctx;
use crate::cmd::{identity, peers};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hako::{ChunkStore, Hash};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;

/// Cap on a single frame's payload, guarding against a bogus length prefix.
const MAX_FRAME: u32 = 1 << 20;
const NONCE_LEN: usize = 32;
const PUBKEY_LEN: usize = 32;
const SIG_LEN: usize = 64;

/// Request tags (first byte of a post-handshake request frame).
const TAG_META_READ: u8 = 1;
/// `MetaWrite` request: payload is `[path_len: u32 BE][path][body]`.
const TAG_META_WRITE: u8 = 2;
/// Data plane (push). `SyncHave` payload is a list of 32-byte object hashes; the
/// reply is the subset the server is missing. `SyncPut` payload is
/// `[obj_len: u32][obj]...`. `SyncRef` payload is
/// `[container_len: u32][container][branch_len: u32][branch][commit: 32]`.
const TAG_SYNC_HAVE: u8 = 3;
const TAG_SYNC_PUT: u8 = 4;
const TAG_SYNC_REF: u8 = 5;
/// Response status (first byte of a response frame).
const RESP_OK: u8 = 0;
const RESP_ERR: u8 = 1;

const HASH_LEN: usize = 32;
/// Flush a `SyncPut` batch before it would approach `MAX_FRAME`.
const PUT_BATCH_LIMIT: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
fn denied(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}
fn random_nonce() -> io::Result<[u8; NONCE_LEN]> {
    let mut n = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut n).map_err(|e| io::Error::other(format!("nonce: {e}")))?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Mutual handshake
// ---------------------------------------------------------------------------

/// Server side: verify the client (via `authorized`, which decides if a pubkey
/// is a registered peer) and prove our own identity. The `authorized` closure
/// keeps this decoupled from `Ctx` so it is unit-testable.
fn handshake_as_server(
    stream: &mut TcpStream,
    id: &identity::Identity,
    authorized: impl Fn(&[u8; PUBKEY_LEN]) -> bool,
) -> io::Result<()> {
    // H1: client_pubkey || client_nonce
    let h1 = read_frame(stream)?;
    if h1.len() != PUBKEY_LEN + NONCE_LEN {
        return Err(invalid("handshake: bad hello"));
    }
    let client_pubkey: [u8; PUBKEY_LEN] = h1[..PUBKEY_LEN].try_into().unwrap();
    let client_nonce = &h1[PUBKEY_LEN..];
    let client_vk =
        VerifyingKey::from_bytes(&client_pubkey).map_err(|_| invalid("client pubkey invalid"))?;
    if !authorized(&client_pubkey) {
        return Err(denied("client is not a registered peer"));
    }
    // H2: our signature over the client's nonce || our nonce
    let server_nonce = random_nonce()?;
    let mut h2 = Vec::with_capacity(SIG_LEN + NONCE_LEN);
    h2.extend_from_slice(&id.sign(client_nonce));
    h2.extend_from_slice(&server_nonce);
    write_frame(stream, &h2)?;
    // H3: client's signature over our nonce
    let h3 = read_frame(stream)?;
    let client_sig: [u8; SIG_LEN] = h3
        .as_slice()
        .try_into()
        .map_err(|_| invalid("handshake: bad client signature"))?;
    client_vk
        .verify(&server_nonce, &Signature::from_bytes(&client_sig))
        .map_err(|_| denied("client failed to prove its identity"))?;
    Ok(())
}

/// Client side: prove our identity and verify the server is `expected`.
fn handshake_as_client(
    stream: &mut TcpStream,
    id: &identity::Identity,
    expected: &VerifyingKey,
) -> io::Result<()> {
    // H1: our pubkey || our nonce
    let client_nonce = random_nonce()?;
    let mut h1 = Vec::with_capacity(PUBKEY_LEN + NONCE_LEN);
    h1.extend_from_slice(&id.verifying_key().to_bytes());
    h1.extend_from_slice(&client_nonce);
    write_frame(stream, &h1)?;
    // H2: server_sig over our nonce || server nonce
    let h2 = read_frame(stream)?;
    if h2.len() != SIG_LEN + NONCE_LEN {
        return Err(invalid("handshake: bad server reply"));
    }
    let server_sig: [u8; SIG_LEN] = h2[..SIG_LEN].try_into().unwrap();
    let server_nonce = &h2[SIG_LEN..];
    expected
        .verify(&client_nonce, &Signature::from_bytes(&server_sig))
        .map_err(|_| denied("peer failed the identity check"))?;
    // H3: our signature over the server's nonce
    write_frame(stream, &id.sign(server_nonce))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// `hako serve [--addr ...]` — listen, authenticate peers, serve requests.
pub fn serve(ctx: &Ctx<'_>, addr: &str) -> io::Result<ExitCode> {
    let id = identity::load_or_create(ctx)?;
    let listener = TcpListener::bind(addr)?;
    println!(
        "hako serve: listening on {} as {}",
        listener.local_addr()?,
        id.node_id()
    );
    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => {
                if let Err(e) = handle_peer(&mut stream, &id, ctx) {
                    eprintln!("hako serve: connection error: {e}");
                }
            }
            Err(e) => eprintln!("hako serve: accept error: {e}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn handle_peer(stream: &mut TcpStream, id: &identity::Identity, ctx: &Ctx<'_>) -> io::Result<()> {
    handshake_as_server(stream, id, |pk| {
        peers::find_by_pubkey(ctx, &hex(pk))
            .ok()
            .flatten()
            .is_some()
    })?;
    loop {
        let req = match read_frame(stream) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some((&tag, payload)) = req.split_first() else {
            return Ok(());
        };
        let result: io::Result<Vec<u8>> = match tag {
            TAG_META_READ => std::str::from_utf8(payload)
                .map_err(|_| invalid("request path is not UTF-8"))
                .and_then(|path| meta_read(ctx, path)),
            TAG_META_WRITE => meta_write(ctx, payload),
            TAG_SYNC_HAVE => sync_have(ctx, payload),
            TAG_SYNC_PUT => sync_put(ctx, payload),
            TAG_SYNC_REF => sync_ref(ctx, payload),
            _ => Err(invalid("unknown request")),
        };
        let resp = match &result {
            Ok(bytes) => {
                let mut r = Vec::with_capacity(1 + bytes.len());
                r.push(RESP_OK);
                r.extend_from_slice(bytes);
                r
            }
            Err(e) => {
                let mut r = vec![RESP_ERR];
                r.extend_from_slice(e.to_string().as_bytes());
                r
            }
        };
        write_frame(stream, &resp)?;
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

/// Serve a meta-fs write. Payload is `[path_len: u32 BE][path][body]`. For now:
/// a container `ctl` verb (run/commit/branch/tag), dispatched on this node with
/// its output captured and returned.
fn meta_write(ctx: &Ctx<'_>, payload: &[u8]) -> io::Result<Vec<u8>> {
    use hako::RouteTarget;
    if payload.len() < 4 {
        return Err(invalid("malformed write request"));
    }
    let plen = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
    let rest = &payload[4..];
    if rest.len() < plen {
        return Err(invalid("malformed write request"));
    }
    let path =
        std::str::from_utf8(&rest[..plen]).map_err(|_| invalid("write path is not UTF-8"))?;
    let body = &rest[plen..];
    match RouteTarget::parse(path) {
        RouteTarget::Container { name, path: sub } if sub == "ctl" => {
            let mut buf = Vec::new();
            crate::cmd::files::dispatch_ctl(ctx, &name, body, &mut buf)?;
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
        if payload.len() < 4 {
            return Err(invalid("malformed put batch"));
        }
        let len = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
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
    if rest.len() != HASH_LEN {
        return Err(invalid("malformed ref request"));
    }
    let commit = Hash(rest.try_into().unwrap());
    if !ctx.state.list_containers()?.iter().any(|c| c == container) {
        ctx.state.create_container(container)?;
    }
    let repo = ctx.state.open_container(container)?;
    repo.write_ref(branch, commit)?;
    Ok(format!("updated {container}:{branch} -> {}", &hex(&commit.0)[..12]).into_bytes())
}

/// Decode a concatenation of 32-byte object hashes.
fn decode_hashes(bytes: &[u8]) -> io::Result<Vec<Hash>> {
    if !bytes.len().is_multiple_of(HASH_LEN) {
        return Err(invalid("malformed hash list"));
    }
    Ok(bytes
        .chunks_exact(HASH_LEN)
        .map(|c| Hash(c.try_into().unwrap()))
        .collect())
}

/// Split a `[len: u32][bytes]` UTF-8 field off the front of `buf`.
fn take_lenprefixed_str(buf: &[u8]) -> io::Result<(&str, &[u8])> {
    if buf.len() < 4 {
        return Err(invalid("truncated request"));
    }
    let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
    let rest = &buf[4..];
    if rest.len() < len {
        return Err(invalid("truncated request"));
    }
    let s = std::str::from_utf8(&rest[..len]).map_err(|_| invalid("field is not UTF-8"))?;
    Ok((s, &rest[len..]))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// `hako peer ping <name>` — handshake with a peer and report success.
pub fn ping(ctx: &Ctx<'_>, name: &str) -> io::Result<ExitCode> {
    let peer = peers::lookup(ctx, name)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {name}")))?;
    let _stream = connect_and_handshake(ctx, &peer)?;
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
    let mut stream = connect_and_handshake(ctx, &peer)?;
    let mut req = vec![TAG_META_READ];
    req.extend_from_slice(format!("/{subpath}").as_bytes());
    write_frame(&mut stream, &req)?;
    read_response(&mut stream, node)
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
    let mut stream = connect_and_handshake(ctx, &peer)?;
    let path = format!("/{subpath}");
    let mut req = vec![TAG_META_WRITE];
    req.extend_from_slice(&(path.len() as u32).to_be_bytes());
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(body);
    write_frame(&mut stream, &req)?;
    read_response(&mut stream, node)
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
    let mut stream = connect_and_handshake(ctx, &peer)?;

    // Offer every reachable hash; the peer replies with the subset it lacks.
    let mut have = Vec::with_capacity(1 + reachable.len() * HASH_LEN);
    have.push(TAG_SYNC_HAVE);
    for h in &reachable {
        have.extend_from_slice(&h.0);
    }
    write_frame(&mut stream, &have)?;
    let missing = decode_hashes(&read_ok_payload(&mut stream, node)?)?;

    // Send the missing objects, batched to stay well under MAX_FRAME.
    let store = ctx.state.store();
    let mut batch = vec![TAG_SYNC_PUT];
    let mut sent = 0usize;
    for h in &missing {
        let bytes = store
            .get(h)?
            .ok_or_else(|| invalid("a reachable object is missing locally"))?;
        if batch.len() > 1 && batch.len() + 4 + bytes.len() > PUT_BATCH_LIMIT {
            write_frame(&mut stream, &batch)?;
            read_ok_payload(&mut stream, node)?;
            batch = vec![TAG_SYNC_PUT];
        }
        batch.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        batch.extend_from_slice(&bytes);
        sent += 1;
    }
    if batch.len() > 1 {
        write_frame(&mut stream, &batch)?;
        read_ok_payload(&mut stream, node)?;
    }

    // Point the peer's ref at the commit (creating the container if needed).
    let mut req = vec![TAG_SYNC_REF];
    let container = ctx.default_container;
    req.extend_from_slice(&(container.len() as u32).to_be_bytes());
    req.extend_from_slice(container.as_bytes());
    req.extend_from_slice(&(branch.len() as u32).to_be_bytes());
    req.extend_from_slice(branch.as_bytes());
    req.extend_from_slice(&commit.0);
    write_frame(&mut stream, &req)?;
    let confirm = read_ok_payload(&mut stream, node)?;
    println!(
        "pushed {sent} objects to {node}; {}",
        String::from_utf8_lossy(&confirm)
    );
    Ok(ExitCode::SUCCESS)
}

/// Read a response frame; return its payload on success, an error otherwise.
fn read_ok_payload(stream: &mut TcpStream, node: &str) -> io::Result<Vec<u8>> {
    let resp = read_frame(stream)?;
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
fn read_response(stream: &mut TcpStream, node: &str) -> io::Result<ExitCode> {
    let payload = read_ok_payload(stream, node)?;
    io::stdout().write_all(&payload)?;
    Ok(ExitCode::SUCCESS)
}

fn connect_and_handshake(ctx: &Ctx<'_>, peer: &peers::Peer) -> io::Result<TcpStream> {
    let expected = peer.verifying_key()?;
    let id = identity::load_or_create(ctx)?;
    let mut stream = TcpStream::connect(&peer.address)?;
    handshake_as_client(&mut stream, &id, &expected)?;
    Ok(stream)
}

/// Lowercase hex encoding.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello hako").unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap(), b"hello hako");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut bytes = (MAX_FRAME + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 8]);
        let mut cur = std::io::Cursor::new(bytes);
        assert!(read_frame(&mut cur).is_err());
    }

    fn id_at(dir: &std::path::Path) -> identity::Identity {
        identity::load_or_create_at(&dir.join("identity")).unwrap()
    }

    #[test]
    fn mutual_handshake_succeeds_when_both_are_registered() {
        let ds = tempfile::tempdir().unwrap();
        let dc = tempfile::tempdir().unwrap();
        let server_id = id_at(ds.path());
        let client_id = id_at(dc.path());
        let server_vk = server_id.verifying_key();
        let client_pk = client_id.verifying_key().to_bytes();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            handshake_as_server(&mut s, &server_id, |pk| *pk == client_pk)
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let client_result = handshake_as_client(&mut stream, &client_id, &server_vk);
        assert!(client_result.is_ok(), "client: {client_result:?}");
        assert!(server.join().unwrap().is_ok(), "server");
    }

    #[test]
    fn handshake_rejects_an_unregistered_client() {
        let ds = tempfile::tempdir().unwrap();
        let dc = tempfile::tempdir().unwrap();
        let server_id = id_at(ds.path());
        let client_id = id_at(dc.path());
        let server_vk = server_id.verifying_key();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // The server authorizes nobody.
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            handshake_as_server(&mut s, &server_id, |_| false)
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let client_result = handshake_as_client(&mut stream, &client_id, &server_vk);
        assert!(
            client_result.is_err(),
            "client handshake must fail when the server rejects it"
        );
        assert!(server.join().unwrap().is_err(), "server rejects");
    }

    #[test]
    fn decode_hashes_roundtrips_and_rejects_garbage() {
        let a = Hash([1u8; 32]);
        let b = Hash([2u8; 32]);
        let mut buf = Vec::new();
        buf.extend_from_slice(&a.0);
        buf.extend_from_slice(&b.0);
        assert_eq!(decode_hashes(&buf).unwrap(), vec![a, b]);
        assert!(
            decode_hashes(&[0u8; 5]).is_err(),
            "non-multiple of 32 rejected"
        );
    }

    #[test]
    fn take_lenprefixed_str_parses_and_rejects_truncation() {
        let mut buf = (3u32).to_be_bytes().to_vec();
        buf.extend_from_slice(b"abc");
        buf.extend_from_slice(b"tail");
        let (s, rest) = take_lenprefixed_str(&buf).unwrap();
        assert_eq!(s, "abc");
        assert_eq!(rest, b"tail");
        assert!(
            take_lenprefixed_str(&[0, 0, 0, 9, 1, 2]).is_err(),
            "a length past the end is rejected"
        );
    }
}
