# Vendored `toybox` binaries — provenance

`src/rootfs/toybox-<arch>` are third-party binaries vendored into the source tree
and embedded via `include_bytes!` (see `mod.rs`), one per supported target arch.
The build embeds the binary matching the **build target**, so the default `hako`
container's `bin/<applet>` → `toybox` userland matches the CPU it runs on. An
x86_64 shell seeded on an arm64 host execs as `ENOEXEC` — issue #34.

| | x86-64 | aarch64 |
|---|---|---|
| Project | toybox — https://landley.net/toybox/ | toybox |
| Version | 0.8.13 | 0.8.13 |
| Source | `…/downloads/binaries/0.8.13/toybox-x86_64` | `…/downloads/binaries/0.8.13/toybox-aarch64` |
| Linkage | static, stripped ELF | static, stripped ELF |
| File | `src/rootfs/toybox-x86_64` | `src/rootfs/toybox-aarch64` |
| Size | 752152 bytes | 833872 bytes |
| SHA-256 | `8c98795a15db31ea55c8065fed379db3669766b7a714c46b009d8bfb87b25ffd` | `b3508e5f51a0d429c1bda9d500d98d97dc0b86571762eeb099495eb238a8c52a` |
| License | 0BSD — https://landley.net/toybox/license.html | 0BSD |

Both are from the same 0.8.13 release directory:
<https://landley.net/toybox/downloads/binaries/0.8.13/>

A target with **no** vendored binary (neither x86_64 nor aarch64) embeds an empty
slice: `rootfs::is_available()` is then false and `hako init` reports "embedded
toybox rootfs not available" rather than seeding a rootfs whose shell can't exec.
Pull an OCI image for a userland on such a target.

## Why vendored (and the plan to stop)

Committed directly so a fresh `cargo build` is self-contained — no network or
build-time fetch. The cost is repo size (~1.5 MB across both arches) and no
build-time integrity check.

**Intended future:** fetch only the build target's binary at build time
(`build.rs`/`xtask`), verifying the SHA-256s above, and drop them from version
control — or move them to release assets / Git LFS. Until then, these files are
the provenance record.

## Updating

1. Download `toybox-x86_64` and `toybox-aarch64` from the **same** release under
   <https://landley.net/toybox/downloads/binaries/> (keep both arches on one
   version).
2. Verify each checksum, replace `src/rootfs/toybox-<arch>`, and update the
   version + SHA-256 + size here and the comment in `mod.rs`.
3. Re-run `cargo test -p hako-core` (the rootfs tests compare against the embedded
   bytes) — and note CI runs the isolation check on **both** x86_64 and arm64, so
   an arch mismatch surfaces there.
