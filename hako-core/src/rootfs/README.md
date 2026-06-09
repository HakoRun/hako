# Vendored `toybox` binary — provenance

`src/rootfs/toybox` is a third-party binary vendored into the source tree and
embedded via `include_bytes!` (see `mod.rs`). It seeds the default `hako`
container with a usable Linux userland (`bin/<applet>` → `toybox`).

| | |
|---|---|
| Project | toybox — https://landley.net/toybox/ |
| Version | 0.8.13 |
| Source | https://landley.net/toybox/bin/toybox-x86_64 |
| Arch / linkage | x86-64, statically linked, stripped (ELF) |
| Size | 752152 bytes |
| SHA-256 | `8c98795a15db31ea55c8065fed379db3669766b7a714c46b009d8bfb87b25ffd` |
| License | 0BSD (Zero-Clause BSD) — see https://landley.net/toybox/license.html |

## Why it's vendored (and the plan to stop)

It's committed directly so a fresh `cargo build` produces a self-contained
binary with no network or build-time fetch. The cost is repo bloat (~734 KB in
every clone) and no build-time integrity check.

**Intended future:** fetch the binary at build time (in `build.rs` or `xtask`),
verifying the SHA-256 above, and drop it from version control — or move it to a
release asset / Git LFS. Until then, this file is the provenance record.

## Updating it

1. Download the new `toybox-<arch>` from the release above.
2. Verify its checksum, replace `src/rootfs/toybox`, and update the version +
   SHA-256 + size in this file and the comment in `mod.rs`.
3. Re-run `cargo test -p hako-core` (the rootfs determinism test pins the tree
   hash, so a binary change is a deliberate, visible change).
