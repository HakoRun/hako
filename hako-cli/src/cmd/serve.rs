//! The node daemon (`hako serve`) and the first authenticated wire exchange
//! (Phase 2 of `docs/distributed.md`). Today: a TCP listener that answers an
//! identity challenge — a peer sends a random nonce, the node signs it with its
//! Ed25519 key. `hako peer ping <name>` is the client: it verifies the
//! signature against the public key it has *registered* for that peer, proving
//! the node on the other end holds the matching private key.
//!
//! This is one-way authentication (the caller verifies the callee), enough for
//! `ping`; mutual auth and the control/data protocols build on this framing.

use super::Ctx;
use crate::cmd::{identity, peers};
use ed25519_dalek::{Signature, Verifier};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;

/// Cap on a single frame's payload. Handshake frames are tiny; this guards
/// against a bogus length prefix forcing a huge allocation.
const MAX_FRAME: u32 = 64 * 1024;
const NONCE_LEN: usize = 32;
const SIG_LEN: usize = 64;

/// Write a length-prefixed frame: u32 big-endian length, then the payload.
fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one length-prefixed frame, rejecting an oversized length.
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

/// `hako serve [--addr ...]` — listen and answer identity challenges.
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
            Ok(stream) => {
                if let Err(e) = answer_challenge(stream, &id) {
                    eprintln!("hako serve: connection error: {e}");
                }
            }
            Err(e) => eprintln!("hako serve: accept error: {e}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Server side of the handshake: read the peer's nonce, return our signature.
fn answer_challenge(mut stream: TcpStream, id: &identity::Identity) -> io::Result<()> {
    let nonce = read_frame(&mut stream)?;
    if nonce.len() != NONCE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "challenge nonce must be 32 bytes",
        ));
    }
    write_frame(&mut stream, &id.sign(&nonce))
}

/// `hako peer ping <name>` — connect and verify the peer proves its identity.
pub fn ping(ctx: &Ctx<'_>, name: &str) -> io::Result<ExitCode> {
    let peer = peers::lookup(ctx, name)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no peer named {name}")))?;
    let expected = peer.verifying_key()?;
    let mut stream = TcpStream::connect(&peer.address)?;
    // Challenge the peer with a fresh nonce; a valid reply must sign exactly it.
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|e| io::Error::other(format!("nonce: {e}")))?;
    write_frame(&mut stream, &nonce)?;
    let sig_bytes = read_frame(&mut stream)?;
    let sig: [u8; SIG_LEN] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "signature must be 64 bytes"))?;
    expected
        .verify(&nonce, &Signature::from_bytes(&sig))
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer {name} failed the identity check (signature did not verify)"),
            )
        })?;
    println!("peer {name} ({}) verified", peer.address);
    Ok(ExitCode::SUCCESS)
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

    #[test]
    fn challenge_handshake_over_loopback_verifies() {
        let d = tempfile::tempdir().unwrap();
        let id = identity::load_or_create_at(&d.path().join("identity")).unwrap();
        let expected = id.verifying_key();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            answer_challenge(stream, &id).unwrap();
        });

        let mut stream = TcpStream::connect(addr).unwrap();
        let nonce = [42u8; NONCE_LEN];
        write_frame(&mut stream, &nonce).unwrap();
        let sig_bytes = read_frame(&mut stream).unwrap();
        let sig: [u8; SIG_LEN] = sig_bytes.as_slice().try_into().unwrap();

        assert!(
            expected
                .verify(&nonce, &Signature::from_bytes(&sig))
                .is_ok(),
            "the node's signature over our nonce verifies against its pubkey"
        );
        assert!(
            expected
                .verify(b"a different message", &Signature::from_bytes(&sig))
                .is_err(),
            "the signature is bound to the challenged nonce"
        );
        server.join().unwrap();
    }
}
