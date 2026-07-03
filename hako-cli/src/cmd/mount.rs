//! FUSE mount. Linux only; this module is `#[cfg]`-gated out elsewhere.

use super::Ctx;
use crate::helpers::resolve_tree;
use crate::DOT_HAKO;
use hako::store::ChunkStore;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

pub fn mount(ctx: &Ctx<'_>, mountpoint: PathBuf, from: String) -> io::Result<ExitCode> {
    let repo = ctx.state.open_container(ctx.default_container)?;
    let root = resolve_tree(&repo, &from)?;
    // FUSE needs a `'static` store it can own across threads. Open a fresh
    // FsStore pointed at the same objects directory.
    let objs = ctx.workdir.join(DOT_HAKO).join(hako::state::OBJECTS);
    drop(repo);
    let store: Arc<dyn ChunkStore + Send + Sync + 'static> = Arc::new(hako::FsStore::new(objs)?);
    crate::diag!(
        "mounting tree {} at {} (read-only; Ctrl+C to unmount)",
        &root.to_hex()[..12],
        mountpoint.display()
    );
    hako::fuse::mount(store, root, &mountpoint)?;
    Ok(ExitCode::SUCCESS)
}
