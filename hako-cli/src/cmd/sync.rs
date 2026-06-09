//! Workspace ↔ workspace sync: fetch, push.

use super::Ctx;
use crate::DOT_HAKO;
use hako::{Hash, Repo, State, WorkspaceLock};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

pub fn fetch(
    ctx: &Ctx<'_>,
    remote: PathBuf,
    branch: String,
    as_ref: Option<String>,
    from_container: Option<String>,
) -> io::Result<ExitCode> {
    let remote_dot = remote.join(DOT_HAKO);
    let remote_state = State::open(&remote_dot)?;
    // We only READ from the remote, but acquire its lock so a concurrent
    // commit on the remote can't change refs out from under us mid-fetch.
    let _remote_lock = WorkspaceLock::acquire(&remote_dot)?;
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
    let remote_dot = remote.join(DOT_HAKO);
    let remote_state = State::open(&remote_dot)?;
    // Hold the remote's lock for the entire push so a concurrent commit on
    // the remote can't clobber the ref we're about to write (and vice versa).
    let _remote_lock = WorkspaceLock::acquire(&remote_dot)?;
    let remote_container = to_container
        .clone()
        .unwrap_or_else(|| ctx.default_container.to_string());
    // Auto-create the remote container — push should "just work" against a fresh remote.
    if !remote_state.list_containers()?.iter().any(|c| c == &remote_container) {
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
