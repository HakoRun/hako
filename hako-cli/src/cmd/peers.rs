//! Static peer registry (Phase 1 of `docs/distributed.md`): `.hako/peers.toml`
//! maps a peer name to its network address and Ed25519 public key. The address
//! is where that node's future `hako serve` listens; the pubkey is the identity
//! its handshake will authenticate. This is a *trusted, explicitly-curated*
//! fleet — peers you add, never discovered strangers.

use super::Ctx;
use crate::DOT_HAKO;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const PEERS_FILE: &str = "peers.toml";

#[derive(Serialize, Deserialize, Clone)]
pub struct Peer {
    pub address: String,
    pub pubkey: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Registry {
    #[serde(default)]
    peers: BTreeMap<String, Peer>,
}

fn registry_path(ctx: &Ctx<'_>) -> PathBuf {
    ctx.workdir.join(DOT_HAKO).join(PEERS_FILE)
}

fn load(path: &Path) -> io::Result<Registry> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("peers.toml: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e),
    }
}

fn save(path: &Path, reg: &Registry) -> io::Result<()> {
    let s = toml::to_string_pretty(reg)
        .map_err(|e| io::Error::other(format!("serialize peers: {e}")))?;
    std::fs::write(path, s)
}

/// Validate that `pubkey` is 64 hex chars decoding to a real Ed25519 key.
fn validate_pubkey(pubkey: &str) -> io::Result<()> {
    let bytes = decode_hex32(pubkey).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "pubkey must be 64 hex chars (a 32-byte Ed25519 key, as `hako id` prints)",
        )
    })?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "pubkey is not a valid Ed25519 public key",
        )
    })?;
    Ok(())
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn add_at(path: &Path, name: &str, address: &str, pubkey: &str) -> io::Result<bool> {
    if name.is_empty() || address.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: hako peer add <name> <address> <pubkey>",
        ));
    }
    validate_pubkey(pubkey)?;
    let mut reg = load(path)?;
    let existed = reg
        .peers
        .insert(
            name.to_string(),
            Peer {
                address: address.to_string(),
                pubkey: pubkey.to_string(),
            },
        )
        .is_some();
    save(path, &reg)?;
    Ok(existed)
}

fn remove_at(path: &Path, name: &str) -> io::Result<()> {
    let mut reg = load(path)?;
    if reg.peers.remove(name).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no peer named {name}"),
        ));
    }
    save(path, &reg)
}

/// `hako peer add <name> <address> <pubkey>`.
pub fn add(ctx: &Ctx<'_>, name: String, address: String, pubkey: String) -> io::Result<ExitCode> {
    let existed = add_at(&registry_path(ctx), &name, &address, &pubkey)?;
    println!(
        "{} peer {}",
        if existed { "updated" } else { "added" },
        name
    );
    Ok(ExitCode::SUCCESS)
}

/// `hako peer list`.
pub fn list(ctx: &Ctx<'_>) -> io::Result<ExitCode> {
    let reg = load(&registry_path(ctx))?;
    if reg.peers.is_empty() {
        println!("no peers — add one with `hako peer add <name> <address> <pubkey>`");
        return Ok(ExitCode::SUCCESS);
    }
    for (name, peer) in &reg.peers {
        println!("{}\t{}\t{}", name, peer.address, peer.pubkey);
    }
    Ok(ExitCode::SUCCESS)
}

/// `hako peer remove <name>`.
pub fn remove(ctx: &Ctx<'_>, name: String) -> io::Result<ExitCode> {
    remove_at(&registry_path(ctx), &name)?;
    println!("removed peer {}", name);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Ed25519 public key (produced by `hako id`).
    const KEY: &str = "b3ceba3c3d2e75e7455d2a1b1b164f295ae81d9d02b96baee2c2a4796a4d6c45";

    #[test]
    fn add_list_remove_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("peers.toml");
        assert!(
            !add_at(&p, "node-a", "10.0.0.1:7777", KEY).unwrap(),
            "first add is new (not an update)"
        );
        let reg = load(&p).unwrap();
        assert_eq!(reg.peers.len(), 1);
        assert_eq!(reg.peers["node-a"].address, "10.0.0.1:7777");
        assert_eq!(reg.peers["node-a"].pubkey, KEY);
        // Re-adding the same name updates in place.
        assert!(
            add_at(&p, "node-a", "10.0.0.2:7777", KEY).unwrap(),
            "second add updates"
        );
        assert_eq!(load(&p).unwrap().peers["node-a"].address, "10.0.0.2:7777");
        // Remove, then removing again errors.
        remove_at(&p, "node-a").unwrap();
        assert!(load(&p).unwrap().peers.is_empty());
        assert!(
            remove_at(&p, "node-a").is_err(),
            "removing a missing peer errors"
        );
    }

    #[test]
    fn rejects_invalid_pubkey() {
        assert!(validate_pubkey("nope").is_err(), "too short");
        assert!(
            validate_pubkey(&"zz".repeat(32)).is_err(),
            "64 chars but not hex"
        );
        assert!(validate_pubkey(KEY).is_ok(), "a real key passes");
    }

    #[test]
    fn missing_registry_loads_empty() {
        let d = tempfile::tempdir().unwrap();
        let reg = load(&d.path().join("nope.toml")).unwrap();
        assert!(reg.peers.is_empty());
    }
}
