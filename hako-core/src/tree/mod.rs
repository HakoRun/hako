// The submodules are crate-internal, not public: `repo`, `fs`, and `rootfs`
// legitimately reach into the tree's low-level ops, but nothing OUTSIDE the crate
// may depend on the on-disk format. The curated `pub use` block below is the
// entire public API (`hako::tree::*`).
pub(crate) mod cursor;
pub(crate) mod diff;
pub(crate) mod merge;
pub(crate) mod node;
pub(crate) mod ops;
pub(crate) mod set_ops;
pub(crate) mod types;

// Public API (`hako::tree::*`): content-addressed KV + set semantics, diff, and
// three-way merge. Deliberately EXCLUDES the on-disk format geometry — `Node`,
// `NodeKind`, `Entry`, the `Cursor`, and the chunking parameters (`is_boundary`,
// `level_salt`, `INLINE_THRESHOLD`, `TARGET_FANOUT`; see the invariants note in
// `types.rs`). Those are format details a consumer must never be able to build on.
pub use diff::diff;
pub use merge::three_way_merge;
pub use ops::{count, load_value};
pub use set_ops::{diff_walk, difference, intersection, merge_walk, union, Pair};
pub use types::{Conflict, DiffEntry, MergeResult, Value};

// Crate-internal, NOT public: the low-level tree KV ops sit behind `fs::ScopedFs`
// (the intended path-level API), and `NodeKind` behind `repo`'s reachable walk.
// Re-exported at `crate::tree::*` so in-crate callers use the short path.
pub(crate) use ops::{bulk_build, delete, empty, get, put, scan, scan_prefix, store_value};
pub(crate) use types::NodeKind;
