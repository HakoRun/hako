# Building hako

## Default build (Linux dev loop or no-bootstrap Win/Mac wrapper)

```bash
cargo build --workspace --release
```

Produces a single `hako` binary at `target/release/hako`. On Linux this is
the full hako: native CLI + runtime. On Windows/macOS it's the native CLI
plus the `host_bridge` that forwards runtime ops (`run`, `apply`, `exec`,
`hako <unknown>`) to a Linux env you've set up yourself (a WSL distro or
Lima VM with the Linux `hako` binary on its PATH). The released Win/Mac
binaries embed the Linux runtime instead and auto-bootstrap it (see below).

This build does not embed a Linux binary — `hako bootstrap` will say so
and exit cleanly without trying to create a distro.

## Embedded-binary build (Win/Mac auto-bootstrap)

To ship a Win/Mac wrapper that auto-creates the WSL distro / Lima VM and
injects the Linux hako binary on first runtime command:

### 1. Cross-compile the Linux binary

The `xtask` helper wraps the cargo invocations:

```bash
cargo xtask build-linux           # x86_64-unknown-linux-musl → vendored/hako-linux-x64
cargo xtask build-linux --arm64   # aarch64-unknown-linux-musl → vendored/hako-linux-arm64
```

This must be run from a host that can cross-compile to musl. From Linux
that means installing the target and a musl linker; from Windows or
macOS the easiest path is `cross` (Docker-based):

```bash
cargo install cross
cross build --release --target x86_64-unknown-linux-musl -p hako-cli
cp target/x86_64-unknown-linux-musl/release/hako vendored/hako-linux-x64
```

The output is a static binary, ~10 MB.

### 2. Build the host wrapper with `--features embedded`

```bash
cargo build --release -p hako-cli --features embedded
```

`include_bytes!` pulls the vendored Linux binary into the wrapper. Total
binary size: ~12 MB (host code + embedded Linux binary).

### 3. Verify

```
> hako bootstrap
hako: setting up WSL distro hako-runtime (one-time)...
hako: runtime ready

> hako run alpine sh
[forwards into WSL, runs alpine sh in a hako-managed namespace]
```

## Build pipeline / CI

For a release build of the cross-platform wrapper, the order is:

1. **Build the Linux binary first** on a Linux runner:
   ```bash
   cargo build --release --target x86_64-unknown-linux-musl -p hako-cli
   ```
2. **Upload the binary as an artifact** (`target/.../release/hako`).
3. **Build the Windows wrapper** on a Windows runner with the artifact
   downloaded into `vendored/hako-linux-x64`:
   ```bash
   cargo build --release -p hako-cli --features embedded
   ```
4. **Build the macOS wrapper** on a macOS runner, similarly. Repeat for
   arm64 if shipping Apple Silicon native.

## Toolchain notes

- **Targets needed**: `x86_64-unknown-linux-musl` (always), optionally
  `aarch64-unknown-linux-musl` for Apple Silicon native.
- **Musl linker** on the build host. On Debian/Ubuntu: `apt install
  musl-tools`. On Alpine: musl is the default. On Windows/macOS: easiest
  is `cross` which uses Docker.
- **`fuser` and musl**: hako-cli depends on `fuser` but builds it with
  `default-features = false` (no libfuse), mounting FUSE via `mount(2)`
  directly. So there is **no libfuse/pkg-config dependency** and the static
  musl build needs no extra C libraries — only the musl cross toolchain.

If `cargo xtask build-linux` fails with linker errors, the error message
points to the most likely fix.

## Knobs that affect runtime behavior (not build-time)

| Env var | Effect |
|---|---|
| `HAKO_DISTRO` | Override the WSL distro name (default `hako-runtime`) |
| `HAKO_LIMA_VM` | Override the Lima VM name (default `hako-runtime`) |
| `HAKO_NO_BRIDGE=1` | Skip the host bridge entirely; runtime ops error with `UnsupportedPlatform` (useful for testing the dispatch path on a non-Linux host) |
