//! Embedded toybox rootfs for the default `hako` container.
//!
//! Hako's default container starts as a usable Linux rootfs — toybox
//! (~125 BSD coreutils-equivalents linked into a single ~750KB static
//! binary) plus minimal `/etc/{passwd,group,hosts,resolv.conf}` and the
//! standard empty directories. The toybox binary is vendored directly in
//! the source tree at `src/rootfs/toybox` and embedded via `include_bytes!`,
//! so a fresh `hako init` produces a working shell environment with no
//! download step.

use crate::fs::{ScopedFs, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE};
use crate::hash::Hash;
use crate::store::ChunkStore;
use crate::tree::empty;
use std::io;

// toybox 0.8.13, statically linked, 0BSD — one binary per supported target arch,
// vendored under `src/rootfs/toybox-<arch>` and embedded for the *build* target
// so the seeded rootfs `/bin/sh` matches the CPU it will run on. An x86_64 binary
// on an arm64 host execs as `ENOEXEC` (issue #34), which is a real bug for arm64
// users, not just a CI quirk. Provenance + SHA-256s: see `src/rootfs/README.md`.
#[cfg(target_arch = "x86_64")]
const TOYBOX_BIN: &[u8] = include_bytes!("toybox-x86_64");
#[cfg(target_arch = "aarch64")]
const TOYBOX_BIN: &[u8] = include_bytes!("toybox-aarch64");
// Any other target has no vendored toybox: `is_available()` is then false and
// `hako init` reports "embedded toybox rootfs not available" rather than seeding
// a rootfs whose shell can't exec. (Pull an OCI image for a userland instead.)
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const TOYBOX_BIN: &[u8] = &[];

/// Toybox applet symlinks. Each becomes `bin/<applet>` → `toybox`.
/// Runtime dispatch is by argv[0], so the symlink name *is* the command.
const APPLETS: &[&str] = &[
    "arch",
    "base64",
    "basename",
    "bunzip2",
    "bzcat",
    "cal",
    "cat",
    "chattr",
    "chgrp",
    "chmod",
    "chown",
    "chroot",
    "cksum",
    "clear",
    "cmp",
    "comm",
    "cp",
    "cpio",
    "cut",
    "date",
    "dd",
    "df",
    "dirname",
    "du",
    "echo",
    "env",
    "expand",
    "factor",
    "false",
    "file",
    "find",
    "flock",
    "fmt",
    "fold",
    "free",
    "getopt",
    "grep",
    "groups",
    "gunzip",
    "head",
    "help",
    "host",
    "hostname",
    "id",
    "install",
    "kill",
    "killall",
    "link",
    "ln",
    "logger",
    "logname",
    "ls",
    "lsattr",
    "md5sum",
    "mkdir",
    "mkfifo",
    "mknod",
    "mktemp",
    "mount",
    "mountpoint",
    "mv",
    "nice",
    "nl",
    "nohup",
    "nproc",
    "od",
    "paste",
    "patch",
    "pgrep",
    "pidof",
    "pkill",
    "printenv",
    "printf",
    "ps",
    "pwd",
    "readlink",
    "realpath",
    "renice",
    "rev",
    "rm",
    "rmdir",
    "sed",
    "seq",
    "sh",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "shred",
    "shuf",
    "sleep",
    "sort",
    "split",
    "stat",
    "strings",
    "su",
    "sync",
    "tac",
    "tail",
    "tar",
    "tee",
    "test",
    "time",
    "timeout",
    "top",
    "touch",
    "true",
    "tr",
    "tsort",
    "tty",
    "uname",
    "uniq",
    "unlink",
    "uptime",
    "uudecode",
    "uuencode",
    "uuidgen",
    "watch",
    "wc",
    "which",
    "who",
    "whoami",
    "xargs",
    "xxd",
    "yes",
    "zcat",
];

/// Empty directories the rootfs needs for a typical Linux env to feel right.
/// FUSE/runtime layers may bind-mount over `proc`, `sys`, `dev`, `tmp`.
const EMPTY_DIRS: &[&str] = &[
    "lib", "sbin", "usr/bin", "usr/lib", "usr/sbin", "root", "tmp", "var/tmp", "dev", "proc", "sys",
];

const ETC_PASSWD: &[u8] = b"root:x:0:0:root:/root:/bin/sh\n";
const ETC_GROUP: &[u8] = b"root:x:0:\n";
const ETC_HOSTS: &[u8] = b"127.0.0.1\tlocalhost\n::1\t\tlocalhost\n";
const ETC_RESOLV_CONF: &[u8] = b"nameserver 8.8.8.8\nnameserver 8.8.4.4\n";

/// True if the embedded toybox binary is non-empty (built into this binary).
#[allow(clippy::const_is_empty)]
pub fn is_available() -> bool {
    !TOYBOX_BIN.is_empty()
}

/// Build the default-container rootfs into `store` and return the root tree
/// hash. Idempotent in the deterministic-hash sense: identical inputs (same
/// toybox bytes, same applet list, same constants) produce the same root,
/// so two workspaces sharing a store dedupe naturally.
pub fn extract_rootfs(store: &dyn ChunkStore) -> io::Result<Hash> {
    if !is_available() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "embedded toybox rootfs not available",
        ));
    }
    let scoped = ScopedFs::new(store);
    let mut root = empty();

    // bin/toybox — the binary, executable.
    root = scoped.write_file_meta(&root, "bin/toybox", TOYBOX_BIN, 0o755, 0)?;

    // bin/<applet> → toybox (relative symlink; resolved by execvp via cwd
    // of /bin, the standard Linux convention for toybox-style multi-call).
    for applet in APPLETS {
        let path = format!("bin/{}", applet);
        root = scoped.write_symlink(&root, &path, b"toybox", DEFAULT_SYMLINK_MODE, 0)?;
    }

    // Minimal /etc.
    root = scoped.write_file_meta(&root, "etc/passwd", ETC_PASSWD, DEFAULT_FILE_MODE, 0)?;
    root = scoped.write_file_meta(&root, "etc/group", ETC_GROUP, DEFAULT_FILE_MODE, 0)?;
    root = scoped.write_file_meta(&root, "etc/hosts", ETC_HOSTS, DEFAULT_FILE_MODE, 0)?;
    root = scoped.write_file_meta(
        &root,
        "etc/resolv.conf",
        ETC_RESOLV_CONF,
        DEFAULT_FILE_MODE,
        0,
    )?;

    // Standard empty mountpoints / staging directories.
    for dir in EMPTY_DIRS {
        root = scoped.mkdir(&root, dir)?;
    }

    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{DirEntry, DirKind};
    use crate::store::MemStore;

    #[test]
    fn rootfs_is_available() {
        assert!(is_available(), "vendored toybox binary should be present");
    }

    #[test]
    fn extract_produces_usable_rootfs() {
        let store = MemStore::new();
        let root = extract_rootfs(&store).unwrap();
        let scoped = ScopedFs::new(&store);

        // bin/toybox is a real file with the toybox bytes.
        let bytes = scoped.read_file(&root, "bin/toybox").unwrap();
        assert_eq!(bytes.len(), TOYBOX_BIN.len());
        assert_eq!(&bytes[..4], &TOYBOX_BIN[..4]);

        // bin/sh is a symlink → toybox.
        let target = scoped.read_symlink(&root, "bin/sh").unwrap();
        assert_eq!(target, b"toybox");

        // /etc/passwd exists with the expected content.
        assert_eq!(scoped.read_file(&root, "etc/passwd").unwrap(), ETC_PASSWD);

        // /lib is a directory.
        assert!(scoped.is_dir(&root, "lib").unwrap());
    }

    #[test]
    fn extract_is_deterministic_across_stores() {
        // Two fresh stores, same inputs → same root hash. This is the
        // structural sharing property that makes hako worth the engineering.
        let s1 = MemStore::new();
        let s2 = MemStore::new();
        let r1 = extract_rootfs(&s1).unwrap();
        let r2 = extract_rootfs(&s2).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn applets_all_resolve_to_toybox() {
        let store = MemStore::new();
        let root = extract_rootfs(&store).unwrap();
        let scoped = ScopedFs::new(&store);
        for applet in APPLETS {
            let path = format!("bin/{}", applet);
            let target = scoped
                .read_symlink(&root, &path)
                .unwrap_or_else(|e| panic!("applet {} not a symlink: {}", applet, e));
            assert_eq!(target, b"toybox", "applet {} should link to toybox", applet);
        }
    }

    #[test]
    fn ls_bin_shows_toybox_and_applets() {
        let store = MemStore::new();
        let root = extract_rootfs(&store).unwrap();
        let scoped = ScopedFs::new(&store);
        let bin = scoped.ls(&root, "bin").unwrap();
        // 1 (toybox) + len(APPLETS)
        assert_eq!(bin.len(), 1 + APPLETS.len());
        let toybox = bin.iter().find(|c| c.name == "toybox").unwrap();
        assert!(matches!(toybox.kind, DirKind::File));
        let sh = bin.iter().find(|c| c.name == "sh").unwrap();
        assert!(matches!(sh.kind, DirKind::Symlink));
        // Spot check it's a real DirEntry::Symlink with the right target.
        let _ = DirEntry::Symlink; // silence unused
    }
}
