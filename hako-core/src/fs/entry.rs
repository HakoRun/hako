//! Directory-entry types and their on-disk encoding. A `DirEntry` is one of
//! file / directory / symlink and carries POSIX-ish metadata (mode + mtime).
//! Files store their content as a `tree::Value` — small files inline, large
//! files as an `External` hash into the chunk store.

use crate::hash::Hash;
use crate::tree::Value;
use std::io;

pub const DEFAULT_FILE_MODE: u32 = 0o644;
pub const DEFAULT_DIR_MODE: u32 = 0o755;
pub const DEFAULT_SYMLINK_MODE: u32 = 0o777;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirEntry {
    File(FileEntry),
    Directory,
    Symlink(SymlinkEntry),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
    pub content: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymlinkEntry {
    pub mode: u32,
    pub mtime: u64,
    pub target: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirChild {
    pub name: String,
    pub kind: DirKind,
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub mtime: Option<u64>,
    pub symlink_target: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirKind {
    File,
    Directory,
    Symlink,
}

/// Bumped whenever the on-disk DirEntry layout changes incompatibly.
/// Decode rejects any other value — there is no in-place migration today.
const ENTRY_VERSION: u8 = 1;

const TAG_FILE: u8 = 0x00;
const TAG_DIR: u8 = 0x01;
const TAG_SYMLINK: u8 = 0x02;
const CONTENT_INLINE: u8 = 0x00;
const CONTENT_EXTERNAL: u8 = 0x01;

pub fn encode_entry(de: &DirEntry) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(ENTRY_VERSION);
    match de {
        DirEntry::Directory => buf.push(TAG_DIR),
        DirEntry::File(f) => {
            buf.push(TAG_FILE);
            buf.extend_from_slice(&f.size.to_be_bytes());
            buf.extend_from_slice(&f.mode.to_be_bytes());
            buf.extend_from_slice(&f.mtime.to_be_bytes());
            match &f.content {
                Value::Inline(data) => {
                    buf.push(CONTENT_INLINE);
                    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
                    buf.extend_from_slice(data);
                }
                Value::External(h) => {
                    buf.push(CONTENT_EXTERNAL);
                    buf.extend_from_slice(&h.0);
                }
            }
        }
        DirEntry::Symlink(s) => {
            buf.push(TAG_SYMLINK);
            buf.extend_from_slice(&s.mode.to_be_bytes());
            buf.extend_from_slice(&s.mtime.to_be_bytes());
            let tlen = s.target.len() as u16;
            buf.extend_from_slice(&tlen.to_be_bytes());
            buf.extend_from_slice(&s.target);
        }
    }
    buf
}

pub fn decode_entry(data: &[u8]) -> io::Result<DirEntry> {
    if data.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty entry"));
    }
    if data[0] != ENTRY_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unknown dir-entry version {} (this build expects {})",
                data[0], ENTRY_VERSION
            ),
        ));
    }
    // Peel the version byte; the rest matches the v1 layout below.
    let data = &data[1..];
    if data.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing dir-entry tag",
        ));
    }
    match data[0] {
        TAG_DIR => {
            if data.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "trailing bytes in dir entry",
                ));
            }
            Ok(DirEntry::Directory)
        }
        TAG_FILE => {
            // tag(1) + size(8) + mode(4) + mtime(8) + content_tag(1) = 22 min
            if data.len() < 1 + 8 + 4 + 8 + 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated file entry",
                ));
            }
            let size = u64::from_be_bytes(data[1..9].try_into().unwrap());
            let mode = u32::from_be_bytes(data[9..13].try_into().unwrap());
            let mtime = u64::from_be_bytes(data[13..21].try_into().unwrap());
            let tag = data[21];
            let content = match tag {
                CONTENT_INLINE => {
                    if data.len() < 22 + 4 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "truncated inline len",
                        ));
                    }
                    let len = u32::from_be_bytes(data[22..26].try_into().unwrap()) as usize;
                    if data.len() != 26 + len {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "wrong inline length",
                        ));
                    }
                    Value::Inline(data[26..].to_vec())
                }
                CONTENT_EXTERNAL => {
                    if data.len() != 22 + 32 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "wrong external length",
                        ));
                    }
                    let mut h = [0u8; 32];
                    h.copy_from_slice(&data[22..54]);
                    Value::External(Hash(h))
                }
                t => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown content tag {}", t),
                    ))
                }
            };
            Ok(DirEntry::File(FileEntry {
                size,
                mode,
                mtime,
                content,
            }))
        }
        TAG_SYMLINK => {
            // tag(1) + mode(4) + mtime(8) + tlen(2) = 15 min
            if data.len() < 1 + 4 + 8 + 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated symlink entry",
                ));
            }
            let mode = u32::from_be_bytes(data[1..5].try_into().unwrap());
            let mtime = u64::from_be_bytes(data[5..13].try_into().unwrap());
            let tlen = u16::from_be_bytes(data[13..15].try_into().unwrap()) as usize;
            if data.len() != 15 + tlen {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "wrong symlink target length",
                ));
            }
            let target = data[15..].to_vec();
            Ok(DirEntry::Symlink(SymlinkEntry {
                mode,
                mtime,
                target,
            }))
        }
        t => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown dir entry tag {}", t),
        )),
    }
}

pub fn normalize_path(path: &str) -> io::Result<String> {
    if path.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains null byte",
        ));
    }
    let mut parts = Vec::new();
    for p in path.split('/') {
        if p.is_empty() || p == "." {
            continue;
        }
        if p == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains ..",
            ));
        }
        parts.push(p);
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_roundtrip_directory() {
        let de = DirEntry::Directory;
        assert_eq!(decode_entry(&encode_entry(&de)).unwrap(), de);
    }

    #[test]
    fn entry_roundtrip_file_inline() {
        let de = DirEntry::File(FileEntry {
            size: 5,
            mode: 0o644,
            mtime: 1700000000,
            content: Value::Inline(b"hello".to_vec()),
        });
        assert_eq!(decode_entry(&encode_entry(&de)).unwrap(), de);
    }

    #[test]
    fn entry_roundtrip_file_external() {
        let de = DirEntry::File(FileEntry {
            size: 1024,
            mode: 0o755,
            mtime: 0,
            content: Value::External(Hash::of(b"chunk")),
        });
        assert_eq!(decode_entry(&encode_entry(&de)).unwrap(), de);
    }

    #[test]
    fn entry_roundtrip_symlink() {
        let de = DirEntry::Symlink(SymlinkEntry {
            mode: 0o777,
            mtime: 1700000000,
            target: b"../other/path".to_vec(),
        });
        assert_eq!(decode_entry(&encode_entry(&de)).unwrap(), de);
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize_path("").unwrap(), "");
        assert_eq!(normalize_path("/").unwrap(), "");
        assert_eq!(normalize_path("/a/b/c").unwrap(), "a/b/c");
        assert_eq!(normalize_path("a/b/c").unwrap(), "a/b/c");
        assert_eq!(normalize_path("/a//b/").unwrap(), "a/b");
        assert_eq!(normalize_path("./a/./b").unwrap(), "a/b");
    }

    #[test]
    fn normalize_rejects_dotdot() {
        assert!(normalize_path("a/../b").is_err());
        assert!(normalize_path("../etc").is_err());
    }

    #[test]
    fn normalize_rejects_null() {
        assert!(normalize_path("a/\0").is_err());
    }
}
