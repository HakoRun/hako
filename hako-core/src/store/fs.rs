use super::ChunkStore;
use crate::hash::Hash;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Byte budget for the in-memory read-through cache. The cache only accelerates
/// reads — every chunk is authoritative on disk and `get` re-reads (and
/// re-verifies) on a miss — so eviction is always safe; it just costs a reread.
/// Bounding it prevents unbounded RAM growth on a long-lived process that
/// touches a large working set (e.g. a FUSE server over a multi-GB image).
const CACHE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// A size-bounded chunk cache. Not an LRU (no access ordering is tracked);
/// when over budget it evicts arbitrary entries, which is fine for a pure
/// read accelerator. Tracks total bytes so the budget check is O(1).
#[derive(Default)]
struct Cache {
    map: HashMap<Hash, Vec<u8>>,
    bytes: usize,
}

impl Cache {
    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.map.get(hash).cloned()
    }

    fn contains(&self, hash: &Hash) -> bool {
        self.map.contains_key(hash)
    }

    fn remove(&mut self, hash: &Hash) {
        if let Some(v) = self.map.remove(hash) {
            self.bytes -= v.len();
        }
    }

    /// Insert unless already present, then evict down to the budget.
    fn store(&mut self, hash: Hash, data: Vec<u8>) {
        // Never let a single chunk larger than the whole budget evict everything
        // else only to not fit anyway — just skip caching it.
        if data.len() > CACHE_BUDGET_BYTES || self.map.contains_key(&hash) {
            return;
        }
        self.bytes += data.len();
        self.map.insert(hash, data);
        while self.bytes > CACHE_BUDGET_BYTES {
            let Some(victim) = self.map.keys().next().copied() else {
                break;
            };
            if let Some(v) = self.map.remove(&victim) {
                self.bytes -= v.len();
            }
        }
    }
}

pub struct FsStore {
    root: PathBuf,
    cache: Mutex<Cache>,
}

impl FsStore {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            cache: Mutex::new(Cache::default()),
        })
    }

    fn chunk_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    fn temp_path(&self, hash: &Hash) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A per-process sequence disambiguates two threads that write the same
        // content within the same sub-second nanosecond, which would otherwise
        // collide on the same temp name and make one `put` fail on create_new.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let hex = hash.to_hex();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(
            ".tmp-{}-{}-{}-{}",
            std::process::id(),
            nanos,
            seq,
            hex
        ))
    }
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> io::Result<()> {
    // Windows: directory entries are durable on rename completion.
    Ok(())
}

/// fsync a directory for durability, surfacing real errors but tolerating
/// `EINVAL` — some filesystems (and Windows) don't support directory fsync, and
/// there a rename is durable without it, so a non-durable-fsync error there must
/// not fail an otherwise-successful `put`.
fn durable_dir_sync(path: &Path) -> io::Result<()> {
    match fsync_dir(path) {
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

impl ChunkStore for FsStore {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        let hash = Hash::of(data);
        let path = self.chunk_path(&hash);

        if path.exists() {
            self.cache.lock().unwrap().store(hash, data.to_vec());
            return Ok(hash);
        }

        let parent = path.parent().expect("chunk path has parent");
        // If this write creates the shard directory, the shard's own entry in
        // the store root must be made durable below too — else a crash could
        // drop the whole shard (and every chunk renamed into it).
        let shard_is_new = !parent.exists();
        fs::create_dir_all(parent)?;

        let tmp = self.temp_path(&hash);
        {
            let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
            f.write_all(data)?;
            // sync data so a crash after rename can't leave a zero-byte chunk.
            f.sync_data()?;
        }

        if let Err(rename_err) = fs::rename(&tmp, &path) {
            // Another writer may have created the chunk between our exists-check
            // and here — content-addressed, so it's the same bytes; that's fine.
            // Any other failure is a real error: do NOT fall back to a non-atomic
            // copy to the final content address (a crash mid-copy would leave a
            // torn object at a canonical hash). `temp_path` is always under
            // `self.root`, the same filesystem, so EXDEV cannot occur here (#60).
            let _ = fs::remove_file(&tmp);
            if !path.exists() {
                return Err(rename_err);
            }
        }

        // Durability: the chunk's dirent (and, for a first-time shard, the shard
        // directory's own entry in the store root) must reach disk, or a crash
        // could leave a committed root referencing a chunk that is gone. Surface
        // real I/O errors instead of reporting a non-durable write as success.
        durable_dir_sync(parent)?;
        if shard_is_new {
            durable_dir_sync(&self.root)?;
        }

        self.cache.lock().unwrap().store(hash, data.to_vec());
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        if let Some(data) = self.cache.lock().unwrap().get(hash) {
            return Ok(Some(data));
        }

        let path = self.chunk_path(hash);
        match fs::read(&path) {
            Ok(data) => {
                if Hash::of(&data) == *hash {
                    self.cache.lock().unwrap().store(*hash, data.clone());
                    Ok(Some(data))
                } else {
                    // The stored bytes don't hash to their address — corruption
                    // (bit rot, a torn write, a poisoned store). Remove the bad
                    // file so a later `put` of the true content can heal the
                    // store, and report missing rather than serve wrong bytes.
                    // Best-effort: a writer racing us just rewrites it (#60).
                    let _ = fs::remove_file(&path);
                    Ok(None)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn has(&self, hash: &Hash) -> io::Result<bool> {
        if self.cache.lock().unwrap().contains(hash) {
            return Ok(true);
        }
        Ok(self.chunk_path(hash).exists())
    }

    fn find_by_prefix(&self, prefix: &str) -> io::Result<Vec<Hash>> {
        let prefix = prefix.to_ascii_lowercase();
        // A hash prefix is hex; anything else matches no object. This also
        // guarantees the prefix is ASCII, so the `[..2]`/`[2..]` byte-slicing
        // below can never split a multi-byte char and panic (#60).
        if !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        // Two cases: prefix is shorter than the shard prefix (2 chars), or longer.
        let scan_dir =
            |dir_name: &str, file_filter: Option<&str>, out: &mut Vec<Hash>| -> io::Result<()> {
                let dir = self.root.join(dir_name);
                if !dir.exists() {
                    return Ok(());
                }
                for entry in fs::read_dir(&dir)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let name = match entry.file_name().into_string() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if let Some(filter) = file_filter {
                        if !name.starts_with(filter) {
                            continue;
                        }
                    }
                    let full_hex = format!("{}{}", dir_name, name);
                    if let Some(h) = Hash::from_hex(&full_hex) {
                        out.push(h);
                    }
                }
                Ok(())
            };

        if prefix.len() >= 2 {
            let shard = &prefix[..2];
            let rest = &prefix[2..];
            scan_dir(
                shard,
                if rest.is_empty() { None } else { Some(rest) },
                &mut out,
            )?;
        } else {
            // Walk every shard directory.
            for entry in fs::read_dir(&self.root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = match entry.file_name().into_string() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if name.len() != 2 || !name.starts_with(&prefix) {
                    continue;
                }
                scan_dir(&name, None, &mut out)?;
            }
        }
        Ok(out)
    }

    fn delete(&self, hash: &Hash) -> io::Result<bool> {
        let path = self.chunk_path(hash);
        match fs::remove_file(&path) {
            Ok(()) => {
                self.cache.lock().unwrap().remove(hash);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.cache.lock().unwrap().remove(hash);
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, FsStore) {
        let d = TempDir::new().unwrap();
        let s = FsStore::new(d.path().to_path_buf()).unwrap();
        (d, s)
    }

    #[test]
    fn put_get_roundtrip() {
        let (_d, s) = store();
        let h = s.put(b"hello").unwrap();
        assert_eq!(s.get(&h).unwrap().as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn skip_if_exists() {
        let (_d, s) = store();
        let h1 = s.put(b"dup").unwrap();
        let h2 = s.put(b"dup").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn missing() {
        let (_d, s) = store();
        assert!(s.get(&Hash::of(b"nope")).unwrap().is_none());
    }

    #[test]
    fn corrupt_chunk_is_removed_on_read_and_re_put_heals() {
        let (d, s) = store();
        let h = s.put(b"original").unwrap();
        let p = s.chunk_path(&h);
        // Bypass cache so the on-disk hash check fires; corrupt the file.
        s.cache.lock().unwrap().remove(&h);
        fs::write(&p, b"tampered").unwrap();
        // get() refuses the corrupted bytes AND removes the bad file, so the
        // store can self-heal rather than shadow the true content forever (#60).
        assert!(s.get(&h).unwrap().is_none());
        assert!(!p.exists(), "corrupt chunk must be removed on read");
        // Re-putting the true content now succeeds (previously a silent no-op:
        // the corrupt file's mere existence shadowed the correct object).
        assert_eq!(s.put(b"original").unwrap(), h);
        assert_eq!(s.get(&h).unwrap().as_deref(), Some(&b"original"[..]));
        drop(d);
    }

    #[test]
    fn find_by_prefix_locates_existing_hash() {
        let (_d, s) = store();
        let h = s.put(b"target").unwrap();
        let _ = s.put(b"distractor").unwrap();
        let prefix = &h.to_hex()[..8];
        let matches = s.find_by_prefix(prefix).unwrap();
        assert!(
            matches.contains(&h),
            "prefix {} should find {}",
            prefix,
            h.to_hex()
        );
    }

    #[test]
    fn find_by_prefix_short_walks_all_shards() {
        let (_d, s) = store();
        let h = s.put(b"x").unwrap();
        let one_char = &h.to_hex()[..1];
        let matches = s.find_by_prefix(one_char).unwrap();
        assert!(matches.contains(&h));
    }

    #[test]
    fn find_by_prefix_no_match() {
        let (_d, s) = store();
        let _ = s.put(b"foo").unwrap();
        // 'zz' is unlikely to match any blake3 hex prefix; if collision, the test
        // becomes flaky — extremely improbable.
        assert!(s.find_by_prefix("zzzzzzzz").unwrap().is_empty());
    }

    #[test]
    fn find_by_prefix_rejects_non_hex_without_panicking() {
        let (_d, s) = store();
        let _ = s.put(b"x").unwrap();
        // A multi-byte prefix used to panic on `&prefix[..2]` (non-char-boundary
        // byte slice); a non-hex prefix now matches nothing (#60). '€' is 3 bytes.
        assert!(s.find_by_prefix("\u{20ac}a").unwrap().is_empty());
        assert!(s.find_by_prefix("nothex!!").unwrap().is_empty());
    }

    #[test]
    fn cache_is_bounded_and_still_serves_after_eviction() {
        let (_d, s) = store();
        // Write more than the budget so eviction must occur. 1 MiB chunks.
        let chunk_mb = 1usize;
        let count = (CACHE_BUDGET_BYTES / (chunk_mb * 1024 * 1024)) + 8;
        let mut hashes = Vec::new();
        for i in 0..count {
            let mut data = vec![0u8; chunk_mb * 1024 * 1024];
            data[..8].copy_from_slice(&(i as u64).to_le_bytes()); // make each distinct
            hashes.push(s.put(&data).unwrap());
        }
        // Cache stayed within budget despite writing well past it.
        assert!(
            s.cache.lock().unwrap().bytes <= CACHE_BUDGET_BYTES,
            "cache exceeded budget"
        );
        // Every chunk is still retrievable — evicted ones are re-read from disk.
        for (i, h) in hashes.iter().enumerate() {
            let got = s.get(h).unwrap().expect("chunk must still be readable");
            assert_eq!(&got[..8], &(i as u64).to_le_bytes());
        }
    }

    #[test]
    fn oversized_chunk_is_not_cached() {
        let (_d, s) = store();
        // A chunk larger than the whole budget shouldn't be cached (or evict all).
        let big = vec![7u8; CACHE_BUDGET_BYTES + 1024];
        let h = s.put(&big).unwrap();
        assert!(!s.cache.lock().unwrap().contains(&h));
        // Still durably stored and readable from disk.
        assert_eq!(s.get(&h).unwrap().map(|v| v.len()), Some(big.len()));
    }
}
