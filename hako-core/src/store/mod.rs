use crate::hash::Hash;
use std::io;

pub mod fs;
pub mod mem;

pub trait ChunkStore: Send + Sync {
    fn put(&self, data: &[u8]) -> io::Result<Hash>;
    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>>;
    fn has(&self, hash: &Hash) -> io::Result<bool>;

    /// Read the `[offset, offset + len)` slice of the object `hash` (clamped to
    /// its end), without materialising the whole object to the caller. `None` if
    /// the object is absent. The default loads the full object and slices; the
    /// FUSE read path relies on `FsStore`'s cache-aware override so serving a
    /// large file across the kernel's ~128 KiB reads doesn't reclone it each time
    /// (an O(n^2) whole-file reload per read, #74).
    fn read_at(&self, hash: &Hash, offset: usize, len: usize) -> io::Result<Option<Vec<u8>>> {
        Ok(self.get(hash)?.map(|b| slice_clamped(&b, offset, len)))
    }

    /// Find every hash whose hex prefix matches `prefix` (lowercase hex).
    /// Used by the CLI to resolve abbreviated commit hashes, and by GC
    /// to enumerate all stored objects (with `prefix == ""`).
    fn find_by_prefix(&self, prefix: &str) -> io::Result<Vec<Hash>>;

    /// Remove an object. Returns `true` if it was present, `false` if not.
    /// Only used by `gc` — never invoke this from regular write paths,
    /// since immutable content addressing means the same bytes can show
    /// up under the same hash again later.
    fn delete(&self, hash: &Hash) -> io::Result<bool>;
}

pub use fs::FsStore;
pub use mem::MemStore;

/// Copy `data[offset .. offset+len]`, clamping both ends to `data`'s length, so
/// an out-of-range offset yields an empty slice rather than panicking (the kernel
/// can issue a read at or past EOF).
pub(crate) fn slice_clamped(data: &[u8], offset: usize, len: usize) -> Vec<u8> {
    let start = offset.min(data.len());
    let end = start.saturating_add(len).min(data.len());
    data[start..end].to_vec()
}
