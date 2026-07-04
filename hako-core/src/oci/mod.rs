//! OCI / Docker v2 registry pull. Supports anonymous and bearer-token auth,
//! image manifest indexes (multi-arch), and gzip- or zstd-compressed tar layers
//! with OverlayFS whiteouts applied on top of a hako `ScopedFs` working tree.

mod layers;
mod reference;
mod registry;

pub use layers::apply_tar_layer;
pub use reference::ImageRef;

use crate::fs::ScopedFs;
use crate::hash::Hash;
use std::io;

pub struct PullOptions {
    /// Target platform, e.g. ("linux", "amd64").
    pub os: String,
    pub arch: String,
    /// If true, apply all layers to a single tree. If false, return per-layer
    /// tree hashes so the caller can emit a commit per layer.
    pub squash: bool,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            os: "linux".into(),
            arch: "amd64".into(),
            squash: true,
        }
    }
}

pub struct PullResult {
    pub root: Hash,
    /// Tree hashes after applying each layer in order. Always non-empty.
    /// If `squash` is true, the caller may choose to make a single commit at `root`.
    pub layer_trees: Vec<Hash>,
}

/// Fetch `image` from its registry and apply its layers on top of `base_root`
/// in the chunk store behind `scoped`. Returns the final tree hash plus the
/// per-layer tree hashes.
pub fn pull(
    image: &ImageRef,
    scoped: &ScopedFs<'_>,
    base_root: Hash,
    opts: &PullOptions,
) -> io::Result<PullResult> {
    let mut client = registry::Client::new(&image.registry, &image.repo);
    let manifest = client.fetch_manifest(&image.reference, opts)?;

    let mut tree = base_root;
    let mut layer_trees = Vec::with_capacity(manifest.layers.len());
    for layer in &manifest.layers {
        let blob = client.fetch_blob(&layer.digest)?;
        let tar_bytes = layers::decompress(&layer.media_type, &blob)?;
        tree = apply_tar_layer(scoped, tree, &tar_bytes)?;
        layer_trees.push(tree);
    }
    let _ = opts.squash; // informational; squash vs per-layer is a caller-side choice
    Ok(PullResult {
        root: tree,
        layer_trees,
    })
}

#[cfg(test)]
mod tests {
    use super::{pull, ImageRef, PullOptions};
    use crate::fs::ScopedFs;
    use crate::store::MemStore;
    use crate::tree::empty;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn tar_with(path: &str, content: &[u8]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, content).unwrap();
        b.into_inner().unwrap()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn sha256_hex(b: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect()
    }

    /// Serve a single-layer image from a throwaway loopback registry, pull it,
    /// and return the bytes of `etc/hello` from the resulting tree. Drives the
    /// real client end to end over TCP: manifest fetch → blob fetch → digest
    /// verify → decompress → layer apply. Loopback ⇒ plain HTTP (no TLS needed).
    fn mock_pull_hello(layer: Vec<u8>, layer_media: &str) -> Vec<u8> {
        let digest = format!("sha256:{}", sha256_hex(&layer));
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[{{"mediaType":"{layer_media}","digest":"{digest}","size":{}}}]}}"#,
            layer.len()
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let manifest_bytes = manifest.into_bytes();
        // A single-layer pull makes exactly two requests — the manifest, then the
        // blob — each on its own connection (we send `Connection: close`). Serve
        // exactly those two and let the thread return: no lingering accept loop,
        // and being detached (never joined) it can't deadlock the test if the
        // pull fails early.
        std::thread::spawn(move || {
            for _ in 0..2 {
                let mut s = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("");
                let (ctype, body): (&str, &[u8]) = if path.contains("/manifests/") {
                    (
                        "application/vnd.oci.image.manifest.v1+json",
                        &manifest_bytes,
                    )
                } else if path.contains("/blobs/") {
                    ("application/octet-stream", &layer)
                } else {
                    let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    continue;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
            }
        });

        let image = ImageRef {
            registry: addr,
            repo: "testrepo".into(),
            reference: "latest".into(),
        };
        let store = MemStore::new();
        let scoped = ScopedFs::new(&store);
        let result = pull(&image, &scoped, empty(), &PullOptions::default())
            .expect("pull from mock registry");
        scoped.read_file(&result.root, "etc/hello").unwrap()
    }

    #[test]
    fn pull_applies_a_gzip_layer() {
        let layer = gzip(&tar_with("etc/hello", b"hi from a gzip layer"));
        assert_eq!(
            mock_pull_hello(layer, "application/vnd.oci.image.layer.v1.tar+gzip"),
            b"hi from a gzip layer"
        );
    }

    #[test]
    fn pull_applies_a_zstd_layer() {
        let tar = tar_with("etc/hello", b"hi from a zstd layer");
        let layer = zstd::encode_all(&tar[..], 3).unwrap();
        assert_eq!(
            mock_pull_hello(layer, "application/vnd.oci.image.layer.v1.tar+zstd"),
            b"hi from a zstd layer"
        );
    }

    /// Serve a single `401 Unauthorized` (with an optional `WWW-Authenticate`
    /// challenge) from a throwaway loopback registry, attempt a pull, and return
    /// the resulting error. Exercises the `Client::get` 401 branch end to end —
    /// the auth path the happy-path pull tests skip. (A full token-retry can't be
    /// mocked over loopback: the token realm must be https — the #41 SSRF guard
    /// forbids http realms — and standing up a trusted-cert TLS mock is out of
    /// proportion for a hardening test.)
    fn mock_pull_401(www_authenticate: Option<&str>) -> std::io::Error {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let challenge = www_authenticate
            .map(|w| format!("WWW-Authenticate: {w}\r\n"))
            .unwrap_or_default();
        // One connection: the client fails at the challenge (bad realm or none)
        // before any retry, so a single accept covers the exchange. Detached, so
        // it can't wedge the test if the pull errors even earlier.
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 401 Unauthorized\r\n{challenge}Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });

        let image = ImageRef {
            registry: addr,
            repo: "testrepo".into(),
            reference: "latest".into(),
        };
        let store = MemStore::new();
        let scoped = ScopedFs::new(&store);
        // `PullResult` isn't `Debug`, so match rather than `expect_err`.
        match pull(&image, &scoped, empty(), &PullOptions::default()) {
            Ok(_) => panic!("a 401 with an unusable challenge must fail the pull"),
            Err(e) => e,
        }
    }

    #[test]
    fn pull_refuses_a_non_https_auth_realm() {
        // A 401 whose challenge points the credential-free token fetch at an
        // http realm must be refused, not followed (SSRF guard, #41).
        let err = mock_pull_401(Some(
            r#"Bearer realm="http://127.0.0.1:1/token",service="r",scope="s""#,
        ));
        assert!(
            err.to_string().contains("non-https auth realm"),
            "expected the http realm to be refused: {err}"
        );
    }

    #[test]
    fn pull_reports_a_401_with_no_challenge() {
        // A 401 with no WWW-Authenticate header can't be retried; the error must
        // say so rather than surface an opaque HTTP failure.
        let err = mock_pull_401(None);
        assert!(
            err.to_string().contains("no challenge"),
            "expected a clear no-challenge error: {err}"
        );
    }
}
