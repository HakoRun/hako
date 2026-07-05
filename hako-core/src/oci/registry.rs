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
    /// Scheme + registry host, e.g. `https://registry-1.docker.io`.
    base: String,
    repo: String,
    token: Option<String>,
    agent: ureq::Agent,
}

/// Scheme + host for a registry. Loopback registries (a local dev registry, or
/// a test) are plain HTTP by default — matching Docker/containerd's "localhost
/// is insecure" convention; everything else is https.
fn registry_base(registry: &str) -> String {
    // Strip a trailing `:port` to get the host, being careful with a bracketed
    // IPv6 host (`[::1]:5000`) whose host part itself contains colons.
    let host = match registry.rfind(']') {
        Some(end) => &registry[..=end], // `[..]`, with or without a `:port`
        None => registry.rsplit_once(':').map_or(registry, |(h, _)| h),
    };
    let scheme = if matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{registry}")
}

impl Client {
    pub fn new(registry: &str, repo: &str) -> Self {
        Self {
            base: registry_base(registry),
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
                    // `registry_base` emits an `http` base only for a loopback
                    // registry, so the scheme is the loopback signal (#41).
                    let registry_is_loopback = self.base.starts_with("http://");
                    self.token = Some(fetch_token(&self.agent, &params, registry_is_loopback)?);
                    attempt += 1;
                }
                Err(e) => return Err(io::Error::other(format!("HTTP {}: {}", url, e))),
            }
        }
    }

    pub fn fetch_manifest(
        &mut self,
        reference: &str,
        opts: &PullOptions,
    ) -> io::Result<RawManifest> {
        let url = format!("{}/v2/{}/manifests/{}", self.base, self.repo, reference);
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

        let is_index =
            ctype.contains("index") || ctype.contains("list") || !manifest.manifests.is_empty();
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
                format!(
                    "manifest has no layers (media type {})",
                    manifest.media_type
                ),
            ));
        }
        Ok(manifest)
    }

    pub fn fetch_blob(&mut self, digest: &str) -> io::Result<Vec<u8>> {
        let url = format!("{}/v2/{}/blobs/{}", self.base, self.repo, digest);
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

/// Whether the (registry-controlled) token `realm` is safe to fetch from.
///
/// The realm comes from the `WWW-Authenticate` header — attacker-controlled if
/// the registry is malicious or MITM'd — and the fetch is a *credential-free*
/// GET, so an unrestricted realm is a blind SSRF: `realm="http://169.254.169.254/…"`
/// would hit the cloud metadata service (issue #41). Rule:
///   - `https` realms are always allowed (unchanged);
///   - an `http` realm is allowed **only** when the realm host *and* the registry
///     being pulled from are both loopback — i.e. the operator deliberately chose
///     a local registry (mirroring [`registry_base`]'s http-for-loopback rule), so
///     a local dev registry can use token auth over http without reopening the
///     SSRF for public/MITM registries. Both conditions are load-bearing: without
///     realm-loopback a local registry could still redirect to the metadata
///     service; without registry-loopback a public registry could aim the fetch
///     at the victim's own loopback.
///
/// The host is read from a real URL parse (the same `url` crate ureq dials with),
/// never a substring test — so `http://127.0.0.1@evil.com/` (host `evil.com`),
/// v4-mapped IPv6, IPv4 shorthand, and trailing-dot/userinfo tricks cannot smuggle
/// a non-loopback target past the check.
fn realm_is_allowed(realm: &str, registry_is_loopback: bool) -> bool {
    let Ok(url) = url::Url::parse(realm) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => registry_is_loopback && url_host_is_loopback(&url),
        _ => false,
    }
}

/// Whether a parsed URL's host is a literal loopback: `localhost` (exact — no
/// trailing dot, no subdomain), `127.0.0.1`, or `::1`. Other 127/8 addresses and
/// v4-mapped IPv6 (`::ffff:127.0.0.1`) are rejected — fail closed, matching the
/// literal set [`registry_base`] accepts.
fn url_host_is_loopback(url: &url::Url) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(a)) => a == Ipv4Addr::new(127, 0, 0, 1),
        Some(url::Host::Ipv6(a)) => a == Ipv6Addr::LOCALHOST,
        None => false,
    }
}

fn fetch_token(
    agent: &ureq::Agent,
    params: &AuthParams,
    registry_is_loopback: bool,
) -> io::Result<String> {
    if !realm_is_allowed(&params.realm, registry_is_loopback) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing auth realm {:?} (only https, or http loopback from a \
                 loopback registry, is allowed)",
                params.realm
            ),
        ));
    }
    let mut req = agent.get(&params.realm);
    if let Some(s) = &params.service {
        req = req.query("service", s);
    }
    if let Some(s) = &params.scope {
        req = req.query("scope", s);
    }
    let resp = req.call().map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("token fetch: {}", e),
        )
    })?;
    let mut buf = String::new();
    resp.into_reader().read_to_string(&mut buf)?;
    parse_token_response(&buf)
}

/// Extract the bearer token from a registry token endpoint's JSON response.
/// Docker/OCI registries return `{"token": "..."}`; some (and the OAuth2 refresh
/// flow) use `{"access_token": "..."}`. Prefer `token`, fall back to
/// `access_token`, and reject a response carrying neither.
fn parse_token_response(body: &str) -> io::Result<String> {
    #[derive(Deserialize)]
    struct Token {
        #[serde(default)]
        token: String,
        #[serde(default, rename = "access_token")]
        access_token: String,
    }
    let t: Token = serde_json::from_str(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("token parse: {}", e)))?;
    if !t.token.is_empty() {
        Ok(t.token)
    } else if !t.access_token.is_empty() {
        Ok(t.access_token)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "empty token",
        ))
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
    fn parse_token_prefers_token_then_access_token() {
        assert_eq!(parse_token_response(r#"{"token":"abc"}"#).unwrap(), "abc");
        assert_eq!(
            parse_token_response(r#"{"access_token":"xyz"}"#).unwrap(),
            "xyz"
        );
        // Both present → `token` wins (the Docker v2 field), regardless of order.
        assert_eq!(
            parse_token_response(r#"{"access_token":"b","token":"a"}"#).unwrap(),
            "a"
        );
    }

    #[test]
    fn parse_token_rejects_empty_and_malformed() {
        // Neither field, or an empty token, is a permission failure — not an
        // accidental empty bearer that would then 401 confusingly downstream.
        assert_eq!(
            parse_token_response("{}").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            parse_token_response(r#"{"token":""}"#).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // Non-JSON is a clear parse error, not a panic.
        assert_eq!(
            parse_token_response("not json").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn realm_https_is_always_allowed() {
        assert!(realm_is_allowed("https://auth.docker.io/token", false));
        assert!(realm_is_allowed("https://auth.docker.io/token", true));
        // https is allowed regardless of host (unchanged from #41).
        assert!(realm_is_allowed("https://evil.example/token", true));
    }

    #[test]
    fn realm_http_needs_both_registry_and_host_loopback() {
        // From a PUBLIC registry, every http realm is refused (the #41 threat).
        assert!(!realm_is_allowed("http://127.0.0.1/token", false));
        assert!(!realm_is_allowed("http://localhost/token", false));
        // From a loopback registry, an http loopback realm is allowed (local dev).
        assert!(realm_is_allowed("http://127.0.0.1:5000/token", true));
        assert!(realm_is_allowed("http://localhost:5000/token", true));
        assert!(realm_is_allowed("http://[::1]:5000/token", true));
        // ...but a NON-loopback http realm is still refused even from a loopback
        // registry — a local registry must not redirect the fetch at the metadata
        // service or the LAN.
        assert!(!realm_is_allowed(
            "http://169.254.169.254/latest/meta-data/",
            true
        ));
        assert!(!realm_is_allowed("http://evil.example/token", true));
    }

    #[test]
    fn realm_loopback_check_resists_host_spoofing() {
        // Userinfo before '@' is NOT the host: ureq would dial evil.example.
        assert!(!realm_is_allowed(
            "http://127.0.0.1@evil.example/token",
            true
        ));
        assert!(!realm_is_allowed(
            "http://localhost@evil.example/token",
            true
        ));
        // ...but a genuine loopback host with a decoy userinfo is fine.
        assert!(realm_is_allowed(
            "http://evil.example@127.0.0.1/token",
            true
        ));
        // v4-mapped IPv6 of loopback is not ::1 → refused (fail closed).
        assert!(!realm_is_allowed("http://[::ffff:127.0.0.1]/token", true));
        // A subdomain of localhost is a different host → refused.
        assert!(!realm_is_allowed("http://x.localhost/token", true));
        // Other 127/8 addresses → refused (matches registry_base's literal set).
        assert!(!realm_is_allowed("http://127.0.0.2/token", true));
        // Non-http(s) schemes and unparseable input → refused.
        assert!(!realm_is_allowed("file:///etc/passwd", true));
        assert!(!realm_is_allowed("gopher://127.0.0.1/", true));
        assert!(!realm_is_allowed("not a url", true));
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

    #[test]
    fn registry_base_uses_http_only_for_loopback() {
        assert_eq!(
            registry_base("registry-1.docker.io"),
            "https://registry-1.docker.io"
        );
        assert_eq!(registry_base("ghcr.io"), "https://ghcr.io");
        assert_eq!(registry_base("localhost:5000"), "http://localhost:5000");
        assert_eq!(registry_base("127.0.0.1:8080"), "http://127.0.0.1:8080");
        // Bracketed IPv6 loopback, with and without a port (the host part itself
        // contains colons, so a naive last-colon split would misparse it).
        assert_eq!(registry_base("[::1]:5000"), "http://[::1]:5000");
        assert_eq!(registry_base("[::1]"), "http://[::1]");
        // A non-loopback host with a port stays https.
        assert_eq!(
            registry_base("registry.example:5000"),
            "https://registry.example:5000"
        );
    }

    #[test]
    fn fetch_token_rejects_dangerous_realm_before_any_request() {
        // A malicious registry challenge pointing the credential-free token fetch
        // at an internal / metadata endpoint must be refused before any request is
        // made (blind SSRF guard, #41). No network is touched — the guard returns
        // first. Each is refused whether or not the *registry* is loopback: a
        // non-loopback http realm is never honoured, even for a local registry.
        let agent = ureq::AgentBuilder::new().build();
        for realm in [
            "http://169.254.169.254/latest/meta-data/",
            "http://auth.internal/token",
            "ftp://example.com/",
            " HTTP://Example.com/token",
        ] {
            for registry_is_loopback in [false, true] {
                let params = AuthParams {
                    realm: realm.to_string(),
                    service: None,
                    scope: None,
                };
                let err = fetch_token(&agent, &params, registry_is_loopback).unwrap_err();
                assert_eq!(
                    err.kind(),
                    io::ErrorKind::PermissionDenied,
                    "realm {realm:?} (registry_loopback={registry_is_loopback}) must be refused"
                );
            }
        }
    }
}
