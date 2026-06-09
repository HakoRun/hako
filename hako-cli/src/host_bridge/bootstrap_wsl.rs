//! Windows-side bootstrap: detect WSL, create the `hako-runtime` distro
//! if missing, inject the embedded Linux hako binary, keep it in sync
//! with the host wrapper's version.
//!
//! The distro is hermetic — its only meaningful contents are a single
//! statically-linked hako binary at `/usr/local/bin/hako`. The user's
//! workspace lives on the Windows filesystem and is accessed inside the
//! distro via `/mnt/c/...`, so no project state lives in the distro.
//!
//! "Error with hint" philosophy: we do NOT shell out `wsl --install`
//! ourselves (that requires admin and triggers a Windows kernel update
//! and prompt-restart). When WSL is missing, we print exactly what to
//! do and exit non-zero.

use crate::host_bridge::{embedded_for_host, has_embedded_binary};
use std::env;
use std::fs;
use std::io;
use std::process::Command;

/// Stable distro name. Single per-host; multiple hako versions on one
/// machine will fight over the binary hash. Acceptable for v1.
pub(crate) fn distro_name() -> String {
    env::var("HAKO_DISTRO").unwrap_or_else(|_| "hako-runtime".into())
}

/// Idempotent bootstrap. Cheap when the distro exists and the installed
/// binary's hash matches the embedded one.
pub fn ensure_runtime() -> io::Result<()> {
    require_wsl_available()?;
    let distro = distro_name();

    if !distro_exists(&distro)? {
        if !has_embedded_binary() {
            return Err(io::Error::other(format!(
                "WSL distro {} not found and this hako wrapper has no embedded \
                 Linux binary (built without --features embedded). \
                 Either rebuild with --features embedded, or set up the distro \
                 manually:\n  \
                   wsl --install -d Ubuntu\n  \
                   wsl -d Ubuntu -- cargo install hako-cli\n  \
                 Then re-run with HAKO_DISTRO=Ubuntu.",
                distro
            )));
        }
        eprintln!("hako: setting up WSL distro {} (one-time)...", distro);
        create_distro(&distro)?;
        inject_binary(&distro)?;
        write_installed_hash(&distro, &binary_hash())?;
        eprintln!("hako: runtime ready");
        return Ok(());
    }

    // Distro exists. If we have an embedded binary, keep the installed
    // copy in sync. If we don't, trust whatever the user installed manually.
    if has_embedded_binary() {
        let want = binary_hash();
        if read_installed_hash(&distro).as_deref() != Some(&want) {
            eprintln!("hako: updating embedded binary inside {}", distro);
            inject_binary(&distro)?;
            write_installed_hash(&distro, &want)?;
        }
    }
    Ok(())
}

fn require_wsl_available() -> io::Result<()> {
    match Command::new("wsl").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(_) | Err(_) => Err(io::Error::other(
            "WSL2 not detected on this system.\n  \
             Install with (admin PowerShell):\n    \
               wsl --install\n  \
             Then sign out / restart and re-run hako.",
        )),
    }
}

fn distro_exists(name: &str) -> io::Result<bool> {
    let out = Command::new("wsl").args(["--list", "--quiet"]).output()?;
    let text = decode_wsl_utf16(&out.stdout);
    Ok(text.lines().any(|l| l.trim() == name))
}

/// Create a minimal hako-runtime distro by writing a tiny rootfs tarball
/// (just `/usr/local/bin/hako` + standard mountpoint dirs) and importing
/// it. Doesn't depend on the user having Ubuntu/Debian already installed.
fn create_distro(name: &str) -> io::Result<()> {
    let install_dir = wsl_install_dir(name)?;
    fs::create_dir_all(&install_dir)?;

    let tar_path = std::env::temp_dir().join(format!("hako-rootfs-{}.tar", name));
    write_minimal_rootfs(&tar_path)?;

    let status = Command::new("wsl")
        .args([
            "--import",
            name,
            install_dir.to_str().unwrap(),
            tar_path.to_str().unwrap(),
            "--version",
            "2",
        ])
        .status()?;
    let _ = fs::remove_file(&tar_path);
    if !status.success() {
        return Err(io::Error::other(format!(
            "wsl --import failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }

    // Disable Windows PATH pollution and interop so commands inside the
    // distro don't accidentally pick up Windows .exes.
    let wsl_conf = "[interop]\nappendWindowsPath=false\nenabled=false\n[automount]\nmountFsTab=false\n";
    run_in_distro(
        name,
        &[
            "sh",
            "-c",
            &format!(
                "mkdir -p /etc && printf '%s' '{}' > /etc/wsl.conf",
                wsl_conf.replace('\'', "'\\''")
            ),
        ],
    )?;
    Ok(())
}

/// Build a minimal rootfs.tar containing just `/usr/local/bin/hako` (the
/// embedded musl-static binary) plus a few empty mount-point dirs.
/// Statically-linked hako means we don't need a libc, shell, or any other
/// userland in the rootfs — `wsl ... -- /usr/local/bin/hako <args>`
/// invokes the binary directly.
fn write_minimal_rootfs(path: &std::path::Path) -> io::Result<()> {
    let bytes = embedded_for_host();
    if bytes.is_empty() {
        return Err(io::Error::other(
            "no embedded Linux hako binary; rebuild with --features embedded",
        ));
    }
    let file = fs::File::create(path)?;
    let mut tar = tar::Builder::new(file);

    // Empty dirs WSL/Linux expect to exist.
    for dir in &["bin", "etc", "tmp", "proc", "sys", "dev", "run", "usr", "usr/local", "usr/local/bin"] {
        let mut h = tar::Header::new_gnu();
        h.set_path(format!("{}/", dir)).map_err(io::Error::other)?;
        h.set_size(0);
        h.set_mode(0o755);
        h.set_entry_type(tar::EntryType::Directory);
        h.set_cksum();
        tar.append(&h, std::io::empty())?;
    }
    // /tmp is world-writable per convention.
    let mut h = tar::Header::new_gnu();
    h.set_path("tmp/").map_err(io::Error::other)?;
    h.set_mode(0o1777);
    h.set_size(0);
    h.set_entry_type(tar::EntryType::Directory);
    h.set_cksum();
    tar.append(&h, std::io::empty())?;

    // The hako binary itself.
    let mut h = tar::Header::new_gnu();
    h.set_path("usr/local/bin/hako").map_err(io::Error::other)?;
    h.set_size(bytes.len() as u64);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    tar.append(&h, bytes)?;

    tar.into_inner()?.sync_all()?;
    Ok(())
}

/// Pipe the embedded binary into `/usr/local/bin/hako` inside the distro,
/// replacing whatever's there. Used both at create-time and on hash
/// mismatch.
fn inject_binary(name: &str) -> io::Result<()> {
    use std::io::Write;
    let bytes = embedded_for_host();
    if bytes.is_empty() {
        return Err(io::Error::other("no embedded Linux binary to inject"));
    }
    let mut child = Command::new("wsl")
        .args([
            "-d",
            name,
            "-u",
            "root",
            "--",
            "sh",
            "-c",
            "cat > /usr/local/bin/hako && chmod +x /usr/local/bin/hako",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(bytes)?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "binary inject failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

fn binary_hash() -> String {
    // hako::Hash::of uses blake3 under the hood; reusing it avoids a
    // direct blake3 dep here.
    hako::Hash::of(embedded_for_host()).to_hex()
}

fn read_installed_hash(name: &str) -> Option<String> {
    let out = Command::new("wsl")
        .args(["-d", name, "--", "cat", "/etc/hako-version"])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn write_installed_hash(name: &str, hash: &str) -> io::Result<()> {
    run_in_distro(
        name,
        &[
            "sh",
            "-c",
            &format!("printf '%s\\n' '{}' > /etc/hako-version", hash),
        ],
    )
}

fn run_in_distro(name: &str, args: &[&str]) -> io::Result<()> {
    let mut cmd = Command::new("wsl");
    cmd.args(["-d", name, "-u", "root", "--"]).args(args);
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "wsl exec failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Where on the Windows filesystem to put the imported distro's vhdx.
/// Convention: `%LOCALAPPDATA%\hako\runtime\<distro>\`.
fn wsl_install_dir(name: &str) -> io::Result<std::path::PathBuf> {
    let base = env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    Ok(base.join("hako").join("runtime").join(name))
}

/// `wsl --list` returns UTF-16LE on Windows. Decode → UTF-8.
fn decode_wsl_utf16(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] == 0xfe {
        // explicit BOM
        return decode_utf16_le(&bytes[2..]);
    }
    // No BOM heuristic: ASCII UTF-16LE has a null byte at every odd
    // index (since ASCII chars encode as `<low><0>`). Sample the first
    // few odd positions; if they're all null, treat as UTF-16LE.
    let probe_len = bytes.len().min(16);
    let odd_nulls = (1..probe_len).step_by(2).all(|i| bytes[i] == 0);
    let has_any_odd = (1..probe_len).step_by(2).next().is_some();
    if has_any_odd && odd_nulls {
        decode_utf16_le(bytes)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn decode_utf16_le(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        units.push(u16::from_le_bytes([pair[0], pair[1]]));
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_name_default() {
        // Don't assume HAKO_DISTRO is unset across the test process; just
        // verify the env-var precedence works for an explicit override.
        std::env::set_var("HAKO_DISTRO", "test-distro");
        assert_eq!(distro_name(), "test-distro");
        std::env::remove_var("HAKO_DISTRO");
    }

    #[test]
    fn decode_utf16_with_bom() {
        // "ab\n" in UTF-16LE with BOM
        let bytes: &[u8] = &[0xff, 0xfe, b'a', 0, b'b', 0, b'\n', 0];
        assert_eq!(decode_wsl_utf16(bytes), "ab\n");
    }

    #[test]
    fn decode_utf16_without_bom() {
        let bytes: &[u8] = &[b'a', 0, b'b', 0, b'c', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let s = decode_wsl_utf16(bytes);
        assert!(s.starts_with("abc"));
    }

    #[test]
    fn decode_utf8_passthrough() {
        let bytes = b"Ubuntu\nDebian\n";
        assert_eq!(decode_wsl_utf16(bytes), "Ubuntu\nDebian\n");
    }
}
