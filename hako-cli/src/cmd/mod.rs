//! Per-command handlers. Each submodule groups commands by topic.
//!
//! Handlers take a `Ctx` (shared workspace state) plus their clap-extracted
//! arguments, and return `io::Result<ExitCode>`. The dispatcher in `main.rs`
//! is just a clap match table that calls these.

use hako::{Config, Session, State};
use std::path::Path;

pub mod apply;
pub mod files;
pub mod maintenance;
pub mod nav;
pub mod oci;
pub mod runtime;
pub mod sync;
pub mod vc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod mount;

/// Shared per-invocation context. Built once in `run()` after parsing CLI
/// args, then passed by reference to each handler.
pub struct Ctx<'a> {
    pub state: &'a State,
    pub session: &'a Session,
    pub default_container: &'a str,
    pub workdir: &'a Path,
    #[allow(dead_code)]
    pub cfg: &'a Config,
}
