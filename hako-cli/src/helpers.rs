//! Shared helpers across CLI commands: path manipulation, ref resolution,
//! conflict marker synthesis, host-fs cfg-gated wrappers, and a couple of
//! tiny formatters.

use hako::fs::{decode_entry, DirEntry, FileEntry};
use hako::store::ChunkStore;
use hako::tree::{Conflict, DiffEntry, Value};
use hako::{Hash, Repo, RouteTarget, ScopedFs, Session, State};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MIN_PREFIX_LEN: usize = 6;

// ============================================================================
// Path helpers (session-aware)
// ============================================================================

/// Prepend the session cwd to a relative vfs path. Absolute paths (starting with '/')
/// pass through unchanged. Empty input becomes the cwd itself.
pub fn apply_cwd(session: &Session, raw: &str) -> String {
    if raw.starts_with('/') {
        return raw.to_string();
    }
    let cwd = session.cwd.trim_end_matches('/');
    if raw.is_empty() {
        return if cwd.is_empty() {
            "/".to_string()
        } else {
            cwd.to_string()
        };
    }
    if cwd.is_empty() {
        format!("/{}", raw)
    } else {
        format!("{}/{}", cwd, raw)
    }
}

/// Collapse `.` and `..` segments. Preserves leading `/` (or adds one).
/// `..` above root is clamped to root rather than erroring.
pub fn collapse_dotdot(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            parts.pop();
            continue;
        }
        parts.push(seg);
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// Resolve a `cd` argument against the current session.
/// Returns (new_container, new_cwd). Handles `/containers/<name>/...` for
/// switching containers and resolves `..` / `.` segments.
pub fn resolve_cd(session: &Session, raw: &str) -> io::Result<(String, String)> {
    // The /containers namespace is only navigable from the host context; from a
    // guest, `/containers/...` is just a path in the guest's own filesystem.
    if is_host_context(&session.container) {
        if raw == "/containers" || raw == "/containers/" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "/containers is virtual; cd into a specific container",
            ));
        }
        if let Some(rest) = raw.strip_prefix("/containers/") {
            let (name, sub) = match rest.split_once('/') {
                Some((n, p)) => (n.to_string(), p.to_string()),
                None => (rest.to_string(), String::new()),
            };
            // The session cwd is a filesystem path. Entering a container lands at
            // its filesystem root; deeper fs paths must go through `root/`. A meta
            // node (e.g. `status`) isn't a directory you can cd into.
            let cwd = if sub.is_empty() || sub == ROOT_BOUNDARY {
                "/".to_string()
            } else if let Some(fs) = sub.strip_prefix("root/") {
                format!("/{}", fs)
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "cannot cd into /containers/{name}/{sub}; \
                         the container filesystem is under /containers/{name}/root/"
                    ),
                ));
            };
            return Ok((name, collapse_dotdot(&cwd)));
        }
    }
    let absolute = apply_cwd(session, raw);
    Ok((session.container.clone(), collapse_dotdot(&absolute)))
}

/// Split `<ref>:<path>` into (ref, path). The ref token must precede any '/'.
/// `HEAD:/file` → ("HEAD", "/file");  `dev:hello` → ("dev", "hello");
/// `/no/colon` → (None, "/no/colon");  `a/b:c` → (None, "a/b:c").
pub fn split_ref_path(s: &str) -> (Option<&str>, &str) {
    if let Some(colon) = s.find(':') {
        let slash = s.find('/').unwrap_or(usize::MAX);
        if colon < slash && colon > 0 {
            return (Some(&s[..colon]), &s[colon + 1..]);
        }
    }
    (None, s)
}

// ============================================================================
// Ref resolution
// ============================================================================

pub fn resolve_tree(repo: &Repo<'_>, refspec: &str) -> io::Result<Hash> {
    if refspec.eq_ignore_ascii_case("HEAD") {
        return repo.head_tree();
    }
    if refspec.eq_ignore_ascii_case("WORKING") {
        return repo.working_tree();
    }
    let commit = resolve_commit(repo, refspec)?;
    repo.load_commit(&commit).map(|c| c.tree)
}

pub fn resolve_commit(repo: &Repo<'_>, refspec: &str) -> io::Result<Hash> {
    if let Some(h) = Hash::from_hex(refspec) {
        return Ok(h);
    }
    // Branches first, then tags. Same name in both → branch wins (matches
    // git's lookup order for ambiguous short refs).
    if let Some(commit) = repo.read_ref(refspec)? {
        return Ok(commit);
    }
    if let Some(commit) = repo.read_tag(refspec)? {
        return Ok(commit);
    }
    if is_hex_prefix(refspec) {
        if refspec.len() < MIN_PREFIX_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "hash prefix too short (need ≥{} chars): {}",
                    MIN_PREFIX_LEN, refspec
                ),
            ));
        }
        let matches = repo.store().find_by_prefix(refspec)?;
        let mut commit_hits: Vec<Hash> = matches
            .into_iter()
            .filter(|h| repo.load_commit(h).is_ok())
            .collect();
        commit_hits.sort();
        commit_hits.dedup();
        match commit_hits.len() {
            0 => {}
            1 => return Ok(commit_hits[0]),
            n => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("ambiguous prefix {}: {} commits match", refspec, n),
                ))
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("unknown ref: {}", refspec),
    ))
}

pub fn is_hex_prefix(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ============================================================================
// Multi-target dispatch (RouteTarget-aware)
// ============================================================================

/// The host container. The workspace-level prefixes (`/containers`,
/// `/workspace`, `/peers`) are recognized *only* when this is the active
/// container. From any other (guest) container they are ordinary paths in the
/// guest's own filesystem — so a guest image is never shadowed by hako's
/// namespace, and the workspace is reachable only from the host.
///
/// Must match the default container seeded by `hako-core`'s `State::init`.
pub const HOST_CONTAINER: &str = "hako";

/// True when the active container is the host, so workspace prefixes resolve.
pub fn is_host_context(active_container: &str) -> bool {
    active_container == HOST_CONTAINER
}

/// Parse a path into a route, honoring host-vs-guest context. The workspace
/// prefixes resolve only from the host container; from a guest every path is
/// guest-local (`Local`), so `/containers/...` reads the guest's own filesystem
/// rather than the workspace.
pub fn route(path: &str, active_container: &str) -> RouteTarget {
    let target = RouteTarget::parse(path);
    if is_host_context(active_container) {
        return target;
    }
    match target {
        RouteTarget::Local(_) => target,
        _ => RouteTarget::Local(path.trim_start_matches('/').to_string()),
    }
}

pub fn with_target<F>(state: &State, default_container: &str, path: &str, f: F) -> io::Result<()>
where
    F: FnOnce(&Repo<'_>, &str) -> io::Result<()>,
{
    let target = route(path, default_container);
    with_target_resolved(state, default_container, target, f)
}

pub fn with_target_mut<F>(
    state: &State,
    default_container: &str,
    path: &str,
    f: F,
) -> io::Result<()>
where
    F: FnOnce(&Repo<'_>, &str) -> io::Result<()>,
{
    with_target(state, default_container, path, f)
}

pub fn with_target_resolved<F>(
    state: &State,
    default_container: &str,
    target: RouteTarget,
    f: F,
) -> io::Result<()>
where
    F: FnOnce(&Repo<'_>, &str) -> io::Result<()>,
{
    match target {
        RouteTarget::Local(p) => {
            let repo = state.open_container(default_container)?;
            f(&repo, &p)
        }
        RouteTarget::Container { name, path } => {
            // The container filesystem lives under `root/`. A bare container
            // path or a meta name (e.g. `status`) is not a filesystem path and
            // must be handled by the meta surface, not raw fs ops.
            let fs = container_fs_path(&path).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "/containers/{name}/{path} is not a filesystem path; \
                         the container filesystem is under /containers/{name}/root/"
                    ),
                )
            })?;
            let repo = state.open_container(&name)?;
            f(&repo, fs)
        }
        RouteTarget::ContainersList => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "/containers is a virtual list, not writable",
        )),
        RouteTarget::Workspace(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "/workspace routing not implemented yet",
        )),
        RouteTarget::Peers(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "/peers routing not implemented yet",
        )),
    }
}

pub fn container_and_path<'a>(
    target: &'a RouteTarget,
    default_container: &'a str,
) -> io::Result<(&'a str, &'a str)> {
    match target {
        RouteTarget::Local(p) => Ok((default_container, p.as_str())),
        RouteTarget::Container { name, path } => {
            // Filesystem only — require the `root/` boundary (see container_fs_path).
            let fs = container_fs_path(path).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "/containers/{name}/{path} is not a filesystem path; \
                         the container filesystem is under /containers/{name}/root/"
                    ),
                )
            })?;
            Ok((name.as_str(), fs))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "this path is not a container path",
        )),
    }
}

// ============================================================================
// Container meta surface (the /containers/<name> everything-is-a-file view)
// ============================================================================

/// The boundary segment under which a container's filesystem (rootfs) lives.
/// Everything at `/containers/<name>/root/...` is the container's filesystem;
/// everything else under `/containers/<name>/` is the synthetic meta surface.
/// Sandboxing the rootfs under `root/` makes meta nodes collision-proof: a
/// container that ships its own `/proc` lives at `…/root/proc`, never clashing
/// with a future meta `…/proc`.
pub const ROOT_BOUNDARY: &str = "root";

/// The reserved leaf name exposed by the container meta surface alongside a
/// container's filesystem. Sits beside `root/` at `/containers/<name>/status`.
pub const META_STATUS: &str = "status";

/// The control node: writing a verb to `/containers/<name>/ctl` runs a control
/// action (e.g. `commit <msg>`) on the container — the Plan 9 ctl-file model.
/// Reading it returns a usage summary.
pub const META_CTL: &str = "ctl";

/// The reserved meta-node leaf names that sit beside `root/` in a container
/// directory. `ls` lists these; the `cat`/`write` interceptors handle each by
/// name. Keeping the listed set in one place is what stops `ls` from drifting
/// out of sync with the interceptors as nodes are added.
pub const META_NODES: &[&str] = &[META_STATUS, META_CTL];

/// Interpret the sub-path after `/containers/<name>` (the raw `path` from
/// `RouteTarget::Container`, with no leading slash) under the `root/` layout.
///
/// Returns `Some(fs_path)` when it addresses the container **filesystem** (the
/// sub-path is `root` or `root/...`), with the `root` boundary stripped off.
/// Returns `None` when it addresses the container directory itself (empty) or a
/// meta node (e.g. `status`) — i.e. anything that is *not* a filesystem path.
pub fn container_fs_path(sub: &str) -> Option<&str> {
    if sub == ROOT_BOUNDARY {
        return Some("");
    }
    sub.strip_prefix("root/")
}

/// Render a one-container status summary as bytes, for `cat /containers/<name>`.
/// Pure `Repo` — no runtime/instance data — so it stays in hako-core's
/// read-only, dependency-light world and needs no workspace lock.
///
/// Reports the active branch, the short HEAD commit (or "(no commits yet)"),
/// and whether the working tree differs from HEAD.
pub fn render_container_status(repo: &Repo<'_>, name: &str) -> io::Result<Vec<u8>> {
    use std::fmt::Write as _;
    let branch = repo
        .current_branch()?
        .unwrap_or_else(|| "(detached)".into());
    let head = repo.head_commit()?;
    let head_tree = repo.head_tree()?;
    let work_tree = repo.working_tree()?;
    let dirty = head_tree != work_tree;

    let mut s = String::new();
    let _ = writeln!(s, "container: {}", name);
    let _ = writeln!(s, "branch:    {}", branch);
    match head {
        Some(h) => {
            let _ = writeln!(s, "head:      {}", &h.to_hex()[..12]);
        }
        None => {
            let _ = writeln!(s, "head:      (no commits yet)");
        }
    }
    let _ = writeln!(s, "working:   {}", if dirty { "modified" } else { "clean" });
    Ok(s.into_bytes())
}

// ============================================================================
// Conflict-marker synthesis (used by merge)
// ============================================================================

/// A content-only conflict (both sides modified or added the same path) with
/// the file bytes from each side, ready to wrap in conflict markers.
pub struct ContentConflict {
    pub path: String,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
}

/// Pull (path, ours, theirs) out of a content-only conflict (BothModified or
/// BothAdded). Returns None for ModifyDelete / DeleteModify and for cases
/// where one side is a directory or symlink (no plain bytes to compare).
pub fn content_conflict(
    c: &Conflict,
    store: &dyn ChunkStore,
) -> io::Result<Option<ContentConflict>> {
    let (key, ours, theirs) = match c {
        Conflict::BothModified {
            key, ours, theirs, ..
        } => (key, ours, theirs),
        Conflict::BothAdded { key, ours, theirs } => (key, ours, theirs),
        _ => return Ok(None),
    };
    let path = std::str::from_utf8(key)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 conflict key"))?
        .to_string();

    let ours_bytes = file_bytes_from_value(ours, store)?;
    let theirs_bytes = file_bytes_from_value(theirs, store)?;
    match (ours_bytes, theirs_bytes) {
        (Some(o), Some(t)) => Ok(Some(ContentConflict {
            path,
            ours: o,
            theirs: t,
        })),
        _ => Ok(None),
    }
}

fn file_bytes_from_value(v: &Value, store: &dyn ChunkStore) -> io::Result<Option<Vec<u8>>> {
    let encoded = match v {
        Value::Inline(b) => b.clone(),
        Value::External(h) => store
            .get(h)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing tree value chunk"))?,
    };
    match decode_entry(&encoded)? {
        DirEntry::File(FileEntry { content, .. }) => {
            let bytes = hako::tree::load_value(store, &content)?;
            Ok(Some(bytes))
        }
        DirEntry::Directory | DirEntry::Symlink(_) => Ok(None),
    }
}

pub fn make_conflict_markers(ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"<<<<<<< ours\n");
    out.extend_from_slice(ours);
    if !ours.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    if !theirs.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b">>>>>>> theirs\n");
    out
}

// ============================================================================
// Pretty-printers
// ============================================================================

pub fn print_diff(d: &DiffEntry) {
    match d {
        DiffEntry::Added { key, .. } => {
            println!("+ {}", String::from_utf8_lossy(key));
        }
        DiffEntry::Removed { key, .. } => {
            println!("- {}", String::from_utf8_lossy(key));
        }
        DiffEntry::Modified { key, .. } => {
            println!("~ {}", String::from_utf8_lossy(key));
        }
    }
}

pub fn print_conflict(c: &Conflict) {
    let key = match c {
        Conflict::BothModified { key, .. }
        | Conflict::ModifyDelete { key, .. }
        | Conflict::DeleteModify { key, .. }
        | Conflict::BothAdded { key, .. } => key,
    };
    let kind = match c {
        Conflict::BothModified { .. } => "both modified",
        Conflict::ModifyDelete { .. } => "modify/delete",
        Conflict::DeleteModify { .. } => "delete/modify",
        Conflict::BothAdded { .. } => "both added",
    };
    eprintln!("  conflict ({}): {}", kind, String::from_utf8_lossy(key));
}

// ============================================================================
// Time helpers
// ============================================================================

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a Unix timestamp (seconds since epoch) as `YYYY-MM-DD HH:MM:SS UTC`.
/// Manual gregorian conversion — no chrono dep. Valid for 1970..9999.
pub fn format_ts(ts: u64) -> String {
    let secs_in_day = 86400u64;
    let days = ts / secs_in_day;
    let rem = ts % secs_in_day;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;

    // Howard Hinnant's date algorithm (public domain).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_cal = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_cal = if m_cal <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y_cal, m_cal, d, h, m, s
    )
}

// ============================================================================
// Host-fs helpers (cfg-gated)
// ============================================================================

#[cfg(unix)]
pub fn host_meta(meta: &std::fs::Metadata, _default_mode: u32) -> (u32, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.mode(), meta.mtime() as u64)
}

#[cfg(not(unix))]
pub fn host_meta(meta: &std::fs::Metadata, default_mode: u32) -> (u32, u64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (default_mode, mtime)
}

#[cfg(unix)]
pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    // Lossy UTF-8 conversion — round-trips for ASCII paths, degrades gracefully otherwise.
    p.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

#[cfg(unix)]
pub fn apply_host_meta(dst: &Path, mode: u32, _mtime: u64) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode & 0o7777))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn apply_host_meta(_dst: &Path, _mode: u32, _mtime: u64) -> io::Result<()> {
    // Windows has no POSIX mode bits to restore. mtime restoration would
    // require the `filetime` crate; skip for now.
    Ok(())
}

#[cfg(unix)]
pub fn create_host_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub fn create_host_symlink(target: &Path, link: &Path) -> io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Fetch the stored (mode, mtime) for a vfs path, if any. Returns `None` for
/// directories (which currently don't carry metadata).
pub fn entry_meta(scoped: &ScopedFs<'_>, root: &Hash, src: &str) -> io::Result<Option<(u32, u64)>> {
    let (parent, name) = match src.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", src),
    };
    for child in scoped.ls(root, parent)? {
        if child.name == name {
            return Ok(match (child.mode, child.mtime) {
                (Some(m), Some(t)) => Some((m, t)),
                _ => None,
            });
        }
    }
    Ok(None)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(container: &str, cwd: &str) -> Session {
        Session {
            container: container.into(),
            cwd: cwd.into(),
        }
    }

    #[test]
    fn split_ref_path_cases() {
        assert_eq!(
            split_ref_path("HEAD:/file.txt"),
            (Some("HEAD"), "/file.txt")
        );
        assert_eq!(split_ref_path("dev:hello"), (Some("dev"), "hello"));
        assert_eq!(split_ref_path("/file:weird.txt"), (None, "/file:weird.txt"));
        assert_eq!(split_ref_path("a/b:c"), (None, "a/b:c"));
        assert_eq!(split_ref_path("plain.txt"), (None, "plain.txt"));
        assert_eq!(split_ref_path(":nope"), (None, ":nope"));
    }

    #[test]
    fn is_hex_prefix_works() {
        assert!(is_hex_prefix("abc123"));
        assert!(is_hex_prefix("DEADBEEF"));
        assert!(!is_hex_prefix(""));
        assert!(!is_hex_prefix("ghxyz"));
        assert!(!is_hex_prefix("abc-123"));
    }

    #[test]
    fn format_ts_known_values() {
        assert_eq!(format_ts(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_ts(86400), "1970-01-02 00:00:00 UTC");
        assert_eq!(format_ts(1700000000), "2023-11-14 22:13:20 UTC");
        assert!(format_ts(1776391310).starts_with("2026-04-"));
    }

    #[test]
    fn make_conflict_markers_format() {
        let out = make_conflict_markers(b"line ours", b"line theirs");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("<<<<<<< ours\nline ours\n"));
        assert!(s.contains("=======\nline theirs\n"));
        assert!(s.contains(">>>>>>> theirs\n"));
    }

    #[test]
    fn make_conflict_markers_preserves_trailing_newline() {
        let out = make_conflict_markers(b"o\n", b"t\n");
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("o\n\n="));
        assert!(!s.contains("t\n\n>"));
    }

    #[test]
    fn apply_cwd_absolute_passthrough() {
        let s = sess("main", "/sub");
        assert_eq!(apply_cwd(&s, "/foo"), "/foo");
        assert_eq!(apply_cwd(&s, "/containers/alpha/x"), "/containers/alpha/x");
    }

    #[test]
    fn apply_cwd_relative_prepends_cwd() {
        let s = sess("main", "/sub/dir");
        assert_eq!(apply_cwd(&s, "foo"), "/sub/dir/foo");
        assert_eq!(apply_cwd(&s, "a/b"), "/sub/dir/a/b");
    }

    #[test]
    fn apply_cwd_root_cwd() {
        let s = sess("main", "/");
        assert_eq!(apply_cwd(&s, "foo"), "/foo");
        assert_eq!(apply_cwd(&s, ""), "/");
    }

    #[test]
    fn apply_cwd_empty_returns_cwd() {
        let s = sess("main", "/sub");
        assert_eq!(apply_cwd(&s, ""), "/sub");
    }

    #[test]
    fn collapse_dotdot_basics() {
        assert_eq!(collapse_dotdot("/a/b/../c"), "/a/c");
        assert_eq!(collapse_dotdot("/a/./b"), "/a/b");
        assert_eq!(collapse_dotdot("/a/b/.."), "/a");
        assert_eq!(collapse_dotdot("/.."), "/");
        assert_eq!(collapse_dotdot("/a/../.."), "/");
        assert_eq!(collapse_dotdot("/"), "/");
        assert_eq!(collapse_dotdot(""), "/");
    }

    #[test]
    fn resolve_cd_within_container() {
        let s = sess("main", "/start");
        assert_eq!(
            resolve_cd(&s, "sub").unwrap(),
            ("main".into(), "/start/sub".into())
        );
        assert_eq!(
            resolve_cd(&s, "/abs").unwrap(),
            ("main".into(), "/abs".into())
        );
        assert_eq!(resolve_cd(&s, "..").unwrap(), ("main".into(), "/".into()));
    }

    #[test]
    fn resolve_cd_switches_container() {
        let s = sess("hako", "/x"); // host context — workspace prefixes resolve
                                    // Filesystem paths are addressed under the `root/` boundary.
        assert_eq!(
            resolve_cd(&s, "/containers/alpha/root/sub").unwrap(),
            ("alpha".into(), "/sub".into())
        );
        // `root` itself, and a bare container, both land at the filesystem root.
        assert_eq!(
            resolve_cd(&s, "/containers/alpha/root").unwrap(),
            ("alpha".into(), "/".into())
        );
        assert_eq!(
            resolve_cd(&s, "/containers/beta").unwrap(),
            ("beta".into(), "/".into())
        );
        assert_eq!(
            resolve_cd(&s, "/containers/beta/").unwrap(),
            ("beta".into(), "/".into())
        );
    }

    #[test]
    fn resolve_cd_rejects_non_root_container_path() {
        // Under the `root/` layout, addressing the filesystem without `root/`
        // (the pre-migration form) is no longer a valid cd target.
        let s = sess("hako", "/x");
        assert!(resolve_cd(&s, "/containers/alpha/etc").is_err());
        // A meta node is not a directory you can cd into.
        assert!(resolve_cd(&s, "/containers/alpha/status").is_err());
    }

    #[test]
    fn resolve_cd_rejects_containers_root() {
        let s = sess("hako", "/");
        assert!(resolve_cd(&s, "/containers").is_err());
        assert!(resolve_cd(&s, "/containers/").is_err());
    }

    #[test]
    fn resolve_cd_from_a_guest_treats_containers_as_local() {
        // From a guest container, /containers is not the workspace namespace —
        // it's an ordinary path in the guest's own filesystem, so cd neither
        // switches containers nor errors.
        let s = sess("ubuntu", "/");
        assert_eq!(
            resolve_cd(&s, "/containers/alpha/root").unwrap(),
            ("ubuntu".into(), "/containers/alpha/root".into())
        );
        assert_eq!(
            resolve_cd(&s, "/containers").unwrap(),
            ("ubuntu".into(), "/containers".into())
        );
    }

    #[test]
    fn container_fs_path_boundary() {
        assert_eq!(container_fs_path("root"), Some(""));
        assert_eq!(container_fs_path("root/etc/hosts"), Some("etc/hosts"));
        assert_eq!(container_fs_path(""), None); // container dir itself
        assert_eq!(container_fs_path("status"), None); // meta node
        assert_eq!(container_fs_path("rootfs"), None); // not the `root` segment
    }

    #[test]
    fn route_resolves_workspace_prefixes_only_from_the_host() {
        // From the host (hako), the workspace prefixes resolve.
        assert_eq!(
            route("/containers", HOST_CONTAINER),
            RouteTarget::ContainersList
        );
        assert_eq!(
            route("/containers/ubuntu/root/etc", HOST_CONTAINER),
            RouteTarget::Container {
                name: "ubuntu".into(),
                path: "root/etc".into()
            }
        );

        // From a guest, the same paths are guest-local — never the workspace.
        assert_eq!(
            route("/containers", "ubuntu"),
            RouteTarget::Local("containers".into())
        );
        assert_eq!(
            route("/containers/debian/root/etc", "ubuntu"),
            RouteTarget::Local("containers/debian/root/etc".into())
        );
        assert_eq!(
            route("/peers/x", "ubuntu"),
            RouteTarget::Local("peers/x".into())
        );

        // Ordinary paths are guest-local in both contexts.
        assert_eq!(
            route("/etc/hosts", HOST_CONTAINER),
            RouteTarget::Local("etc/hosts".into())
        );
        assert_eq!(
            route("/etc/hosts", "ubuntu"),
            RouteTarget::Local("etc/hosts".into())
        );
    }
}
