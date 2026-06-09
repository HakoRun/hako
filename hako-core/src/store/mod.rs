use crate::hash::Hash;
use std::io;

pub mod fs;
pub mod mem;

pub trait ChunkStore: Send + Sync {
    fn put(&self, data: &[u8]) -> io::Result<Hash>;
    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>>;
    fn has(&self, hash: &Hash) -> io::Result<bool>;

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
