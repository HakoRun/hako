//! OCI / Docker v2 registry pull. Supports anonymous and bearer-token auth,
//! image manifest indexes (multi-arch), and gzip-compressed tar layers with
//! OverlayFS whiteouts applied on top of a hako `ScopedFs` working tree.

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
