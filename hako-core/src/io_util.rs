//! Small filesystem utilities shared across the crate.
//!
//! `atomic_write` does write-temp + fsync + rename + parent-fsync so a
//! crash mid-write never leaves a partially-written file at the target
//! path. Used by ref writes, the session file, and anywhere else two
//! processes might race.

use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically write `data` to `path`. Internally:
///   1. write to a unique temp file in the same directory,
///   2. fsync the temp's data,
///   3. rename the temp into place (atomic on Unix; on Windows, ReplaceFile-
///      backed by std::fs::rename is best-effort),
///   4. fsync the parent directory so the rename is durable.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    use std::sync::atomic::{AtomicU64, Ordering};
    // A per-process sequence disambiguates two threads writing the same target
    // within the same sub-second nanosecond, which would otherwise pick the same
    // temp name and let one clobber the other's in-flight write.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
        std::process::id(),
        nanos,
        seq
    ));

    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    let _ = fsync_dir(parent);
    Ok(())
}

/// Wall-clock seconds since the Unix epoch, or 0 if the system clock is
/// before the epoch (which would be very surprising). Used by anywhere
/// inside the lib that needs a timestamp without taking a dep on chrono.
pub fn now_secs_or_zero() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> io::Result<()> {
    // Windows: directory entries are durable on rename completion.
    Ok(())
}

/// Exclusive workspace lock guarding RMW sequences against concurrent CLI
/// processes. Acquired via `flock(LOCK_EX)` on Unix and `LockFileEx` on
/// Windows; released automatically when dropped (lock survives cross-thread
/// moves of the held `File`). The lock file (`.hako/lock`) is created if
/// missing and never deleted — its mere existence is harmless.
pub struct WorkspaceLock {
    // Hold the File alive; dropping it releases the lock.
    _file: File,
}

impl WorkspaceLock {
    /// Acquire the workspace lock at `dot_hako/lock`, blocking until granted.
    /// `dot_hako` is the path to the workspace's `.hako/` directory.
    pub fn acquire(dot_hako: &Path) -> io::Result<Self> {
        fs::create_dir_all(dot_hako)?;
        let path = dot_hako.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&path)?;
        file.lock_exclusive()?;
        Ok(WorkspaceLock { _file: file })
    }

    /// Try to acquire without blocking. Returns `None` if another process holds it.
    pub fn try_acquire(dot_hako: &Path) -> io::Result<Option<Self>> {
        fs::create_dir_all(dot_hako)?;
        let path = dot_hako.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(WorkspaceLock { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_creates_file() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("a.txt");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn write_overwrites() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("a.txt");
        atomic_write(&p, b"first").unwrap();
        atomic_write(&p, b"second").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn write_creates_parents() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("nested/deeper/file.txt");
        atomic_write(&p, b"x").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"x");
    }

    #[test]
    fn no_temp_files_left_behind() {
        let d = TempDir::new().unwrap();
        atomic_write(&d.path().join("a.txt"), b"x").unwrap();
        // Only the target file should remain — no .tmp leftover.
        let names: Vec<String> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
    }

    #[test]
    fn lock_basic_acquire_release() {
        let d = TempDir::new().unwrap();
        let lock = WorkspaceLock::acquire(d.path()).unwrap();
        drop(lock);
        // Re-acquiring after drop should succeed.
        let _lock2 = WorkspaceLock::acquire(d.path()).unwrap();
    }

    #[test]
    fn lock_try_acquire_unblocked() {
        let d = TempDir::new().unwrap();
        // No one's holding the lock — try_acquire returns Some.
        let opt = WorkspaceLock::try_acquire(d.path()).unwrap();
        assert!(opt.is_some());
    }
}
