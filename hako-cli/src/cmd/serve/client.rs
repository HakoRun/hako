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
