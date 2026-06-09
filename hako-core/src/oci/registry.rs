//! Docker v2 registry HTTP client: bearer-token auth, manifest + blob fetch,
//! and platform selection from a multi-arch manifest index.

use super::PullOptions;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{self, Read};

const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json",
);

#[derive(Deserialize, Debug, Clone)]
pub(super) struct RawDescriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub platform: Option<RawPlatform>,
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct RawPlatform {
    pub architecture: String,
    pub os: String,
}

#[derive(Deserialize, Debug)]
pub(super) struct RawManifest {
    #[serde(rename = "schemaVersion", default)]
    _schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    #[serde(default)]
    pub layers: Vec<RawDescriptor>,
    #[serde(default)]
    pub manifests: Vec<RawDescriptor>,
}

pub(super) struct Client {
    registry: String,
    repo: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(registry: &str, repo: &str) -> Self {
        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            token: None,
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    fn get(&mut self, url: &str, accept: &str) -> io::Result<(Vec<u8>, String)> {
        let mut attempt = 0;
        loop {
            let mut req = self.agent.get(url).set("Accept", accept);
            if let Some(tok) = &self.token {
                req = req.set("Authorization", &format!("Bearer {}", tok));
            }
            match req.call() {
                Ok(resp) => {
                    let ctype = resp
                        .header("content-type")
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    let mut buf = Vec::new();
                    resp.into_reader()
                        .take(1024 * 1024 * 1024)
                        .read_to_end(&mut buf)?;
                    return Ok((buf, ctype));
                }
                Err(ureq::Error::Status(401, resp)) if attempt == 0 => {
                    let chal = resp.header("www-authenticate").unwrap_or("").to_string();
                    drop(resp);
                    let params = parse_auth_challenge(&chal).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("unauthorized, no challenge: {}", chal),
                        )
                    })?;
                    self.token = Some(fetch_token(&self.agent, &params)?);
                    attempt += 1;
                }
                Err(e) => {
                    return Err(io::Error::other(
                        format!("HTTP {}: {}", url, e),
                    ))
                }
            }
        }
    }

    pub fn fetch_manifest(
        &mut self,
        reference: &str,
        opts: &PullOptions,
    ) -> io::Result<RawManifest> {
        let url = format!(
            "https://{}/v2/{}/manifests/{}",
            self.registry, self.repo, reference
        );
        let (body, ctype) = self.get(&url, MANIFEST_ACCEPT)?;
        // If the caller specified a digest (rather than a tag), verify the
        // bytes match. Tag fetches have nothing to verify against; we trust
        // the registry to serve the right manifest for a tag.
        if reference.contains(':') {
            verify_digest(reference, &body)?;
        }
        let manifest: RawManifest = serde_json::from_slice(&body).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("manifest parse: {}", e))
        })?;

        let is_index = ctype.contains("index")
            || ctype.contains("list")
            || !manifest.manifests.is_empty();
        if is_index {
            let pick = manifest
                .manifests
                .iter()
                .find(|m| match &m.platform {
                    Some(p) => p.os == opts.os && p.architecture == opts.arch,
                    None => false,
                })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "no manifest for {}/{} in index (repo has {} entries)",
                            opts.os,
                            opts.arch,
                            manifest.manifests.len()
                        ),
                    )
                })?
                .clone();
            return self.fetch_manifest(&pick.digest, opts);
        }
        if manifest.layers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest has no layers (media type {})", manifest.media_type),
            ));
        }
        Ok(manifest)
    }

    pub fn fetch_blob(&mut self, digest: &str) -> io::Result<Vec<u8>> {
        let url = format!(
            "https://{}/v2/{}/blobs/{}",
            self.registry, self.repo, digest
        );
        let (body, _) = self.get(&url, "*/*")?;
        verify_digest(digest, &body)?;
        Ok(body)
    }
}

/// Verify `bytes` hash to `digest`. `digest` is OCI-format `algo:hex`,
/// today only `sha256` is accepted (sha512 is rare in the wild and we
/// fail fast rather than silently skipping verification).
fn verify_digest(digest: &str, bytes: &[u8]) -> io::Result<()> {
    let (algo, expected_hex) = digest.split_once(':').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed digest (no algo): {}", digest),
        )
    })?;
    if !algo.eq_ignore_ascii_case("sha256") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "digest algo {} not supported (only sha256); refusing to skip verification",
                algo
            ),
        ));
    }
    let actual = Sha256::digest(bytes);
    let actual_hex = hex_encode(&actual);
    if !expected_hex.eq_ignore_ascii_case(&actual_hex) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "digest mismatch: expected {}, got sha256:{} ({} bytes)",
                digest,
                actual_hex,
                bytes.len()
            ),
        ));
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[derive(Debug)]
struct AuthParams {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_auth_challenge(header: &str) -> Option<AuthParams> {
    let trimmed = header.trim();
    if !trimmed.to_ascii_lowercase().starts_with("bearer ") {
        return None;
    }
    let rest = &trimmed[7..];
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for kv in split_challenge_params(rest) {
        let mut it = kv.splitn(2, '=');
        let k = it.next()?.trim().to_ascii_lowercase();
        let v = it.next()?.trim().trim_matches('"').to_string();
        match k.as_str() {
            "realm" => realm = Some(v),
            "service" => service = Some(v),
            "scope" => scope = Some(v),
            _ => {}
        }
    }
    Some(AuthParams {
        realm: realm?,
        service,
        scope,
    })
}

fn split_challenge_params(s: &str) -> Vec<String> {
    // Split on commas that are outside quoted strings.
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                buf.push(c);
            }
            ',' if !in_q => {
                out.push(std::mem::take(&mut buf));
            }
            _ => buf.push(c),
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn fetch_token(agent: &ureq::Agent, params: &AuthParams) -> io::Result<String> {
    let mut req = agent.get(&params.realm);
    if let Some(s) = &params.service {
        req = req.query("service", s);
    }
    if let Some(s) = &params.scope {
        req = req.query("scope", s);
    }
    let resp = req.call().map_err(|e| {
        io::Error::new(io::ErrorKind::PermissionDenied, format!("token fetch: {}", e))
    })?;
    let mut buf = String::new();
    resp.into_reader().read_to_string(&mut buf)?;

    #[derive(Deserialize)]
    struct Token {
        #[serde(default)]
        token: String,
        #[serde(default, rename = "access_token")]
        access_token: String,
    }
    let t: Token = serde_json::from_str(&buf).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("token parse: {}", e))
    })?;
    if !t.token.is_empty() {
        Ok(t.token)
    } else if !t.access_token.is_empty() {
        Ok(t.access_token)
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "empty token"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auth_challenge_basic() {
        let c = parse_auth_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/busybox:pull""#,
        )
        .unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(c.scope.as_deref(), Some("repository:library/busybox:pull"));
    }

    #[test]
    fn verify_digest_accepts_matching_sha256() {
        // sha256 of "hello" = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let digest = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify_digest(digest, b"hello").unwrap();
    }

    #[test]
    fn verify_digest_rejects_wrong_bytes() {
        let digest = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let err = verify_digest(digest, b"hellp").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn verify_digest_rejects_unknown_algo() {
        let digest = "md5:5d41402abc4b2a76b9719d911017c592";
        let err = verify_digest(digest, b"hello").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn verify_digest_rejects_malformed() {
        let err = verify_digest("not-a-digest", b"hello").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn verify_digest_case_insensitive_algo() {
        let digest = "SHA256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify_digest(digest, b"hello").unwrap();
    }
}
