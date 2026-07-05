//! Tar-layer application: decompress (gzip / zstd), normalize archive paths,
//! and apply OverlayFS-style whiteouts on top of a hako tree.

use crate::fs::ScopedFs;
use crate::hash::Hash;
use flate2::read::GzDecoder;
use std::io::{self, Read};

/// Upper bound on the decompressed size of a single layer. Guards against
/// gzip bombs: a small compressed blob can otherwise expand without limit.
const MAX_DECOMPRESSED_LAYER: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB
/// Upper bound on a single extracted file's size.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB
/// Cap on how much we pre-allocate from an (attacker-controlled) tar header
/// size hint before reading a byte, so a lying header can't trigger a huge
/// up-front allocation.
const MAX_PREALLOC: u64 = 1024 * 1024; // 1 MiB

/// Decompress an OCI/Docker layer blob. Selects the codec by media type,
/// falling back to magic-byte sniffing (some registries mislabel), and treats
/// anything unrecognized as an already-uncompressed tar.
pub(super) fn decompress(media_type: &str, blob: &[u8]) -> io::Result<Vec<u8>> {
    if media_type.contains("+gzip") || media_type.contains(".gzip") || is_gzip(blob) {
        read_capped(GzDecoder::new(blob))
    } else if media_type.contains("+zstd") || media_type.contains(".zstd") || is_zstd(blob) {
        // Pure-Rust zstd decoder (no C dependency, unlike the reference lib).
        //
        // Bomb safety: unlike gzip's fixed tiny window, a zstd frame declares its
        // window size in the header, which a decoder could allocate up front —
        // before `read_capped` ever sees a byte. We rely on `ruzstd` NOT eagerly
        // allocating the declared window: `StreamingDecoder::new` builds an empty
        // ring buffer that grows lazily as output is produced, so the same
        // `read_capped` output bound below caps peak memory. (Note this is our
        // guardrail, not ruzstd's own `MAX_WINDOW_SIZE` check, which the
        // first-frame path skips.) If a future ruzstd made the window buffer
        // eager, this bound would no longer cover it — revisit on upgrade.
        let dec = ruzstd::StreamingDecoder::new(blob)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zstd init: {e}")))?;
        read_capped(dec)
    } else {
        Ok(blob.to_vec())
    }
}

/// Read a decompressor to EOF, capped at [`MAX_DECOMPRESSED_LAYER`]: read at
/// most MAX+1 bytes and fail past the cap, so a decompression bomb (a tiny blob
/// that expands without bound) can't exhaust memory. Shared by every codec.
fn read_capped(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader
        .take(MAX_DECOMPRESSED_LAYER + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > MAX_DECOMPRESSED_LAYER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "decompressed layer exceeds size limit",
        ));
    }
    Ok(out)
}

fn is_gzip(blob: &[u8]) -> bool {
    blob.len() >= 2 && blob[0] == 0x1f && blob[1] == 0x8b
}

/// zstd frame magic number (`0xFD2FB528`, little-endian on the wire).
fn is_zstd(blob: &[u8]) -> bool {
    blob.len() >= 4 && blob[0] == 0x28 && blob[1] == 0xB5 && blob[2] == 0x2F && blob[3] == 0xFD
}

/// Apply a decompressed tar archive as an OverlayFS-style layer on top of
/// `root`, honoring `.wh.*` whiteouts and `.wh..wh..opq` opaque markers.
pub fn apply_tar_layer(
    scoped: &ScopedFs<'_>,
    mut root: Hash,
    tar_bytes: &[u8],
) -> io::Result<Hash> {
    let mut archive = tar::Archive::new(tar_bytes);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path_bytes().into_owned();
        let norm = match normalize_archive_path(&raw_path) {
            Some(p) => p,
            None => continue, // skip `.` and path-escapes
        };

        // Whiteout handling. OverlayFS convention:
        //   `foo/.wh..wh..opq` → opaque directory `foo` (drop lower content)
        //   `foo/.wh.bar`       → delete `foo/bar`
        let (parent, fname) = split_parent_name(&norm);
        if fname == ".wh..wh..opq" {
            if !parent.is_empty() && scoped.is_dir(&root, parent)? {
                root = scoped.delete(&root, parent)?;
                root = scoped.mkdir(&root, parent)?;
            }
            continue;
        }
        if let Some(name) = fname.strip_prefix(".wh.") {
            if name.is_empty() {
                // A bare `.wh.` names no target. Ignore it rather than letting the
                // empty target resolve to the tree root, whose delete errors and
                // would abort the whole pull on one malformed entry (#62).
                continue;
            }
            let target = if parent.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", parent, name)
            };
            if scoped.is_file(&root, &target)?
                || scoped.is_dir(&root, &target)?
                || scoped.is_symlink(&root, &target)?
            {
                root = scoped.delete(&root, &target)?;
            }
            continue;
        }

        let header = entry.header();
        let kind = header.entry_type();
        let mode = header.mode().unwrap_or(0o644);
        let mtime = header.mtime().unwrap_or(0);
        let declared_size = header.size().unwrap_or(0);

        if kind.is_dir() {
            root = scoped.mkdir(&root, &norm)?;
        } else if kind.is_symlink() {
            let target = header
                .link_name_bytes()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "symlink without target")
                })?
                .into_owned();
            root = scoped.write_symlink(&root, &norm, &target, mode, mtime)?;
        } else if kind.is_hard_link() {
            // Hardlinks point to another path already placed in the tree.
            let target = header.link_name_bytes().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "hardlink without target")
            })?;
            let target_norm = normalize_archive_path(&target)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad hardlink path"))?;
            root = scoped.cp_to(&root, &root, &target_norm, &norm)?;
        } else if kind.is_file() || matches!(kind, tar::EntryType::Continuous) {
            // Pre-allocate only up to MAX_PREALLOC even if the header claims a
            // larger size, then read at most MAX_FILE_BYTES+1 and reject if the
            // real content exceeds the cap.
            let prealloc = declared_size.min(MAX_PREALLOC) as usize;
            let mut buf = Vec::with_capacity(prealloc);
            entry
                .by_ref()
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut buf)?;
            if buf.len() as u64 > MAX_FILE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tar entry exceeds file size limit",
                ));
            }
            root = scoped.write_file_meta(&root, &norm, &buf, mode, mtime)?;
        }
        // char/block/fifo/socket: silently ignored
    }
    Ok(root)
}

/// Normalize a tar archive path (raw bytes, `/`-separated) to a hako vfs path.
/// Drops `.`, leading `/`, and empty components; returns `None` for empty
/// paths or anything containing `..`.
fn normalize_archive_path(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut parts = Vec::new();
    for p in s.split('/') {
        if p.is_empty() || p == "." {
            continue;
        }
        if p == ".." {
            return None;
        }
        parts.push(p);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Split a normalized path into `(parent, last_component)`. For a single-
/// component path, returns `("", path)`.
fn split_parent_name(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;
    use crate::tree::empty;

    #[test]
    fn decompress_handles_gzip_zstd_and_plain() {
        use std::io::Write;
        let payload = b"hako layer bytes, long enough to actually compress. ".repeat(20);

        // gzip
        let gz = {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&payload).unwrap();
            e.finish().unwrap()
        };
        assert_eq!(
            decompress("application/vnd.oci.image.layer.v1.tar+gzip", &gz).unwrap(),
            payload
        );

        // zstd — compressed by the C reference (dev-dep), decoded by our
        // pure-Rust ruzstd, both by media type and by magic-byte sniffing.
        let zst = zstd::encode_all(&payload[..], 3).unwrap();
        assert!(is_zstd(&zst), "fixture should carry the zstd magic");
        assert_eq!(
            decompress("application/vnd.oci.image.layer.v1.tar+zstd", &zst).unwrap(),
            payload
        );
        assert_eq!(
            decompress("application/octet-stream", &zst).unwrap(),
            payload,
            "magic-byte sniffing should catch a mislabeled zstd layer"
        );

        // plain (uncompressed tar) passes through untouched.
        assert_eq!(
            decompress("application/vnd.oci.image.layer.v1.tar", &payload).unwrap(),
            payload
        );
    }

    #[test]
    fn normalize_drops_leading_slash() {
        assert_eq!(normalize_archive_path(b"./foo").as_deref(), Some("foo"));
        assert_eq!(normalize_archive_path(b"a/b/c").as_deref(), Some("a/b/c"));
        assert_eq!(normalize_archive_path(b"/a//b/").as_deref(), Some("a/b"));
    }

    #[test]
    fn normalize_rejects_parent_dir() {
        assert_eq!(normalize_archive_path(b"../escape"), None);
        assert_eq!(normalize_archive_path(b"a/../b"), None);
    }

    #[test]
    fn split_parent_name_cases() {
        assert_eq!(split_parent_name("a/b/c"), ("a/b", "c"));
        assert_eq!(split_parent_name("foo"), ("", "foo"));
    }

    /// Build a minimal tar archive on the fly and drive `apply_tar_layer`.
    fn build_tar(entries: Vec<(&str, tar::EntryType, Vec<u8>, Option<&str>)>) -> Vec<u8> {
        let buf: Vec<u8> = Vec::new();
        let mut b = tar::Builder::new(buf);
        for (path, kind, data, linkname) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path(path).unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(0);
            h.set_entry_type(kind);
            if let Some(target) = linkname {
                h.set_link_name(target).unwrap();
            }
            h.set_cksum();
            b.append(&h, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn apply_layer_adds_file() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let tar = build_tar(vec![(
            "bin/sh",
            tar::EntryType::Regular,
            b"#!/bin/sh\n".to_vec(),
            None,
        )]);
        let root = apply_tar_layer(&fs, empty(), &tar).unwrap();
        assert_eq!(fs.read_file(&root, "bin/sh").unwrap(), b"#!/bin/sh\n");
    }

    #[test]
    fn apply_layer_reads_file_larger_than_prealloc_cap() {
        // A legitimate file bigger than MAX_PREALLOC (1 MiB) must extract
        // intact — the bounded read must not truncate real content.
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let big = vec![0xABu8; (MAX_PREALLOC as usize) + 4096];
        let tar = build_tar(vec![(
            "data/blob.bin",
            tar::EntryType::Regular,
            big.clone(),
            None,
        )]);
        let root = apply_tar_layer(&fs, empty(), &tar).unwrap();
        assert_eq!(fs.read_file(&root, "data/blob.bin").unwrap(), big);
    }

    #[test]
    fn apply_layer_creates_symlink() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let tar = build_tar(vec![(
            "lib/libc.so.6",
            tar::EntryType::Symlink,
            Vec::new(),
            Some("libc-2.35.so"),
        )]);
        let root = apply_tar_layer(&fs, empty(), &tar).unwrap();
        assert!(fs.is_symlink(&root, "lib/libc.so.6").unwrap());
        assert_eq!(
            fs.read_symlink(&root, "lib/libc.so.6").unwrap(),
            b"libc-2.35.so"
        );
    }

    #[test]
    fn apply_layer_ignores_a_bare_whiteout_entry() {
        // A malformed layer with a bare `.wh.` entry (empty whiteout target) must
        // not abort the whole apply by resolving to and deleting the tree root
        // (#62); it is skipped and the rest of the layer still applies.
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let tar = build_tar(vec![
            (".wh.", tar::EntryType::Regular, Vec::new(), None),
            ("etc/ok", tar::EntryType::Regular, b"present".to_vec(), None),
        ]);
        let root = apply_tar_layer(&fs, empty(), &tar).unwrap();
        assert_eq!(fs.read_file(&root, "etc/ok").unwrap(), b"present");
    }

    #[test]
    fn apply_layer_honors_whiteout() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let base = fs.write_file(&empty(), "etc/secret", b"top").unwrap();
        let base = fs.write_file(&base, "etc/keep", b"yes").unwrap();
        let tar = build_tar(vec![(
            "etc/.wh.secret",
            tar::EntryType::Regular,
            Vec::new(),
            None,
        )]);
        let root = apply_tar_layer(&fs, base, &tar).unwrap();
        assert!(!fs.exists(&root, "etc/secret").unwrap());
        assert_eq!(fs.read_file(&root, "etc/keep").unwrap(), b"yes");
    }

    #[test]
    fn apply_layer_honors_opaque() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let base = fs.write_file(&empty(), "app/a.txt", b"a").unwrap();
        let base = fs.write_file(&base, "app/b.txt", b"b").unwrap();
        let base = fs.write_file(&base, "other/c.txt", b"c").unwrap();
        // Opaque marker for `app/` wipes it before layer content is applied.
        let tar = build_tar(vec![
            (
                "app/.wh..wh..opq",
                tar::EntryType::Regular,
                Vec::new(),
                None,
            ),
            (
                "app/fresh.txt",
                tar::EntryType::Regular,
                b"new".to_vec(),
                None,
            ),
        ]);
        let root = apply_tar_layer(&fs, base, &tar).unwrap();
        assert!(!fs.exists(&root, "app/a.txt").unwrap());
        assert!(!fs.exists(&root, "app/b.txt").unwrap());
        assert_eq!(fs.read_file(&root, "app/fresh.txt").unwrap(), b"new");
        assert_eq!(fs.read_file(&root, "other/c.txt").unwrap(), b"c");
    }

    #[test]
    fn apply_layer_hardlink_duplicates_content() {
        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let tar = build_tar(vec![
            (
                "bin/first",
                tar::EntryType::Regular,
                b"executable".to_vec(),
                None,
            ),
            (
                "bin/second",
                tar::EntryType::Link,
                Vec::new(),
                Some("bin/first"),
            ),
        ]);
        let root = apply_tar_layer(&fs, empty(), &tar).unwrap();
        assert_eq!(fs.read_file(&root, "bin/first").unwrap(), b"executable");
        assert_eq!(fs.read_file(&root, "bin/second").unwrap(), b"executable");
    }

    #[test]
    fn apply_layer_preserves_mode_and_mtime() {
        use tar::{Builder, Header};
        let buf: Vec<u8> = Vec::new();
        let mut b = Builder::new(buf);
        let data = b"script\n";
        let mut h = Header::new_gnu();
        h.set_path("run.sh").unwrap();
        h.set_size(data.len() as u64);
        h.set_mode(0o755);
        h.set_mtime(12345);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append(&h, data.as_slice()).unwrap();
        let tar = b.into_inner().unwrap();

        let s = MemStore::new();
        let fs = ScopedFs::new(&s);
        let root = apply_tar_layer(&fs, empty(), &tar).unwrap();
        let children = fs.ls(&root, "").unwrap();
        let run = children.iter().find(|c| c.name == "run.sh").unwrap();
        assert_eq!(run.mode, Some(0o755));
        assert_eq!(run.mtime, Some(12345));
    }
}
