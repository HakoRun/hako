//! hako-runtime — container transformation for hako.
//!
//! This crate provides the "become a container" runtime: namespaces, mount
//! setup, `pivot_root`, and detached-container state management. It powers
//! the user-facing commands `hako is`, `hako as`, and `hako run`.
//!
//! The bulk of the runtime is Linux-only (it uses Linux user/mount namespaces
//! and `pivot_root`). Detached-container state management (the `containers`
//! module) is cross-platform — it's just files-and-JSON — so `hako ps`,
//! `hako logs`, and `hako rm` can list and inspect state from any platform
//! even when the actual runtime can't run there.
//!
//! # Status by platform
//!
//! | Platform | `is`/`as`/`run` | `ps`/`logs`/`rm` |
//! |----------|-----------------|------------------|
//! | Linux    | ✓ native        | ✓                |
//! | macOS    | ✗ (use VM)      | ✓ (read state)   |
//! | Windows  | ✗ (use WSL2)    | ✓ (read state)   |
//!
//! On non-Linux platforms, the runtime functions return
//! `RuntimeError::UnsupportedPlatform`, with a friendly hint at the
//! supported alternatives.

pub mod instances;
pub mod proc;

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
pub mod transform;

use std::path::PathBuf;

/// A host-to-container bind mount declared via `-v host:container[:ro]`.
///
/// On Linux, the runtime bind-mounts `host` at `container` inside the
/// containerized rootfs after the standard mounts but before `pivot_root`,
/// honoring `readonly`. On non-Linux, the field is accepted but ignored
/// (the stub returns `UnsupportedPlatform` anyway).
#[derive(Clone, Debug, Default)]
pub struct VolumeMount {
    pub host: PathBuf,
    pub container: String,
    pub readonly: bool,
    /// Sub-paths (relative to `container`) to shadow with an empty read-only
    /// tmpfs immediately after the bind mount, hiding host content the workload
    /// must not see or modify through this mount. Used to keep the implicit
    /// `/workspace` mount from exposing the workspace's own `.hako/` store,
    /// refs, and (cluster builds) identity key. Empty for user `-v` mounts —
    /// an explicit bind is the caller's choice.
    pub mask: Vec<String>,
}

impl VolumeMount {
    /// Parse a `host:container[:ro]` spec.
    /// Examples:
    ///   `/home/me/src:/workspace`
    ///   `/home/me/src:/workspace:ro`
    pub fn parse(spec: &str) -> Result<Self, String> {
        // Two forms: HOST:CONTAINER or HOST:CONTAINER:ro. Counting `:` works
        // because container paths are absolute (no `:` inside).
        let parts: Vec<&str> = spec.splitn(3, ':').collect();
        if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(format!(
                "bad volume spec {:?}: want HOST:CONTAINER or HOST:CONTAINER:ro",
                spec
            ));
        }
        let readonly = match parts.get(2) {
            None => false,
            Some(&"ro") => true,
            Some(&"rw") => false,
            Some(other) => return Err(format!("unknown volume mode {:?}", other)),
        };
        if !parts[1].starts_with('/') {
            return Err(format!("container path must be absolute: {:?}", parts[1]));
        }
        Ok(VolumeMount {
            host: PathBuf::from(parts[0]),
            container: parts[1].into(),
            readonly,
            mask: Vec::new(),
        })
    }
}

/// Networking mode for a `run` workload (`hako run --network`).
///
/// `Isolated` (the default) unshares an empty network namespace: the workload
/// has no connectivity and cannot accept a connection. `Host` skips the
/// network unshare so the workload shares the host's network — it can listen
/// on and connect from host ports like an ordinary process (weaker isolation;
/// P0-1 of docs/push-to-deploy.md). Rootless port publishing (`-p` via
/// pasta/slirp4netns) is a separate, later mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    #[default]
    Isolated,
    Host,
}

impl Network {
    /// Parse the `--network` flag / `network` config value.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Network::Isolated),
            "host" => Ok(Network::Host),
            other => Err(format!(
                "unknown network mode '{other}' (expected 'none' or 'host')"
            )),
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub mod transform {
    //! Stub implementation for non-Linux platforms.
    //!
    //! The real runtime requires Linux user/mount namespaces and `pivot_root`.
    //! On macOS and Windows, callers should run hako inside a Linux VM
    //! (Docker Desktop's Linux VM, Lima, OrbStack, WSL2, ...).

    use crate::RuntimeError;
    use hako::{Hash, Repo};
    use std::path::Path;

    pub fn become_container(
        _repo: &Repo<'_>,
        _branch: &str,
        _volumes: &[crate::VolumeMount],
        _network: crate::Network,
    ) -> Result<i32, RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform {
            operation: "hako run",
            hint: "Container transformation requires Linux. \
                   On macOS/Windows, run hako inside a Linux VM.",
        })
    }

    pub fn run_container(
        _repo: &Repo<'_>,
        _branch: &str,
        _command: Vec<String>,
        _volumes: &[crate::VolumeMount],
        _network: crate::Network,
    ) -> Result<i32, RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform {
            operation: "hako run",
            hint: "Container transformation requires Linux. \
                   On macOS/Windows, run hako inside a Linux VM.",
        })
    }

    pub fn run_container_rw(
        _repo: &Repo<'_>,
        _branch: &str,
        _command: Vec<String>,
        _volumes: &[crate::VolumeMount],
    ) -> Result<(i32, Hash), RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform {
            operation: "hako apply (RW runtime)",
            hint: "Mutating a container via the runtime requires Linux. \
                   On macOS/Windows, run hako inside a Linux VM.",
        })
    }

    pub fn run_container_detached(
        _repo: &Repo<'_>,
        _branch: &str,
        _command: Option<Vec<String>>,
        _volumes: &[crate::VolumeMount],
        _network: crate::Network,
    ) -> Result<String, RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform {
            operation: "hako run -d",
            hint: "Container transformation requires Linux. \
                   On macOS/Windows, run hako inside a Linux VM.",
        })
    }

    pub fn exec_in_instance(
        _workdir: &Path,
        _id: &str,
        _command: Vec<String>,
    ) -> Result<i32, RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform {
            operation: "hako exec",
            hint: "Entering a running instance's namespaces requires Linux. \
                   On macOS/Windows, run hako inside a Linux VM.",
        })
    }

    // Suppress unused-import warnings for items only used on Linux.
    #[allow(dead_code)]
    fn _suppress_unused() {
        let _: Option<Hash> = None;
        let _: Option<&Path> = None;
    }
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug)]
pub enum RuntimeError {
    /// Operation isn't supported on this platform (typically: container
    /// transformation on non-Linux).
    UnsupportedPlatform {
        operation: &'static str,
        hint: &'static str,
    },
    /// Branch doesn't exist or doesn't resolve to a tree.
    BranchNotFound(String),
    /// I/O error during runtime setup.
    Io(std::io::Error),
    /// Container with this id doesn't exist.
    InstanceNotFound(String),
    /// Generic runtime error with a message.
    Other(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::UnsupportedPlatform { operation, hint } => {
                write!(
                    f,
                    "{} is not supported on this platform: {}",
                    operation, hint
                )
            }
            RuntimeError::BranchNotFound(name) => write!(f, "branch not found: {}", name),
            RuntimeError::Io(e) => write!(f, "io error: {}", e),
            RuntimeError::InstanceNotFound(id) => write!(f, "instance not found: {}", id),
            RuntimeError::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RuntimeError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RuntimeError {
    fn from(e: std::io::Error) -> Self {
        RuntimeError::Io(e)
    }
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let v = VolumeMount::parse("/host/src:/workspace").unwrap();
        assert_eq!(v.host, PathBuf::from("/host/src"));
        assert_eq!(v.container, "/workspace");
        assert!(!v.readonly);
    }

    #[test]
    fn parse_readonly() {
        let v = VolumeMount::parse("/h:/c:ro").unwrap();
        assert!(v.readonly);
    }

    #[test]
    fn parse_explicit_rw() {
        let v = VolumeMount::parse("/h:/c:rw").unwrap();
        assert!(!v.readonly);
    }

    #[test]
    fn parse_rejects_relative_container() {
        assert!(VolumeMount::parse("/h:relative").is_err());
    }

    #[test]
    fn parse_rejects_missing_target() {
        assert!(VolumeMount::parse("/h").is_err());
        assert!(VolumeMount::parse("/h:").is_err());
        assert!(VolumeMount::parse(":").is_err());
    }

    #[test]
    fn parse_rejects_unknown_mode() {
        assert!(VolumeMount::parse("/h:/c:weird").is_err());
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert_eq!(Network::parse("none").unwrap(), Network::Isolated);
        assert_eq!(Network::parse("host").unwrap(), Network::Host);
        assert_eq!(Network::default(), Network::Isolated);
    }

    #[test]
    fn parse_rejects_unknown() {
        // `bridge`/`slirp` are future modes, not silent aliases of anything.
        assert!(Network::parse("bridge").is_err());
        assert!(Network::parse("slirp").is_err());
        assert!(Network::parse("").is_err());
    }
}
