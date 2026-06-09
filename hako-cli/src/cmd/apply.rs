//! `hako apply` — bring the workspace into the state declared by hako.toml.
//!
//! For the active profile (dev or prod), this:
//!
//! 1. Pulls the configured `image` into the configured `name` container if
//!    it doesn't exist yet.
//! 2. Walks the `setup` list. For each step we haven't seen before, runs
//!    `sh -c <step>` inside the container (RW mount, workspace bound per
//!    `workspace` mode), then commits the resulting tree to the
//!    container's branch as a new commit. Records the step's hash in
//!    `.hako/applied` so re-running apply skips it.
//!
//! Idempotent: rerunning `apply` after no changes is a no-op. `--force`
//! forgets the applied hashes and re-runs everything. `--dry-run` reports
//! the plan without touching disk or network.

use super::Ctx;
use crate::DOT_HAKO;
use hako::{AppConfig, AppOverrides, Config, Hash, ImageRef, WorkspaceMode};
use hako_runtime::VolumeMount;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const APPLIED_FILE: &str = "applied";

pub fn apply(
    ctx: &Ctx<'_>,
    cfg: &Config,
    overrides: &AppOverrides,
    dry_run: bool,
    force: bool,
) -> io::Result<ExitCode> {
    let app_base = match &cfg.app {
        Some(a) => a,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no hako.toml in workspace root (run from a directory with hako.toml, or pass -w <project>)",
            ));
        }
    };
    // Clone-then-mutate so the caller's Config is unaffected.
    let mut owned = app_base.clone();
    overrides.apply_to(&mut owned);
    let app = &owned;

    println!(
        "applying hako.toml: image={}, name={}, workspace={:?}",
        app.image, app.name, app.workspace
    );

    // Phase 1: ensure the container exists, pulling the image if needed.
    let needs_pull = !ctx.state.list_containers()?.iter().any(|c| c == &app.name);
    if needs_pull {
        if dry_run {
            println!("would pull {} into container {}", app.image, app.name);
        } else {
            let image_ref = ImageRef::parse(&app.image).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("hako.toml image: {}", e),
                )
            })?;
            super::oci::pull_into(ctx.state, &image_ref, &app.name, "linux", "amd64", false)?;
        }
    }

    // Phase 2: setup steps.
    if app.setup.is_empty() {
        println!("no setup steps; container ready");
        return Ok(ExitCode::SUCCESS);
    }

    let applied_path = ctx.workdir.join(DOT_HAKO).join(APPLIED_FILE);
    let mut applied: BTreeSet<String> = if force {
        BTreeSet::new()
    } else {
        read_applied(&applied_path).unwrap_or_default()
    };

    let mut ran = 0;
    let mut skipped = 0;
    for step in &app.setup {
        // Identify the step by hash of its bytes. hako::Hash uses blake3
        // under the hood, so the hex is stable across versions.
        let step_hash = Hash::of(step.as_bytes()).to_hex();
        let short_hash = &step_hash[..12];

        if applied.contains(&step_hash) {
            println!("skip [{}]  {}", short_hash, truncate(step, 60));
            skipped += 1;
            continue;
        }

        if dry_run {
            println!("would run [{}]  {}", short_hash, truncate(step, 60));
            continue;
        }

        println!("run  [{}]  {}", short_hash, truncate(step, 60));
        let new_root = run_setup_step(ctx, app, step)?;

        // Commit the result on the container's current branch.
        let repo = ctx.state.open_container(&app.name)?;
        let parents: Vec<Hash> = repo.head_commit()?.into_iter().collect();
        let ts = hako::io_util::now_secs_or_zero();
        let msg = format!("apply: {}", truncate(step, 100));
        let commit = repo.commit(new_root, parents, "hako-apply", &msg, ts)?;
        let branch = repo.current_branch()?.unwrap_or_else(|| "main".into());
        repo.write_ref(&branch, commit)?;
        repo.set_working(new_root)?;

        applied.insert(step_hash);
        write_applied(&applied_path, &applied)?;
        ran += 1;
    }

    if dry_run {
        println!(
            "dry-run summary: {} step(s) would run, {} cached",
            app.setup.len() - skipped,
            skipped
        );
    } else {
        println!("apply complete: {} step(s) ran, {} cached", ran, skipped);
    }

    Ok(ExitCode::SUCCESS)
}

/// Run one setup step inside the container's runtime, returning the new
/// tree root captured via the FUSE RW round-trip. The step is invoked as
/// `sh -c <step>` so the user can use shell features (`&&`, pipes, etc.).
fn run_setup_step(ctx: &Ctx<'_>, app: &AppConfig, step: &str) -> io::Result<Hash> {
    let repo = ctx.state.open_container(&app.name)?;
    let branch = repo.current_branch()?.unwrap_or_else(|| "main".into());
    let cmd = vec!["/bin/sh".into(), "-c".into(), step.into()];
    let volumes = build_volumes(ctx, app);
    let (exit, new_root) = hako_runtime::transform::run_container_rw(&repo, &branch, cmd, &volumes)
        .map_err(|e| io::Error::other(format!("setup step failed to start: {}", e)))?;
    if exit != 0 {
        return Err(io::Error::other(format!(
            "setup step exited with code {}: {}",
            exit, step
        )));
    }
    Ok(new_root)
}

/// Build the volume mount list for a hako.toml apply: just the workspace
/// mount per the configured mode. (Custom mounts via `[volumes]` are a
/// future addition.)
fn build_volumes(ctx: &Ctx<'_>, app: &AppConfig) -> Vec<VolumeMount> {
    match app.workspace {
        WorkspaceMode::None => Vec::new(),
        WorkspaceMode::Ro | WorkspaceMode::Rw => vec![VolumeMount {
            host: ctx.workdir.to_path_buf(),
            container: "/workspace".into(),
            readonly: matches!(app.workspace, WorkspaceMode::Ro),
        }],
    }
}

fn read_applied(path: &PathBuf) -> io::Result<BTreeSet<String>> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

fn write_applied(path: &Path, set: &BTreeSet<String>) -> io::Result<()> {
    let mut buf = String::new();
    for h in set {
        buf.push_str(h);
        buf.push('\n');
    }
    hako::io_util::atomic_write(path, buf.as_bytes())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Snap the cut to a UTF-8 char boundary so multibyte content (any
        // non-ASCII setup string from hako.toml) can't panic the slice.
        let mut end = n.saturating_sub(1).min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("echo hi", 80), "echo hi");
    }

    #[test]
    fn truncate_does_not_panic_on_multibyte_boundary() {
        // Each "é" is 2 bytes; cutting mid-char must snap to a boundary,
        // not panic. Exercise every cut length across a multibyte string.
        let s = "ééééééééééééééééééé"; // 19 × 2 bytes
        for n in 0..=s.len() + 2 {
            let t = truncate(s, n); // old impl panicked here on odd n
            if n < s.len() {
                assert!(t.ends_with('…'), "n={n} should mark truncation");
            } else {
                assert_eq!(t, s);
            }
        }
    }
}
