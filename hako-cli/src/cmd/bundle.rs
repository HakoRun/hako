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
    let base = bundle_cache_dir();
    let cache = base.join(&id);
    // `.ready` is written INSIDE the temp dir *before* the dir is moved into
    // place, so the cache appears atomically and complete — never half-
    // extracted. Concurrent launches each extract to their own temp dir and
    // race to rename; the loser sees the cache already present and discards.
    if !cache.join(".ready").exists() {
        std::fs::create_dir_all(&base)?;
        let tmp = base.join(format!(".{id}.tmp.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let gz = flate2::read::GzDecoder::new(payload);
        tar::Archive::new(gz).unpack(&tmp)?;
        std::fs::write(tmp.join(".ready"), b"")?;
        // Atomically publish by renaming our temp dir into place.
        if std::fs::rename(&tmp, &cache).is_err() {
            if cache.join(".ready").exists() {
                // Another process published first — discard our temp.
                let _ = std::fs::remove_dir_all(&tmp);
            } else {
                // A stale/partial cache is in the way (e.g. an older hako, or a
                // crash). Replace it, then retry the publish once.
                let _ = std::fs::remove_dir_all(&cache);
                std::fs::rename(&tmp, &cache).map_err(|e| {
                    io::Error::other(format!("failed to publish bundle cache: {e}"))
                })?;
            }
        }
    }

    // Manifest: container \0 branch \0 display("1"/"0") \0 arg0 \0 arg1 ...
    let manifest = std::fs::read(cache.join("manifest"))?;
    let mut fields = manifest
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned());
    let container = fields.next().unwrap_or_default();
    let branch = fields.next().unwrap_or_default();
    let display = fields.next().as_deref() == Some("1");
    let cmd: Vec<String> = fields.collect();

    // Re-exec ourselves as the normal CLI against the extracted workspace.
    let exe = std::env::current_exe()?;
    let mut c = Command::new(&exe);
    c.env(GUARD_ENV, "1")
        .arg("-w")
        .arg(cache.join("ws"))
        .arg("-c")
        .arg(&container)
        .arg("run");
    if display {
        c.arg("--display");
    }
    c.arg(&branch).args(&cmd);
    let status = c.status()?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

fn bundle_cache_dir() -> PathBuf {
    use std::env;
    // On Windows prefer LOCALAPPDATA (HOME is usually unset outside Git Bash).
    #[cfg(windows)]
    if let Some(d) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(d).join("hako-bundles");
    }
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
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
    display: bool,
) -> io::Result<ExitCode> {
    // Bake display passthrough in if asked, or if the workspace's hako.toml
    // opts in. Off by default — a bundle runs headless unless the author
    // chose otherwise.
    let display = display || ctx.cfg.app.as_ref().is_some_and(|a| a.display);
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

    // Manifest: container \0 branch \0 display("1"/"0") \0 cmd...
    let mut manifest = Vec::new();
    manifest.extend_from_slice(container.as_bytes());
    manifest.push(0);
    manifest.extend_from_slice(branch.as_bytes());
    manifest.push(0);
    manifest.extend_from_slice(if display { b"1" } else { b"0" });
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

    // The bundle's base binary is the LAUNCHER, which must be native to the OS
    // the bundle runs on (a bundle targets the platform it's built on: an ELF
    // on Linux, a PE on Windows, a Mach-O on macOS). So we always bake the
    // host-native running binary. On Linux it is also the runtime (runs the
    // container natively); on Windows/macOS it is the launcher that bridges to
    // WSL/Lima, where — for full self-containment — it injects its OWN embedded
    // Linux binary (an `--features embedded` build). Without that feature the
    // target's WSL/Lima must already have a current hako, so warn.
    if !cfg!(target_os = "linux") && !crate::host_bridge::has_embedded_binary() {
        eprintln!(
            "hako: warning: building a {} bundle without an embedded Linux \
             runtime (--features embedded). It will run on {0}, but the target \
             machine's WSL/Lima must already have a current hako installed; it \
             is not fully self-contained.",
            std::env::consts::OS
        );
    }
    let base: Vec<u8> = std::fs::read(std::env::current_exe()?)?;

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
    // A bundle is native to the platform it was built on — say which, rather
    // than implying it's portable across OSes.
    let target = match std::env::consts::OS {
        "linux" => "Linux".to_string(),
        "windows" => "Windows (uses WSL2)".to_string(),
        "macos" => "macOS (uses Lima)".to_string(),
        other => other.to_string(),
    };
    println!(
        "a single native executable for {}; run it directly: {}",
        target,
        output.display()
    );
    Ok(ExitCode::SUCCESS)
}
