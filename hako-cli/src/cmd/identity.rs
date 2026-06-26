//! Cluster node identity (Phase 1 of `docs/distributed.md`): an Ed25519 keypair
//! stored per workspace at `.hako/identity`. The public key is the node's
//! stable id — what a peer records when it adds this node, and what the future
//! `hako serve` handshake authenticates. Gated behind the `cluster` feature, so
//! the base binary carries no crypto.

use super::Ctx;
use crate::DOT_HAKO;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::io;
use std::path::Path;
use std::process::ExitCode;

const IDENTITY_FILE: &str = "identity";

/// A node's signing identity (its Ed25519 keypair).
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// The public key — the node's stable, shareable id.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// The node id: lowercase hex of the 32-byte public key.
    pub fn node_id(&self) -> String {
        hex(self.verifying_key().to_bytes().as_slice())
    }

    /// Sign a message with this node's key (e.g. a peer's connection challenge).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.signing.sign(msg).to_bytes()
    }
}

/// Load the workspace's identity, generating and persisting one on first use.
pub fn load_or_create(ctx: &Ctx<'_>) -> io::Result<Identity> {
    load_or_create_at(&ctx.workdir.join(DOT_HAKO).join(IDENTITY_FILE))
}

/// Load (or first-time create + persist) an identity from a specific seed file.
pub fn load_or_create_at(path: &Path) -> io::Result<Identity> {
    let seed: [u8; 32] = match std::fs::read(path) {
        Ok(bytes) => bytes.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "identity file is corrupt (expected a 32-byte seed)",
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut s = [0u8; 32];
            getrandom::getrandom(&mut s)
                .map_err(|e| io::Error::other(format!("entropy for new identity: {e}")))?;
            write_secret(path, &s)?;
            s
        }
        Err(e) => return Err(e),
    };
    Ok(Identity {
        signing: SigningKey::from_bytes(&seed),
    })
}

/// Write the 32-byte secret seed, owner-only where the platform supports it.
#[cfg(unix)]
fn write_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn write_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

/// `hako id` — print this node's identity (its public key).
pub fn show(ctx: &Ctx<'_>) -> io::Result<ExitCode> {
    let id = load_or_create(ctx)?;
    println!("{}", id.node_id());
    Ok(ExitCode::SUCCESS)
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
    fn node_id_is_64_hex_chars_and_deterministic() {
        let a = Identity {
            signing: SigningKey::from_bytes(&[7u8; 32]),
        };
        let b = Identity {
            signing: SigningKey::from_bytes(&[7u8; 32]),
        };
        assert_eq!(a.node_id().len(), 64, "32-byte pubkey → 64 hex chars");
        assert!(a.node_id().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.node_id(), b.node_id(), "same seed → same id");
    }

    #[test]
    fn identity_persists_across_loads() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("identity");
        let first = load_or_create_at(&p).unwrap().node_id();
        assert!(p.exists(), "seed persisted on first use");
        let second = load_or_create_at(&p).unwrap().node_id();
        assert_eq!(first, second, "second load reuses the persisted seed");
    }
}
