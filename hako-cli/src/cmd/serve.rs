//! The node daemon (`hako serve`) and the cluster wire protocol (Phase 2 of
//! `docs/distributed.md`), split across submodules:
//!
//! - `proto` — the pure wire format (frame codec, request/response tags, byte
//!   parse helpers). No crypto, no TCP; unit-tested over `std::io::Cursor`.
//! - `channel` — the mutually-authenticated `Noise_IK_25519_ChaChaPoly_BLAKE2s`
//!   handshake and the encrypted, forward-secret `NoiseChannel` every request and
//!   response rides inside.
//! - `server` — the `hako serve` daemon: bind + safety gate, per-connection
//!   handshake, and request dispatch (meta-fs reads/writes + the
//!   `SyncHave`/`SyncPut`/`SyncRef` push data plane).
//! - `client` — the `hako peer` / remote verbs (ping, cat, write, push), each
//!   opening a `NoiseChannel` to a peer.
//!
//! `hako peer ping <name>` does the handshake and stops (a reachability +
//! identity check); `cat /peers/...` does the handshake then a meta read.

mod channel;
mod client;
mod deploy;
mod proto;
mod server;

pub use client::{ping, remote_cat, remote_fetch, remote_push, remote_write};
pub use server::serve;
