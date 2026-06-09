//! Image-reference parsing: `registry/repo:tag`, `registry/repo@digest`,
//! Docker Hub library shorthand, custom registries, localhost+port.

use std::io;

/// A parsed image reference: `registry/repo:tag` (or `@digest`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub repo: String,
    /// Either a tag (`latest`) or a full digest (`sha256:...`).
    pub reference: String,
}

impl ImageRef {
    /// Parse a user-supplied image reference, applying Docker Hub defaults.
    pub fn parse(s: &str) -> io::Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty image ref"));
        }

        // Split off registry: the first path segment is a registry iff it
        // contains a dot or colon (port) or is literally "localhost".
        let (registry, rest) = match s.find('/') {
            Some(i) => {
                let head = &s[..i];
                if head == "localhost" || head.contains('.') || head.contains(':') {
                    (head.to_string(), &s[i + 1..])
                } else {
                    ("registry-1.docker.io".to_string(), s)
                }
            }
            None => ("registry-1.docker.io".to_string(), s),
        };

        // Split off @digest or :tag.
        let (name, reference) = if let Some(i) = rest.find('@') {
            (&rest[..i], rest[i + 1..].to_string())
        } else if let Some(i) = rest.rfind(':') {
            // A colon before the last `/` is a port, not a tag. Since we
            // already peeled the registry, any `:` here is a tag delimiter.
            (&rest[..i], rest[i + 1..].to_string())
        } else {
            (rest, "latest".to_string())
        };

        // Docker Hub shorthand: bare `busybox` means `library/busybox`.
        let repo = if registry == "registry-1.docker.io" && !name.contains('/') {
            format!("library/{}", name)
        } else {
            name.to_string()
        };

        Ok(ImageRef {
            registry,
            repo,
            reference,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dockerhub_library_shorthand() {
        let r = ImageRef::parse("busybox").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repo, "library/busybox");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn with_tag() {
        let r = ImageRef::parse("busybox:1.36").unwrap();
        assert_eq!(r.repo, "library/busybox");
        assert_eq!(r.reference, "1.36");
    }

    #[test]
    fn with_namespace() {
        let r = ImageRef::parse("myuser/myrepo:v1").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repo, "myuser/myrepo");
        assert_eq!(r.reference, "v1");
    }

    #[test]
    fn with_custom_registry() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repo, "foo/bar");
        assert_eq!(r.reference, "v2");
    }

    #[test]
    fn localhost_port() {
        let r = ImageRef::parse("localhost:5000/x:y").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repo, "x");
        assert_eq!(r.reference, "y");
    }

    #[test]
    fn digest() {
        let r = ImageRef::parse("alpine@sha256:abc123").unwrap();
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.reference, "sha256:abc123");
    }
}
