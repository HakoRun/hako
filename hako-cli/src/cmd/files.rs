//! File operations: write, cat, mkdir, del, cp, mv, import, export.

use super::Ctx;
use crate::helpers::{
    apply_cwd, apply_host_meta, bytes_to_path, container_and_path, container_fs_path,
    create_host_symlink, entry_meta, host_meta, path_to_bytes, render_container_status,
    resolve_tree, split_ref_path, with_target, with_target_mut, META_CTL, META_STATUS,
};
use hako::fs::{DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE};
use hako::{Hash, RouteTarget, ScopedFs};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Usage text returned by `cat /containers/<name>/ctl`. The control node is
/// write-driven: `hako write /containers/<name>/ctl "<command>"`.
const CTL_USAGE: &str = "\
ctl — write a command to control this container:
  hako write /containers/<name>/ctl \"commit [message]\"   snapshot the working tree
  hako write /containers/<name>/ctl \"branch <name>\"      create a branch at HEAD
  hako write /containers/<name>/ctl \"tag <name>\"         tag HEAD
";

pub fn write(
    ctx: &Ctx<'_>,
    path: String,
    file: Option<PathBuf>,
    content: Option<String>,
) -> io::Result<ExitCode> {
    let bytes = if let Some(p) = file {
        std::fs::read(p)?
    } else if let Some(s) = content {
        // `-` is the conventional stdin sentinel (matches the help text).
        // To write a literal `-`, pass via `--file` or stdin instead.
        if s == "-" {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf)?;
            buf
        } else {
            s.into_bytes()
        }
    } else {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        buf
    };
    let path = apply_cwd(ctx.session, &path);
    // Meta surface: writing under /containers/<name> that is NOT a filesystem
    // path (i.e. not under root/) targets a control/meta node. `ctl` dispatches
    // a control verb; other meta nodes are read-only.
    if let RouteTarget::Container { name, path: sub } = RouteTarget::parse(&path) {
        if container_fs_path(&sub).is_none() {
            if sub == META_CTL {
                return dispatch_ctl(ctx, &name, &bytes);
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "/containers/{name}/{sub} is not writable; write the filesystem under \
                     /containers/{name}/root/, or a command to /containers/{name}/ctl"
                ),
            ));
        }
    }
    with_target_mut(ctx.state, ctx.default_container, &path, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
        let new_root = scoped.write_file(&root, p, &bytes)?;
        repo.set_working(new_root)
    })?;
    Ok(ExitCode::SUCCESS)
}

/// Dispatch a container control verb written to `/containers/<name>/ctl`.
/// The body is a text command: the first token is the verb, the rest its
/// argument (the Plan 9 ctl-file model). Holds the workspace lock via the
/// `write` command, so the dispatched action is serialized.
///
/// Supported today (all container-addressed and cross-platform): `commit
/// [message]`, `branch <name>`, `tag <name>`. Instance verbs (start/stop/exec)
/// are intentionally not here yet: they are instance-addressed and
/// platform-specific, so they land in a later pass.
fn dispatch_ctl(ctx: &Ctx<'_>, name: &str, body: &[u8]) -> io::Result<ExitCode> {
    let text = std::str::from_utf8(body)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ctl command must be UTF-8"))?
        .trim();
    let mut parts = text.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match verb {
        "" => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ctl: empty command; try `commit [message]`",
        )),
        "commit" => {
            let repo = ctx.state.open_container(name)?;
            let message = if arg.is_empty() {
                "commit via ctl"
            } else {
                arg
            };
            super::vc::commit_repo(&repo, message, "ctl")
        }
        "branch" => {
            let new_branch = require_ctl_arg(verb, arg, "<name>")?;
            let repo = ctx.state.open_container(name)?;
            let target = repo
                .head_commit()?
                .ok_or_else(|| io::Error::other("no HEAD commit to branch from"))?;
            repo.write_ref(new_branch, target)?;
            println!(
                "created branch {} at {}",
                new_branch,
                &target.to_hex()[..12]
            );
            Ok(ExitCode::SUCCESS)
        }
        "tag" => {
            let tag_name = require_ctl_arg(verb, arg, "<name>")?;
            let repo = ctx.state.open_container(name)?;
            let target = repo
                .head_commit()?
                .ok_or_else(|| io::Error::other("no HEAD commit to tag"))?;
            repo.write_tag(tag_name, target)?;
            println!("tagged {} at {}", tag_name, &target.to_hex()[..12]);
            Ok(ExitCode::SUCCESS)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ctl: unsupported command {other:?}; \
                 supported: commit [message], branch <name>, tag <name>"
            ),
        )),
    }
}

/// Require a non-empty argument for a ctl verb, with a usage-shaped error.
fn require_ctl_arg<'a>(verb: &str, arg: &'a str, shape: &str) -> io::Result<&'a str> {
    if arg.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ctl: `{verb}` needs an argument: {verb} {shape}"),
        ));
    }
    Ok(arg)
}

pub fn cat(ctx: &Ctx<'_>, path: String) -> io::Result<ExitCode> {
    let (refspec, rest) = split_ref_path(&path);
    let rest = apply_cwd(ctx.session, rest);
    // Meta surface: under the `root/` layout, the container filesystem is at
    // `/containers/<name>/root/...`; anything else under the container is meta.
    // `cat /containers/<name>` (the container dir) and `cat /containers/<name>/status`
    // both read the synthetic status readout. A ref (`<ref>:<path>`) always means
    // the filesystem tree, so meta interception only applies without a ref.
    if refspec.is_none() {
        if let RouteTarget::Container { name, path } = RouteTarget::parse(&rest) {
            if container_fs_path(&path).is_none() {
                // Not a filesystem path → the meta surface.
                if path.is_empty() || path == META_STATUS {
                    let repo = ctx.state.open_container(&name)?;
                    let bytes = render_container_status(&repo, &name)?;
                    io::stdout().write_all(&bytes)?;
                    return Ok(ExitCode::SUCCESS);
                }
                if path == META_CTL {
                    // The control node reads back its usage (it is write-driven).
                    let _ = ctx.state.open_container(&name)?; // validate it exists
                    io::stdout().write_all(CTL_USAGE.as_bytes())?;
                    return Ok(ExitCode::SUCCESS);
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such meta node: /containers/{name}/{path}"),
                ));
            }
        }
    }
    with_target(ctx.state, ctx.default_container, &rest, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = match refspec {
            Some(r) => resolve_tree(repo, r)?,
            None => repo.working_tree()?,
        };
        let bytes = scoped.read_file(&root, p)?;
        io::stdout().write_all(&bytes)
    })?;
    Ok(ExitCode::SUCCESS)
}

pub fn mkdir(ctx: &Ctx<'_>, path: String) -> io::Result<ExitCode> {
    let path = apply_cwd(ctx.session, &path);
    with_target_mut(ctx.state, ctx.default_container, &path, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
        let new_root = scoped.mkdir(&root, p)?;
        repo.set_working(new_root)
    })?;
    Ok(ExitCode::SUCCESS)
}

pub fn del(ctx: &Ctx<'_>, path: String) -> io::Result<ExitCode> {
    let path = apply_cwd(ctx.session, &path);
    with_target_mut(ctx.state, ctx.default_container, &path, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
        let new_root = scoped.delete(&root, p)?;
        repo.set_working(new_root)
    })?;
    Ok(ExitCode::SUCCESS)
}

pub fn cp(ctx: &Ctx<'_>, src: String, dst: String) -> io::Result<ExitCode> {
    let src = apply_cwd(ctx.session, &src);
    let dst = apply_cwd(ctx.session, &dst);
    two_target_cp(ctx, &src, &dst, false)?;
    Ok(ExitCode::SUCCESS)
}

pub fn mv(ctx: &Ctx<'_>, src: String, dst: String) -> io::Result<ExitCode> {
    let src = apply_cwd(ctx.session, &src);
    let dst = apply_cwd(ctx.session, &dst);
    two_target_cp(ctx, &src, &dst, true)?;
    Ok(ExitCode::SUCCESS)
}

pub fn import(ctx: &Ctx<'_>, src: PathBuf, dst: String, force: bool) -> io::Result<ExitCode> {
    let dst = apply_cwd(ctx.session, &dst);
    let mut count = 0u64;
    with_target_mut(ctx.state, ctx.default_container, &dst, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
        // Don't silently clobber an existing vfs file (symmetric with export's
        // --force). Importing INTO a directory still merges, as expected.
        if !force && scoped.is_file(&root, p)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} already exists in the container; pass --force to overwrite",
                    p
                ),
            ));
        }
        let new_root = import_path(&scoped, &root, &src, p, &mut count)?;
        repo.set_working(new_root)
    })?;
    println!(
        "imported {} file(s) from {} to {}",
        count,
        src.display(),
        dst
    );
    Ok(ExitCode::SUCCESS)
}

pub fn export(ctx: &Ctx<'_>, src: String, dst: PathBuf, force: bool) -> io::Result<ExitCode> {
    let (refspec, rest) = split_ref_path(&src);
    let rest = apply_cwd(ctx.session, rest);
    let mut count = 0u64;
    with_target(ctx.state, ctx.default_container, &rest, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = match refspec {
            Some(r) => resolve_tree(repo, r)?,
            None => repo.working_tree()?,
        };
        export_path(&scoped, &root, p, &dst, force, &mut count)?;
        Ok(())
    })?;
    println!(
        "exported {} file(s) from {} to {}",
        count,
        src,
        dst.display()
    );
    Ok(ExitCode::SUCCESS)
}

// ----------------------------------------------------------------------------
// Internals
// ----------------------------------------------------------------------------

/// Copy (or move) src→dst, possibly crossing containers. Both containers share the
/// workspace's chunk store, so cross-container copies don't duplicate file content
/// chunks — only tree nodes change.
fn two_target_cp(ctx: &Ctx<'_>, src: &str, dst: &str, is_move: bool) -> io::Result<()> {
    let src_t = RouteTarget::parse(src);
    let dst_t = RouteTarget::parse(dst);
    let (src_container, src_path) = container_and_path(&src_t, ctx.default_container)?;
    let (dst_container, dst_path) = container_and_path(&dst_t, ctx.default_container)?;

    if src_container == dst_container {
        let repo = ctx.state.open_container(src_container)?;
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
        let new_root = if is_move {
            scoped.mv(&root, src_path, dst_path)?
        } else {
            scoped.cp(&root, src_path, dst_path)?
        };
        repo.set_working(new_root)?;
        return Ok(());
    }

    let src_repo = ctx.state.open_container(src_container)?;
    let dst_repo = ctx.state.open_container(dst_container)?;
    let scoped = ScopedFs::new(src_repo.store());
    let src_root = src_repo.working_tree()?;
    let dst_root = dst_repo.working_tree()?;
    let new_dst = scoped.cp_to(&src_root, &dst_root, src_path, dst_path)?;
    dst_repo.set_working(new_dst)?;
    if is_move {
        let new_src = scoped.delete(&src_root, src_path)?;
        src_repo.set_working(new_src)?;
    }
    Ok(())
}

/// Recursively import a host file or directory into the vfs at `dst`.
/// Preserves POSIX mode (on unix), mtime, and symlinks.
fn import_path(
    scoped: &ScopedFs<'_>,
    root: &Hash,
    src: &Path,
    dst: &str,
    count: &mut u64,
) -> io::Result<Hash> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        let target_bytes = path_to_bytes(&target);
        let (mode, mtime) = host_meta(&meta, DEFAULT_SYMLINK_MODE);
        *count += 1;
        return scoped.write_symlink(root, dst, &target_bytes, mode, mtime);
    }
    if meta.is_file() {
        let bytes = std::fs::read(src)?;
        let (mode, mtime) = host_meta(&meta, DEFAULT_FILE_MODE);
        *count += 1;
        return scoped.write_file_meta(root, dst, &bytes, mode, mtime);
    }
    if meta.is_dir() {
        let mut new_root = *root;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => {
                    eprintln!("hako: skipping non-utf8 name in {}", src.display());
                    continue;
                }
            };
            let child_src = entry.path();
            let child_dst = if dst.is_empty() {
                name
            } else {
                format!("{}/{}", dst.trim_end_matches('/'), name)
            };
            new_root = import_path(scoped, &new_root, &child_src, &child_dst, count)?;
        }
        return Ok(new_root);
    }
    // Sockets, devices, fifos: skip silently.
    Ok(*root)
}

/// Recursively export a vfs file or directory to the host at `dst`.
/// `force` allows overwriting existing host files. Restores POSIX mode on
/// unix; recreates symlinks where the host supports them.
fn export_path(
    scoped: &ScopedFs<'_>,
    root: &Hash,
    src: &str,
    dst: &Path,
    force: bool,
    count: &mut u64,
) -> io::Result<()> {
    if scoped.is_symlink(root, src)? {
        if std::fs::symlink_metadata(dst).is_ok() {
            if !force {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists (use --force)", dst.display()),
                ));
            }
            std::fs::remove_file(dst).or_else(|_| std::fs::remove_dir(dst))?;
        }
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let target_bytes = scoped.read_symlink(root, src)?;
        let target = bytes_to_path(&target_bytes);
        if let Err(e) = create_host_symlink(&target, dst) {
            eprintln!(
                "hako: could not create symlink {} -> {}: {} (writing target as a regular file)",
                dst.display(),
                target.display(),
                e
            );
            std::fs::write(dst, &target_bytes)?;
        }
        *count += 1;
        return Ok(());
    }
    if scoped.is_file(root, src)? {
        if dst.exists() && !force {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists (use --force)", dst.display()),
            ));
        }
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let bytes = scoped.read_file(root, src)?;
        std::fs::write(dst, bytes)?;
        let (mode, mtime) = entry_meta(scoped, root, src)?.unwrap_or((DEFAULT_FILE_MODE, 0));
        apply_host_meta(dst, mode, mtime)?;
        *count += 1;
        return Ok(());
    }
    if scoped.is_dir(root, src)? || src.is_empty() {
        std::fs::create_dir_all(dst)?;
        for child in scoped.ls(root, src)? {
            let child_src = if src.is_empty() {
                child.name.clone()
            } else {
                format!("{}/{}", src.trim_end_matches('/'), child.name)
            };
            let child_dst = dst.join(&child.name);
            export_path(scoped, root, &child_src, &child_dst, force, count)?;
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("vfs path not found: /{}", src),
    ))
}
