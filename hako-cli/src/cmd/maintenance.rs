//! Workspace maintenance: gc + fsck.

use super::Ctx;
use crate::DOT_HAKO;
use std::io;
use std::process::ExitCode;

pub fn gc(ctx: &Ctx<'_>, dry_run: bool) -> io::Result<ExitCode> {
    // A live container writes new (uncommitted) chunks into the shared store
    // via its RW FUSE mount, but holds no workspace lock — so those chunks are
    // unreachable from any ref and a real gc would delete them out from under
    // the running workload, corrupting it. Refuse while any instance runs.
    // (A dry-run only reports, so it's safe.)
    if !dry_run {
        let runtime_root = ctx.workdir.join(DOT_HAKO);
        if let Ok(instances) = hako_runtime::instances::list(&runtime_root) {
            let running: Vec<&str> = instances
                .iter()
                .filter(|i| i.is_running())
                .map(|i| i.id.as_str())
                .collect();
            if !running.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    format!(
                        "refusing to gc while {} container(s) are running ({}): \
                         their uncommitted data would be deleted. Stop them \
                         (`hako stop <id>`) or use `hako gc --dry-run`.",
                        running.len(),
                        running.join(", ")
                    ),
                ));
            }
        }
    }
    let report = hako::gc(ctx.state, dry_run).map_err(|e| io::Error::other(e.to_string()))?;
    let action = if dry_run { "would delete" } else { "deleted" };
    println!(
        "objects: {} total, {} reachable, {} unreachable",
        report.total_objects, report.reachable, report.deleted
    );
    println!(
        "{} {} object(s); freed {} bytes",
        action, report.deleted, report.bytes_freed
    );
    Ok(ExitCode::SUCCESS)
}

pub fn fsck(ctx: &Ctx<'_>) -> io::Result<ExitCode> {
    let report = hako::fsck(ctx.state).map_err(|e| io::Error::other(e.to_string()))?;
    println!("checked {} reachable object(s)", report.checked);
    if report.ok() {
        println!("ok");
        Ok(ExitCode::SUCCESS)
    } else {
        for (h, msg) in &report.problems {
            eprintln!("  {}  {}", &h.to_hex()[..12], msg);
        }
        eprintln!("{} problem(s) found", report.problems.len());
        Ok(ExitCode::from(1))
    }
}
