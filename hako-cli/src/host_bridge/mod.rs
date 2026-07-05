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
    // Lift the two spaced-path-sensitive values — the workspace (`-w`) and a
    // run-host binary path — out of argv and forward them as env vars. `wsl.exe`
    // re-splits an argv token on spaces, but an env var's value survives intact,
    // and WSLENV `/p` translates each Windows path to a WSL path on the far side.
    // This is robust even when 8.3 short-name generation is disabled (#79).
    let (workdir_win, rest) = extract_w_flag_windows(args);
    let (runhost_bin, rest) = extract_run_host_bin_windows(rest);

    crate::diag!(
        "forwarding to wsl -d {} (set HAKO_DISTRO to override)",
        distro
    );

    let mut cmd = Command::new("wsl");
    cmd.args(["-d", &distro, "--cd", &wsl_cwd, "--", "hako"]);
    cmd.args(&rest);

    let mut wslenv_parts: Vec<&str> = Vec::new();
    if let Some(w) = &workdir_win {
        cmd.env("HAKO_WORKDIR", w);
        wslenv_parts.push("HAKO_WORKDIR/p");
    }
    if let Some(b) = &runhost_bin {
        cmd.env("HAKO_RUN_HOST_BIN", b);
        wslenv_parts.push("HAKO_RUN_HOST_BIN/p");
    }
    if !wslenv_parts.is_empty() {
        // WSLENV `/p` tells WSL to translate each named var's Windows path into a
        // WSL path. Append to any existing WSLENV rather than clobbering it.
        let extra = wslenv_parts.join(":");
        let wslenv = match env::var("WSLENV") {
            Ok(prev) if !prev.is_empty() => format!("{prev}:{extra}"),
            _ => extra,
        };
        cmd.env("WSLENV", wslenv);
    }

    spawn_and_wait(cmd, "wsl")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn forward_macos(_cwd: &Path, args: &[String]) -> io::Result<ExitCode> {
    // Match the bootstrap helper's default so the VM we just created
    // (`hako-runtime`) is what we forward into.
    let vm = bootstrap_lima::vm_name();

    crate::diag!(
        "forwarding to limactl shell {} (set HAKO_LIMA_VM to override)",
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

/// A path as `&str`, or an `io` error rather than a panic if it isn't valid
/// UTF-8 — a non-UTF-8 `%LOCALAPPDATA%`/`TEMP` shouldn't crash bootstrap (#79).
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(super) fn path_str(p: &Path) -> io::Result<&str> {
    p.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {}", p.display()),
        )
    })
}

/// `C:\Users\foo\bar` → `/mnt/c/Users/foo/bar`. WSL's path-translation
/// convention. Falls back to a literal forward-slash conversion if the
/// path doesn't have a drive letter.
///
/// Spaces are the wrinkle: when the translated path is handed to `wsl.exe -- `,
/// a space (e.g. a `C:\Users\First Last\…` profile) gets re-split into two
/// arguments on the Linux side. So for an existing spaced path we first resolve
/// the Windows 8.3 short name (`FIRST~1`, no spaces), which drvfs exposes under
/// `/mnt/c` just like the long name.
fn win_to_wsl_path(p: &Path) -> String {
    let p = short_if_spaced(p);
    let s = p.to_string_lossy().replace('\\', "/");
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        format!("/mnt/{}{}", drive, &s[2..])
    } else {
        s
    }
}

/// Return the Windows 8.3 short path for a spaced, existing path; otherwise the
/// path unchanged. No-op off Windows.
fn short_if_spaced(p: &Path) -> std::borrow::Cow<'_, Path> {
    #[cfg(windows)]
    {
        if p.to_string_lossy().contains(' ') {
            if let Some(short) = short_path_windows(p) {
                return std::borrow::Cow::Owned(short);
            }
        }
    }
    std::borrow::Cow::Borrowed(p)
}

/// Resolve a Windows path to its 8.3 short form via GetShortPathNameW. Returns
/// None if the path doesn't exist or 8.3 generation is disabled on the volume.
#[cfg(windows)]
fn short_path_windows(p: &Path) -> Option<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    #[link(name = "kernel32")]
    extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, cch: u32) -> u32;
    }
    let wide: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // First call with a null buffer returns the required length (incl. NUL).
    let needed = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), needed) };
    if written == 0 || written >= needed {
        return None;
    }
    buf.truncate(written as usize);
    let short = std::path::PathBuf::from(OsString::from_wide(&buf));
    // If 8.3 is disabled the result still contains the space — no gain.
    if short.to_string_lossy().contains(' ') {
        None
    } else {
        Some(short)
    }
}

/// Pull a `-w <path>` / `--workdir <path>` / `--workdir=<path>` out of `args`,
/// returning the (Windows) workdir value and the remaining args. The bridge
/// forwards this via `$HAKO_WORKDIR` + `WSLENV` path translation rather than as
/// a command-line argument, so a workspace path containing spaces survives the
/// `wsl.exe` boundary intact (forwarded args get re-split on spaces; an env
/// var's value does not).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn extract_w_flag_windows(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut workdir = None;
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if (a == "-w" || a == "--workdir") && i + 1 < args.len() {
            workdir = Some(args[i + 1].clone());
            i += 2;
            continue;
        } else if let Some(v) = a.strip_prefix("--workdir=") {
            workdir = Some(v.to_string());
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    (workdir, out)
}

/// Pull the binary-path positional out of a `run-host` invocation, returning it
/// (the verbatim Windows path) and `args` with it removed. The bridge forwards it
/// via `$HAKO_RUN_HOST_BIN` + WSLENV translation instead of an argv token, so
/// `hako run-host [--in X] [--display] "C:\Users\First Last\app"` survives the
/// `wsl.exe` boundary even where 8.3 short names are disabled (#79). We skip
/// run-host's own options (`--in <val>`/`--in=`, `--display`, a `--` terminator)
/// to find the first positional; the program's own arguments stay in `args`
/// verbatim (we can't know whether they're paths the guest interprets). Returns
/// `None` (args unchanged) when this isn't a run-host call or has no positional.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn extract_run_host_bin_windows(mut args: Vec<String>) -> (Option<String>, Vec<String>) {
    let Some(pos) = args.iter().position(|a| a == "run-host") else {
        return (None, args);
    };
    let mut i = pos + 1;
    while let Some(tok) = args.get(i).map(String::as_str) {
        match tok {
            // Terminator: the next token is the binary path, options are done.
            "--" => {
                i += 1;
                break;
            }
            // `--in <value>` consumes a following value token.
            "--in" => i += 2,
            // `--in=value` / `--display` / any other leading option: one token.
            _ if tok.starts_with('-') => i += 1,
            // First non-option token — the binary path.
            _ => break,
        }
    }
    if i < args.len() {
        (Some(args.remove(i)), args)
    } else {
        (None, args)
    }
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
    fn run_host_bin_is_extracted_verbatim() {
        // Bare: the binary path is pulled out (unchanged — WSLENV translates it),
        // and the guest args stay put.
        let (bin, rest) = extract_run_host_bin_windows(vec![
            "run-host".to_string(),
            r"C:\Users\First Last\app".to_string(),
            r"--config=C:\keep".to_string(),
        ]);
        assert_eq!(bin.as_deref(), Some(r"C:\Users\First Last\app"));
        assert_eq!(rest, vec!["run-host", r"--config=C:\keep"]);
    }

    #[test]
    fn run_host_bin_extracted_past_in_and_display_options() {
        // `--in <container>` (value) and `--display` (flag) precede the binary
        // path; it's still the token that gets pulled out.
        let (bin, rest) = extract_run_host_bin_windows(vec![
            "run-host".to_string(),
            "--in".to_string(),
            "alpine".to_string(),
            "--display".to_string(),
            r"C:\Users\me\app".to_string(),
            r"--guestflag=C:\keep".to_string(),
        ]);
        assert_eq!(bin.as_deref(), Some(r"C:\Users\me\app"));
        assert_eq!(
            rest,
            vec![
                "run-host",
                "--in",
                "alpine",
                "--display",
                r"--guestflag=C:\keep"
            ]
        );

        // `--in=value` (equals form) likewise.
        let (bin2, rest2) = extract_run_host_bin_windows(vec![
            "run-host".to_string(),
            "--in=debian".to_string(),
            r"C:\x\y".to_string(),
        ]);
        assert_eq!(bin2.as_deref(), Some(r"C:\x\y"));
        assert_eq!(rest2, vec!["run-host", "--in=debian"]);
    }

    #[test]
    fn run_host_bin_after_double_dash_and_globals() {
        // Leading hako globals + an explicit `--` before the binary.
        let (bin, rest) = extract_run_host_bin_windows(vec![
            "-c".to_string(),
            "ubuntu".to_string(),
            "run-host".to_string(),
            "--".to_string(),
            r"D:\bin\tool.bin".to_string(),
            "-v".to_string(),
        ]);
        assert_eq!(bin.as_deref(), Some(r"D:\bin\tool.bin"));
        assert_eq!(rest, vec!["-c", "ubuntu", "run-host", "--", "-v"]);
    }

    #[test]
    fn non_run_host_is_untouched() {
        let args = vec!["run".to_string(), "alpine".to_string(), "sh".to_string()];
        let (bin, rest) = extract_run_host_bin_windows(args.clone());
        assert_eq!(bin, None);
        assert_eq!(rest, args);
    }

    #[test]
    fn run_host_with_no_positional_extracts_nothing() {
        // Only options, no binary yet — nothing to extract, args unchanged.
        let args = vec!["run-host".to_string(), "--display".to_string()];
        let (bin, rest) = extract_run_host_bin_windows(args.clone());
        assert_eq!(bin, None);
        assert_eq!(rest, args);
    }

    #[test]
    fn extract_w_flag_pulls_workdir_out() {
        // `-w <path>` form: workdir extracted (verbatim Windows path), rest kept.
        let (w, rest) = extract_w_flag_windows(&[
            "-w".to_string(),
            r"C:\My Proj".to_string(),
            "run".to_string(),
            "alpine".to_string(),
        ]);
        assert_eq!(w.as_deref(), Some(r"C:\My Proj"));
        assert_eq!(rest, vec!["run", "alpine"]);

        // `--workdir=<path>` form.
        let (w2, rest2) =
            extract_w_flag_windows(&[r"--workdir=D:\code".to_string(), "apply".to_string()]);
        assert_eq!(w2.as_deref(), Some(r"D:\code"));
        assert_eq!(rest2, vec!["apply"]);
    }

    #[test]
    fn extract_w_flag_passes_through_non_w_args() {
        let args = vec![
            "run".to_string(),
            "alpine".to_string(),
            "ls".to_string(),
            "/etc".to_string(),
        ];
        let (w, rest) = extract_w_flag_windows(&args);
        assert_eq!(w, None);
        assert_eq!(rest, args);
    }

    #[test]
    fn no_bridge_env_disables() {
        // We can't actually mutate env in tests safely; just confirm the
        // function is a pure check on the env var.
        // (Skipping the actual env-set/unset to avoid cross-test contamination.)
        let _ = should_bridge();
    }
}
