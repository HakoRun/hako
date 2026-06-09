# Contributing to hako

Thanks for your interest! hako is a content-addressed, version-controlled
filesystem with a built-in container runtime. This guide covers how to build,
test, and submit changes.

## Build

Requires a recent stable Rust toolchain (developed against 1.96).

```sh
cargo build --workspace        # full build
cargo build -p hako-cli        # just the `hako` binary
```

The Linux container runtime links `libfuse`; on Debian/Ubuntu install
`libfuse3-dev` + `pkg-config`. For the cross-platform / embedded build, see
[BUILD.md](BUILD.md).

## Before you open a PR

CI gates every push on these — run them locally first:

```sh
cargo fmt --all                                        # format
cargo fmt --all -- --check                             # CI check
cargo clippy --workspace --all-targets -- -D warnings  # lint (warnings are errors)
cargo test --workspace                                 # unit + integration tests
```

### Runtime / isolation changes

`hako-runtime` (namespaces, mounts, `pivot_root`, PID-1 init) is **Linux-only**
and security-critical. It is not exercised by `cargo test` — verify it on Linux
(native or WSL2) by running a real container and checking the isolation
properties:

```sh
HAKO=target/debug/hako bash scripts/isolation-check.sh
```

This asserts a private PID view, no host `$HOME`, a private `/tmp`, and network
isolation. CI runs the same script in the `isolation` job; **a runtime PR must
keep it green.** If you touch the namespace/mount/fork logic, run it before
submitting — and treat the comments in `transform.rs` (the fork ordering, why
`AutoUnmount`/`AllowOther` are off, the store-survives-pivot reasoning) as
load-bearing.

## Conventions

- **Commits:** Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `ci:`).
- **Branches + PRs:** land changes via a reviewed PR; keep `main` green.
- **Errors:** prefer typed errors over `unwrap()`/`panic!` in library code.
- **Docs:** keep `README.md` and `docs/runtime-isolation.md` accurate when
  behavior changes.

## Project layout

| Crate | Responsibility |
|-------|----------------|
| `hako-core` | storage engine: prolly tree, chunk store, FS, repo, OCI, FUSE |
| `hako-cli` | the `hako` binary: command dispatch + host bridge |
| `hako-runtime` | Linux container instances (namespaces, lifecycle) |
| `xtask` | build automation (cross-compiles the embedded Linux binary) |

See `docs/runtime-isolation.md` for the runtime's design and open hardening items.
