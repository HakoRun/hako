//! The cluster wire format: frame codec, request/response tags, and the
//! byte-level parse helpers. Pure — no Noise channel, no TCP, no `Ctx` — so it
//! unit-tests over `std::io::Cursor` and can be reused by any transport.

use hako::Hash;
use std::io::{self, Read, Write};

/// Cap on a single application message, guarding against a bogus length prefix.
pub(crate) const MAX_FRAME: u32 = 1 << 20;

/// Request tags (first byte of a post-handshake request frame).
pub(crate) const TAG_META_READ: u8 = 1;
/// `MetaWrite` request: payload is `[path_len: u32 BE][path][body]`.
pub(crate) const TAG_META_WRITE: u8 = 2;
/// Data plane (push). `SyncHave` payload is a list of 32-byte object hashes; the
/// reply is the subset the server is missing. `SyncPut` payload is
/// `[obj_len: u32][obj]...`. `SyncRef` payload is
/// `[container_len: u32][container][branch_len: u32][branch][commit: 32]`.
pub(crate) const TAG_SYNC_HAVE: u8 = 3;
pub(crate) const TAG_SYNC_PUT: u8 = 4;
pub(crate) const TAG_SYNC_REF: u8 = 5;
/// Pull (fetch). `SyncWant` payload is
/// `[container_len: u32][container][branch_len: u32][branch]`; the reply is
/// `[tip commit: 32]` followed by the tip's reachable object hashes (32 bytes
/// each). `SyncGet` payload is a list of 32-byte hashes; the reply is
/// `[obj_len: u32][obj]...` for the longest requested prefix that fits under a
/// batch cap — the client stores those (in order) and re-requests the rest.
pub(crate) const TAG_SYNC_WANT: u8 = 6;
pub(crate) const TAG_SYNC_GET: u8 = 7;
/// Response status (first byte of a response frame).
pub(crate) const RESP_OK: u8 = 0;
pub(crate) const RESP_ERR: u8 = 1;

pub(crate) const HASH_LEN: usize = 32;
/// Flush a `SyncPut` batch before it would approach `MAX_FRAME`.
pub(crate) const PUT_BATCH_LIMIT: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

pub(crate) fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub(crate) fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
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

pub(crate) fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
pub(crate) fn denied(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}

pub(crate) fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// Read the first `N` bytes of `b` as a fixed array, erroring (never panicking)
/// if `b` is too short — so the network parse path can't be turned into a remote
/// panic by a future refactor that drops a length guard.
pub(crate) fn first_array<const N: usize>(b: &[u8], what: &'static str) -> io::Result<[u8; N]> {
    b.get(..N)
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| invalid(what))
}

/// Decode a concatenation of 32-byte object hashes.
pub(crate) fn decode_hashes(bytes: &[u8]) -> io::Result<Vec<Hash>> {
    if !bytes.len().is_multiple_of(HASH_LEN) {
        return Err(invalid("malformed hash list"));
    }
    bytes
        .chunks_exact(HASH_LEN)
        .map(|c| {
            <[u8; HASH_LEN]>::try_from(c)
                .map(Hash)
                .map_err(|_| invalid("malformed hash list"))
        })
        .collect()
}

/// Split a `[len: u32][bytes]` UTF-8 field off the front of `buf`.
pub(crate) fn take_lenprefixed_str(buf: &[u8]) -> io::Result<(&str, &[u8])> {
    let len = u32::from_be_bytes(first_array::<4>(buf, "truncated request")?) as usize;
    let rest = &buf[4..];
    if rest.len() < len {
        return Err(invalid("truncated request"));
    }
    let s = std::str::from_utf8(&rest[..len]).map_err(|_| invalid("field is not UTF-8"))?;
    Ok((s, &rest[len..]))
}

/// Lowercase hex encoding.
pub(crate) fn hex(bytes: &[u8]) -> String {
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

    #[test]
    fn first_array_is_panic_free_on_short_input() {
        assert!(first_array::<4>(&[1, 2, 3], "x").is_err()); // too short
        assert!(first_array::<4>(&[], "x").is_err()); // empty
        assert_eq!(
            first_array::<4>(&[1, 2, 3, 4, 5], "x").unwrap(),
            [1u8, 2, 3, 4]
        );
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
