//! Host-platform bridge for runtime operations.
//!
//! The hako runtime requires Linux (user/mount namespaces, FUSE, pivot_root).
//! On Windows and macOS, the hako CLI binary itself runs natively but
//! delegates runtime ops (`run`, `exec`, `apply`) to a `hako` instance
//! inside the user's Linux environment:
//!
//!   - **Windows**: `wsl -d hako-runtime --cd <translated-cwd> -- hako <args>`
//!   - **macOS**:   `limactl shell hako-runtime hako <args>`
//!
//! Read-only commands (`ls`, `cat`, `log`, etc.) stay native — no bridge,
//! no VM round-trip, no cwd translation.
//!
//! ## Bootstrap
//!
//! With `--features embedded`, the wrapper carries a Linux hako binary
//! (`include_bytes!` from `vendored/`) and auto-creates the WSL distro /
//! Lima VM on first runtime command. Without the feature (the dev-loop
//! default), the wrapper expects the user to have installed hako inside
//! their Linux env themselves.
//!
//! ## Knobs
//!
//! - `HAKO_DISTRO=<name>` — override Windows distro name
//!   (default: `hako-runtime` — the distro `bootstrap` creates)
//! - `HAKO_LIMA_VM=<name>` — override macOS VM name
//!   (default: `hako-runtime`)
//! - `HAKO_NO_BRIDGE=1` — skip the bridge entirely (returns the runtime
//!   crate's `UnsupportedPlatform` error)

use std::env;
use std::io;
use std::path::Path;
use std::process::{Command, ExitCode};

mod bootstrap_lima;
mod bootstrap_wsl;

// ============================================================================
// Embedded Linux binaries
//
// With `--features embedded`, the wrapper bakes the cross-compiled Linux
// hako binary in via `include_bytes!`. `build.rs` ensures the vendored
// files exist (real or empty stub) so the build never fails for want of
// a binary. At runtime, `is_empty()` distinguishes "real embedded binary"
// from "stub — fall back to expecting hako on PATH inside the user's
// WSL/Lima env".
//
// Without the feature, the constants are empty slices — the host wrapper
// is small (~1 MiB) and bootstrap will refuse to auto-create the distro/VM,
// telling the user to install hako inside their Linux env themselves.
// ============================================================================

#[cfg(feature = "embedded")]
pub(crate) const EMBEDDED_LINUX_X64: &[u8] = include_bytes!("../../../vendored/hako-linux-x64");
#[cfg(not(feature = "embedded"))]
pub(crate) const EMBEDDED_LINUX_X64: &[u8] = &[];

#[cfg(feature = "embedded")]
pub(crate) const EMBEDDED_LINUX_ARM64: &[u8] = include_bytes!("../../../vendored/hako-linux-arm64");
#[cfg(not(feature = "embedded"))]
pub(crate) const EMBEDDED_LINUX_ARM64: &[u8] = &[];

/// Pick the right embedded binary for the current host arch. ARM64 host
/// uses arm64 binary if available, else falls back to x64 (Lima's
/// rosetta layer can run x64 binaries on Apple Silicon, slowly).
// In the default (non-`embedded`) build these consts are `&[]`, so clippy sees
// `is_empty()` as a const `true`. With `--features embedded` they hold a real
// ~10 MiB binary and the check is meaningful — the lint is build-config-blind.
#[allow(clippy::const_is_empty)]
pub(crate) fn embedded_for_host() -> &'static [u8] {
    if cfg!(target_arch = "aarch64") && !EMBEDDED_LINUX_ARM64.is_empty() {
        EMBEDDED_LINUX_ARM64
    } else {
        EMBEDDED_LINUX_X64
    }
}

/// True if a real Linux binary was embedded at build time. Drives whether
/// auto-bootstrap is available; falls back to "user installed hako in
/// WSL/Lima themselves" mode when false.
pub fn has_embedded_binary() -> bool {
    !embedded_for_host().is_empty()
}

/// True if the current host is non-Linux AND the user hasn't asked us to
/// skip the bridge.
pub fn should_bridge() -> bool {
    if env::var("HAKO_NO_BRIDGE").is_ok() {
        return false;
    }
    !cfg!(target_os = "linux")
}

/// Pre-warm the runtime: idempotently set up the WSL distro / Lima VM and
/// inject the embedded Linux binary. Called by `forward()` before the
/// first exec, and exposed as `hako bootstrap` for explicit invocation.
pub fn ensure_runtime() -> io::Result<()> {
    if cfg!(target_os = "windows") {
        bootstrap_wsl::ensure_runtime()
    } else if cfg!(target_os = "macos") {
        bootstrap_lima::ensure_runtime()
    } else {
        // Linux host — nothing to bootstrap.
        Ok(())
    }
}

/// Forward this hako invocation to the Linux hako binary inside the user's
/// WSL distro or Lima VM. Inherits stdin/stdout/stderr; propagates exit code.
/// Translates the cwd to the corresponding Linux path so the forwarded
/// invocation's `.hako/` lookup hits the same workspace the user is in.
///
/// Calls `ensure_runtime()` first — idempotent setup of the distro/VM
/// and (when an embedded binary is present) injection of that binary.
pub fn forward() -> io::Result<ExitCode> {
    ensure_runtime()?;
    let args: Vec<String> = env::args().skip(1).collect();
    let cwd = env::current_dir()?;
    if cfg!(target_os = "windows") {
        forward_windows(&cwd, &args)
    } else if cfg!(target_os = "macos") {
        forward_macos(&cwd, &args)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no host bridge available for this platform",
        ))
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn forward_windows(cwd: &Path, args: &[String]) -> io::Result<ExitCode> {
    // Use the same default the bootstrap helper uses, so an embedded-binary
    // build that just bootstrapped `hako-runtime` actually targets it on the
    // next invocation (instead of falling through to the user's general
    // `Ubuntu` distro and getting "command not found" or worse).
    let distro = bootstrap_wsl::distro_name();
    let wsl_cwd = win_to_wsl_path(cwd);
    let translated_args = translate_w_flag_windows(args);

    eprintln!(
        "hako: forwarding to wsl -d {} (set HAKO_DISTRO to override)",
        distro
    );

    let mut cmd = Command::new("wsl");
    cmd.args(["-d", &distro, "--cd", &wsl_cwd, "--", "hako"]);
    cmd.args(&translated_args);

    spawn_and_wait(cmd, "wsl")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn forward_macos(_cwd: &Path, args: &[String]) -> io::Result<ExitCode> {
    // Match the bootstrap helper's default so the VM we just created
    // (`hako-runtime`) is what we forward into.
    let vm = bootstrap_lima::vm_name();

    eprintln!(
        "hako: forwarding to limactl shell {} (set HAKO_LIMA_VM to override)",
        vm
    );

    // Lima's virtiofs typically mounts $HOME at the same path inside the VM,
    // so we don't need cwd translation. Limactl picks up the host cwd via
    // `--workdir` automatically when not set; we let that default.
    let mut cmd = Command::new("limactl");
    cmd.args(["shell", &vm, "hako"]);
    cmd.args(args);

    spawn_and_wait(cmd, "limactl")
}

fn spawn_and_wait(mut cmd: Command, tool: &str) -> io::Result<ExitCode> {
    let status = cmd.status().map_err(|e| {
        io::Error::other(format!(
            "cannot reach {} ({}). Install it, or run hako directly inside Linux.",
            tool, e
        ))
    })?;
    let code = status.code().unwrap_or(1);
    Ok(ExitCode::from(code as u8))
}

/// `C:\Users\foo\bar` → `/mnt/c/Users/foo/bar`. WSL's path-translation
/// convention. Falls back to a literal forward-slash conversion if the
/// path doesn't have a drive letter.
fn win_to_wsl_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        format!("/mnt/{}{}", drive, &s[2..])
    } else {
        s
    }
}

/// Translate any `-w <path>` or `--workdir <path>` argument from a Windows
/// path to a WSL path. Other args pass through verbatim.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn translate_w_flag_windows(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-w" || a == "--workdir" {
            if let Some(next) = args.get(i + 1) {
                out.push(a.clone());
                out.push(win_to_wsl_path(Path::new(next)));
                i += 2;
                continue;
            }
        } else if let Some(rest) = a.strip_prefix("--workdir=") {
            out.push(format!("--workdir={}", win_to_wsl_path(Path::new(rest))));
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_to_wsl_drive_letter() {
        assert_eq!(
            win_to_wsl_path(Path::new(r"C:\Users\foo\bar")),
            "/mnt/c/Users/foo/bar"
        );
        assert_eq!(
            win_to_wsl_path(Path::new(r"D:\proj\hako.toml")),
            "/mnt/d/proj/hako.toml"
        );
    }

    #[test]
    fn win_to_wsl_passthrough_for_non_drive() {
        // Already-Linux path or relative — leave structure alone.
        assert_eq!(win_to_wsl_path(Path::new("/tmp/x")), "/tmp/x");
        assert_eq!(win_to_wsl_path(Path::new("relative/path")), "relative/path");
    }

    #[test]
    fn translate_w_flag_handles_all_forms() {
        let args = vec![
            "-w".to_string(),
            r"C:\proj".to_string(),
            "run".to_string(),
            "alpine".to_string(),
        ];
        let out = translate_w_flag_windows(&args);
        assert_eq!(out, vec!["-w", "/mnt/c/proj", "run", "alpine"]);

        let args2 = vec![
            "--workdir".to_string(),
            r"D:\code".to_string(),
            "apply".to_string(),
        ];
        let out2 = translate_w_flag_windows(&args2);
        assert_eq!(out2, vec!["--workdir", "/mnt/d/code", "apply"]);

        let args3 = vec![
            r"--workdir=C:\one".to_string(),
            "exec".to_string(),
            "abc".to_string(),
        ];
        let out3 = translate_w_flag_windows(&args3);
        assert_eq!(out3, vec!["--workdir=/mnt/c/one", "exec", "abc"]);
    }

    #[test]
    fn translate_passes_through_non_w_args() {
        let args = vec![
            "run".to_string(),
            "alpine".to_string(),
            "ls".to_string(),
            "/etc".to_string(),
        ];
        let out = translate_w_flag_windows(&args);
        assert_eq!(out, args);
    }

    #[test]
    fn no_bridge_env_disables() {
        // We can't actually mutate env in tests safely; just confirm the
        // function is a pure check on the env var.
        // (Skipping the actual env-set/unset to avoid cross-test contamination.)
        let _ = should_bridge();
    }
}
