//! `hako bundle` — package a container + command into a single self-contained
//! NATIVE executable.
//!
//! A bundle is just `[hako binary][payload][trailer]`: the hako binary with a
//! gzipped-tar payload (the pruned container store + a manifest) appended, and
//! a 16-byte magic trailer at EOF. At startup the same hako binary checks for
//! that trailer; if present it runs in "bundle mode" — extract to a cache and
//! exec the baked command — otherwise it's the normal CLI.
//!
//! Because the launcher *is* hako, the payload carries only the store, not a
//! second copy of the runtime. And because it's a native binary (not a shell
//! script), the exact same mechanism compiles to an ELF on Linux and a PE
//! (`.exe`) on Windows, with no `sh`/`tar` dependency on the target.

use super::Ctx;
use crate::DOT_HAKO;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const TRAILER_MAGIC: &[u8; 8] = b"HAKOBND1";
const TRAILER_LEN: usize = 16; // magic(8) + payload_len: u64 LE (8)
/// Set on the bundle's re-exec so the child runs as the normal CLI instead of
/// re-entering bundle mode.
const GUARD_ENV: &str = "HAKO_BUNDLE_LAUNCHED";

// ============================================================================
// Bundle mode — entered from main() before normal CLI dispatch
// ============================================================================

/// If this executable carries an appended bundle payload (and we are not
/// already inside a bundle's re-exec), extract it and run the baked command.
/// Returns `Some(exit)` when it acted as a bundle; `None` for a normal CLI run.
pub fn maybe_run_as_bundle() -> io::Result<Option<ExitCode>> {
    if std::env::var_os(GUARD_ENV).is_some() {
        return Ok(None);
    }
    let exe = std::env::current_exe()?;
    match read_appended_payload(&exe)? {
        Some(payload) => Ok(Some(launch(&payload)?)),
        None => Ok(None),
    }
}

/// Read the appended payload from `exe` if it ends with our magic trailer.
fn read_appended_payload(exe: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut f = std::fs::File::open(exe)?;
    let len = f.metadata()?.len();
    if len < TRAILER_LEN as u64 {
        return Ok(None);
    }
    f.seek(SeekFrom::End(-(TRAILER_LEN as i64)))?;
    let mut trailer = [0u8; TRAILER_LEN];
    f.read_exact(&mut trailer)?;
    if &trailer[..8] != TRAILER_MAGIC {
        return Ok(None);
    }
    let payload_len = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
    if payload_len == 0 || payload_len + TRAILER_LEN as u64 > len {
        return Ok(None);
    }
    let start = len - TRAILER_LEN as u64 - payload_len;
    f.seek(SeekFrom::Start(start))?;
    let mut payload = vec![0u8; payload_len as usize];
    f.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// Extract (once) and run the bundled container's command.
fn launch(payload: &[u8]) -> io::Result<ExitCode> {
    let id = hako::Hash::of(payload).to_hex()[..12].to_string();
    let cache = bundle_cache_dir().join(&id);
    if !cache.join(".ready").exists() {
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache)?;
        let gz = flate2::read::GzDecoder::new(payload);
        tar::Archive::new(gz).unpack(&cache)?;
        std::fs::write(cache.join(".ready"), b"")?;
    }

    // Manifest: container \0 branch \0 arg0 \0 arg1 ...
    let manifest = std::fs::read(cache.join("manifest"))?;
    let mut fields = manifest
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned());
    let container = fields.next().unwrap_or_default();
    let branch = fields.next().unwrap_or_default();
    let cmd: Vec<String> = fields.collect();

    // Re-exec ourselves as the normal CLI against the extracted workspace.
    let exe = std::env::current_exe()?;
    let mut c = Command::new(&exe);
    c.env(GUARD_ENV, "1")
        .arg("-w")
        .arg(cache.join("ws"))
        .arg("-c")
        .arg(&container)
        .arg("run")
        .arg(&branch)
        .args(&cmd);
    let status = c.status()?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

fn bundle_cache_dir() -> PathBuf {
    use std::env;
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(env::temp_dir);
    base.join("hako-bundles")
}

// ============================================================================
// Bundle creation — `hako bundle <container> [-o out] [cmd...]`
// ============================================================================

/// Build a self-contained bundle executable for `container` running `cmd`.
pub fn create(
    ctx: &Ctx<'_>,
    container: String,
    cmd: Vec<String>,
    output: PathBuf,
) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(&container).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("container '{}' not found ({})", container, e),
        )
    })?;
    let branch = repo.current_branch()?.unwrap_or_else(|| "main".into());
    let commit = repo.read_ref(&branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("branch '{}' not found in container '{}'", branch, container),
        )
    })?;

    // Stage: <tmp>/{manifest, ws/.hako}
    let stage_id =
        hako::Hash::of(format!("{container}\u{0}{branch}").as_bytes()).to_hex()[..12].to_string();
    let stage = std::env::temp_dir().join(format!("hako-bundle-stage-{stage_id}"));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;

    // Pruned workspace: only this container's reachable objects (a fresh
    // `State::init` seeds a toybox `hako` container; drop it and gc).
    let ws_dot = stage.join("ws").join(DOT_HAKO);
    let dst_state = hako::State::init(&ws_dot)?;
    let _ = dst_state.delete_container("hako");
    let dst_repo = dst_state.create_container(&container)?;
    let mut copied = 0usize;
    for h in repo.reachable_objects(commit)? {
        if dst_repo.store().has(&h)? {
            continue;
        }
        let bytes = repo
            .store()
            .get(&h)?
            .ok_or_else(|| io::Error::other(format!("source missing object {}", h.to_hex())))?;
        dst_repo.store().put(&bytes)?;
        copied += 1;
    }
    dst_repo.write_ref(&branch, commit)?;
    let _ = hako::gc(&dst_state, false);
    drop(dst_repo);
    drop(dst_state);

    // Manifest: container \0 branch \0 cmd...
    let mut manifest = Vec::new();
    manifest.extend_from_slice(container.as_bytes());
    manifest.push(0);
    manifest.extend_from_slice(branch.as_bytes());
    for a in &cmd {
        manifest.push(0);
        manifest.extend_from_slice(a.as_bytes());
    }
    std::fs::write(stage.join("manifest"), &manifest)?;

    // Pack the stage into a gzipped tar (in-process — no shell-out, and the
    // target needs no `tar`).
    let payload = {
        let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tarball = tar::Builder::new(enc);
        tarball.append_dir_all(".", &stage)?;
        tarball.into_inner()?.finish()?
    };

    // The runtime the bundle ships must be a *Linux* hako (the app always runs
    // in Linux). Prefer the embedded, cross-compiled, release-stripped binary;
    // fall back to the running binary in a dev build (itself native Linux hako).
    let embedded = crate::host_bridge::embedded_for_host();
    let base: Vec<u8> = if embedded.is_empty() {
        std::fs::read(std::env::current_exe()?)?
    } else {
        embedded.to_vec()
    };

    // Assemble: base binary + payload + trailer(magic, payload_len).
    let mut out = std::fs::File::create(&output)?;
    out.write_all(&base)?;
    out.write_all(&payload)?;
    out.write_all(TRAILER_MAGIC)?;
    out.write_all(&(payload.len() as u64).to_le_bytes())?;
    out.flush()?;
    drop(out);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755))?;
    }

    let _ = std::fs::remove_dir_all(&stage);

    let size = std::fs::metadata(&output)?.len() as f64 / (1024.0 * 1024.0);
    let shown = if cmd.is_empty() {
        "(interactive shell)".to_string()
    } else {
        cmd.join(" ")
    };
    eprintln!(
        "hako: bundled {} reachable objects for '{}'",
        copied, container
    );
    println!(
        "bundled container '{}' [{}] → {} ({:.1} MiB)",
        container,
        shown,
        output.display(),
        size
    );
    println!(
        "a single native executable; run it directly: {}",
        output.display()
    );
    Ok(ExitCode::SUCCESS)
}
