use super::ChunkStore;
use crate::hash::Hash;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct FsStore {
    root: PathBuf,
    cache: Mutex<HashMap<Hash, Vec<u8>>>,
}

impl FsStore {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn chunk_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    fn temp_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        self.root
            .join(format!(".tmp-{}-{}-{}", std::process::id(), nanos, hex))
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

impl ChunkStore for FsStore {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        let hash = Hash::of(data);
        let path = self.chunk_path(&hash);

        if path.exists() {
            self.cache
                .lock()
                .unwrap()
                .entry(hash)
                .or_insert_with(|| data.to_vec());
            return Ok(hash);
        }

        let parent = path.parent().expect("chunk path has parent");
        fs::create_dir_all(parent)?;

        let tmp = self.temp_path(&hash);
        {
            let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
            f.write_all(data)?;
            // sync data so a crash after rename can't leave a zero-byte chunk.
            f.sync_data()?;
        }

        if let Err(rename_err) = fs::rename(&tmp, &path) {
            if path.exists() {
                let _ = fs::remove_file(&tmp);
            } else {
                // Possibly EXDEV (temp on a different filesystem); fall back to copy.
                match fs::copy(&tmp, &path) {
                    Ok(_) => {
                        let _ = fs::remove_file(&tmp);
                    }
                    Err(_) => {
                        let _ = fs::remove_file(&tmp);
                        return Err(rename_err);
                    }
                }
            }
        }

        let _ = fsync_dir(parent);

        self.cache.lock().unwrap().insert(hash, data.to_vec());
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        if let Some(data) = self.cache.lock().unwrap().get(hash) {
            return Ok(Some(data.clone()));
        }

        let path = self.chunk_path(hash);
        match fs::read(&path) {
            Ok(data) => {
                if Hash::of(&data) == *hash {
                    self.cache.lock().unwrap().insert(*hash, data.clone());
                    Ok(Some(data))
                } else {
                    // Bit rot: treat as missing rather than serve corrupted bytes.
                    Ok(None)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn has(&self, hash: &Hash) -> io::Result<bool> {
        if self.cache.lock().unwrap().contains_key(hash) {
            return Ok(true);
        }
        Ok(self.chunk_path(hash).exists())
    }

    fn find_by_prefix(&self, prefix: &str) -> io::Result<Vec<Hash>> {
        let prefix = prefix.to_ascii_lowercase();
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
    fn detects_bit_rot() {
        let (d, s) = store();
        let h = s.put(b"original").unwrap();
        let p = s.chunk_path(&h);
        // Bypass cache so the bit-rot check fires.
        s.cache.lock().unwrap().remove(&h);
        fs::write(&p, b"tampered").unwrap();
        assert!(s.get(&h).unwrap().is_none());
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
}
