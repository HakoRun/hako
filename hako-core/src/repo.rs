use crate::fs::{decode_entry, DirEntry};
use crate::hash::Hash;
use crate::io_util::atomic_write;
use crate::store::ChunkStore;
use crate::tree::node::load_node;
use crate::tree::{NodeKind, Value};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const HEAD_FILE: &str = "HEAD";
const WORKING_FILE: &str = "WORKING";
const REFS_HEADS: &str = "refs/heads";
const REFS_TAGS: &str = "refs/tags";
const DEFAULT_BRANCH: &str = "main";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub parents: Vec<Hash>,
    pub tree: Hash,
    pub author: String,
    pub message: String,
    pub timestamp: u64,
}

pub struct Repo<'s> {
    root: PathBuf,
    store: &'s dyn ChunkStore,
}

impl<'s> Repo<'s> {
    pub fn init(root: &Path, store: &'s dyn ChunkStore) -> io::Result<Self> {
        fs::create_dir_all(root.join(REFS_HEADS))?;
        let head = root.join(HEAD_FILE);
        if !head.exists() {
            atomic_write(&head, format!("ref: {}\n", DEFAULT_BRANCH).as_bytes())?;
        }
        Ok(Self {
            root: root.to_path_buf(),
            store,
        })
    }

    pub fn open(root: &Path, store: &'s dyn ChunkStore) -> io::Result<Self> {
        if !root.join(HEAD_FILE).exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no HEAD: not a hako repo",
            ));
        }
        Ok(Self {
            root: root.to_path_buf(),
            store,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &dyn ChunkStore {
        self.store
    }

    pub fn current_branch(&self) -> io::Result<Option<String>> {
        let head = fs::read_to_string(self.root.join(HEAD_FILE))?;
        let head = head.trim();
        Ok(head.strip_prefix("ref: ").map(|s| s.to_string()))
    }

    pub fn set_branch(&self, name: &str) -> io::Result<()> {
        validate_branch_name(name)?;
        atomic_write(
            &self.root.join(HEAD_FILE),
            format!("ref: {}\n", name).as_bytes(),
        )
    }

    pub fn read_ref(&self, branch: &str) -> io::Result<Option<Hash>> {
        validate_branch_name(branch)?;
        match fs::read_to_string(self.ref_path(branch)) {
            Ok(s) => Hash::from_hex(s.trim())
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid ref hex")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn write_ref(&self, branch: &str, commit: Hash) -> io::Result<()> {
        validate_branch_name(branch)?;
        let p = self.ref_path(branch);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&p, format!("{}\n", commit.to_hex()).as_bytes())
    }

    /// Returns `Ok(true)` if the ref existed and was removed, `Ok(false)` if
    /// it didn't exist. Letting callers distinguish these is what makes
    /// `branch -d nonexistent` a meaningful error in the CLI.
    pub fn delete_ref(&self, branch: &str) -> io::Result<bool> {
        validate_branch_name(branch)?;
        match fs::remove_file(self.ref_path(branch)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn list_branches(&self) -> io::Result<Vec<String>> {
        let dir = self.root.join(REFS_HEADS);
        let mut names = Vec::new();
        if !dir.exists() {
            return Ok(names);
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(s) = entry.file_name().to_str() {
                    names.push(s.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn head_commit(&self) -> io::Result<Option<Hash>> {
        match self.current_branch()? {
            Some(b) => self.read_ref(&b),
            None => {
                let head = fs::read_to_string(self.root.join(HEAD_FILE))?;
                Hash::from_hex(head.trim()).map(Some).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid HEAD hash")
                })
            }
        }
    }

    pub fn head_tree(&self) -> io::Result<Hash> {
        match self.head_commit()? {
            Some(c) => Ok(self.load_commit(&c)?.tree),
            None => Ok(Hash::zero()),
        }
    }

    pub fn working_tree(&self) -> io::Result<Hash> {
        match fs::read_to_string(self.root.join(WORKING_FILE)) {
            Ok(s) => Hash::from_hex(s.trim()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid WORKING hex")
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.head_tree(),
            Err(e) => Err(e),
        }
    }

    pub fn set_working(&self, tree: Hash) -> io::Result<()> {
        atomic_write(
            &self.root.join(WORKING_FILE),
            format!("{}\n", tree.to_hex()).as_bytes(),
        )
    }

    pub fn clear_working(&self) -> io::Result<()> {
        match fs::remove_file(self.root.join(WORKING_FILE)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn commit(
        &self,
        tree: Hash,
        parents: Vec<Hash>,
        author: &str,
        message: &str,
        timestamp: u64,
    ) -> io::Result<Hash> {
        let c = Commit {
            parents,
            tree,
            author: author.to_string(),
            message: message.to_string(),
            timestamp,
        };
        let bytes = encode_commit(&c);
        self.store.put(&bytes)
    }

    pub fn load_commit(&self, hash: &Hash) -> io::Result<Commit> {
        let bytes = self
            .store
            .get(hash)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing commit"))?;
        decode_commit(&bytes)
    }

    pub fn log(&self, from: Hash) -> io::Result<Vec<(Hash, Commit)>> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        let mut q = VecDeque::new();
        q.push_back(from);
        while let Some(h) = q.pop_front() {
            if h == Hash::zero() || !visited.insert(h) {
                continue;
            }
            let c = self.load_commit(&h)?;
            for p in &c.parents {
                q.push_back(*p);
            }
            out.push((h, c));
        }
        out.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(out)
    }

    pub fn common_ancestor(&self, a: Hash, b: Hash) -> io::Result<Option<Hash>> {
        if a == Hash::zero() || b == Hash::zero() {
            return Ok(None);
        }
        let mut a_anc = HashSet::new();
        let mut q = VecDeque::new();
        q.push_back(a);
        while let Some(h) = q.pop_front() {
            if h == Hash::zero() || !a_anc.insert(h) {
                continue;
            }
            let c = self.load_commit(&h)?;
            for p in &c.parents {
                q.push_back(*p);
            }
        }
        let mut visited = HashSet::new();
        let mut q = VecDeque::new();
        q.push_back(b);
        while let Some(h) = q.pop_front() {
            if h == Hash::zero() || !visited.insert(h) {
                continue;
            }
            if a_anc.contains(&h) {
                return Ok(Some(h));
            }
            let c = self.load_commit(&h)?;
            for p in &c.parents {
                q.push_back(*p);
            }
        }
        Ok(None)
    }

    fn ref_path(&self, branch: &str) -> PathBuf {
        self.root.join(REFS_HEADS).join(branch)
    }

    fn tag_path(&self, name: &str) -> PathBuf {
        self.root.join(REFS_TAGS).join(name)
    }

    /// Create or move a tag pointing at `commit`. Tags live in a parallel
    /// namespace from branches (refs/tags/ vs refs/heads/) — they share the
    /// same name validation rules and atomic-write guarantees.
    pub fn write_tag(&self, name: &str, commit: Hash) -> io::Result<()> {
        validate_branch_name(name)?;
        let p = self.tag_path(name);
        atomic_write(&p, format!("{}\n", commit.to_hex()).as_bytes())
    }

    /// Read a tag's commit, or None if missing.
    pub fn read_tag(&self, name: &str) -> io::Result<Option<Hash>> {
        validate_branch_name(name)?;
        match fs::read_to_string(self.tag_path(name)) {
            Ok(s) => Hash::from_hex(s.trim())
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid tag hex")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete a tag. Returns whether it existed.
    pub fn delete_tag(&self, name: &str) -> io::Result<bool> {
        validate_branch_name(name)?;
        match fs::remove_file(self.tag_path(name)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn list_tags(&self) -> io::Result<Vec<String>> {
        let dir = self.root.join(REFS_TAGS);
        let mut names = Vec::new();
        if !dir.exists() {
            return Ok(names);
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(s) = entry.file_name().to_str() {
                    names.push(s.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// All chunk-store object hashes transitively reachable from `from` (a commit
    /// hash). Includes: every commit in the parent chain, every tree node, every
    /// externally-stored leaf value (encoded DirEntry), and every external file
    /// content chunk. The returned set is exactly what must be present in a remote
    /// store to read the commit.
    pub fn reachable_objects(&self, from: Hash) -> io::Result<HashSet<Hash>> {
        let mut visited = HashSet::new();
        let mut commits: VecDeque<Hash> = VecDeque::new();
        commits.push_back(from);

        while let Some(ch) = commits.pop_front() {
            if ch == Hash::zero() || !visited.insert(ch) {
                continue;
            }
            let commit = self.load_commit(&ch)?;
            for p in &commit.parents {
                commits.push_back(*p);
            }
            walk_tree(self.store, commit.tree, &mut visited)?;
        }
        Ok(visited)
    }
}

/// Public-to-the-crate alias so the `maintenance` module can also walk the
/// working tree (which has no parent commit).
pub(crate) fn walk_tree_for_maintenance(
    store: &dyn ChunkStore,
    tree: Hash,
    visited: &mut HashSet<Hash>,
) -> io::Result<()> {
    walk_tree(store, tree, visited)
}

fn walk_tree(
    store: &dyn ChunkStore,
    tree: Hash,
    visited: &mut HashSet<Hash>,
) -> io::Result<()> {
    let mut stack: Vec<Hash> = vec![tree];
    while let Some(h) = stack.pop() {
        if h == Hash::zero() || !visited.insert(h) {
            continue;
        }
        let node = load_node(store, &h)?;
        match node.kind {
            NodeKind::Internal { child_hashes, .. } => {
                for ch in child_hashes {
                    stack.push(ch);
                }
            }
            NodeKind::Leaf { entries } => {
                for e in entries {
                    walk_value(store, e.value, visited)?;
                }
            }
        }
    }
    Ok(())
}

fn walk_value(
    store: &dyn ChunkStore,
    value: Value,
    visited: &mut HashSet<Hash>,
) -> io::Result<()> {
    let bytes = match value {
        Value::Inline(b) => b,
        Value::External(h) => {
            visited.insert(h);
            store
                .get(&h)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing leaf value"))?
        }
    };
    if let DirEntry::File(f) = decode_entry(&bytes)? {
        if let Value::External(h) = f.content {
            visited.insert(h);
        }
    }
    Ok(())
}

fn validate_branch_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\0')
        || name.contains("..")
        || name.starts_with('.')
        || name.starts_with('-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid branch name",
        ));
    }
    Ok(())
}

const COMMIT_VERSION: u8 = 1;

fn encode_commit(c: &Commit) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(COMMIT_VERSION);
    buf.extend_from_slice(&(c.parents.len() as u32).to_be_bytes());
    for p in &c.parents {
        buf.extend_from_slice(&p.0);
    }
    buf.extend_from_slice(&c.tree.0);
    let a = c.author.as_bytes();
    buf.extend_from_slice(&(a.len() as u16).to_be_bytes());
    buf.extend_from_slice(a);
    let m = c.message.as_bytes();
    buf.extend_from_slice(&(m.len() as u32).to_be_bytes());
    buf.extend_from_slice(m);
    buf.extend_from_slice(&c.timestamp.to_be_bytes());
    buf
}

fn decode_commit(data: &[u8]) -> io::Result<Commit> {
    if data.len() < 1 + 4 + 32 + 2 + 4 + 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated commit",
        ));
    }
    let mut p = 0;
    if data[p] != COMMIT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown commit version",
        ));
    }
    p += 1;
    let n_parents = u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as usize;
    p += 4;
    if data.len() < p + n_parents * 32 + 32 + 2 + 4 + 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated parents",
        ));
    }
    let mut parents = Vec::with_capacity(n_parents);
    for _ in 0..n_parents {
        let mut h = [0u8; 32];
        h.copy_from_slice(&data[p..p + 32]);
        parents.push(Hash(h));
        p += 32;
    }
    let mut tree_h = [0u8; 32];
    tree_h.copy_from_slice(&data[p..p + 32]);
    p += 32;
    let alen = u16::from_be_bytes(data[p..p + 2].try_into().unwrap()) as usize;
    p += 2;
    if data.len() < p + alen + 4 + 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated author"));
    }
    let author = std::str::from_utf8(&data[p..p + alen])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 author"))?
        .to_string();
    p += alen;
    let mlen = u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as usize;
    p += 4;
    if data.len() != p + mlen + 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wrong commit length",
        ));
    }
    let message = std::str::from_utf8(&data[p..p + mlen])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 message"))?
        .to_string();
    p += mlen;
    let timestamp = u64::from_be_bytes(data[p..p + 8].try_into().unwrap());
    Ok(Commit {
        parents,
        tree: Hash(tree_h),
        author,
        message,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;
    use tempfile::TempDir;

    struct Fixture {
        _d: TempDir,
        store: MemStore,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let d = TempDir::new().unwrap();
            let path = d.path().to_path_buf();
            Self {
                _d: d,
                store: MemStore::new(),
                path,
            }
        }
        fn repo(&self) -> Repo<'_> {
            Repo::init(&self.path, &self.store).unwrap()
        }
    }

    #[test]
    fn init_creates_layout() {
        let f = Fixture::new();
        let _r = f.repo();
        assert!(f.path.join(HEAD_FILE).exists());
        assert!(f.path.join(REFS_HEADS).is_dir());
    }

    #[test]
    fn default_branch_is_main() {
        let f = Fixture::new();
        let r = f.repo();
        assert_eq!(r.current_branch().unwrap().as_deref(), Some("main"));
        assert_eq!(r.head_commit().unwrap(), None);
        assert_eq!(r.head_tree().unwrap(), Hash::zero());
    }

    #[test]
    fn open_after_init() {
        let f = Fixture::new();
        let _ = Repo::init(&f.path, &f.store).unwrap();
        let r = Repo::open(&f.path, &f.store).unwrap();
        assert_eq!(r.current_branch().unwrap().as_deref(), Some("main"));
    }

    #[test]
    fn open_missing_repo_errors() {
        let f = Fixture::new();
        match Repo::open(&f.path, &f.store) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn ref_roundtrip() {
        let f = Fixture::new();
        let r = f.repo();
        let h = Hash::of(b"abc");
        r.write_ref("main", h).unwrap();
        assert_eq!(r.read_ref("main").unwrap(), Some(h));
    }

    #[test]
    fn list_branches_sorted() {
        let f = Fixture::new();
        let r = f.repo();
        let h = Hash::of(b"x");
        r.write_ref("zeta", h).unwrap();
        r.write_ref("alpha", h).unwrap();
        r.write_ref("beta", h).unwrap();
        assert_eq!(r.list_branches().unwrap(), vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn delete_ref_works() {
        let f = Fixture::new();
        let r = f.repo();
        let h = Hash::of(b"x");
        r.write_ref("dead", h).unwrap();
        assert!(r.delete_ref("dead").unwrap(), "delete returned true for existing ref");
        assert_eq!(r.read_ref("dead").unwrap(), None);
        assert!(!r.delete_ref("dead").unwrap(), "delete returned false for missing ref");
    }

    #[test]
    fn commit_roundtrip() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"tree");
        let h = r.commit(tree, vec![], "alice", "init", 100).unwrap();
        let c = r.load_commit(&h).unwrap();
        assert_eq!(c.tree, tree);
        assert_eq!(c.parents, Vec::<Hash>::new());
        assert_eq!(c.author, "alice");
        assert_eq!(c.message, "init");
        assert_eq!(c.timestamp, 100);
    }

    #[test]
    fn commit_is_deterministic() {
        let f1 = Fixture::new();
        let f2 = Fixture::new();
        let r1 = f1.repo();
        let r2 = f2.repo();
        let tree = Hash::of(b"tree");
        let h1 = r1.commit(tree, vec![], "alice", "msg", 100).unwrap();
        let h2 = r2.commit(tree, vec![], "alice", "msg", 100).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn commit_chain_logs_in_order() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"t");
        let c1 = r.commit(tree, vec![], "a", "first", 100).unwrap();
        let c2 = r.commit(tree, vec![c1], "a", "second", 200).unwrap();
        let c3 = r.commit(tree, vec![c2], "a", "third", 300).unwrap();
        let log = r.log(c3).unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].0, c3);
        assert_eq!(log[1].0, c2);
        assert_eq!(log[2].0, c1);
    }

    #[test]
    fn common_ancestor_in_chain() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"t");
        let base = r.commit(tree, vec![], "a", "base", 100).unwrap();
        let ours = r.commit(tree, vec![base], "a", "ours", 200).unwrap();
        let theirs = r.commit(tree, vec![base], "a", "theirs", 200).unwrap();
        assert_eq!(r.common_ancestor(ours, theirs).unwrap(), Some(base));
    }

    #[test]
    fn common_ancestor_self() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"t");
        let c = r.commit(tree, vec![], "a", "c", 100).unwrap();
        assert_eq!(r.common_ancestor(c, c).unwrap(), Some(c));
    }

    #[test]
    fn common_ancestor_unrelated_returns_none() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"t");
        let a = r.commit(tree, vec![], "x", "a", 100).unwrap();
        let b = r.commit(tree, vec![], "y", "b", 100).unwrap();
        assert_eq!(r.common_ancestor(a, b).unwrap(), None);
    }

    #[test]
    fn set_and_check_head_branch() {
        let f = Fixture::new();
        let r = f.repo();
        r.set_branch("dev").unwrap();
        assert_eq!(r.current_branch().unwrap().as_deref(), Some("dev"));
    }

    #[test]
    fn head_commit_via_branch() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"t");
        let c = r.commit(tree, vec![], "a", "msg", 100).unwrap();
        r.write_ref("main", c).unwrap();
        assert_eq!(r.head_commit().unwrap(), Some(c));
        assert_eq!(r.head_tree().unwrap(), tree);
    }

    #[test]
    fn working_tree_falls_back_to_head() {
        let f = Fixture::new();
        let r = f.repo();
        let tree = Hash::of(b"t");
        let c = r.commit(tree, vec![], "a", "msg", 100).unwrap();
        r.write_ref("main", c).unwrap();
        assert_eq!(r.working_tree().unwrap(), tree);
    }

    #[test]
    fn working_tree_overrides_head() {
        let f = Fixture::new();
        let r = f.repo();
        let head_tree = Hash::of(b"head");
        let work_tree = Hash::of(b"work");
        let c = r.commit(head_tree, vec![], "a", "msg", 100).unwrap();
        r.write_ref("main", c).unwrap();
        r.set_working(work_tree).unwrap();
        assert_eq!(r.working_tree().unwrap(), work_tree);
        r.clear_working().unwrap();
        assert_eq!(r.working_tree().unwrap(), head_tree);
    }

    #[test]
    fn reachable_objects_from_commit_chain() {
        use crate::fs::ScopedFs;
        use crate::tree::empty;

        let f = Fixture::new();
        let r = f.repo();
        let scoped = ScopedFs::new(&f.store);

        // Build a small commit chain with two files; the second is large enough
        // to live in an external chunk so we exercise the file-content edge.
        let small = b"small file";
        let big = vec![3u8; 4096];

        let t1 = scoped.write_file(&empty(), "small.txt", small).unwrap();
        let c1 = r.commit(t1, vec![], "a", "first", 100).unwrap();

        let t2 = scoped.write_file(&t1, "big.bin", &big).unwrap();
        let c2 = r.commit(t2, vec![c1], "a", "second", 200).unwrap();

        let reachable = r.reachable_objects(c2).unwrap();
        // Both commits should be in the set.
        assert!(reachable.contains(&c1));
        assert!(reachable.contains(&c2));
        // The big file's content chunk should be in the set.
        let big_hash = Hash::of(&big);
        assert!(
            reachable.contains(&big_hash),
            "external file content chunk should be reachable"
        );
        // Both tree roots should be in the set.
        assert!(reachable.contains(&t1));
        assert!(reachable.contains(&t2));
    }

    #[test]
    fn reachable_objects_from_zero_is_empty() {
        let f = Fixture::new();
        let r = f.repo();
        let reachable = r.reachable_objects(Hash::zero()).unwrap();
        assert!(reachable.is_empty());
    }

    #[test]
    fn invalid_branch_name_rejected() {
        let f = Fixture::new();
        let r = f.repo();
        let h = Hash::of(b"x");
        assert!(r.write_ref("", h).is_err());
        assert!(r.write_ref("a/b", h).is_err());
        assert!(r.write_ref("..", h).is_err());
        assert!(r.write_ref(".hidden", h).is_err());
        assert!(r.write_ref("-flag", h).is_err());
        assert!(r.write_ref("with\0null", h).is_err());
    }
}
