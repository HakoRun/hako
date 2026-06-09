//! Workspace maintenance: gc + fsck.

use super::Ctx;
use std::io;
use std::process::ExitCode;

pub fn gc(ctx: &Ctx<'_>, dry_run: bool) -> io::Result<ExitCode> {
    let report = hako::gc(ctx.state, dry_run).map_err(|e| io::Error::other(e.to_string()))?;
    let action = if dry_run { "would delete" } else { "deleted" };
    println!(
        "objects: {} total, {} reachable, {} unreachable",
        report.total_objects,
        report.reachable,
        report.deleted
    );
    println!("{} {} object(s); freed {} bytes", action, report.deleted, report.bytes_freed);
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
