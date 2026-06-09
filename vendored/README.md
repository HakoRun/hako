# `vendored/` — pre-built artifacts embedded in host wrappers

When hako-cli is built with `--features embedded`, the Windows and macOS
wrappers `include_bytes!` the Linux hako binary from this directory and
inject it into a WSL distro / Lima VM at first use.

## Files this dir holds when populated

| File | Source | Used on |
|---|---|---|
| `hako-linux-x64`   | `cargo xtask build-linux`         | Windows + Intel macOS |
| `hako-linux-arm64` | `cargo xtask build-linux --arm64` | Apple Silicon macOS |

## Producing the binaries

The cross-compile target is `*-unknown-linux-musl` (static, no glibc
dependency) so the binary runs in a minimal rootfs without needing
shared libraries.

```bash
# from a Linux host or WSL/Docker on Windows:
cargo xtask build-linux           # x86_64
cargo xtask build-linux --arm64   # aarch64
```

If neither file exists, `build.rs` writes empty stubs so the build
still completes; `include_bytes!` then yields a zero-length blob and
`host_bridge` falls back to expecting `hako` on PATH inside the
user's WSL/Lima env (the no-bootstrap dev workflow).
