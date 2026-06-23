//! `hako bundle` — package a container + command into a single self-contained
//! executable that runs the app through hako with no prior hako install.
//!
//! First cut: a Unix self-extracting bundle — a `/bin/sh` header with a
//! gzipped tar payload appended (makeself-style). The payload carries the hako
//! runtime binary plus the workspace store; on first run it extracts to a cache
//! dir and execs `hako run` (display passthrough is automatic). The native
//! Windows `.exe` stub — embedding the Linux runtime and reusing the WSL
//! bootstrap — is the next phase.

use super::Ctx;
use crate::DOT_HAKO;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// Build a self-contained bundle for `container` running `cmd`.
pub fn create(
    ctx: &Ctx<'_>,
    container: String,
    cmd: Vec<String>,
    output: PathBuf,
) -> io::Result<ExitCode> {
    // Validate the container exists and resolve its branch.
    let repo = ctx.state.open_container(&container).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("container '{}' not found ({})", container, e),
        )
    })?;
    let branch = repo.current_branch()?.unwrap_or_else(|| "main".into());

    // Stable cache id (keys the extraction dir on the target machine).
    let id =
        hako::Hash::of(format!("{container}\u{0}{branch}\u{0}{}", cmd.join("\u{0}")).as_bytes());
    let id = id.to_hex()[..12].to_string();

    // Stage layout: <tmp>/{hako, ws/.hako}
    let stage = std::env::temp_dir().join(format!("hako-bundle-stage-{id}"));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(stage.join("ws"))?;

    // The runtime the bundle ships must be a *Linux* hako (the app always runs
    // in Linux). Prefer the embedded, cross-compiled, release-stripped binary
    // — it's small and correct even when bundling from a Windows/macOS host
    // (where `current_exe()` is not a Linux binary). Fall back to the running
    // binary only in a dev build with no embedded runtime, which by definition
    // is itself the native Linux hako.
    let staged_hako = stage.join("hako");
    let embedded = crate::host_bridge::embedded_for_host();
    if embedded.is_empty() {
        std::fs::copy(std::env::current_exe()?, &staged_hako)?;
    } else {
        std::fs::write(&staged_hako, embedded)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged_hako, std::fs::Permissions::from_mode(0o755))?;
    }

    // Build a PRUNED workspace holding only this container's reachable objects
    // (not the whole source .hako, which carries every other container and all
    // of history). A fresh `State::init` seeds a toybox `hako` container; we
    // drop it and gc so the bundle ships just the target.
    let commit = repo.read_ref(&branch)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("branch '{}' not found in container '{}'", branch, container),
        )
    })?;
    let ws_dot = stage.join("ws").join(DOT_HAKO);
    let dst_state = hako::State::init(&ws_dot)?;
    let _ = dst_state.delete_container("hako");
    let dst_repo = dst_state.create_container(&container)?;

    // Copy only the objects reachable from the target commit.
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
    // Reclaim the toybox seed's now-unreferenced objects.
    let _ = hako::gc(&dst_state, false);
    drop(dst_repo);
    drop(dst_state);
    eprintln!(
        "hako: bundled {} reachable objects for '{}'",
        copied, container
    );

    // tar.gz the stage into a payload.
    let payload = stage.with_extension("tgz");
    run_tool(
        Command::new("tar")
            .arg("czf")
            .arg(&payload)
            .arg("-C")
            .arg(&stage)
            .arg("."),
        "build the bundle payload (tar)",
    )?;
    let payload_bytes = std::fs::read(&payload)?;

    // Assemble: header script + marker line + payload bytes.
    let header = render_header(&id, &container, &branch, &cmd);
    let mut out = std::fs::File::create(&output)?;
    out.write_all(header.as_bytes())?;
    out.write_all(b"__HAKO_PAYLOAD_BELOW__\n")?;
    out.write_all(&payload_bytes)?;
    out.flush()?;
    drop(out);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&output)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&output, perms)?;
    }

    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_file(&payload);

    let size = std::fs::metadata(&output)?.len() as f64 / (1024.0 * 1024.0);
    let shown = if cmd.is_empty() {
        "(interactive shell)".to_string()
    } else {
        cmd.join(" ")
    };
    println!(
        "bundled container '{}' [{}] → {} ({:.1} MiB)",
        container,
        shown,
        output.display(),
        size
    );
    println!("run it on any Linux host with:  {}", output.display());
    Ok(ExitCode::SUCCESS)
}

fn run_tool(cmd: &mut Command, what: &str) -> io::Result<()> {
    let status = cmd
        .status()
        .map_err(|e| io::Error::other(format!("failed to {what}: {e}")))?;
    if !status.success() {
        return Err(io::Error::other(format!("failed to {what}")));
    }
    Ok(())
}

fn render_header(id: &str, container: &str, branch: &str, cmd: &[String]) -> String {
    let cmd_str = cmd
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"#!/bin/sh
# Self-contained hako application bundle.
# Carries the hako runtime + a container store; extracts on first run and
# executes the app through hako (display passthrough is automatic).
set -eu
ID="{id}"
CACHE="${{XDG_CACHE_HOME:-$HOME/.cache}}/hako-bundles/$ID"
if [ ! -f "$CACHE/.ready" ]; then
  mkdir -p "$CACHE"
  start=$(awk '/^__HAKO_PAYLOAD_BELOW__$/ {{ print NR + 1; exit }}' "$0")
  tail -n +"$start" "$0" | tar xzf - -C "$CACHE"
  touch "$CACHE/.ready"
fi
exec "$CACHE/hako" -w "$CACHE/ws" -c "{container}" run "{branch}" {cmd_str}
echo "hako: failed to launch bundle" >&2
exit 1
"#
    )
}

/// POSIX single-quote a shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
