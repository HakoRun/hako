//! File operations: write, cat, mkdir, del, cp, mv, import, export.

use super::Ctx;
use crate::helpers::{
    apply_cwd, apply_host_meta, bytes_to_path, container_and_path, create_host_symlink, entry_meta,
    host_meta, path_to_bytes, resolve_tree, split_ref_path, with_target, with_target_mut,
};
use hako::fs::{DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE};
use hako::{Hash, RouteTarget, ScopedFs};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
    with_target_mut(ctx.state, ctx.default_container, &path, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
        let new_root = scoped.write_file(&root, p, &bytes)?;
        repo.set_working(new_root)
    })?;
    Ok(ExitCode::SUCCESS)
}

pub fn cat(ctx: &Ctx<'_>, path: String) -> io::Result<ExitCode> {
    let (refspec, rest) = split_ref_path(&path);
    let rest = apply_cwd(ctx.session, rest);
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

pub fn import(ctx: &Ctx<'_>, src: PathBuf, dst: String) -> io::Result<ExitCode> {
    let dst = apply_cwd(ctx.session, &dst);
    let mut count = 0u64;
    with_target_mut(ctx.state, ctx.default_container, &dst, |repo, p| {
        let scoped = ScopedFs::new(repo.store());
        let root = repo.working_tree()?;
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
