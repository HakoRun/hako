//! Ensure the embedded-binary slots in `vendored/` exist as files so
//! `include_bytes!` in `host_bridge` always compiles.
//!
//! When `cargo xtask build-linux` has produced a real binary, leave it
//! alone. When it hasn't, drop a tiny zero-length stub so the build
//! still succeeds. `host_bridge` checks the byte slice's length at
//! runtime to distinguish "real binary" from "stub".

use std::fs;
use std::path::PathBuf;

fn main() {
    // build.rs lives in hako-cli/, vendored/ is at workspace root.
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .expect("hako-cli has a parent (workspace root)");
    let vendored = workspace_root.join("vendored");

    // Best-effort: if the directory doesn't exist (someone deleted it),
    // recreate it so include_bytes! can find the stubs.
    let _ = fs::create_dir_all(&vendored);

    for arch_suffix in &["x64", "arm64"] {
        let p = vendored.join(format!("hako-linux-{}", arch_suffix));
        if !p.exists() {
            // Empty file. host_bridge::EMBEDDED_LINUX_*.is_empty() → fallback.
            let _ = fs::write(&p, b"");
        }
        // Tell cargo to re-run if the file changes (so a fresh xtask
        // build-linux output gets picked up next build).
        println!("cargo:rerun-if-changed={}", p.display());
    }
}
