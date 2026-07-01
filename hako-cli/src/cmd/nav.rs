//! Navigation: ls, cd, pwd, tree, status.

use super::Ctx;
use crate::helpers::{
    apply_cwd, container_fs_path, print_diff, resolve_cd, resolve_tree, route, split_ref_path,
    with_target, with_target_resolved, MetaNodeKind, META_NODES, ROOT_BOUNDARY,
};
use hako::fs::DirKind;
use hako::{Hash, RouteTarget, ScopedFs, Session};
use std::io;
use std::process::ExitCode;

pub fn ls(ctx: &Ctx<'_>, path: Option<String>) -> io::Result<ExitCode> {
    let path = path.unwrap_or_default();
    let (refspec, rest) = split_ref_path(&path);
    let rest = apply_cwd(ctx.session, rest);
    match route(&rest, ctx.default_container) {
        RouteTarget::ContainersList => {
            for c in ctx.state.list_containers()? {
                println!("{}/", c);
            }
        }
        // The container "directory" itself: list the synthetic surface — the
        // filesystem under `root/` plus the meta nodes (e.g. `status`). The
        // rootfs entries themselves are listed via `ls /containers/<name>/root`.
        RouteTarget::Container { name, path }
            if refspec.is_none() && container_fs_path(&path).is_none() && path.is_empty() =>
        {
            // Validate the container exists (open errors with NotFound otherwise).
            let _ = ctx.state.open_container(&name)?;
            println!("{}/", ROOT_BOUNDARY);
            for (node, kind) in META_NODES {
                match kind {
                    MetaNodeKind::Leaf => println!("{} (meta)", node),
                    MetaNodeKind::Dir => println!("{}/ (meta)", node),
                }
            }
        }
        // `proc/` and `proc/<pid>` are runtime-backed: listing reads /proc on
        // the kernel the container runs on (bridged from Windows/macOS).
        RouteTarget::Container { name, path }
            if refspec.is_none() && crate::cmd::proc_meta::proc_subpath(&path).is_some() =>
        {
            let sub = crate::cmd::proc_meta::proc_subpath(&path).unwrap();
            return crate::cmd::proc_meta::ls(ctx, &name, sub);
        }
        target => {
            with_target_resolved(ctx.state, ctx.default_container, target, |repo, p| {
                let scoped = ScopedFs::new(repo.store());
                let root = match refspec {
                    Some(r) => resolve_tree(repo, r)?,
                    None => repo.working_tree()?,
                };
                for child in scoped.ls(&root, p)? {
                    let suffix = match child.kind {
                        DirKind::Directory => "/",
                        DirKind::File => "",
                        DirKind::Symlink => "@",
                    };
                    match child.kind {
                        DirKind::Symlink => {
                            let tgt = child
                                .symlink_target
                                .as_deref()
                                .map(|b| String::from_utf8_lossy(b).into_owned())
                                .unwrap_or_default();
                            println!("{}{} -> {}", child.name, suffix, tgt);
                        }
                        _ => match child.size {
                            Some(s) => println!("{}{} ({} bytes)", child.name, suffix, s),
                            None => println!("{}{}", child.name, suffix),
                        },
                    }
                }
                Ok(())
            })?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn cd(ctx: &Ctx<'_>, path: String) -> io::Result<ExitCode> {
    let (new_container, new_cwd) = resolve_cd(ctx.session, &path)?;
    if !ctx
        .state
        .list_containers()?
        .iter()
        .any(|c| c == &new_container)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such container: {}", new_container),
        ));
    }
    let repo = ctx.state.open_container(&new_container)?;
    let scoped = ScopedFs::new(repo.store());
    let root = repo.working_tree()?;
    let key = new_cwd.trim_start_matches('/');
    if !key.is_empty() && !scoped.is_dir(&root, key)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("not a directory: {}", fs_address(&new_container, &new_cwd)),
        ));
    }
    let new_session = Session {
        container: new_container.clone(),
        cwd: new_cwd.clone(),
    };
    ctx.state.write_session(&new_session)?;
    println!("{}", fs_address(&new_container, &new_cwd));
    Ok(ExitCode::SUCCESS)
}

/// Format a container + filesystem cwd as an addressable path under the `root/`
/// boundary: `(alpine, "/")` → `/containers/alpine/root`,
/// `(alpine, "/etc")` → `/containers/alpine/root/etc`.
fn fs_address(container: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        format!("/containers/{}/{}", container, ROOT_BOUNDARY)
    } else {
        format!("/containers/{}/{}{}", container, ROOT_BOUNDARY, cwd)
    }
}

/// Switch the workspace's identity to `branch`. If no container by that
/// name exists locally, treat the name as an OCI image reference, pull it
/// into a fresh container, and switch into it — the headline `hako is
/// alpine` flow. Resets cwd to `/`.
pub fn switch_identity(ctx: &Ctx<'_>, branch: String) -> io::Result<ExitCode> {
    let exists = ctx.state.list_containers()?.iter().any(|c| c == &branch);
    if !exists {
        // The container doesn't exist; the user said `is X` so they're
        // asking us to BE X — go fetch it. ImageRef::parse is permissive
        // (accepts any non-empty trimmed string), so a typo will still
        // attempt a registry round-trip and fail with that error rather
        // than a local one. That's the right tradeoff: the user already
        // signed up for "make me X, however that takes."
        let image_ref = hako::ImageRef::parse(&branch).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no container {} and not a valid image ref: {}", branch, e),
            )
        })?;
        super::oci::pull_into(
            ctx.state,
            &image_ref,
            &branch,
            "linux",
            super::oci::host_oci_arch(),
            false,
        )?;
    }
    let new_session = Session {
        container: branch.clone(),
        cwd: "/".into(),
    };
    ctx.state.write_session(&new_session)?;
    println!("now: {}", branch);
    Ok(ExitCode::SUCCESS)
}

pub fn pwd(ctx: &Ctx<'_>) -> io::Result<ExitCode> {
    // Reflect the effective default container (config-aware), not the bare
    // session value, so `pwd` matches what other commands route to. The cwd is
    // a filesystem path, so it renders under the `root/` boundary.
    println!(
        "{}",
        fs_address(ctx.default_container, ctx.session.cwd.as_str())
    );
    Ok(ExitCode::SUCCESS)
}

pub fn tree(ctx: &Ctx<'_>, path: Option<String>, depth: Option<usize>) -> io::Result<ExitCode> {
    let path = path.unwrap_or_default();
    let (refspec, rest) = split_ref_path(&path);
    let rest = apply_cwd(ctx.session, rest);
    with_target(ctx.state, ctx.default_container, &rest, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = match refspec {
            Some(r) => resolve_tree(repo, r)?,
            None => repo.working_tree()?,
        };
        let label = if p.is_empty() { "/" } else { p };
        println!("{}", label);
        print_tree(&scoped, &root, p, "", depth, 0)?;
        Ok(())
    })?;
    Ok(ExitCode::SUCCESS)
}

pub fn status(ctx: &Ctx<'_>) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    let branch = repo
        .current_branch()?
        .unwrap_or_else(|| "(detached)".into());
    let head_tree = repo.head_tree()?;
    let work_tree = repo.working_tree()?;
    println!("on branch {}", branch);
    if head_tree == work_tree {
        println!("nothing to commit, working tree clean");
    } else {
        println!("changes since HEAD:");
        let diffs = hako::tree::diff(repo.store(), &head_tree, &work_tree)?;
        for d in diffs {
            print_diff(&d);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Recursive ASCII tree printer. `depth` caps recursion (None = unlimited).
fn print_tree(
    scoped: &ScopedFs<'_>,
    root: &Hash,
    path: &str,
    prefix: &str,
    depth: Option<usize>,
    level: usize,
) -> io::Result<()> {
    if let Some(d) = depth {
        if level >= d {
            return Ok(());
        }
    }
    let children = scoped.ls(root, path)?;
    let n = children.len();
    for (i, child) in children.into_iter().enumerate() {
        let is_last = i + 1 == n;
        let branch = if is_last { "└── " } else { "├── " };
        let suffix = match child.kind {
            DirKind::Directory => "/",
            DirKind::File => "",
            DirKind::Symlink => "@",
        };
        println!("{}{}{}{}", prefix, branch, child.name, suffix);
        if matches!(child.kind, DirKind::Directory) {
            let sub_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
            let sub_path = if path.is_empty() {
                child.name.clone()
            } else {
                format!("{}/{}", path, child.name)
            };
            print_tree(scoped, root, &sub_path, &sub_prefix, depth, level + 1)?;
        }
    }
    Ok(())
}
