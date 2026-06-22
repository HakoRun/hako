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

    let hako_bin = std::env::current_exe()?;

    // Stage layout: <tmp>/{hako, ws/.hako}
    let stage = std::env::temp_dir().join(format!("hako-bundle-stage-{id}"));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(stage.join("ws"))?;
    std::fs::copy(&hako_bin, stage.join("hako"))?;

    // Copy the workspace store. First cut ships the whole .hako; pruning to the
    // single container's reachable chunks is a follow-up.
    let dot_hako = ctx.workdir.join(DOT_HAKO);
    if !dot_hako.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no {} workspace at {}", DOT_HAKO, ctx.workdir.display()),
        ));
    }
    run_tool(
        Command::new("cp")
            .arg("-a")
            .arg(&dot_hako)
            .arg(stage.join("ws").join(DOT_HAKO)),
        "copy workspace into the bundle stage",
    )?;

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
