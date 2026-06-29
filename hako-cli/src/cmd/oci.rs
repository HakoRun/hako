//! OCI image pulls.

use super::Ctx;
use crate::helpers::now_secs;
use hako::{Hash, ImageRef, ScopedFs, State};
use std::io;
use std::process::ExitCode;

/// CLI handler for `hako pull <image> [--into <container>]`.
///
/// Defaults the target container to the image's repo basename: `alpine` →
/// `alpine`, `library/alpine` → `alpine`, `ghcr.io/foo/bar` → `bar`. Pass
/// `--into <name>` to override.
pub fn pull(
    ctx: &Ctx<'_>,
    image: String,
    per_layer: bool,
    os: String,
    arch: String,
    into: Option<String>,
) -> io::Result<ExitCode> {
    let image_ref = hako::ImageRef::parse(&image).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("bad image ref: {}", e))
    })?;
    let container = into.unwrap_or_else(|| derive_container_name(&image_ref));
    let result_root = pull_into(ctx.state, &image_ref, &container, &os, &arch, per_layer)?;
    println!(
        "pulled {}/{}:{} into container {} (root {})",
        image_ref.registry,
        image_ref.repo,
        image_ref.reference,
        container,
        &result_root.to_hex()[..12]
    );
    Ok(ExitCode::SUCCESS)
}

/// The OCI architecture matching the host the Linux runtime runs on. hako always
/// runs a Linux runtime — native, or bridged into WSL2/Lima — so a pulled image's
/// arch must match the host CPU, not a hardcoded `amd64`. (`x86_64`→`amd64`,
/// `aarch64`→`arm64`; other arches fall through as their Rust name, best-effort.)
pub fn host_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Pull `image_ref` into a container named `container`, creating the
/// container if it doesn't exist. Returns the new root tree hash. Used by
/// the `pull` CLI handler AND by `nav::switch_identity`'s auto-bootstrap
/// when `hako is alpine` runs against a workspace that doesn't have alpine yet.
pub fn pull_into(
    state: &State,
    image_ref: &ImageRef,
    container: &str,
    os: &str,
    arch: &str,
    per_layer: bool,
) -> io::Result<Hash> {
    eprintln!(
        "hako: pulling {}/{}:{} ({}/{}) into container {}",
        image_ref.registry, image_ref.repo, image_ref.reference, os, arch, container
    );
    // Track whether THIS call created the container, so a mid-pull failure
    // (network drop, registry 5xx, bad layer) doesn't leave a half-created,
    // empty container behind for `hako containers` to list and a later op to
    // trip over. We roll it back on error; an existing container is untouched.
    let newly_created = !state.list_containers()?.iter().any(|c| c == container);
    if newly_created {
        state.create_container(container)?;
    }
    let outcome = pull_into_inner(state, image_ref, container, os, arch, per_layer);
    if outcome.is_err() && newly_created {
        let _ = state.delete_container(container);
    }
    outcome
}

/// The fetch-and-commit body of `pull_into`, separated so the caller can roll
/// back a freshly-created container if any step here fails.
fn pull_into_inner(
    state: &State,
    image_ref: &ImageRef,
    container: &str,
    os: &str,
    arch: &str,
    per_layer: bool,
) -> io::Result<Hash> {
    let repo = state.open_container(container)?;
    let scoped = ScopedFs::new(repo.store());
    let base = repo.working_tree()?;
    let opts = hako::PullOptions {
        os: os.into(),
        arch: arch.into(),
        squash: !per_layer,
    };
    let result = hako::oci_pull(image_ref, &scoped, base, &opts)?;

    let author = "oci-pull";
    let ts = now_secs();
    if per_layer {
        let mut parents: Vec<Hash> = repo.head_commit()?.into_iter().collect();
        for (i, tree) in result.layer_trees.iter().enumerate() {
            let msg = format!(
                "oci layer {} of {} ({}/{}:{})",
                i + 1,
                result.layer_trees.len(),
                image_ref.registry,
                image_ref.repo,
                image_ref.reference
            );
            let c = repo.commit(*tree, parents.clone(), author, &msg, ts)?;
            let branch = repo.current_branch()?.unwrap_or_else(|| "main".into());
            repo.write_ref(&branch, c)?;
            parents = vec![c];
        }
        repo.set_working(result.root)?;
    } else {
        repo.set_working(result.root)?;
        let parents: Vec<Hash> = repo.head_commit()?.into_iter().collect();
        let msg = format!(
            "oci pull {}/{}:{}",
            image_ref.registry, image_ref.repo, image_ref.reference
        );
        let c = repo.commit(result.root, parents, author, &msg, ts)?;
        let branch = repo.current_branch()?.unwrap_or_else(|| "main".into());
        repo.write_ref(&branch, c)?;
    }
    Ok(result.root)
}

/// Derive a container name from an image ref. Takes the last path segment
/// of the repo (`library/alpine` → `alpine`, `foo/bar/baz` → `baz`). For
/// trivial repos with no slashes, returns the repo as-is.
fn derive_container_name(image_ref: &ImageRef) -> String {
    image_ref
        .repo
        .rsplit('/')
        .next()
        .unwrap_or(&image_ref.repo)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_oci_arch_matches_current() {
        let a = host_oci_arch();
        assert!(!a.is_empty());
        #[cfg(target_arch = "x86_64")]
        assert_eq!(a, "amd64");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(a, "arm64");
    }
}
