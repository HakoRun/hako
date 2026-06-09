//! Filesystem layer over the prolly tree: `DirEntry` encoding (file /
//! directory / symlink with POSIX-ish metadata) and the `ScopedFs` API
//! that hangs path-based read/write/ls/cp/mv/delete operations off any
//! `ChunkStore`.

mod entry;
mod scoped;

pub use entry::{
    decode_entry, encode_entry, normalize_path, DirChild, DirEntry, DirKind, FileEntry,
    SymlinkEntry, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE,
};
pub use scoped::ScopedFs;
