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
    // Checked bound: `len >= TRAILER_LEN` above, so `len - TRAILER_LEN` can't
    // underflow. Writing it this way avoids the `payload_len + TRAILER_LEN`
    // overflow that a hostile trailer could use to wrap past the check and then
    // attempt a ~2^64-byte allocation (#58).
    if payload_len == 0 || payload_len > len - TRAILER_LEN as u64 {
        return Ok(None);
    }
    let start = len - TRAILER_LEN as u64 - payload_len;
    f.seek(SeekFrom::Start(start))?;
    let mut payload = vec![0u8; payload_len as usize];
    f.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// True if `p` is a plain relative path with no component that could escape the
/// extraction root (no absolute/root/prefix component, no `..`).
fn is_safe_relative(p: &Path) -> bool {
    use std::path::Component;
    p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// True if a sym/hard-link `target` declared at archive path `entry_path` would
/// resolve *outside* the extraction root. Absolute targets always escape;
/// relative targets are walked while tracking depth below the root.
fn link_target_escapes(entry_path: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return true;
    }
    // Depth of the directory containing the link, relative to the root.
    let mut depth: isize = entry_path
        .parent()
        .map(|p| {
            p.components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .count() as isize
        })
        .unwrap_or(0);
    for c in target.components() {
        match c {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

/// Extract the gzip-tar `payload` into `dst` with hardened defaults: never
/// overwrite an existing path, reject any entry whose path escapes `dst`, and
/// reject sym/hard links whose target would resolve outside `dst`. This is the
/// only place an untrusted archive touches the real filesystem, so safety is
/// enforced here rather than delegated to the `tar` crate's defaults.
fn unpack_hardened(payload: &[u8], dst: &Path) -> io::Result<()> {
    let gz = flate2::read::GzDecoder::new(payload);
    let mut archive = tar::Archive::new(gz);
    archive.set_overwrite(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !is_safe_relative(&path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bundle: unsafe archive entry path: {}", path.display()),
            ));
        }
        if matches!(
            entry.header().entry_type(),
            tar::EntryType::Symlink | tar::EntryType::Link
        ) {
            let target = entry.link_name()?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bundle: link entry with no target",
                )
            })?;
            if link_target_escapes(&path, &target) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "bundle: unsafe link target: {} -> {}",
                        path.display(),
                        target.display()
                    ),
                ));
            }
        }
        // `unpack_in` adds the tar crate's own dst-containment check as a backstop.
        if !entry.unpack_in(dst)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bundle: refused unsafe archive entry: {}", path.display()),
            ));
        }
    }
    Ok(())
}

/// Extract (once) and run the bundled container's command.
fn launch(payload: &[u8]) -> io::Result<ExitCode> {
    let id = hako::Hash::of(payload).to_hex()[..12].to_string();
    let base = bundle_cache_dir();
    // The cache path is predictable (`id` is derived from the public payload), so
    // make the cache root a private directory we own *before* trusting any
    // `.ready` marker in it — otherwise a pre-seeded cache could make us run an
    // attacker's baked command/workspace (#58). Everything below is then created
    // inside our own 0700 directory, which only we can write.
    ensure_private_dir(&base)?;
    let cache = base.join(&id);
    // `.ready` is written INSIDE the temp dir *before* the dir is moved into
    // place, so the cache appears atomically and complete — never half-
    // extracted. Concurrent launches each extract to their own temp dir and
    // race to rename; the loser sees the cache already present and discards.
    if !cache.join(".ready").exists() {
        let tmp = base.join(format!(".{id}.tmp.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        unpack_hardened(payload, &tmp)?;
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
    let wants_display = fields.next().as_deref() == Some("1");
    let cmd: Vec<String> = fields.collect();

    // Display passthrough is RUNNER-consented, not bundle-author-controlled.
    // The baked `display` bit is only a request: granting it lets this (foreign)
    // executable reach the host's X11/Wayland session — screenshots, keystrokes.
    // So we never force it on from the manifest; the runner opts in by setting
    // HAKO_DISPLAY=1 themselves (which flows through to the runtime). If the
    // bundle wants a GUI but the runner hasn't consented, run headless and say
    // how to allow it — rather than silently widening the sandbox.
    let runner_consented =
        std::env::var_os("HAKO_DISPLAY").is_some_and(|v| v != "0" && !v.is_empty());
    if wants_display && !runner_consented {
        crate::diag!(
            "this bundle can render a GUI on your desktop, which grants it \
             access to your display session (X11/Wayland — it could read your \
             screen and keystrokes). To allow it, re-run with HAKO_DISPLAY=1."
        );
    }

    // Re-exec ourselves as the normal CLI against the extracted workspace. We do
    // NOT pass --display; if the runner set HAKO_DISPLAY it is inherited here and
    // reaches the runtime on its own.
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
    // On Windows prefer LOCALAPPDATA (HOME is usually unset outside Git Bash).
    #[cfg(windows)]
    if let Some(d) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(d).join("hako-bundles");
    }
    if let Some(d) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(d).join("hako-bundles");
    }
    if let Some(h) = env::var_os("HOME") {
        return PathBuf::from(h).join(".cache").join("hako-bundles");
    }
    // No per-user cache home (systemd services, cron, minimal containers). Use a
    // per-uid name under the (possibly world-shared) temp dir — never a shared
    // `hako-bundles` a local attacker on a multi-user host could pre-seed. Its
    // ownership is verified by `ensure_private_dir` regardless (#58).
    #[cfg(unix)]
    {
        env::temp_dir().join(format!("hako-bundles-{}", current_euid()))
    }
    #[cfg(not(unix))]
    {
        env::temp_dir().join("hako-bundles")
    }
}

#[cfg(unix)]
fn current_euid() -> u32 {
    // SAFETY: geteuid() always succeeds and has no preconditions.
    unsafe { libc::geteuid() }
}

/// Ensure `dir` is a private directory owned by the current user before we trust
/// anything inside it. Create it `0700` if absent; if it already exists, refuse
/// (fail closed) when it is a symlink, not owned by us, or accessible to group/
/// other. The bundle cache path is predictable (derived from the public payload
/// hash), so without this a local attacker could pre-seed it and make us run
/// their baked command against their workspace (#58).
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {} // verify below
        Err(e) => return Err(e),
    }
    let md = std::fs::symlink_metadata(dir)?; // does NOT follow a symlink
    let deny = |msg: String| Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
    if md.file_type().is_symlink() || !md.is_dir() {
        return deny(format!(
            "refusing bundle cache {}: not a real directory (possible attack)",
            dir.display()
        ));
    }
    let me = current_euid();
    if md.uid() != me {
        return deny(format!(
            "refusing bundle cache {}: owned by uid {}, not {}",
            dir.display(),
            md.uid(),
            me
        ));
    }
    if md.mode() & 0o077 != 0 {
        return deny(format!(
            "refusing bundle cache {}: accessible to group/other (mode {:03o})",
            dir.display(),
            md.mode() & 0o777
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
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
    force: bool,
    display: bool,
) -> io::Result<ExitCode> {
    if !force && output.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists; pass --force to overwrite",
                output.display()
            ),
        ));
    }
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
        crate::diag!(
            "warning: building a {} bundle without an embedded Linux \
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
    crate::diag!("bundled {} reachable objects for '{}'", copied, container);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `[prefix][payload][magic(8)][payload_len: u64 LE]` — the on-disk bundle layout.
    fn write_bundle(path: &Path, prefix: &[u8], payload: &[u8]) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(TRAILER_MAGIC);
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        std::fs::write(path, &bytes).unwrap();
    }

    #[test]
    fn appended_payload_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fakehako");
        let payload = b"my bundle payload bytes";
        write_bundle(&path, b"ELF...pretend binary...", payload);
        assert_eq!(
            read_appended_payload(&path).unwrap().as_deref(),
            Some(&payload[..])
        );
    }

    #[test]
    fn no_trailer_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        std::fs::write(&path, b"a normal binary, no trailer").unwrap();
        assert!(read_appended_payload(&path).unwrap().is_none());
    }

    #[test]
    fn bogus_payload_len_reads_as_none() {
        // Magic present but the declared payload length overruns the file: reject
        // (guards the byte-offset arithmetic against a corrupt/forged trailer).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad");
        let mut bytes = b"short".to_vec();
        bytes.extend_from_slice(TRAILER_MAGIC);
        bytes.extend_from_slice(&9_999_999u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_appended_payload(&path).unwrap().is_none());
    }

    #[test]
    fn overflowing_payload_len_reads_as_none() {
        // A hostile `payload_len` chosen so `payload_len + TRAILER_LEN` would wrap
        // must still be rejected — not slip past the bound and attempt a giant
        // allocation (#58).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evil");
        let mut bytes = b"prefix".to_vec();
        bytes.extend_from_slice(TRAILER_MAGIC);
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_appended_payload(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_creates_0700_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("hako-bundles");
        ensure_private_dir(&base).unwrap();
        assert_eq!(
            std::fs::metadata(&base).unwrap().permissions().mode() & 0o777,
            0o700,
            "cache dir must be private"
        );
        // Idempotent on our own 0700 directory.
        ensure_private_dir(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_group_or_world_access_and_symlinks() {
        use std::os::unix::fs::{symlink, DirBuilderExt};
        let d = tempfile::tempdir().unwrap();
        // A group/other-accessible dir is refused (an attacker could have pre-
        // created it as world-writable and seeded a cache).
        let shared = d.path().join("shared");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&shared)
            .unwrap();
        assert_eq!(
            ensure_private_dir(&shared).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // A symlink at the cache path is refused (never followed).
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        assert_eq!(
            ensure_private_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn is_safe_relative_cases() {
        assert!(is_safe_relative(Path::new("a/b/c")));
        assert!(is_safe_relative(Path::new("./a")));
        assert!(!is_safe_relative(Path::new("../escape")));
        assert!(!is_safe_relative(Path::new("a/../../b")));
        assert!(!is_safe_relative(Path::new("/abs")));
    }

    #[test]
    fn link_target_escape_cases() {
        // relative target that stays inside the tree is fine
        assert!(!link_target_escapes(
            Path::new("a/b/link"),
            Path::new("../c")
        ));
        assert!(!link_target_escapes(
            Path::new("a/b/link"),
            Path::new("c/d")
        ));
        // climbs out of the root
        assert!(link_target_escapes(Path::new("link"), Path::new("../x")));
        assert!(link_target_escapes(
            Path::new("a/link"),
            Path::new("../../x")
        ));
        // absolute always escapes (would resolve to a host path)
        assert!(link_target_escapes(
            Path::new("a/link"),
            Path::new("/etc/passwd")
        ));
    }

    /// gzip a tar built from `(path, entry_type, data, linkname)` entries.
    fn gz_tar(entries: &[(&str, tar::EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, kind, data, linkname) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_mode(0o644);
            h.set_size(data.len() as u64);
            if let Some(ln) = linkname {
                h.set_link_name(ln).unwrap();
            }
            b.append_data(&mut h, path, &data[..]).unwrap();
        }
        let tar_bytes = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn safe_tar_extracts() {
        let payload = gz_tar(&[
            ("hello.txt", tar::EntryType::Regular, b"hi", None),
            ("sub/world.txt", tar::EntryType::Regular, b"earth", None),
        ]);
        let dst = tempfile::tempdir().unwrap();
        unpack_hardened(&payload, dst.path()).unwrap();
        assert_eq!(std::fs::read(dst.path().join("hello.txt")).unwrap(), b"hi");
        assert_eq!(
            std::fs::read(dst.path().join("sub/world.txt")).unwrap(),
            b"earth"
        );
    }

    #[test]
    fn rejects_escaping_symlink_target() {
        // A symlink whose target climbs out of the extraction root is refused
        // *before* the link is created (so this is safe to run on Windows too).
        let payload = gz_tar(&[(
            "link",
            tar::EntryType::Symlink,
            b"",
            Some("../../etc/passwd"),
        )]);
        let dst = tempfile::tempdir().unwrap();
        let err = unpack_hardened(&payload, dst.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!dst.path().join("link").exists());
    }
}
