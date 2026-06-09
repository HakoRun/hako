use super::ChunkStore;
use crate::hash::Hash;
use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

#[derive(Default)]
pub struct MemStore {
    data: Mutex<HashMap<Hash, Vec<u8>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ChunkStore for MemStore {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        let h = Hash::of(data);
        self.data
            .lock()
            .unwrap()
            .entry(h)
            .or_insert_with(|| data.to_vec());
        Ok(h)
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        Ok(self.data.lock().unwrap().get(hash).cloned())
    }

    fn has(&self, hash: &Hash) -> io::Result<bool> {
        Ok(self.data.lock().unwrap().contains_key(hash))
    }

    fn find_by_prefix(&self, prefix: &str) -> io::Result<Vec<Hash>> {
        let prefix = prefix.to_ascii_lowercase();
        let mut out = Vec::new();
        for h in self.data.lock().unwrap().keys() {
            if h.to_hex().starts_with(&prefix) {
                out.push(*h);
            }
        }
        Ok(out)
    }

    fn delete(&self, hash: &Hash) -> io::Result<bool> {
        Ok(self.data.lock().unwrap().remove(hash).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let s = MemStore::new();
        let h = s.put(b"hello").unwrap();
        assert_eq!(s.get(&h).unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(s.has(&h).unwrap());
    }

    #[test]
    fn idempotent_put() {
        let s = MemStore::new();
        let h1 = s.put(b"x").unwrap();
        let h2 = s.put(b"x").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn missing_returns_none() {
        let s = MemStore::new();
        assert!(s.get(&Hash::of(b"nope")).unwrap().is_none());
        assert!(!s.has(&Hash::of(b"nope")).unwrap());
    }

    #[test]
    fn find_by_prefix_works() {
        let s = MemStore::new();
        let h1 = s.put(b"alpha").unwrap();
        let _ = s.put(b"beta").unwrap();
        let prefix = &h1.to_hex()[..6];
        let matches = s.find_by_prefix(prefix).unwrap();
        assert!(matches.contains(&h1));
        assert_eq!(matches.len(), 1, "prefix of h1 should match exactly h1 (collision unlikely)");
    }

    #[test]
    fn find_by_prefix_empty_matches_all() {
        let s = MemStore::new();
        let h1 = s.put(b"a").unwrap();
        let h2 = s.put(b"b").unwrap();
        let all = s.find_by_prefix("").unwrap();
        assert!(all.contains(&h1));
        assert!(all.contains(&h2));
    }
}
