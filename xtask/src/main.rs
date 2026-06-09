//! Build helpers for hako. Run via `cargo xtask <subcommand>`.
//!
//! Set up the alias once in `.cargo/config.toml`:
//!
//! ```toml
//! [alias]
//! xtask = "run --package xtask --"
//! ```

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

#[derive(Parser)]
#[command(about = "hako build helpers")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cross-compile hako-cli for Linux musl and copy the binary into
    /// `vendored/hako-linux-{x64|arm64}` so the host wrappers can embed
    /// it via `--features embedded`.
    BuildLinux {
        /// Build for aarch64 (Apple Silicon native) instead of x86_64.
        #[arg(long)]
        arm64: bool,
        /// Build profile (default: release).
        #[arg(long, default_value = "release")]
        profile: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::BuildLinux { arm64, profile } => build_linux(arm64, &profile),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn build_linux(arm64: bool, profile: &str) -> Result<(), String> {
    let target = if arm64 {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    };
    let arch_suffix = if arm64 { "arm64" } else { "x64" };

    eprintln!("xtask: cross-compiling hako-cli for {}", target);

    // Run cargo build at the workspace root.
    let workspace_root = workspace_root();
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&workspace_root)
        .arg("build")
        .arg("--target")
        .arg(target)
        .arg("--package")
        .arg("hako-cli");
    if profile == "release" {
        cmd.arg("--release");
    } else if profile != "dev" && profile != "debug" {
        cmd.arg("--profile").arg(profile);
    }

    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn cargo: {}", e))?;
    if !status.success() {
        return Err(format!(
            "cargo build failed (exit {}). \
             Cross-compilation to {} from this host typically needs:\n  \
               rustup target add {}\n  \
               and a musl linker (apt install musl-tools, or use `cross`)",
            status.code().unwrap_or(-1),
            target,
            target
        ));
    }

    // Copy the resulting binary into vendored/.
    let profile_dir = match profile {
        "release" => "release",
        "dev" | "debug" => "debug",
        other => other,
    };
    let src = workspace_root
        .join("target")
        .join(target)
        .join(profile_dir)
        .join("hako");
    let dst = workspace_root
        .join("vendored")
        .join(format!("hako-linux-{}", arch_suffix));

    eprintln!("xtask: copying {} → {}", src.display(), dst.display());
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("copy {} → {}: {}", src.display(), dst.display(), e))?;

    let size = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "xtask: vendored {} ({:.1} MiB). Build hako-cli with --features embedded to bundle it.",
        dst.file_name().unwrap_or_default().to_string_lossy(),
        size as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

fn workspace_root() -> PathBuf {
    // xtask lives at <workspace-root>/xtask/, so its CARGO_MANIFEST_DIR
    // parent is the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .expect("xtask Cargo.toml has a parent")
}
