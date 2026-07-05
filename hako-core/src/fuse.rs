//! FUSE mount of a hako tree. Linux only.
//!
//! Two modes:
//!   - **Read-only** (`mount`, `mount_session`): the tree is fixed at mount
//!     time. Used for browsing snapshots.
//!   - **Read-write** (`mount_session_rw`): mutations through the mount
//!     update the chunk store and bump an in-memory tree root the caller
//!     can read back when the session ends. Used by the runtime so that
//!     `apt install python3` inside a container actually changes the
//!     hako-managed tree.
//!
//! The inode table maps FUSE inodes ↔ vfs paths and lives only for the
//! lifetime of the mount. New files allocate fresh inodes; renamed files
//! preserve their inode but update the recorded path. Concurrent ops
//! serialize on the tree-root mutex — fine for the workloads hako runs
//! through FUSE (sequential setup steps, single-process containers).

use crate::fs::{DirEntry, DirKind, ScopedFs};
use crate::hash::Hash;
use crate::io_util::now_secs_or_zero;
use crate::store::ChunkStore;
use crate::tree;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyWrite, Request, TimeOrNow,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
const ENOENT: i32 = libc::ENOENT;
const EIO: i32 = libc::EIO;
const EEXIST: i32 = libc::EEXIST;
const EACCES: i32 = libc::EACCES;
const EINVAL: i32 = libc::EINVAL;
const ENOTEMPTY: i32 = libc::ENOTEMPTY;
const EISDIR: i32 = libc::EISDIR;
const ENOSYS: i32 = libc::ENOSYS;
const EFBIG: i32 = libc::EFBIG;

/// Upper bound on a single file's size in the FUSE layer. A write past this
/// offset, or a truncate to a larger size, is rejected with `EFBIG` rather than
/// attempting the allocation: the write path is read-modify-write of the whole
/// file, so an unbounded container-supplied size is an OOM / DoS vector.
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

/// Mount `root` read-only at `mountpoint`. Blocks until the mount is
/// unmounted (Ctrl+C → SIGTERM, `umount`, `fusermount -u`).
pub fn mount(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    mountpoint: &Path,
) -> io::Result<()> {
    let fs = HakoFs::new(store, Arc::new(Mutex::new(root)), /* writable */ false);
    let opts = ro_opts();
    fuser::mount2(fs, mountpoint, &opts).map_err(io::Error::other)
}

/// Mount `root` read-only at `mountpoint`, returning a background session
/// that unmounts when dropped. Used by the runtime for browsing snapshots.
pub fn mount_session(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    mountpoint: &Path,
) -> io::Result<fuser::BackgroundSession> {
    let fs = HakoFs::new(store, Arc::new(Mutex::new(root)), /* writable */ false);
    // The runtime mounts this INSIDE a user namespace. Two options that are fine
    // for `hako mount` (foreground, no userns) break it here:
    //   * AllowOther — invalid in a non-initial userns (serves an empty tree).
    //   * AutoUnmount — forces libfuse to mount via the `fusermount3` helper
    //     child process; that child's mount does not propagate back to the FUSE
    //     server when `/`'s propagation isn't shared (the WSL2 default), so the
    //     server sees an empty mount. Without it, fuser mounts via `mount(2)`
    //     directly in-process (we hold CAP_SYS_ADMIN in the userns), which is
    //     immediately visible. The mount is unmounted on session drop and the
    //     old root is detached by pivot_root, so AutoUnmount isn't needed.
    let opts = vec![MountOption::RO, MountOption::FSName("hako".into())];
    let session = fuser::Session::new(fs, mountpoint, &opts).map_err(io::Error::other)?;
    session.spawn().map_err(io::Error::other)
}

/// Mount `root` read-write at `mountpoint`. Returns a handle pairing the
/// background FUSE session with an `Arc<Mutex<Hash>>` the caller reads at
/// session end to discover the new tree root after mutations.
///
/// Used by the runtime for `setup` execution: mount, run setup commands,
/// drop the handle to unmount, then commit the resulting tree.
pub fn mount_session_rw(
    store: Arc<dyn ChunkStore + Send + Sync + 'static>,
    root: Hash,
    mountpoint: &Path,
) -> io::Result<RwSession> {
    let root_mu = Arc::new(Mutex::new(root));
    let fs = HakoFs::new(store, Arc::clone(&root_mu), /* writable */ true);
    let opts = rw_opts();
    let session = fuser::Session::new(fs, mountpoint, &opts).map_err(io::Error::other)?;
    let bg = session.spawn().map_err(io::Error::other)?;
    Ok(RwSession {
        _bg: bg,
        root: root_mu,
    })
}

/// Handle to a live RW FUSE mount. Drop unmounts. `current_root()` returns
/// the latest tree hash reflecting all mutations through the mount so far.
pub struct RwSession {
    _bg: fuser::BackgroundSession,
    root: Arc<Mutex<Hash>>,
}

impl RwSession {
    /// The latest tree root hash. Safe to call at any time; commonly read
    /// after the mount user (e.g., `apt install` running in a container)
    /// has finished its work but before this `RwSession` is dropped.
    pub fn current_root(&self) -> Hash {
        *self.root.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn ro_opts() -> Vec<MountOption> {
    vec![
        MountOption::RO,
        MountOption::FSName("hako".into()),
        MountOption::AutoUnmount,
    ]
}

fn rw_opts() -> Vec<MountOption> {
    // No AllowOther and no AutoUnmount: this is mounted inside a user namespace
    // by the runtime. allow_other serves an empty mount in a non-init userns,
    // and AutoUnmount forces the fusermount3 helper (which on shared-propagation
    // systems doesn't propagate the mount back, and prints a spurious
    // "not mounted" on teardown). fuser mounts via mount(2) in-process instead;
    // cleanup is via session drop + mount-namespace teardown. See mount_session.
    vec![MountOption::RW, MountOption::FSName("hako".into())]
}

struct HakoFs {
    store: Arc<dyn ChunkStore + Send + Sync>,
    /// Current tree root. RO mounts share an Arc with no other writers;
    /// RW mounts share with the caller's `RwSession::current_root()`.
    root: Arc<Mutex<Hash>>,
    inodes: Mutex<InodeTable>,
    writable: bool,
}

impl HakoFs {
    fn new(
        store: Arc<dyn ChunkStore + Send + Sync>,
        root: Arc<Mutex<Hash>>,
        writable: bool,
    ) -> Self {
        Self {
            store,
            root,
            inodes: Mutex::new(InodeTable::new()),
            writable,
        }
    }

    fn scoped(&self) -> ScopedFs<'_> {
        ScopedFs::new(&*self.store)
    }

    fn current_root(&self) -> Hash {
        *self.root.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lookup_entry(&self, path: &str) -> io::Result<Option<DirEntry>> {
        if path.is_empty() {
            return Ok(Some(DirEntry::Directory));
        }
        let (parent, name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let scoped = self.scoped();
        let root = self.current_root();
        let children = match scoped.ls(&root, parent) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        for c in children {
            if c.name == name {
                return Ok(Some(child_to_entry(&c)));
            }
        }
        Ok(None)
    }

    /// Apply a tree mutation under the root lock. The closure receives the
    /// current root and a `ScopedFs`; returns the new root. Held lock
    /// serializes concurrent FUSE workers — fine for our workloads.
    fn mutate<F>(&self, f: F) -> io::Result<Hash>
    where
        F: FnOnce(&Hash, &ScopedFs<'_>) -> io::Result<Hash>,
    {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only mount",
            ));
        }
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        let scoped = ScopedFs::new(&*self.store);
        let new_root = f(&root, &scoped)?;
        *root = new_root;
        Ok(new_root)
    }
}

fn child_to_entry(c: &crate::fs::DirChild) -> DirEntry {
    match c.kind {
        DirKind::Directory => DirEntry::Directory,
        DirKind::File => DirEntry::File(crate::fs::FileEntry {
            size: c.size.unwrap_or(0),
            mode: c.mode.unwrap_or(0o644),
            mtime: c.mtime.unwrap_or(0),
            content: tree::Value::Inline(Vec::new()),
        }),
        DirKind::Symlink => DirEntry::Symlink(crate::fs::SymlinkEntry {
            mode: c.mode.unwrap_or(0o777),
            mtime: c.mtime.unwrap_or(0),
            target: c.symlink_target.clone().unwrap_or_default(),
        }),
    }
}

fn entry_to_attr(
    ino: u64,
    entry: &DirEntry,
    path: &str,
    scoped: &ScopedFs<'_>,
    root: &Hash,
) -> io::Result<FileAttr> {
    let (kind, size, mode, mtime) = match entry {
        DirEntry::Directory => {
            let n = scoped.ls(root, path).map(|v| v.len()).unwrap_or(0) as u64;
            (FileType::Directory, n, 0o755, 0)
        }
        DirEntry::File(f) => (FileType::RegularFile, f.size, f.mode, f.mtime),
        DirEntry::Symlink(s) => (FileType::Symlink, s.target.len() as u64, s.mode, s.mtime),
    };
    let t = UNIX_EPOCH + Duration::from_secs(mtime);
    Ok(FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: t,
        mtime: t,
        ctime: t,
        crtime: t,
        kind,
        perm: (mode & 0o7777) as u16,
        nlink: if matches!(kind, FileType::Directory) {
            2
        } else {
            1
        },
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    })
}

fn join_child(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent, name)
    }
}

/// Map `io::Error::Kind` to a POSIX errno for FUSE replies.
fn errno_for(e: &io::Error) -> i32 {
    use io::ErrorKind::*;
    match e.kind() {
        NotFound => ENOENT,
        AlreadyExists => EEXIST,
        PermissionDenied => EACCES,
        InvalidInput | InvalidData => EINVAL,
        Unsupported => ENOSYS,
        _ => EIO,
    }
}

impl Filesystem for HakoFs {
    // ========================================================================
    // Read path (works for both RO and RW)
    // ========================================================================

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(parent)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let child_path = join_child(&parent_path, name_str);
        let entry = match self.lookup_entry(&child_path) {
            Ok(Some(e)) => e,
            Ok(None) => {
                reply.error(ENOENT);
                return;
            }
            Err(_) => {
                reply.error(EIO);
                return;
            }
        };
        let ino = self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .intern(&child_path);
        let root = self.current_root();
        match entry_to_attr(ino, &entry, &child_path, &self.scoped(), &root) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(_) => reply.error(EIO),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(ino)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let entry = match self.lookup_entry(&path) {
            Ok(Some(e)) => e,
            Ok(None) => {
                reply.error(ENOENT);
                return;
            }
            Err(_) => {
                reply.error(EIO);
                return;
            }
        };
        let root = self.current_root();
        match entry_to_attr(ino, &entry, &path, &self.scoped(), &root) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(_) => reply.error(EIO),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(ino)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let scoped = self.scoped();
        let root = self.current_root();
        let children = match scoped.ls(&root, &path) {
            Ok(c) => c,
            Err(_) => {
                reply.error(ENOENT);
                return;
            }
        };

        let mut entries: Vec<(u64, FileType, String)> = Vec::with_capacity(children.len() + 2);
        entries.push((ino, FileType::Directory, ".".into()));
        let parent_ino = if path.is_empty() {
            ino
        } else {
            let parent = match path.rfind('/') {
                Some(i) => &path[..i],
                None => "",
            };
            self.inodes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .intern(parent)
        };
        entries.push((parent_ino, FileType::Directory, "..".into()));

        for c in children {
            let child_path = join_child(&path, &c.name);
            let child_ino = self
                .inodes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .intern(&child_path);
            let kind = match c.kind {
                DirKind::Directory => FileType::Directory,
                DirKind::File => FileType::RegularFile,
                DirKind::Symlink => FileType::Symlink,
            };
            entries.push((child_ino, kind, c.name));
        }

        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child_ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(ino)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let scoped = self.scoped();
        let root = self.current_root();
        // Serve only the requested window: the kernel reads a large file in many
        // ~128 KiB calls, and loading the whole file each time is O(n^2) (#74). A
        // negative offset is nonsensical for a plain file — treat it as EOF.
        if offset < 0 {
            reply.data(&[]);
            return;
        }
        match scoped.read_file_range(&root, &path, offset as u64, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(_) => reply.error(ENOENT),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(ino)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let scoped = self.scoped();
        let root = self.current_root();
        match scoped.read_symlink(&root, &path) {
            Ok(target) => reply.data(&target),
            Err(_) => reply.error(ENOENT),
        }
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }

    // ========================================================================
    // Write path (RW mounts only — fails EACCES on RO)
    // ========================================================================

    /// `write(2)` — splice `data` into the file at `offset`. Naive impl:
    /// read the whole file, splice in the new bytes, write back. Adequate
    /// for setup workloads; a buffer cache would be the natural next step
    /// if performance matters.
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(ino)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        if offset < 0 {
            reply.error(EINVAL);
            return;
        }
        if (offset as u64).saturating_add(data.len() as u64) > MAX_FILE_SIZE {
            reply.error(EFBIG);
            return;
        }
        let written = data.len() as u32;
        let result = self.mutate(|root, scoped| {
            // Read existing content (or empty if doesn't exist).
            let mut bytes = scoped.read_file(root, &path).unwrap_or_default();
            let off = offset as usize;
            if bytes.len() < off {
                bytes.resize(off, 0); // pad with zeros (POSIX sparse-file fill)
            }
            // Splice data in at offset.
            let end = off + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[off..end].copy_from_slice(data);
            // Preserve mode/mtime where possible; default to 0o644 + now.
            let (mode, _) = read_meta(scoped, root, &path).unwrap_or((0o644, 0));
            scoped.write_file_meta(root, &path, &bytes, mode, now_secs_or_zero())
        });
        match result {
            Ok(_) => reply.written(written),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    /// `creat(2)` / `open(O_CREAT)` — make a new empty file under `parent`.
    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let parent_path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(parent)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let child_path = join_child(&parent_path, name_str);
        let result = self.mutate(|root, scoped| {
            scoped.write_file_meta(root, &child_path, b"", mode, now_secs_or_zero())
        });
        match result {
            Ok(_) => {
                let ino = self
                    .inodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .intern(&child_path);
                let root = self.current_root();
                let entry = DirEntry::File(crate::fs::FileEntry {
                    size: 0,
                    mode,
                    mtime: now_secs_or_zero(),
                    content: tree::Value::Inline(Vec::new()),
                });
                match entry_to_attr(ino, &entry, &child_path, &self.scoped(), &root) {
                    Ok(attr) => reply.created(&TTL, &attr, 0, 0, 0),
                    Err(e) => reply.error(errno_for(&e)),
                }
            }
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let parent_path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(parent)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let child_path = join_child(&parent_path, name_str);
        let result = self.mutate(|root, scoped| scoped.mkdir(root, &child_path));
        match result {
            Ok(_) => {
                let ino = self
                    .inodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .intern(&child_path);
                let root = self.current_root();
                let entry = DirEntry::Directory;
                match entry_to_attr(ino, &entry, &child_path, &self.scoped(), &root) {
                    Ok(attr) => reply.entry(&TTL, &attr, 0),
                    Err(e) => reply.error(errno_for(&e)),
                }
            }
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let parent_path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(parent)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let child_path = join_child(&parent_path, name_str);
        let result = self.mutate(|root, scoped| {
            // Refuse to unlink directories — that's rmdir's job.
            if scoped.is_dir(root, &child_path)? {
                return Err(io::Error::from_raw_os_error(EISDIR));
            }
            scoped.delete(root, &child_path)
        });
        match result {
            Ok(_) => {
                self.inodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .forget_path(&child_path);
                reply.ok();
            }
            Err(e) => reply.error(e.raw_os_error().unwrap_or_else(|| errno_for(&e))),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let parent_path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(parent)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let child_path = join_child(&parent_path, name_str);
        let result = self.mutate(|root, scoped| {
            // POSIX rmdir refuses non-empty dirs. Hako's `delete` is recursive,
            // so we check emptiness first.
            let entries = scoped.ls(root, &child_path)?;
            if !entries.is_empty() {
                return Err(io::Error::from_raw_os_error(ENOTEMPTY));
            }
            scoped.delete(root, &child_path)
        });
        match result {
            Ok(_) => {
                self.inodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .forget_path(&child_path);
                reply.ok();
            }
            Err(e) => reply.error(e.raw_os_error().unwrap_or_else(|| errno_for(&e))),
        }
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let parent_path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(parent)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match link_name.to_str() {
            Some(s) => s,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let target_str = match target.to_str() {
            Some(s) => s,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let child_path = join_child(&parent_path, name_str);
        let result = self.mutate(|root, scoped| {
            scoped.write_symlink(
                root,
                &child_path,
                target_str.as_bytes(),
                0o777,
                now_secs_or_zero(),
            )
        });
        match result {
            Ok(_) => {
                let ino = self
                    .inodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .intern(&child_path);
                let root = self.current_root();
                let entry = DirEntry::Symlink(crate::fs::SymlinkEntry {
                    mode: 0o777,
                    mtime: now_secs_or_zero(),
                    target: target_str.as_bytes().to_vec(),
                });
                match entry_to_attr(ino, &entry, &child_path, &self.scoped(), &root) {
                    Ok(attr) => reply.entry(&TTL, &attr, 0),
                    Err(e) => reply.error(errno_for(&e)),
                }
            }
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if !self.writable {
            reply.error(EACCES);
            return;
        }
        let (old_path, new_path) = {
            let inodes = self.inodes.lock().unwrap_or_else(|e| e.into_inner());
            let p = match inodes.path_of(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let np = match inodes.path_of(newparent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let n = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(EINVAL);
                    return;
                }
            };
            let nn = match newname.to_str() {
                Some(s) => s,
                None => {
                    reply.error(EINVAL);
                    return;
                }
            };
            (join_child(&p, n), join_child(&np, nn))
        };
        let result = self.mutate(|root, scoped| {
            // POSIX rename clobbers the destination; ScopedFs::cp errors if
            // the destination has a file ancestor, which is what we want.
            // Use mv (cp + delete src).
            let r = scoped.cp_to(root, root, &old_path, &new_path)?;
            scoped.delete(&r, &old_path)
        });
        match result {
            Ok(_) => {
                self.inodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rename_subtree(&old_path, &new_path);
                reply.ok();
            }
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    /// `setattr(2)` — chmod, chown, truncate, utimes. We honor mode and
    /// truncate-to-size; ignore chown (single-user model); update mtime.
    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if !self.writable && (mode.is_some() || size.is_some() || mtime.is_some()) {
            reply.error(EACCES);
            return;
        }
        let path = match self
            .inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path_of(ino)
        {
            Some(p) => p.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        // No-op setattr (stat-only access) returns current attrs.
        if mode.is_none() && size.is_none() && mtime.is_none() {
            let entry = match self.lookup_entry(&path) {
                Ok(Some(e)) => e,
                _ => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let root = self.current_root();
            match entry_to_attr(ino, &entry, &path, &self.scoped(), &root) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(_) => reply.error(EIO),
            }
            return;
        }
        // Reject a truncate to an absurd size before allocating for it.
        if let Some(new_size) = size {
            if new_size > MAX_FILE_SIZE {
                reply.error(EFBIG);
                return;
            }
        }
        let result = self.mutate(|root, scoped| {
            // Only files support all of chmod/truncate/utimes; for dirs/symlinks
            // we accept the call as a best-effort no-op (toybox/install touch dirs).
            if scoped.is_file(root, &path)? {
                let mut bytes = scoped.read_file(root, &path).unwrap_or_default();
                if let Some(new_size) = size {
                    bytes.resize(new_size as usize, 0);
                }
                let (current_mode, current_mtime) =
                    read_meta(scoped, root, &path).unwrap_or((0o644, 0));
                let new_mode = mode.unwrap_or(current_mode);
                let new_mtime = match mtime {
                    Some(TimeOrNow::SpecificTime(t)) => t
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(current_mtime),
                    Some(TimeOrNow::Now) => now_secs_or_zero(),
                    None => current_mtime,
                };
                scoped.write_file_meta(root, &path, &bytes, new_mode, new_mtime)
            } else {
                // No tree change for dir/symlink chmod (we'd need to round-trip
                // through delete+recreate; defer until needed).
                Ok(*root)
            }
        });
        match result {
            Ok(_) => {
                let entry = match self.lookup_entry(&path) {
                    Ok(Some(e)) => e,
                    _ => {
                        reply.error(ENOENT);
                        return;
                    }
                };
                let root = self.current_root();
                match entry_to_attr(ino, &entry, &path, &self.scoped(), &root) {
                    Ok(attr) => reply.attr(&TTL, &attr),
                    Err(_) => reply.error(EIO),
                }
            }
            Err(e) => reply.error(errno_for(&e)),
        }
    }
}

/// Read `(mode, mtime)` for `path` from the tree, by `ls`-ing the parent
/// dir and matching by name. Returns `None` if not present or for entries
/// with no metadata recorded.
fn read_meta(scoped: &ScopedFs<'_>, root: &Hash, path: &str) -> Option<(u32, u64)> {
    let (parent, name) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let entries = scoped.ls(root, parent).ok()?;
    let child = entries.into_iter().find(|c| c.name == name)?;
    Some((child.mode?, child.mtime.unwrap_or(0)))
}

/// Maps between FUSE inode numbers and vfs paths. Root is always inode 1.
struct InodeTable {
    fwd: HashMap<u64, String>,
    rev: HashMap<String, u64>,
    next: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut t = Self {
            fwd: HashMap::new(),
            rev: HashMap::new(),
            next: 2,
        };
        t.fwd.insert(1, String::new());
        t.rev.insert(String::new(), 1);
        t
    }

    fn path_of(&self, ino: u64) -> Option<&str> {
        self.fwd.get(&ino).map(|s| s.as_str())
    }

    fn intern(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.rev.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.fwd.insert(ino, path.to_string());
        self.rev.insert(path.to_string(), ino);
        ino
    }

    /// Drop the inode for `path` (and any descendant paths), so a future
    /// `intern` allocates a fresh inode. Used after unlink/rmdir so a new
    /// file at the same path doesn't reuse a stale inode mapping.
    fn forget_path(&mut self, path: &str) {
        let prefix = format!("{}/", path);
        let to_remove: Vec<String> = self
            .rev
            .keys()
            .filter(|k| k.as_str() == path || k.starts_with(&prefix))
            .cloned()
            .collect();
        for p in to_remove {
            if let Some(ino) = self.rev.remove(&p) {
                self.fwd.remove(&ino);
            }
        }
    }

    /// Move every inode whose path starts with `old` to a corresponding
    /// path under `new`. Preserves inode numbers across the rename so any
    /// open fds keep working.
    fn rename_subtree(&mut self, old: &str, new: &str) {
        let old_prefix = format!("{}/", old);
        let entries: Vec<(String, u64)> = self
            .rev
            .iter()
            .filter(|(k, _)| k.as_str() == old || k.starts_with(&old_prefix))
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        for (old_path, ino) in entries {
            let new_path = if old_path == old {
                new.to_string()
            } else {
                format!("{}{}", new, &old_path[old.len()..])
            };
            self.rev.remove(&old_path);
            self.rev.insert(new_path.clone(), ino);
            self.fwd.insert(ino, new_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_inode_one() {
        let t = InodeTable::new();
        assert_eq!(t.path_of(1), Some(""));
    }

    #[test]
    fn intern_stable_for_same_path() {
        let mut t = InodeTable::new();
        let a = t.intern("bin/sh");
        let b = t.intern("bin/sh");
        assert_eq!(a, b);
        assert!(a >= 2);
    }

    #[test]
    fn distinct_paths_get_distinct_inodes() {
        let mut t = InodeTable::new();
        let a = t.intern("a");
        let b = t.intern("b");
        assert_ne!(a, b);
    }

    #[test]
    fn path_lookup_round_trips() {
        let mut t = InodeTable::new();
        let ino = t.intern("etc/hostname");
        assert_eq!(t.path_of(ino), Some("etc/hostname"));
    }

    #[test]
    fn forget_drops_path_and_descendants() {
        let mut t = InodeTable::new();
        let a = t.intern("a");
        let ab = t.intern("a/b");
        let abc = t.intern("a/b/c");
        let other = t.intern("other");
        t.forget_path("a");
        assert!(t.path_of(a).is_none());
        assert!(t.path_of(ab).is_none());
        assert!(t.path_of(abc).is_none());
        assert!(t.path_of(other).is_some());
    }

    #[test]
    fn rename_subtree_preserves_inodes() {
        let mut t = InodeTable::new();
        let a = t.intern("src");
        let ab = t.intern("src/file.txt");
        t.rename_subtree("src", "dst");
        // inodes preserved; paths moved.
        assert_eq!(t.path_of(a), Some("dst"));
        assert_eq!(t.path_of(ab), Some("dst/file.txt"));
        // old paths gone.
        assert!(!t.rev.contains_key("src"));
        assert!(!t.rev.contains_key("src/file.txt"));
    }
}
