//! The Noise-encrypted transport: the mutually-authenticated `Noise_IK` handshake
//! and the `NoiseChannel` that frames application messages into <=64 KiB encrypted
//! records (`[final flag][data]` inside the AEAD, #59). Sits on `proto`'s plaintext
//! framing for the handshake only, and is otherwise independent of the server/
//! client command logic — so it unit-tests over a loopback socket.

use super::proto::{denied, invalid, read_frame, read_u32, write_frame, MAX_FRAME};
use crate::cmd::identity;
use ed25519_dalek::VerifyingKey;
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// Noise pattern for the cluster channel: mutual-auth **IK** (the initiator knows
/// the responder's static ahead of time from `peers.toml`; the responder learns
/// and authorizes the initiator during the handshake), X25519 DH, ChaCha20-Poly1305
/// AEAD, BLAKE2s. Gives confidentiality + integrity + forward secrecy, closing the
/// gap the old sign-the-nonce handshake left (it authenticated but did not bind
/// or encrypt the session — an active MITM could inject a forged `ctl`). See #40.
const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// A Noise transport message is capped at 65535 bytes; a plaintext chunk is that
/// minus the 16-byte ChaChaPoly tag.
const NOISE_MSG_MAX: usize = 65535;
const NOISE_PT_MAX: usize = NOISE_MSG_MAX - 16;
/// Data bytes carried per chunk: the plaintext room minus the 1-byte `[final]`
/// flag that authenticates the message boundary inside the AEAD (#59).
const NOISE_PT_DATA: usize = NOISE_PT_MAX - 1;
/// Cap on chunks per application message (> MAX_FRAME / NOISE_PT_MAX), bounding
/// reassembly memory against a bogus chunk count.
const MAX_APP_CHUNKS: u32 = 64;

/// Read/write timeout applied to every peer connection (server *and* client).
/// The daemon is blocking and single-threaded, so without this a peer that
/// connects and stalls (or stops reading) would wedge it indefinitely. Generous
/// enough for a burst of sync rounds; bounded so a hung connection is dropped and
/// the daemon recovers to serve the next peer.
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Apply [`IO_TIMEOUT`] to a freshly accepted or connected stream.
pub(crate) fn set_io_timeouts(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

fn noise_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("noise: {e}"))
}

/// An authenticated, encrypted channel to a peer: the TCP stream plus the Noise
/// transport state the IK handshake established. An application message is split
/// into ≤64 KiB Noise messages (the protocol's per-message limit) and sent as a
/// sequence of `[u32 ct_len][ciphertext]` records. Each record's *authenticated*
/// plaintext is `[final: u8][data…]`; `recv` reassembles until it decrypts a
/// record whose `final` flag is set. Because the terminator lives inside the
/// AEAD, tampering — a flipped byte, or a dropped / reordered / truncated chunk —
/// fails the per-message AEAD or leaves the stream with no valid final record, so
/// `recv` errors rather than returning truncated or forged bytes (#59). Nothing
/// outside the AEAD (no plaintext length or chunk count) is trusted for framing.
pub(crate) struct NoiseChannel {
    stream: TcpStream,
    transport: snow::TransportState,
}

const CHUNK_MORE: u8 = 0;
const CHUNK_FINAL: u8 = 1;

impl NoiseChannel {
    pub(crate) fn send(&mut self, plaintext: &[u8]) -> io::Result<()> {
        let mut buf = vec![0u8; NOISE_MSG_MAX];
        // Split into ≤NOISE_PT_DATA-byte data chunks; an empty message is one
        // empty final chunk (so both directions always exchange ≥1 record and
        // the Noise nonces stay in lockstep). Each Noise message's authenticated
        // plaintext is [final flag][data].
        let data_chunks: Vec<&[u8]> = if plaintext.is_empty() {
            vec![&[][..]]
        } else {
            plaintext.chunks(NOISE_PT_DATA).collect()
        };
        let last = data_chunks.len() - 1;
        let mut framed = Vec::with_capacity(NOISE_PT_MAX);
        for (i, data) in data_chunks.iter().enumerate() {
            framed.clear();
            framed.push(if i == last { CHUNK_FINAL } else { CHUNK_MORE });
            framed.extend_from_slice(data);
            let n = self
                .transport
                .write_message(&framed, &mut buf)
                .map_err(noise_err)?;
            self.stream.write_all(&(n as u32).to_be_bytes())?;
            self.stream.write_all(&buf[..n])?;
        }
        self.stream.flush()
    }

    pub(crate) fn recv(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; NOISE_MSG_MAX];
        // Read records until an authenticated `final` chunk, bounded by
        // MAX_APP_CHUNKS. A truncated stream (missing final record) hits EOF or a
        // nonce mismatch here rather than being accepted as complete.
        for _ in 0..MAX_APP_CHUNKS {
            let clen = read_u32(&mut self.stream)? as usize;
            if clen > NOISE_MSG_MAX {
                return Err(invalid("noise: message too large"));
            }
            let mut ct = vec![0u8; clen];
            self.stream.read_exact(&mut ct)?;
            let n = self
                .transport
                .read_message(&ct, &mut buf)
                .map_err(|_| invalid("noise: decrypt failed"))?;
            let (&flag, data) = buf[..n]
                .split_first()
                .ok_or_else(|| invalid("noise: empty chunk (no flag)"))?;
            out.extend_from_slice(data);
            if out.len() > MAX_FRAME as usize {
                return Err(invalid("frame too large"));
            }
            match flag {
                CHUNK_FINAL => return Ok(out),
                CHUNK_MORE => continue,
                _ => return Err(invalid("noise: bad chunk flag")),
            }
        }
        Err(invalid("noise: too many chunks"))
    }
}

// ---------------------------------------------------------------------------
// Noise IK handshake  (handshake messages ride the plaintext [u32 len][msg]
// framing; everything after is the encrypted NoiseChannel)
// ---------------------------------------------------------------------------

/// Server (Noise IK responder). Reads the initiator's first message — which
/// carries its static key encrypted to us — hands that key to `authorized` (a
/// registered peer?), replies, and upgrades to the encrypted transport. The
/// static passed to `authorized` is the peer's **X25519** key; the registry
/// stores Ed25519, so the caller compares against the converted form.
pub(crate) fn handshake_as_server(
    mut stream: TcpStream,
    id: &identity::Identity,
    authorized: impl Fn(&[u8; 32]) -> bool,
) -> io::Result<NoiseChannel> {
    // `snow::Builder` borrows the secret, so keep it in a local until `build_*`
    // copies it into the handshake state.
    let params = NOISE_PARAMS.parse().map_err(noise_err)?;
    let secret = id.x25519_secret();
    let mut hs = snow::Builder::new(params)
        .local_private_key(&secret)
        .build_responder()
        .map_err(noise_err)?;
    let mut buf = vec![0u8; NOISE_MSG_MAX];

    // msg1 (initiator → responder): e, es, s, ss — carries the initiator's static.
    let m1 = read_frame(&mut stream)?;
    hs.read_message(&m1, &mut buf)
        .map_err(|_| denied("handshake: bad client hello"))?;
    let rs: [u8; 32] = hs
        .get_remote_static()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| invalid("handshake: missing client static"))?;
    if !authorized(&rs) {
        return Err(denied("client is not a registered peer"));
    }

    // msg2 (responder → initiator): e, ee, se.
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    write_frame(&mut stream, &buf[..n])?;

    let transport = hs.into_transport_mode().map_err(noise_err)?;
    Ok(NoiseChannel { stream, transport })
}

/// Client (Noise IK initiator). `expected` is the server's registered Ed25519
/// identity; IK requires knowing the responder's static up front, so we convert
/// it to X25519 and encrypt msg1 to it — which also authenticates the server
/// (only the real holder of that key can complete the handshake).
pub(crate) fn handshake_as_client(
    mut stream: TcpStream,
    id: &identity::Identity,
    expected: &VerifyingKey,
) -> io::Result<NoiseChannel> {
    let server_x = identity::ed25519_pubkey_to_x25519(&expected.to_bytes())
        .ok_or_else(|| invalid("peer pubkey is not a valid point"))?;
    let params = NOISE_PARAMS.parse().map_err(noise_err)?;
    let secret = id.x25519_secret();
    let mut hs = snow::Builder::new(params)
        .local_private_key(&secret)
        .remote_public_key(&server_x)
        .build_initiator()
        .map_err(noise_err)?;
    let mut buf = vec![0u8; NOISE_MSG_MAX];

    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    write_frame(&mut stream, &buf[..n])?;

    let m2 = read_frame(&mut stream)?;
    hs.read_message(&m2, &mut buf)
        .map_err(|_| denied("peer failed the identity check"))?;

    let transport = hs.into_transport_mode().map_err(noise_err)?;
    Ok(NoiseChannel { stream, transport })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn read_frame_honors_read_timeout_on_silent_peer() {
        // A peer that connects but never sends must not hang the reader forever:
        // with a read timeout set, read_frame returns promptly with a timeout error.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = TcpStream::connect(addr).unwrap(); // connects, sends nothing
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(150)))
            .unwrap();
        let start = std::time::Instant::now();
        let err = read_frame(&mut server).unwrap_err();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "read should have timed out promptly, not blocked"
        );
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "expected a timeout error kind, got {:?}",
            err.kind()
        );
    }

    fn id_at(dir: &std::path::Path) -> identity::Identity {
        identity::load_or_create_at(&dir.join("identity")).unwrap()
    }

    #[test]
    fn mutual_handshake_establishes_a_working_channel() {
        let ds = tempfile::tempdir().unwrap();
        let dc = tempfile::tempdir().unwrap();
        let server_id = id_at(ds.path());
        let client_id = id_at(dc.path());
        let server_vk = server_id.verifying_key();
        // The server authorizes the client by its X25519 static; the registry
        // stores Ed25519, so the daemon compares against the converted form.
        let client_x = client_id.x25519_public();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let (s, _) = listener.accept().unwrap();
            let mut ch = handshake_as_server(s, &server_id, move |x| *x == client_x)?;
            let greeting = ch.recv()?; // decrypt the client's first message
            ch.send(b"pong")?; // reply over the same encrypted channel
            Ok(greeting)
        });
        let stream = TcpStream::connect(addr).unwrap();
        let mut ch = handshake_as_client(stream, &client_id, &server_vk)
            .expect("client handshake should succeed");
        ch.send(b"ping").unwrap();
        assert_eq!(ch.recv().unwrap(), b"pong", "encrypted reply round-trips");
        assert_eq!(
            server.join().unwrap().unwrap(),
            b"ping",
            "server decrypted the client's greeting"
        );
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
        // The server authorizes nobody: it reads the client's static from msg1,
        // rejects it, and hangs up without completing the handshake.
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handshake_as_server(s, &server_id, |_| false).map(|_| ())
        });
        let stream = TcpStream::connect(addr).unwrap();
        let client_result = handshake_as_client(stream, &client_id, &server_vk);
        assert!(
            client_result.is_err(),
            "client handshake must fail when the server rejects it"
        );
        assert!(server.join().unwrap().is_err(), "server rejects");
    }

    #[test]
    fn recv_rejects_a_tampered_ciphertext() {
        let ds = tempfile::tempdir().unwrap();
        let dc = tempfile::tempdir().unwrap();
        let server_id = id_at(ds.path());
        let client_id = id_at(dc.path());
        let server_vk = server_id.verifying_key();
        let client_x = client_id.x25519_public();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handshake_as_server(s, &server_id, move |x| *x == client_x).unwrap()
        });
        let stream = TcpStream::connect(addr).unwrap();
        let mut client_ch = handshake_as_client(stream, &client_id, &server_vk).unwrap();
        let mut server_ch = server.join().unwrap();

        // Encrypt one transport frame exactly as `send` does (authenticated
        // plaintext = [final flag][data]), but flip a single ciphertext byte in
        // flight. The AEAD tag must fail closed rather than surface corrupt bytes.
        let mut buf = vec![0u8; NOISE_MSG_MAX];
        let mut framed = vec![CHUNK_FINAL];
        framed.extend_from_slice(b"top secret");
        let n = server_ch
            .transport
            .write_message(&framed, &mut buf)
            .unwrap();
        buf[0] ^= 0xff;
        server_ch
            .stream
            .write_all(&(n as u32).to_be_bytes())
            .unwrap();
        server_ch.stream.write_all(&buf[..n]).unwrap();
        server_ch.stream.flush().unwrap();

        let err = client_ch
            .recv()
            .expect_err("a tampered ciphertext must be rejected");
        assert!(
            err.to_string().contains("decrypt"),
            "expected a decrypt failure, got: {err}"
        );
    }

    #[test]
    fn recv_rejects_a_truncated_multi_chunk_message() {
        // #59: an application message split across chunks must not be accepted
        // when the trailing chunk(s) are dropped. The server emits a valid
        // non-final chunk and hangs up; `recv` must error (the authenticated
        // terminator never arrives) rather than return the partial plaintext.
        let ds = tempfile::tempdir().unwrap();
        let dc = tempfile::tempdir().unwrap();
        let server_id = id_at(ds.path());
        let client_id = id_at(dc.path());
        let server_vk = server_id.verifying_key();
        let client_x = client_id.x25519_public();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handshake_as_server(s, &server_id, move |x| *x == client_x).unwrap()
        });
        let stream = TcpStream::connect(addr).unwrap();
        let mut client_ch = handshake_as_client(stream, &client_id, &server_vk).unwrap();
        let mut server_ch = server.join().unwrap();

        // One authentic non-final chunk, then hang up: the final chunk that would
        // complete the message never arrives.
        let mut buf = vec![0u8; NOISE_MSG_MAX];
        let mut framed = vec![CHUNK_MORE];
        framed.extend_from_slice(b"first half only");
        let n = server_ch
            .transport
            .write_message(&framed, &mut buf)
            .unwrap();
        server_ch
            .stream
            .write_all(&(n as u32).to_be_bytes())
            .unwrap();
        server_ch.stream.write_all(&buf[..n]).unwrap();
        server_ch.stream.flush().unwrap();
        drop(server_ch); // close the connection with no final chunk

        client_ch
            .recv()
            .expect_err("a truncated multi-chunk message must be rejected, not returned");
    }
}
