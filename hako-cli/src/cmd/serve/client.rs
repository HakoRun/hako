//! Client side of the cluster protocol: the `hako peer` / remote verbs — ping,
//! remote cat/write, and push — each opening an authenticated `NoiseChannel` to a
//! peer and driving the wire protocol.

use super::channel::*;
use super::proto::*;
use crate::cmd::{identity, peers, Ctx};
use hako::{ChunkStore, Hash};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;

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

/// `hako peer fetch <node> <branch>` — the pull half of push: ask a peer for the
/// branch tip + its reachable object hashes, download the objects we lack (in
/// bounded, verified batches), then fast-forward the local ref. FF-only — refuses
/// if the peer's tip doesn't descend from the local one.
pub fn remote_fetch(ctx: &Ctx<'_>, node: &str, branch: &str) -> io::Result<ExitCode> {
    let peer = peers::lookup(ctx, node)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {node}")))?;
    let container = ctx.default_container;
    if !ctx.state.list_containers()?.iter().any(|c| c == container) {
        ctx.state.create_container(container)?;
    }
    let repo = ctx.state.open_container(container)?;
    let local_tip = repo.read_ref(branch)?;
    let mut ch = connect_and_handshake(ctx, &peer)?;

    // Step 1: WANT the tip and its reachable object hashes.
    let mut want = vec![TAG_SYNC_WANT];
    want.extend_from_slice(&(container.len() as u32).to_be_bytes());
    want.extend_from_slice(container.as_bytes());
    want.extend_from_slice(&(branch.len() as u32).to_be_bytes());
    want.extend_from_slice(branch.as_bytes());
    ch.send(&want)?;
    let reply = read_ok_payload(&mut ch, node)?;
    let tip = Hash(first_array::<HASH_LEN>(&reply, "malformed want reply")?);
    let reachable = decode_hashes(&reply[HASH_LEN..])?;

    if local_tip == Some(tip) {
        println!(
            "{node}: already up to date ({container}:{branch} at {})",
            &tip.to_hex()[..12]
        );
        return Ok(ExitCode::SUCCESS);
    }

    // Step 2: GET the objects we lack, in bounded batches, verifying each against
    // the hash we asked for (`put` re-hashes, so integrity is checked; the order
    // check catches a peer that returns the wrong object for a slot).
    let store = ctx.state.store();
    let mut missing: Vec<Hash> = Vec::new();
    for h in reachable {
        if !store.has(&h)? {
            missing.push(h);
        }
    }
    // Hashes per request, bounded so the request frame stays under MAX_FRAME.
    const GET_REQUEST_MAX: usize = 8192;
    let mut fetched = 0usize;
    let mut idx = 0;
    while idx < missing.len() {
        let end = (idx + GET_REQUEST_MAX).min(missing.len());
        let mut req = vec![TAG_SYNC_GET];
        for h in &missing[idx..end] {
            req.extend_from_slice(&h.0);
        }
        ch.send(&req)?;
        let objs = read_ok_payload(&mut ch, node)?;
        let mut p = &objs[..];
        let mut got = 0;
        while !p.is_empty() {
            let len = u32::from_be_bytes(first_array::<4>(p, "malformed get reply")?) as usize;
            p = &p[4..];
            if p.len() < len {
                return Err(invalid("malformed get reply"));
            }
            let (obj, rest) = p.split_at(len);
            if store.put(obj)? != missing[idx + got] {
                return Err(invalid("fetched object does not match the requested hash"));
            }
            got += 1;
            p = rest;
        }
        if got == 0 {
            return Err(invalid("peer returned no objects for a non-empty request"));
        }
        idx += got;
        fetched += got;
    }

    // The tip's full closure is now local — fast-forward-only, mirroring `sync_ref`.
    if let Some(local) = local_tip {
        if repo.common_ancestor(local, tip)? != Some(local) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing non-fast-forward fetch of {container}:{branch} \
                     (local tip {} is not an ancestor of {})",
                    &local.to_hex()[..12],
                    &tip.to_hex()[..12]
                ),
            ));
        }
    }
    repo.write_ref(branch, tip)?;
    println!(
        "fetched {fetched} objects from {node}; {container}:{branch} -> {}",
        &tip.to_hex()[..12]
    );
    Ok(ExitCode::SUCCESS)
}

/// Read a response message; return its payload on success, an error otherwise.
fn read_ok_payload<S: Read + Write>(ch: &mut NoiseChannel<S>, node: &str) -> io::Result<Vec<u8>> {
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
fn read_response<S: Read + Write>(ch: &mut NoiseChannel<S>, node: &str) -> io::Result<ExitCode> {
    let payload = read_ok_payload(ch, node)?;
    io::stdout().write_all(&payload)?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn connect_and_handshake(
    ctx: &Ctx<'_>,
    peer: &peers::Peer,
) -> io::Result<NoiseChannel<TcpStream>> {
    let expected = peer.verifying_key()?;
    let id = identity::load_or_create(ctx)?;
    let stream = TcpStream::connect(&peer.address)?;
    set_io_timeouts(&stream)?;
    handshake_as_client(stream, &id, &expected)
}
