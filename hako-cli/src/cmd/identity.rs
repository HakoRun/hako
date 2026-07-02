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

    /// This node's X25519 static **secret** for the Noise (IK) cluster handshake,
    /// derived from the Ed25519 seed exactly as libsodium's
    /// `crypto_sign_ed25519_sk_to_curve25519`: clamp the low 32 bytes of
    /// SHA-512(seed). Once Noise replaces the bespoke handshake the Ed25519 key
    /// has no signing use left, so this single-purpose derivation carries no
    /// sign-vs-DH key-reuse hazard (issue #40).
    pub fn x25519_secret(&self) -> [u8; 32] {
        use sha2::{Digest, Sha512};
        let h = Sha512::digest(self.signing.to_bytes());
        let mut s = [0u8; 32];
        s.copy_from_slice(&h[..32]);
        s[0] &= 248;
        s[31] &= 127;
        s[31] |= 64;
        s
    }

    /// This node's X25519 static **public** key — the Montgomery form of its
    /// Ed25519 public key. Infallible: the node's own key is a valid point.
    /// Test-only: the daemon authorizes *peers'* statics (via
    /// [`ed25519_pubkey_to_x25519`]) and never needs its own.
    #[cfg(test)]
    pub fn x25519_public(&self) -> [u8; 32] {
        ed25519_pubkey_to_x25519(&self.verifying_key().to_bytes())
            .expect("own Ed25519 public key is a valid Edwards point")
    }
}

/// Convert an Ed25519 public key to its X25519 (Montgomery-u) form, matching
/// libsodium's `crypto_sign_ed25519_pk_to_curve25519`. Returns `None` if the
/// bytes are not a valid compressed Edwards point, so a corrupt registry entry
/// can never panic the handshake. Used to authenticate a peer's Noise static
/// against its registered Ed25519 identity.
pub fn ed25519_pubkey_to_x25519(pk: &[u8; 32]) -> Option<[u8; 32]> {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    Some(
        CompressedEdwardsY(*pk)
            .decompress()?
            .to_montgomery()
            .to_bytes(),
    )
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
    fn x25519_conversion_agrees_on_dh() {
        use curve25519_dalek::montgomery::MontgomeryPoint;
        let a = Identity {
            signing: SigningKey::from_bytes(&[3u8; 32]),
        };
        let b = Identity {
            signing: SigningKey::from_bytes(&[9u8; 32]),
        };
        // The derived secret's basepoint mult equals the converted public.
        assert_eq!(
            MontgomeryPoint::mul_base_clamped(a.x25519_secret()).to_bytes(),
            a.x25519_public()
        );
        // A Diffie-Hellman between the two identities agrees from both sides —
        // the end-to-end proof the conversion is mutually consistent (exactly the
        // agreement the Noise handshake relies on).
        let ab = MontgomeryPoint(b.x25519_public()).mul_clamped(a.x25519_secret());
        let ba = MontgomeryPoint(a.x25519_public()).mul_clamped(b.x25519_secret());
        assert_eq!(ab, ba);
        assert_ne!(ab.to_bytes(), [0u8; 32]);
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
