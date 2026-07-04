//! Version control: commit, log, branch, checkout, merge, diff.

use super::Ctx;
use crate::helpers::{
    content_conflict, format_ts, make_conflict_markers, now_secs, print_conflict, print_diff,
    resolve_commit, resolve_tree,
};
use hako::{Hash, ScopedFs};
use std::io;
use std::process::ExitCode;

pub fn commit(ctx: &Ctx<'_>, message: String, author: Option<String>) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    commit_repo(&repo, &message, &resolve_author(author), &mut io::stdout())
}

/// Resolve the commit author: the explicit `--author` flag, else `$HAKO_AUTHOR`,
/// else the literal `"user"`. (Previously every commit was hardcoded to "user".)
pub fn resolve_author(author: Option<String>) -> String {
    author
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HAKO_AUTHOR").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "user".to_string())
}

/// Commit a repo's working tree onto its current branch. Shared by the `commit`
/// command and the container `ctl` control node (`write …/ctl "commit <msg>"`),
/// so both go through identical snapshot semantics. The result line is written to
/// `out` (stdout locally, a capture buffer when served to a peer). Returns exit
/// code 1 (and a stderr note) when there is nothing to commit.
pub fn commit_repo(
    repo: &hako::Repo<'_>,
    message: &str,
    author: &str,
    out: &mut dyn io::Write,
) -> io::Result<ExitCode> {
    let work = repo.working_tree()?;
    let head = repo.head_commit()?;
    // Propagate (don't swallow) a failure to load HEAD: a corrupt/missing HEAD
    // commit must surface as an error, not be masked as a tree mismatch that
    // then commits on top of the unreadable parent.
    let head_tree = match head {
        Some(h) => Some(repo.load_commit(&h)?.tree),
        None => None,
    };
    if Some(work) == head_tree {
        crate::diag!("nothing to commit (working tree matches HEAD)");
        return Ok(ExitCode::from(1));
    }
    let parents = head.into_iter().collect();
    let ts = now_secs();
    let commit = repo.commit(work, parents, author, message, ts)?;
    let branch = repo
        .current_branch()?
        .ok_or_else(|| io::Error::other("detached HEAD"))?;
    repo.write_ref(&branch, commit)?;
    writeln!(out, "{} {}", &commit.to_hex()[..12], message)?;
    Ok(ExitCode::SUCCESS)
}

pub fn log(ctx: &Ctx<'_>, json: bool) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    let head = match repo.head_commit()? {
        Some(h) => h,
        None => {
            if json {
                println!("[]");
            } else {
                println!("(no commits yet)");
            }
            return Ok(ExitCode::SUCCESS);
        }
    };
    let entries = repo.log(head)?;
    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|(h, c)| {
                serde_json::json!({
                    "hash": h.to_hex(),
                    "parents": c.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
                    "tree": c.tree.to_hex(),
                    "author": c.author,
                    "message": c.message,
                    "timestamp": c.timestamp,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(ExitCode::SUCCESS);
    }
    for (h, c) in entries {
        println!(
            "{}  {}  {} -- {}",
            &h.to_hex()[..12],
            format_ts(c.timestamp),
            c.author,
            c.message
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn branch(
    ctx: &Ctx<'_>,
    name: Option<String>,
    start: Option<String>,
    delete: bool,
) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    match (name, delete) {
        (None, _) => {
            let current = repo.current_branch()?.unwrap_or_default();
            for b in repo.list_branches()? {
                let marker = if b == current { "*" } else { " " };
                println!("{} {}", marker, b);
            }
        }
        (Some(n), true) => {
            // Refuse to delete the current branch — doing so leaves HEAD
            // pointing at a missing ref, which silently breaks log/status
            // and lets subsequent commits orphan themselves off-graph.
            if repo.current_branch()?.as_deref() == Some(n.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to delete the current branch {} \
                         (checkout another branch first)",
                        n
                    ),
                ));
            }
            if !repo.delete_ref(&n)? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such branch: {}", n),
                ));
            }
            println!("deleted branch {}", n);
        }
        (Some(n), false) => {
            let target = match start {
                Some(s) => resolve_commit(&repo, &s)?,
                None => repo
                    .head_commit()?
                    .ok_or_else(|| io::Error::other("no HEAD commit to branch from"))?,
            };
            repo.write_ref(&n, target)?;
            println!("created branch {} at {}", n, &target.to_hex()[..12]);
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn checkout(ctx: &Ctx<'_>, branch: String, force: bool) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    if repo.current_branch()?.as_deref() == Some(branch.as_str()) {
        println!("already on branch {}", branch);
        return Ok(ExitCode::SUCCESS);
    }
    if !force {
        let head_tree = repo.head_tree()?;
        let work_tree = repo.working_tree()?;
        if head_tree != work_tree {
            crate::diag!("uncommitted changes in working tree; commit, discard, or pass --force");
            return Ok(ExitCode::from(1));
        }
    }
    let target = repo.read_ref(&branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such branch: {}", branch),
        )
    })?;
    let tree = repo.load_commit(&target)?.tree;
    // Update the working tree FIRST, then move HEAD. If a crash interleaves,
    // the refs stay intact (HEAD still on the old branch) and the workspace
    // simply shows the target branch's tree as uncommitted changes on the old
    // branch — recoverable by re-running checkout, with no ref lost. The reverse
    // order could leave HEAD on the new branch while WORKING still holds the old
    // branch's tree, silently mislabeling that content as edits on the new branch.
    repo.set_working(tree)?;
    repo.set_branch(&branch)?;
    println!("switched to branch {}", branch);
    Ok(ExitCode::SUCCESS)
}

pub fn merge(
    ctx: &Ctx<'_>,
    branch: Option<String>,
    author: Option<String>,
    abort: bool,
) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    if abort {
        let head_tree = repo.head_tree()?;
        repo.set_working(head_tree)?;
        println!("merge aborted; working tree reset to HEAD");
        return Ok(ExitCode::SUCCESS);
    }
    let author = resolve_author(author);
    let branch = branch.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "merge requires a branch (or --abort)",
        )
    })?;
    let head = repo
        .head_commit()?
        .ok_or_else(|| io::Error::other("no HEAD commit to merge into"))?;
    let theirs = repo.read_ref(&branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such branch: {}", branch),
        )
    })?;
    if head == theirs {
        // Already pointing at the same commit — merging would create a
        // redundant commit with [head, head] as parents and no tree change.
        crate::diag!("already up to date (HEAD == {})", branch);
        return Ok(ExitCode::SUCCESS);
    }
    let base = repo.common_ancestor(head, theirs)?.unwrap_or(Hash::zero());
    // Fast-forward: theirs is a descendant of head, so merging is a no-op
    // tree-wise — just move the current branch to point at theirs and
    // update working. Avoids creating a redundant merge commit.
    if base == head {
        let theirs_tree = repo.load_commit(&theirs)?.tree;
        repo.set_working(theirs_tree)?;
        let cur = repo
            .current_branch()?
            .ok_or_else(|| io::Error::other("detached HEAD; cannot fast-forward"))?;
        repo.write_ref(&cur, theirs)?;
        println!(
            "fast-forwarded {} to {} ({})",
            cur,
            branch,
            &theirs.to_hex()[..12]
        );
        return Ok(ExitCode::SUCCESS);
    }
    let base_tree = if base == Hash::zero() {
        Hash::zero()
    } else {
        repo.load_commit(&base)?.tree
    };
    let ours_tree = repo.load_commit(&head)?.tree;
    let theirs_tree = repo.load_commit(&theirs)?.tree;
    let result = hako::tree::three_way_merge(repo.store(), &base_tree, &ours_tree, &theirs_tree)?;

    let scoped = ScopedFs::new(repo.store());
    let mut merged_root = result.merged;
    for c in &result.conflicts {
        if let Some(cc) = content_conflict(c, repo.store())? {
            let marked = make_conflict_markers(&cc.ours, &cc.theirs);
            merged_root = scoped.write_file(&merged_root, &cc.path, &marked)?;
        }
    }
    repo.set_working(merged_root)?;

    if !result.conflicts.is_empty() {
        eprintln!(
            "merge produced {} conflict(s); resolve and commit:",
            result.conflicts.len()
        );
        for c in &result.conflicts {
            print_conflict(c);
        }
        return Ok(ExitCode::from(2));
    }
    let msg = format!(
        "merge {} into {}",
        branch,
        repo.current_branch()?.unwrap_or_default()
    );
    let cur = repo.current_branch()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot merge with a detached HEAD — check out a branch first",
        )
    })?;
    let commit = repo.commit(merged_root, vec![head, theirs], &author, &msg, now_secs())?;
    repo.write_ref(&cur, commit)?;
    println!(
        "merged {} into {} as {}",
        branch,
        cur,
        &commit.to_hex()[..12]
    );
    Ok(ExitCode::SUCCESS)
}

pub fn tag(
    ctx: &Ctx<'_>,
    name: Option<String>,
    start: Option<String>,
    delete: bool,
) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    match (name, delete) {
        (None, _) => {
            for t in repo.list_tags()? {
                println!("{}", t);
            }
        }
        (Some(n), true) => {
            if !repo.delete_tag(&n)? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such tag: {}", n),
                ));
            }
            println!("deleted tag {}", n);
        }
        (Some(n), false) => {
            // Refuse to overwrite an existing tag — tags are meant to be
            // immutable. The user can `tag -d` first if they really mean it.
            if repo.read_tag(&n)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("tag {} already exists (delete it first to move it)", n),
                ));
            }
            let target = match start {
                Some(s) => resolve_commit(&repo, &s)?,
                None => repo
                    .head_commit()?
                    .ok_or_else(|| io::Error::other("no HEAD commit to tag"))?,
            };
            repo.write_tag(&n, target)?;
            println!("created tag {} at {}", n, &target.to_hex()[..12]);
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn diff(ctx: &Ctx<'_>, from: Option<String>, to: Option<String>) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    let from_tree = match from {
        None => repo.head_tree()?,
        Some(s) => resolve_tree(&repo, &s)?,
    };
    let to_tree = match to {
        None => repo.working_tree()?,
        Some(s) => resolve_tree(&repo, &s)?,
    };
    let diffs = hako::tree::diff(repo.store(), &from_tree, &to_tree)?;
    for d in diffs {
        print_diff(&d);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_author_flag_then_default() {
        // an explicit, non-empty flag always wins
        assert_eq!(resolve_author(Some("alice".into())), "alice");
        // an empty flag is ignored; with HAKO_AUTHOR unset it falls back to "user"
        // (only assert the env-free path so we don't mutate process-global env)
        if std::env::var_os("HAKO_AUTHOR").is_none() {
            assert_eq!(resolve_author(None), "user");
            assert_eq!(resolve_author(Some(String::new())), "user");
        }
    }
}
