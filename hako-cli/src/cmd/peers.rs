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

/// What a registered peer is allowed to do on this node. A capability tier, not
/// a set: each role includes the ones below it (`read` < `sync` < `deploy`).
///
/// - `read` — observe only: `status`, `proc/`, and fetch (`want`/`get`).
/// - `sync` — everything `read` can, plus **replicate**: push objects and move
///   refs (`have`/`put`/`ref`) and the version-control `ctl` verbs
///   (`commit`/`branch`/`tag`).
/// - `deploy` — everything `sync` can, plus **run code**: `ctl run` and the
///   push-to-deploy hook (both still also require the node's `--allow-remote-run`
///   / `--allow-deploy` master switch).
///
/// A peer with no `role` in `peers.toml` defaults to `sync` (the pre-capability
/// behaviour: registered peers could push/replicate, and `run` was already
/// flag-gated). Grant `deploy` explicitly to let a peer run code here.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    #[default]
    Sync,
    Deploy,
}

impl Role {
    /// Whether this role is permitted an operation requiring at least `needed`.
    /// The `derive(PartialOrd, Ord)` orders variants by declaration, so
    /// `Read < Sync < Deploy` — exactly the capability tiers.
    pub fn allows(self, needed: Role) -> bool {
        self >= needed
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Sync => "sync",
            Role::Deploy => "deploy",
        }
    }

    /// Parse a `--role` value.
    pub fn parse(s: &str) -> io::Result<Self> {
        match s {
            "read" => Ok(Role::Read),
            "sync" => Ok(Role::Sync),
            "deploy" => Ok(Role::Deploy),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown role '{other}' (expected read, sync, or deploy)"),
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Peer {
    pub address: String,
    pub pubkey: String,
    /// Capability tier for THIS peer when it connects to us. `#[serde(default)]`
    /// keeps pre-capability `peers.toml` entries valid (they decode to `sync`).
    #[serde(default)]
    pub role: Role,
}

impl Peer {
    /// Decode the stored hex pubkey into an Ed25519 verifying key.
    pub fn verifying_key(&self) -> io::Result<ed25519_dalek::VerifyingKey> {
        let bytes = decode_hex32(&self.pubkey).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "stored pubkey is not 64 hex")
        })?;
        ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stored pubkey is not a valid key",
            )
        })
    }
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

fn add_at(path: &Path, name: &str, address: &str, pubkey: &str, role: Role) -> io::Result<bool> {
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
                role,
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

/// `hako peer add <name> <address> <pubkey> [--role read|sync|deploy]`.
pub fn add(
    ctx: &Ctx<'_>,
    name: String,
    address: String,
    pubkey: String,
    role: Role,
) -> io::Result<ExitCode> {
    let existed = add_at(&registry_path(ctx), &name, &address, &pubkey, role)?;
    println!(
        "{} peer {} (role={})",
        if existed { "updated" } else { "added" },
        name,
        role.as_str()
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
        println!(
            "{}\t{}\t{}\t{}",
            name,
            peer.role.as_str(),
            peer.address,
            peer.pubkey
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `hako peer remove <name>`.
pub fn remove(ctx: &Ctx<'_>, name: String) -> io::Result<ExitCode> {
    remove_at(&registry_path(ctx), &name)?;
    println!("removed peer {}", name);
    Ok(ExitCode::SUCCESS)
}

/// Look up a registered peer by name.
pub fn lookup(ctx: &Ctx<'_>, name: &str) -> io::Result<Option<Peer>> {
    Ok(load(&registry_path(ctx))?.peers.get(name).cloned())
}

/// The (X25519 key, role) of every registered peer — the daemon authorizes a
/// connecting peer against these keys AND learns its capability tier in one
/// lookup. The registry stores Ed25519 identities, so each is converted to the
/// X25519 form the Noise IK handshake learns. Malformed entries are skipped
/// rather than failing the whole check.
fn registered_x25519_roles(ctx: &Ctx<'_>) -> io::Result<Vec<([u8; 32], Role)>> {
    let reg = load(&registry_path(ctx))?;
    let mut out = Vec::new();
    for peer in reg.peers.values() {
        if let Ok(vk) = peer.verifying_key() {
            if let Some(x) = crate::cmd::identity::ed25519_pubkey_to_x25519(&vk.to_bytes()) {
                out.push((x, peer.role));
            }
        }
    }
    Ok(out)
}

/// The role of the peer whose Noise static is `x25519`, or `None` if no
/// registered peer matches (i.e. the connecting peer is not authorized).
pub fn role_for_x25519(ctx: &Ctx<'_>, x25519: &[u8; 32]) -> Option<Role> {
    registered_x25519_roles(ctx)
        .ok()?
        .into_iter()
        .find(|(k, _)| k == x25519)
        .map(|(_, role)| role)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Ed25519 public key (produced by `hako id`).
    const KEY: &str = "b3ceba3c3d2e75e7455d2a1b1b164f295ae81d9d02b96baee2c2a4796a4d6c45";

    #[test]
    fn role_parse_and_hierarchy() {
        assert_eq!(Role::parse("read").unwrap(), Role::Read);
        assert_eq!(Role::parse("sync").unwrap(), Role::Sync);
        assert_eq!(Role::parse("deploy").unwrap(), Role::Deploy);
        assert!(Role::parse("admin").is_err());
        assert_eq!(Role::default(), Role::Sync);
        // Tiers: deploy ⊇ sync ⊇ read.
        assert!(Role::Deploy.allows(Role::Read));
        assert!(Role::Deploy.allows(Role::Sync));
        assert!(Role::Deploy.allows(Role::Deploy));
        assert!(Role::Sync.allows(Role::Read));
        assert!(!Role::Sync.allows(Role::Deploy));
        assert!(Role::Read.allows(Role::Read));
        assert!(!Role::Read.allows(Role::Sync));
    }

    #[test]
    fn peer_without_role_field_defaults_to_sync() {
        // A pre-capability peers.toml (no `role`) must still parse — its peers
        // decode to `sync`, the historical push/replicate behaviour.
        let toml = format!("[peers.node-a]\naddress = \"10.0.0.1:7777\"\npubkey = \"{KEY}\"\n");
        let reg: Registry = toml::from_str(&toml).unwrap();
        assert_eq!(reg.peers["node-a"].role, Role::Sync);
        // An explicit role round-trips.
        let toml = format!("[peers.b]\naddress = \"x\"\npubkey = \"{KEY}\"\nrole = \"read\"\n");
        assert_eq!(
            toml::from_str::<Registry>(&toml).unwrap().peers["b"].role,
            Role::Read
        );
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("peers.toml");
        assert!(
            !add_at(&p, "node-a", "10.0.0.1:7777", KEY, Role::Sync).unwrap(),
            "first add is new (not an update)"
        );
        let reg = load(&p).unwrap();
        assert_eq!(reg.peers.len(), 1);
        assert_eq!(reg.peers["node-a"].address, "10.0.0.1:7777");
        assert_eq!(reg.peers["node-a"].pubkey, KEY);
        assert_eq!(reg.peers["node-a"].role, Role::Sync);
        // Re-adding the same name updates in place, including its role.
        assert!(
            add_at(&p, "node-a", "10.0.0.2:7777", KEY, Role::Deploy).unwrap(),
            "second add updates"
        );
        assert_eq!(load(&p).unwrap().peers["node-a"].role, Role::Deploy);
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
