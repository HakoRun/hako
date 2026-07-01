//! `ScopedFs`: path-based file operations over a `ChunkStore`. Wraps the
//! underlying prolly tree with familiar read/write/ls/mkdir/cp/mv/delete
//! semantics. All operations return a new tree root — the existing root is
//! never mutated.

use super::entry::{
    decode_entry, encode_entry, normalize_path, DirChild, DirEntry, DirKind, FileEntry,
    SymlinkEntry, DEFAULT_FILE_MODE,
};
use crate::hash::Hash;
use crate::store::ChunkStore;
use crate::tree::{self, Value};
use std::collections::BTreeMap;
use std::io;

pub struct ScopedFs<'s> {
    store: &'s dyn ChunkStore,
}

impl<'s> ScopedFs<'s> {
    pub fn new(store: &'s dyn ChunkStore) -> Self {
        Self { store }
    }

    pub fn write_file(&self, root: &Hash, path: &str, content: &[u8]) -> io::Result<Hash> {
        self.write_file_meta(root, path, content, DEFAULT_FILE_MODE, 0)
    }

    pub fn write_file_meta(
        &self,
        root: &Hash,
        path: &str,
        content: &[u8],
        mode: u32,
        mtime: u64,
    ) -> io::Result<Hash> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot write to root",
            ));
        }
        self.reject_file_ancestor(root, &key)?;
        let value_content = tree::store_value(self.store, content)?;
        let entry = DirEntry::File(FileEntry {
            size: content.len() as u64,
            mode,
            mtime,
            content: value_content,
        });
        let value = Value::Inline(encode_entry(&entry));
        tree::put(self.store, root, key.into_bytes(), value)
    }

    pub fn write_symlink(
        &self,
        root: &Hash,
        path: &str,
        target: &[u8],
        mode: u32,
        mtime: u64,
    ) -> io::Result<Hash> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot symlink at root",
            ));
        }
        self.reject_file_ancestor(root, &key)?;
        let entry = DirEntry::Symlink(SymlinkEntry {
            mode,
            mtime,
            target: target.to_vec(),
        });
        let value = Value::Inline(encode_entry(&entry));
        tree::put(self.store, root, key.into_bytes(), value)
    }

    pub fn read_file(&self, root: &Hash, path: &str) -> io::Result<Vec<u8>> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root is not a file",
            ));
        }
        let v = tree::get(self.store, root, key.as_bytes())?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        let bytes = require_inline(v)?;
        match decode_entry(&bytes)? {
            DirEntry::File(f) => tree::load_value(self.store, &f.content),
            DirEntry::Directory => Err(io::Error::other("is a directory")),
            DirEntry::Symlink(_) => Err(io::Error::other("is a symlink")),
        }
    }

    pub fn read_symlink(&self, root: &Hash, path: &str) -> io::Result<Vec<u8>> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root is not a symlink",
            ));
        }
        let v = tree::get(self.store, root, key.as_bytes())?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such path"))?;
        let bytes = require_inline(v)?;
        match decode_entry(&bytes)? {
            DirEntry::Symlink(s) => Ok(s.target),
            _ => Err(io::Error::other("not a symlink")),
        }
    }

    pub fn mkdir(&self, root: &Hash, path: &str) -> io::Result<Hash> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Ok(*root);
        }
        if let Some(existing) = self.entry(root, &key)? {
            return match existing {
                DirEntry::Directory => Ok(*root),
                DirEntry::File(_) => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path exists as file",
                )),
                DirEntry::Symlink(_) => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path exists as symlink",
                )),
            };
        }
        self.reject_file_ancestor(root, &key)?;
        let value = Value::Inline(encode_entry(&DirEntry::Directory));
        tree::put(self.store, root, key.into_bytes(), value)
    }

    pub fn exists(&self, root: &Hash, path: &str) -> io::Result<bool> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Ok(true);
        }
        if tree::get(self.store, root, key.as_bytes())?.is_some() {
            return Ok(true);
        }
        let prefix = format!("{}/", key);
        Ok(!tree::scan_prefix(self.store, root, prefix.as_bytes())?.is_empty())
    }

    pub fn is_dir(&self, root: &Hash, path: &str) -> io::Result<bool> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Ok(true);
        }
        if let Some(de) = self.entry(root, &key)? {
            return Ok(matches!(de, DirEntry::Directory));
        }
        let prefix = format!("{}/", key);
        Ok(!tree::scan_prefix(self.store, root, prefix.as_bytes())?.is_empty())
    }

    pub fn is_file(&self, root: &Hash, path: &str) -> io::Result<bool> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Ok(false);
        }
        Ok(matches!(self.entry(root, &key)?, Some(DirEntry::File(_))))
    }

    pub fn is_symlink(&self, root: &Hash, path: &str) -> io::Result<bool> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Ok(false);
        }
        Ok(matches!(
            self.entry(root, &key)?,
            Some(DirEntry::Symlink(_))
        ))
    }

    pub fn ls(&self, root: &Hash, path: &str) -> io::Result<Vec<DirChild>> {
        let key = normalize_path(path)?;
        if !key.is_empty() && !self.is_dir(root, &key)? {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such directory"));
        }
        let prefix = if key.is_empty() {
            String::new()
        } else {
            format!("{}/", key)
        };
        let entries = tree::scan_prefix(self.store, root, prefix.as_bytes())?;
        type ChildInfo = (
            DirKind,
            Option<u64>,
            Option<u32>,
            Option<u64>,
            Option<Vec<u8>>,
        );
        let mut seen: BTreeMap<String, ChildInfo> = BTreeMap::new();
        for (k, v) in entries {
            let suffix = &k[prefix.len()..];
            if suffix.is_empty() {
                continue;
            }
            let slash_pos = suffix.iter().position(|&b| b == b'/');
            match slash_pos {
                None => {
                    let name = std::str::from_utf8(suffix)
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 path"))?
                        .to_string();
                    let bytes = require_inline(v)?;
                    let de = decode_entry(&bytes)?;
                    let info: ChildInfo = match de {
                        DirEntry::File(f) => (
                            DirKind::File,
                            Some(f.size),
                            Some(f.mode),
                            Some(f.mtime),
                            None,
                        ),
                        DirEntry::Directory => (DirKind::Directory, None, None, None, None),
                        DirEntry::Symlink(s) => (
                            DirKind::Symlink,
                            None,
                            Some(s.mode),
                            Some(s.mtime),
                            Some(s.target),
                        ),
                    };
                    seen.insert(name, info);
                }
                Some(p) => {
                    let name = std::str::from_utf8(&suffix[..p])
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 path"))?
                        .to_string();
                    seen.entry(name)
                        .or_insert((DirKind::Directory, None, None, None, None));
                }
            }
        }
        Ok(seen
            .into_iter()
            .map(
                |(name, (kind, size, mode, mtime, symlink_target))| DirChild {
                    name,
                    kind,
                    size,
                    mode,
                    mtime,
                    symlink_target,
                },
            )
            .collect())
    }

    pub fn delete(&self, root: &Hash, path: &str) -> io::Result<Hash> {
        let key = normalize_path(path)?;
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot delete root",
            ));
        }
        // Fast path: a file or symlink can have no children — the file-ancestor
        // invariant (`reject_file_ancestor`) forbids writing `key/...` beneath a
        // non-directory — so a single O(log n) tree delete removes it completely.
        // This avoids the O(n) scan-and-rebuild of the whole tree for the common
        // `rm <file>` case (which matters at pulled-image scale). The result is
        // identical: `tree::delete` yields the same canonical tree bulk_build
        // would from the remaining entries.
        if matches!(
            self.entry(root, &key)?,
            Some(DirEntry::File(_)) | Some(DirEntry::Symlink(_))
        ) {
            return tree::delete(self.store, root, key.as_bytes());
        }
        // Directory (explicit marker or implicit via descendant keys): drop the
        // entry and everything beneath it — this genuinely needs the full scan.
        let mut all = tree::scan(self.store, root)?;
        let prefix = format!("{}/", key);
        all.retain(|(k, _)| k != key.as_bytes() && !k.starts_with(prefix.as_bytes()));
        tree::bulk_build(self.store, all)
    }

    pub fn cp(&self, root: &Hash, src: &str, dst: &str) -> io::Result<Hash> {
        self.cp_to(root, root, src, dst)
    }

    /// Copy `src_path` from `src_root` into `dst_root` at `dst_path`. Returns the
    /// new dst root. Both trees must live in the same chunk store (this `ScopedFs`'s
    /// store). The same-tree case (`src_root == dst_root`) refuses to copy a directory
    /// into itself; cross-tree copies have no such restriction since the source tree
    /// is not modified.
    pub fn cp_to(
        &self,
        src_root: &Hash,
        dst_root: &Hash,
        src_path: &str,
        dst_path: &str,
    ) -> io::Result<Hash> {
        let src_key = normalize_path(src_path)?;
        let dst_key = normalize_path(dst_path)?;
        if src_key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot copy root",
            ));
        }
        if dst_key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot copy to root",
            ));
        }
        let src_pref = format!("{}/", src_key);
        let dst_pref = format!("{}/", dst_key);
        if src_root == dst_root && dst_key == src_key {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source and destination are the same",
            ));
        }
        if src_root == dst_root
            && (dst_pref.starts_with(&src_pref)
                || dst_key.as_bytes().starts_with(src_pref.as_bytes()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination is inside source",
            ));
        }
        self.reject_file_ancestor(dst_root, &dst_key)?;
        let src_all = tree::scan(self.store, src_root)?;
        let mut to_add: Vec<(Vec<u8>, Value)> = Vec::new();
        for (k, v) in &src_all {
            if k == src_key.as_bytes() {
                to_add.push((dst_key.clone().into_bytes(), v.clone()));
            } else if k.starts_with(src_pref.as_bytes()) {
                let suffix = &k[src_pref.len()..];
                let mut new_key = dst_pref.clone().into_bytes();
                new_key.extend_from_slice(suffix);
                to_add.push((new_key, v.clone()));
            }
        }
        if to_add.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such source"));
        }
        let mut dst_all = tree::scan(self.store, dst_root)?;
        // Drop anything that would be shadowed by the new entries. Without
        // this, `bulk_build` ends up with two entries at the same key and
        // lookup returns the older one — i.e. cp/mv silently fail to
        // overwrite, and mv loses the source's contents (data loss).
        dst_all.retain(|(k, _)| k != dst_key.as_bytes() && !k.starts_with(dst_pref.as_bytes()));
        dst_all.extend(to_add);
        tree::bulk_build(self.store, dst_all)
    }

    pub fn mv(&self, root: &Hash, src: &str, dst: &str) -> io::Result<Hash> {
        let new_root = self.cp(root, src, dst)?;
        self.delete(&new_root, src)
    }

    fn entry(&self, root: &Hash, key: &str) -> io::Result<Option<DirEntry>> {
        match tree::get(self.store, root, key.as_bytes())? {
            None => Ok(None),
            Some(v) => Ok(Some(decode_entry(&require_inline(v)?)?)),
        }
    }

    /// Walk every prefix of `key` (excluding `key` itself) and reject if any
    /// is a file or symlink. This stops the user from writing `/a/b` while
    /// `/a` is a file, which would otherwise leave `/a` and `/a/b` coexisting
    /// invisibly.
    fn reject_file_ancestor(&self, root: &Hash, key: &str) -> io::Result<()> {
        let mut start = 0;
        while let Some(off) = key[start..].find('/') {
            let end = start + off;
            let ancestor = &key[..end];
            if let Some(de) = self.entry(root, ancestor)? {
                match de {
                    DirEntry::File(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("ancestor /{} exists as file", ancestor),
                        ));
                    }
                    DirEntry::Symlink(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("ancestor /{} exists as symlink", ancestor),
                        ));
                    }
                    DirEntry::Directory => {} // explicit directory marker — fine
                }
            }
            // implicit directories (path used as prefix in keys) are also fine
            start = end + 1;
        }
        Ok(())
    }
}

fn require_inline(v: Value) -> io::Result<Vec<u8>> {
    match v {
        Value::Inline(b) => Ok(b),
        Value::External(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tree value not inline DirEntry",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;
    use crate::tree::empty;

    #[test]
    fn symlink_roundtrip_in_tree() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs
            .write_symlink(&empty(), "link", b"target", 0o777, 0)
            .unwrap();
        assert_eq!(fs.read_symlink(&root, "link").unwrap(), b"target");
        assert!(fs.is_symlink(&root, "link").unwrap());
        assert!(!fs.is_file(&root, "link").unwrap());
        assert!(!fs.is_dir(&root, "link").unwrap());
    }

    #[test]
    fn ls_returns_symlink_kind() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs
            .write_symlink(&empty(), "link", b"target", 0o777, 42)
            .unwrap();
        let top = fs.ls(&root, "").unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].kind, DirKind::Symlink);
        assert_eq!(top[0].symlink_target.as_deref(), Some(b"target".as_ref()));
        assert_eq!(top[0].mtime, Some(42));
    }

    #[test]
    fn file_mode_and_mtime_preserved() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs
            .write_file_meta(&empty(), "exec.sh", b"#!/bin/sh\n", 0o755, 12345)
            .unwrap();
        let top = fs.ls(&root, "").unwrap();
        assert_eq!(top[0].mode, Some(0o755));
        assert_eq!(top[0].mtime, Some(12345));
    }

    #[test]
    fn write_then_read() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b/c.txt", b"hello").unwrap();
        assert_eq!(fs.read_file(&root, "a/b/c.txt").unwrap(), b"hello");
    }

    #[test]
    fn write_large_file_external() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let big = vec![7u8; 5000];
        let root = fs.write_file(&empty(), "big", &big).unwrap();
        assert_eq!(fs.read_file(&root, "big").unwrap(), big);
    }

    #[test]
    fn read_missing_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let err = fs.read_file(&empty(), "nope").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn read_directory_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.mkdir(&empty(), "dir").unwrap();
        assert!(fs.read_file(&root, "dir").is_err());
    }

    #[test]
    fn ls_empty_root() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        assert!(fs.ls(&empty(), "").unwrap().is_empty());
    }

    #[test]
    fn ls_implicit_dirs_from_files() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b/c.txt", b"x").unwrap();
        let root = fs.write_file(&root, "a/d.txt", b"y").unwrap();
        let root = fs.write_file(&root, "z.txt", b"z").unwrap();
        let top = fs.ls(&root, "").unwrap();
        assert_eq!(top.len(), 2);
        let names: Vec<_> = top.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "z.txt"]);
        assert_eq!(top[0].kind, DirKind::Directory);
        assert_eq!(top[1].kind, DirKind::File);
        assert_eq!(top[1].size, Some(1));
    }

    #[test]
    fn ls_explicit_empty_directory() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.mkdir(&empty(), "empty").unwrap();
        let top = fs.ls(&root, "").unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].name, "empty");
        assert_eq!(top[0].kind, DirKind::Directory);
    }

    #[test]
    fn ls_subdirectory() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b/c.txt", b"x").unwrap();
        let root = fs.write_file(&root, "a/d.txt", b"y").unwrap();
        let dir = fs.ls(&root, "a").unwrap();
        let names: Vec<_> = dir.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["b", "d.txt"]);
    }

    #[test]
    fn ls_nonexistent_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        assert!(fs.ls(&empty(), "missing").is_err());
    }

    #[test]
    fn exists_and_is_dir_is_file() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b/c.txt", b"x").unwrap();
        assert!(fs.exists(&root, "a/b/c.txt").unwrap());
        assert!(fs.is_file(&root, "a/b/c.txt").unwrap());
        assert!(!fs.is_dir(&root, "a/b/c.txt").unwrap());
        assert!(fs.exists(&root, "a/b").unwrap());
        assert!(fs.is_dir(&root, "a/b").unwrap());
        assert!(!fs.is_file(&root, "a/b").unwrap());
        assert!(!fs.exists(&root, "missing").unwrap());
        assert!(fs.exists(&root, "").unwrap());
        assert!(fs.is_dir(&root, "").unwrap());
    }

    #[test]
    fn delete_file() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b.txt", b"x").unwrap();
        let root = fs.delete(&root, "a/b.txt").unwrap();
        assert!(!fs.exists(&root, "a/b.txt").unwrap());
        assert!(!fs.exists(&root, "a").unwrap());
    }

    #[test]
    fn delete_file_matches_rebuild_of_remaining() {
        // The single-file fast path (tree::delete) must produce the exact same
        // canonical tree as the scan + bulk_build path it replaces.
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let mut root = empty();
        for i in 0..200 {
            root = fs
                .write_file(&root, &format!("dir/f{:03}", i), format!("v{i}").as_bytes())
                .unwrap();
        }
        let after = fs.delete(&root, "dir/f100").unwrap();

        let mut all = crate::tree::scan(&s, &root).unwrap();
        all.retain(|(k, _)| k.as_slice() != b"dir/f100");
        let rebuilt = crate::tree::bulk_build(&s, all).unwrap();

        assert_eq!(after, rebuilt, "fast-path delete must match full rebuild");
        assert!(!fs.exists(&after, "dir/f100").unwrap());
        assert!(fs.exists(&after, "dir/f099").unwrap());
        assert!(fs.exists(&after, "dir/f101").unwrap());
    }

    #[test]
    fn delete_directory_recursive() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b/c.txt", b"x").unwrap();
        let root = fs.write_file(&root, "a/b/d.txt", b"y").unwrap();
        let root = fs.write_file(&root, "z.txt", b"z").unwrap();
        let root = fs.delete(&root, "a").unwrap();
        assert!(!fs.exists(&root, "a/b/c.txt").unwrap());
        assert!(!fs.exists(&root, "a/b/d.txt").unwrap());
        assert!(!fs.exists(&root, "a").unwrap());
        assert!(fs.exists(&root, "z.txt").unwrap());
    }

    #[test]
    fn cp_file() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a.txt", b"hello").unwrap();
        let root = fs.cp(&root, "a.txt", "b.txt").unwrap();
        assert_eq!(fs.read_file(&root, "a.txt").unwrap(), b"hello");
        assert_eq!(fs.read_file(&root, "b.txt").unwrap(), b"hello");
    }

    #[test]
    fn cp_directory_recursive() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src/a.txt", b"x").unwrap();
        let root = fs.write_file(&root, "src/b/c.txt", b"y").unwrap();
        let root = fs.cp(&root, "src", "dst").unwrap();
        assert_eq!(fs.read_file(&root, "dst/a.txt").unwrap(), b"x");
        assert_eq!(fs.read_file(&root, "dst/b/c.txt").unwrap(), b"y");
        assert_eq!(fs.read_file(&root, "src/a.txt").unwrap(), b"x");
    }

    #[test]
    fn cp_to_across_trees() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let src_root = fs.write_file(&empty(), "hello.txt", b"hi").unwrap();
        let dst_root = fs.write_file(&empty(), "existing.txt", b"keep").unwrap();
        let new_dst = fs
            .cp_to(&src_root, &dst_root, "hello.txt", "imported.txt")
            .unwrap();
        // Source unchanged.
        assert_eq!(fs.read_file(&src_root, "hello.txt").unwrap(), b"hi");
        // Dst has both files.
        assert_eq!(fs.read_file(&new_dst, "existing.txt").unwrap(), b"keep");
        assert_eq!(fs.read_file(&new_dst, "imported.txt").unwrap(), b"hi");
    }

    #[test]
    fn cp_to_directory_across_trees() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let src_root = fs.write_file(&empty(), "src/a.txt", b"x").unwrap();
        let src_root = fs.write_file(&src_root, "src/b/c.txt", b"y").unwrap();
        let dst_root = empty();
        let new_dst = fs.cp_to(&src_root, &dst_root, "src", "imported").unwrap();
        assert_eq!(fs.read_file(&new_dst, "imported/a.txt").unwrap(), b"x");
        assert_eq!(fs.read_file(&new_dst, "imported/b/c.txt").unwrap(), b"y");
        // Source still has its own copy.
        assert_eq!(fs.read_file(&src_root, "src/a.txt").unwrap(), b"x");
    }

    #[test]
    fn cp_to_same_tree_into_self_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src/a.txt", b"x").unwrap();
        // Same root acts like cp() — destination inside source is rejected.
        assert!(fs.cp_to(&root, &root, "src", "src/sub").is_err());
    }

    #[test]
    fn cp_to_cross_tree_into_namesake_works() {
        // Cross-tree copy doesn't have the "destination inside source" footgun
        // because the source tree isn't mutated.
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let src = fs.write_file(&empty(), "src/a.txt", b"x").unwrap();
        let dst = empty();
        let new_dst = fs.cp_to(&src, &dst, "src", "src").unwrap();
        assert_eq!(fs.read_file(&new_dst, "src/a.txt").unwrap(), b"x");
    }

    #[test]
    fn cp_into_self_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src/a.txt", b"x").unwrap();
        assert!(fs.cp(&root, "src", "src/sub").is_err());
    }

    #[test]
    fn mv_file() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a.txt", b"hello").unwrap();
        let root = fs.mv(&root, "a.txt", "b.txt").unwrap();
        assert!(!fs.exists(&root, "a.txt").unwrap());
        assert_eq!(fs.read_file(&root, "b.txt").unwrap(), b"hello");
    }

    #[test]
    fn mv_overwrites_existing_dst() {
        // Regression: cp_to used to extend dst_all without removing entries
        // at dst_key, leaving two entries at the same key in bulk_build.
        // For mv, lookup returned the OLD dst content and the source's
        // bytes were lost.
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src", b"SOURCE").unwrap();
        let root = fs.write_file(&root, "dst", b"DEST").unwrap();
        let root = fs.mv(&root, "src", "dst").unwrap();
        assert!(!fs.exists(&root, "src").unwrap());
        assert_eq!(fs.read_file(&root, "dst").unwrap(), b"SOURCE");
    }

    #[test]
    fn cp_overwrites_existing_dst() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src", b"SOURCE").unwrap();
        let root = fs.write_file(&root, "dst", b"DEST").unwrap();
        let root = fs.cp(&root, "src", "dst").unwrap();
        assert_eq!(fs.read_file(&root, "src").unwrap(), b"SOURCE");
        assert_eq!(fs.read_file(&root, "dst").unwrap(), b"SOURCE");
    }

    #[test]
    fn cp_directory_overwrites_existing_subtree() {
        // Replacing a populated subtree must drop the old children, not
        // merge them with the source's.
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src/a", b"new-a").unwrap();
        let root = fs.write_file(&root, "src/b", b"new-b").unwrap();
        let root = fs.write_file(&root, "dst/old-only", b"vestigial").unwrap();
        let root = fs.write_file(&root, "dst/a", b"old-a").unwrap();
        let root = fs.cp(&root, "src", "dst").unwrap();
        assert_eq!(fs.read_file(&root, "dst/a").unwrap(), b"new-a");
        assert_eq!(fs.read_file(&root, "dst/b").unwrap(), b"new-b");
        assert!(!fs.exists(&root, "dst/old-only").unwrap());
    }

    #[test]
    fn mkdir_idempotent() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let r1 = fs.mkdir(&empty(), "d").unwrap();
        let r2 = fs.mkdir(&r1, "d").unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn mkdir_over_file_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "f", b"x").unwrap();
        assert!(fs.mkdir(&root, "f").is_err());
    }

    #[test]
    fn write_under_file_ancestor_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a", b"i am a file").unwrap();
        let err = fs.write_file(&root, "a/b", b"shadowed").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn write_symlink_under_file_ancestor_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a", b"i am a file").unwrap();
        assert!(fs
            .write_symlink(&root, "a/link", b"target", 0o777, 0)
            .is_err());
    }

    #[test]
    fn write_under_symlink_ancestor_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs
            .write_symlink(&empty(), "a", b"target", 0o777, 0)
            .unwrap();
        assert!(fs.write_file(&root, "a/b", b"shadowed").is_err());
    }

    #[test]
    fn mkdir_under_file_ancestor_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a", b"i am a file").unwrap();
        assert!(fs.mkdir(&root, "a/b").is_err());
    }

    #[test]
    fn write_under_implicit_directory_works() {
        // `a/b` exists as a file → `a` is an implicit directory. Writing
        // `a/c` should succeed (no conflicting file ancestor on the path).
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "a/b", b"x").unwrap();
        let root = fs.write_file(&root, "a/c", b"y").unwrap();
        assert_eq!(fs.read_file(&root, "a/b").unwrap(), b"x");
        assert_eq!(fs.read_file(&root, "a/c").unwrap(), b"y");
    }

    #[test]
    fn cp_to_under_file_ancestor_errors() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = fs.write_file(&empty(), "src", b"x").unwrap();
        let root = fs.write_file(&root, "blocker", b"f").unwrap();
        // Try to copy /src to /blocker/inside — blocker is a file.
        assert!(fs.cp(&root, "src", "blocker/inside").is_err());
    }
}
