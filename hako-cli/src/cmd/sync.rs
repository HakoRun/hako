//! Workspace ↔ workspace sync: fetch, push.

use super::Ctx;
use crate::DOT_HAKO;
use hako::{Hash, Repo, State, WorkspaceLock};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub fn fetch(
    ctx: &Ctx<'_>,
    remote: PathBuf,
    branch: String,
    as_ref: Option<String>,
    from_container: Option<String>,
) -> io::Result<ExitCode> {
    let local_dot = ctx.workdir.join(DOT_HAKO);
    let remote_dot = remote.join(DOT_HAKO);
    let remote_state = State::open(&remote_dot)?;
    // Lock BOTH workspaces for the whole fetch: the remote so its refs can't
    // change out from under us mid-read, the local so our ref write serializes
    // against local commands. `main` no longer locks the local workspace for
    // fetch/push (see `holds_workspace_lock`), so both locks are taken here in a
    // deterministic global order — refusing a self-sync — to avoid deadlock (#75).
    let _locks = lock_pair(&local_dot, &remote_dot)?;
    let remote_container = from_container
        .clone()
        .unwrap_or_else(|| ctx.default_container.to_string());
    let remote_repo = remote_state.open_container(&remote_container)?;
    let commit = remote_repo.read_ref(&branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "remote branch {} not found in {}/{}",
                branch,
                remote.display(),
                remote_container
            ),
        )
    })?;
    let local_repo = ctx.state.open_container(ctx.default_container)?;
    let copied = sync_objects(&remote_repo, &local_repo, commit)?;
    let local_branch = as_ref.unwrap_or(branch.clone());
    local_repo.write_ref(&local_branch, commit)?;
    println!(
        "fetched {} objects from {} ({}:{}); local ref {} -> {}",
        copied,
        remote.display(),
        remote_container,
        branch,
        local_branch,
        &commit.to_hex()[..12]
    );
    Ok(ExitCode::SUCCESS)
}

pub fn push(
    ctx: &Ctx<'_>,
    remote: PathBuf,
    branch: String,
    as_ref: Option<String>,
    to_container: Option<String>,
) -> io::Result<ExitCode> {
    let local_dot = ctx.workdir.join(DOT_HAKO);
    let remote_dot = remote.join(DOT_HAKO);
    let remote_state = State::open(&remote_dot)?;
    // Lock BOTH workspaces for the whole push: the local so its refs/objects are
    // stable while we read them, the remote so a concurrent commit can't clobber
    // the ref we write (and vice versa). `main` no longer locks the local
    // workspace for fetch/push (see `holds_workspace_lock`), so both locks are
    // taken here in a deterministic global order — refusing a self-sync — to
    // avoid deadlock (#75).
    let _locks = lock_pair(&local_dot, &remote_dot)?;
    let local_repo = ctx.state.open_container(ctx.default_container)?;
    let commit = local_repo.read_ref(&branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "local branch {} not found in {}",
                branch, ctx.default_container
            ),
        )
    })?;
    let remote_container = to_container
        .clone()
        .unwrap_or_else(|| ctx.default_container.to_string());
    // Auto-create the remote container — push should "just work" against a fresh remote.
    if !remote_state
        .list_containers()?
        .iter()
        .any(|c| c == &remote_container)
    {
        remote_state.create_container(&remote_container)?;
    }
    let remote_repo = remote_state.open_container(&remote_container)?;
    let copied = sync_objects(&local_repo, &remote_repo, commit)?;
    let remote_branch = as_ref.unwrap_or(branch.clone());
    remote_repo.write_ref(&remote_branch, commit)?;
    println!(
        "pushed {} objects to {} ({}:{}); remote ref updated to {}",
        copied,
        remote.display(),
        remote_container,
        remote_branch,
        &commit.to_hex()[..12]
    );
    Ok(ExitCode::SUCCESS)
}

/// Copy every object reachable from `commit` from `src.store()` into `dst.store()`,
/// skipping objects that already exist locally. Returns the number of new objects copied.
fn sync_objects(src: &Repo<'_>, dst: &Repo<'_>, commit: Hash) -> io::Result<usize> {
    let reachable = src.reachable_objects(commit)?;
    let mut copied = 0;
    for h in reachable {
        if dst.store().has(&h)? {
            continue;
        }
        let bytes = src.store().get(&h)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote missing object {}", h.to_hex()),
            )
        })?;
        dst.store().put(&bytes)?;
        copied += 1;
    }
    Ok(copied)
}

/// Acquire the workspace locks for the `local` and `remote` `.hako` dirs and
/// return both guards. Two safety properties (#75):
///
///   * **No self-deadlock.** If the two paths canonicalize to the same workspace,
///     refuse: syncing to yourself is meaningless, and a single process cannot
///     re-acquire its own `flock` (a second `WorkspaceLock::acquire` on the same
///     file would block forever). Canonicalizing catches aliases (`.`/`..`,
///     symlinks, a trailing slash) that a textual compare would miss.
///   * **No AB-BA deadlock.** Lock the lexicographically-smaller canonical path
///     first, so two concurrent syncs in opposite directions (`A -> B` and
///     `B -> A`) acquire the two locks in the same global order instead of each
///     grabbing one and waiting on the other.
fn lock_pair(local: &Path, remote: &Path) -> io::Result<(WorkspaceLock, WorkspaceLock)> {
    let local_c = std::fs::canonicalize(local)?;
    let remote_c = std::fs::canonicalize(remote)?;
    if local_c == remote_c {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to sync a workspace with itself (local and remote resolve \
             to the same path)",
        ));
    }
    // Acquire in canonical order; return in (local, remote) order regardless.
    if local_c < remote_c {
        let l = WorkspaceLock::acquire(local)?;
        let r = WorkspaceLock::acquire(remote)?;
        Ok((l, r))
    } else {
        let r = WorkspaceLock::acquire(remote)?;
        let l = WorkspaceLock::acquire(local)?;
        Ok((l, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_dot(base: &Path, name: &str) -> PathBuf {
        let dot = base.join(name).join(DOT_HAKO);
        std::fs::create_dir_all(&dot).unwrap();
        dot
    }

    #[test]
    fn lock_pair_refuses_the_same_workspace() {
        let base = TempDir::new().unwrap();
        let dot = make_dot(base.path(), "ws");
        // The exact same path — and a `.`-laden alias of it — are both self.
        assert!(lock_pair(&dot, &dot).is_err());
        let aliased = dot.join(".").join("."); // still resolves to `dot`
        assert!(lock_pair(&dot, &aliased).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn lock_pair_refuses_a_symlinked_alias() {
        let base = TempDir::new().unwrap();
        let dot = make_dot(base.path(), "ws");
        // `alias` -> `ws`, so `alias/.hako` canonicalizes to `ws/.hako`.
        let link = base.path().join("alias");
        std::os::unix::fs::symlink(base.path().join("ws"), &link).unwrap();
        assert!(lock_pair(&dot, &link.join(DOT_HAKO)).is_err());
    }

    #[test]
    fn lock_pair_locks_two_distinct_workspaces() {
        let base = TempDir::new().unwrap();
        let a = make_dot(base.path(), "a");
        let b = make_dot(base.path(), "b");
        // Distinct workspaces: both orders succeed and yield two live guards.
        assert!(lock_pair(&a, &b).is_ok());
        assert!(lock_pair(&b, &a).is_ok());
    }
}
