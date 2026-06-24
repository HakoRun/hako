//! Workspace + project config from `<workspace>/hako.toml`.
//!
//! Two layered concepts in one file:
//!
//! 1. **Workspace defaults** — what container is the active identity if
//!    nothing else is specified. Set implicitly: `default_container = name`
//!    where `name = app.name | basename(app.image) | "hako"`.
//! 2. **Application config (`app`)** — declarative per-project setup,
//!    runtime, env, etc. Optional; absent for plain hako workspaces.
//!
//! Application config supports arbitrarily-named profile sections at the
//! top level. Any TOML table that isn't a known field becomes a profile
//! that can be selected via `hako apply --profile <name>`:
//!
//! ```toml
//! image = "python:3.12-slim@sha256:..."
//! setup = ["pip install -r /workspace/requirements.txt"]
//! run   = "python -m myapp"
//!
//! [dev]
//! workspace  = "rw"
//! env_pass   = ["OPENAI_API_KEY"]
//! autocommit = true
//!
//! [prod]
//! workspace  = "none"
//!
//! [ci]
//! autocommit = false
//! env_pass   = []
//! ```
//!
//! `hako apply` with no `--profile` uses just the base config. `--profile X`
//! overlays the named section's fields on top of the base. Unknown profile
//! names error explicitly (no silent fallback).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

const CONFIG_FILE: &str = "hako.toml";

// ============================================================================
// Resolved (merged) config — what the rest of the codebase consumes.
// ============================================================================

/// Top-level resolved config. `app` is None when no hako.toml exists.
#[derive(Clone, Debug)]
pub struct Config {
    /// Container that's the workspace's default identity. Resolution order:
    /// `app.name` → basename of `app.image` → literal `"hako"` (the
    /// init-bootstrapped toybox container).
    pub default_container: String,
    /// Application config from hako.toml, if present.
    pub app: Option<AppConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_container: "hako".into(),
            app: None,
        }
    }
}

/// Resolved application config — the result of merging the hako.toml base
/// section with the active profile (dev/prod).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    /// Base OCI image reference (required).
    pub image: String,
    /// Container name in the workspace. Defaults to image's repo basename.
    pub name: String,
    /// Setup commands run on `hako apply`. Each becomes a commit on the
    /// container's branch.
    pub setup: Vec<String>,
    /// What `hako run` (with no command) executes.
    pub run: Option<RunSpec>,
    /// Catch-all forwarding target for unknown subcommands.
    pub bin: Option<String>,
    /// User to run inside the container (uid name or numeric).
    pub user: Option<String>,
    /// Env vars set inside the container.
    pub env: BTreeMap<String, String>,
    /// Host env var names to forward into the container.
    pub env_pass: Vec<String>,
    /// Snapshot the container's tree after each exec.
    pub autocommit: bool,
    /// Workspace bind-mount mode.
    pub workspace: WorkspaceMode,
    /// Pass the host display (X11/Wayland) into the container so a GUI app can
    /// render on the host desktop. Off by default: it exposes the host display
    /// socket to the workload, which weakens isolation (see the runtime's
    /// `setup_display`). Opt in here, with `--display`, or `HAKO_DISPLAY=1`.
    pub display: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunSpec {
    /// `run = "python -m myapp"` — interpreted as `/bin/sh -c "..."`.
    Shell(String),
    /// `run = ["python", "-m", "myapp"]` — direct exec, no shell.
    Exec(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WorkspaceMode {
    /// No workspace mount (production-style; everything's in the image).
    None,
    /// Read-only mount.
    Ro,
    /// Read-write mount (the default for dev work).
    #[default]
    Rw,
}

/// CLI-supplied per-field overrides applied AFTER profile resolution.
/// Lets callers tweak a single setting without editing hako.toml.
/// `None`-valued fields leave the resolved AppConfig untouched.
#[derive(Clone, Debug, Default)]
pub struct AppOverrides {
    pub user: Option<String>,
    pub workspace: Option<WorkspaceMode>,
    /// Env additions/overrides as (key, value) pairs.
    pub env: Vec<(String, String)>,
    /// Additional host env vars to forward (appended to env_pass).
    pub env_pass: Vec<String>,
    pub autocommit: Option<bool>,
}

impl AppOverrides {
    pub fn apply_to(&self, app: &mut AppConfig) {
        if let Some(u) = &self.user {
            app.user = Some(u.clone());
        }
        if let Some(w) = self.workspace {
            app.workspace = w;
        }
        for (k, v) in &self.env {
            app.env.insert(k.clone(), v.clone());
        }
        for k in &self.env_pass {
            if !app.env_pass.iter().any(|x| x == k) {
                app.env_pass.push(k.clone());
            }
        }
        if let Some(a) = self.autocommit {
            app.autocommit = a;
        }
    }
}

// ============================================================================
// Loading
// ============================================================================

impl Config {
    /// Load `<workspace>/hako.toml`, returning defaults if missing.
    /// No profile is applied — base config only. Use `load_with_profile`
    /// to overlay a named profile.
    pub fn load(workspace: &Path) -> io::Result<Self> {
        Self::load_with_profile(workspace, None)
    }

    pub fn load_with_profile(workspace: &Path, profile: Option<&str>) -> io::Result<Self> {
        let p = workspace.join(CONFIG_FILE);
        let text = match fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let raw: AppRaw = toml::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("hako.toml: {}", e)))?;
        let app = raw.resolve(profile)?;
        let default_container = app.name.clone();
        Ok(Self {
            default_container,
            app: Some(app),
        })
    }
}

// ============================================================================
// Raw on-disk schema (with profile sections)
// ============================================================================

/// On-disk hako.toml shape, before profile resolution. Most fields optional
/// so partial files are accepted and the resolver fills defaults.
///
/// Any top-level TOML table not matching a known field is collected into
/// `profiles` via `serde(flatten)`. So `[dev]` and `[my-experiment]` both
/// become available as `--profile dev` / `--profile my-experiment`.
#[derive(Deserialize, Debug, Default)]
struct AppRaw {
    image: Option<String>,
    name: Option<String>,
    #[serde(default)]
    setup: Vec<String>,
    run: Option<RunRaw>,
    bin: Option<String>,
    user: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    env_pass: Vec<String>,
    autocommit: Option<bool>,
    workspace: Option<WorkspaceRaw>,
    display: Option<bool>,

    /// Catch-all for unknown top-level tables → user-named profiles.
    #[serde(flatten)]
    profiles: BTreeMap<String, ProfileRaw>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct ProfileRaw {
    image: Option<String>,
    setup: Option<Vec<String>>,
    run: Option<RunRaw>,
    bin: Option<String>,
    user: Option<String>,
    env: Option<BTreeMap<String, String>>,
    env_pass: Option<Vec<String>>,
    autocommit: Option<bool>,
    workspace: Option<WorkspaceRaw>,
    display: Option<bool>,
}

/// `run` accepts either a single shell string or an exec-form array.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum RunRaw {
    Shell(String),
    Exec(Vec<String>),
}

impl From<RunRaw> for RunSpec {
    fn from(r: RunRaw) -> Self {
        match r {
            RunRaw::Shell(s) => RunSpec::Shell(s),
            RunRaw::Exec(v) => RunSpec::Exec(v),
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum WorkspaceRaw {
    None,
    Ro,
    Rw,
}

impl From<WorkspaceRaw> for WorkspaceMode {
    fn from(w: WorkspaceRaw) -> Self {
        match w {
            WorkspaceRaw::None => WorkspaceMode::None,
            WorkspaceRaw::Ro => WorkspaceMode::Ro,
            WorkspaceRaw::Rw => WorkspaceMode::Rw,
        }
    }
}

// ============================================================================
// Resolution: AppRaw → AppConfig (applying profile)
// ============================================================================

impl AppRaw {
    fn resolve(self, profile_name: Option<&str>) -> io::Result<AppConfig> {
        // Look up the named profile, if any. Unknown names error explicitly
        // — better than silently falling through to base config and leaving
        // the user wondering why their `--profile typo` didn't take effect.
        let profile: Option<ProfileRaw> = match profile_name {
            None => None,
            Some(name) => Some(self.profiles.get(name).cloned().ok_or_else(|| {
                // Detect the legacy `[profiles.X]` nesting: if there's a
                // profile literally named `profiles` and all its known fields
                // are None, the user almost certainly wrote `[profiles.dev]`
                // instead of `[dev]`. Surface that explicitly so they don't
                // have to figure out the schema migration from a bare
                // "available: profiles" hint.
                if let Some(p) = self.profiles.get("profiles") {
                    if looks_like_legacy_nested(p) {
                        return io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "hako.toml: looks like profiles are nested under [profiles.X] \
                                 — flatten them to top-level tables, e.g. [{}] instead of \
                                 [profiles.{}]",
                                name, name
                            ),
                        );
                    }
                }
                let available: Vec<&str> = self.profiles.keys().map(|s| s.as_str()).collect();
                let hint = if available.is_empty() {
                    "no profiles defined".to_string()
                } else {
                    format!("available: {}", available.join(", "))
                };
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("hako.toml: no profile {:?} ({})", name, hint),
                )
            })?),
        };

        // Merge: profile field if Some, else base field.
        let image = profile
            .as_ref()
            .and_then(|p| p.image.clone())
            .or(self.image)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "hako.toml: `image` is required (either at top level or inside the active profile)",
                )
            })?;
        if image.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hako.toml: `image` cannot be empty",
            ));
        }

        let setup = profile
            .as_ref()
            .and_then(|p| p.setup.clone())
            .unwrap_or(self.setup);
        let run = profile
            .as_ref()
            .and_then(|p| p.run.clone())
            .or(self.run)
            .map(RunSpec::from);
        let bin = profile.as_ref().and_then(|p| p.bin.clone()).or(self.bin);
        let user = profile.as_ref().and_then(|p| p.user.clone()).or(self.user);
        let env = profile
            .as_ref()
            .and_then(|p| p.env.clone())
            .unwrap_or(self.env);
        let env_pass = profile
            .as_ref()
            .and_then(|p| p.env_pass.clone())
            .unwrap_or(self.env_pass);
        let autocommit = profile
            .as_ref()
            .and_then(|p| p.autocommit)
            .or(self.autocommit)
            .unwrap_or(false);
        let workspace = profile
            .as_ref()
            .and_then(|p| p.workspace)
            .or(self.workspace)
            .map(WorkspaceMode::from)
            .unwrap_or_default();
        let display = profile
            .as_ref()
            .and_then(|p| p.display)
            .or(self.display)
            .unwrap_or(false);

        // Container name: explicit `name`, else basename of image's repo.
        let name = self
            .name
            .clone()
            .unwrap_or_else(|| derive_name_from_image(&image));

        Ok(AppConfig {
            image,
            name,
            setup,
            run,
            bin,
            user,
            env,
            env_pass,
            autocommit,
            workspace,
            display,
        })
    }
}

/// True if a `[profiles]` table parsed as a `ProfileRaw` with no known
/// fields populated. That's the fingerprint of someone writing
/// `[profiles.dev]` (the legacy schema) instead of `[dev]`: serde flatten
/// captures `profiles` as a single profile, but every field is None
/// because the actual content is in `profiles.dev` / `profiles.prod`,
/// which a profile-shaped struct doesn't recognize.
fn looks_like_legacy_nested(p: &ProfileRaw) -> bool {
    p.image.is_none()
        && p.setup.is_none()
        && p.run.is_none()
        && p.bin.is_none()
        && p.user.is_none()
        && p.env.is_none()
        && p.env_pass.is_none()
        && p.autocommit.is_none()
        && p.workspace.is_none()
        && p.display.is_none()
}

/// Extract a sensible container name from an image ref. `python:3.12-slim`
/// → `python`; `ghcr.io/foo/bar:v1` → `bar`. We're not strict about
/// validation here — this is a default, the user can override with `name`.
fn derive_name_from_image(image: &str) -> String {
    let no_tag = image.split('@').next().unwrap_or(image);
    let no_tag = no_tag.split(':').next().unwrap_or(no_tag);
    no_tag.rsplit('/').next().unwrap_or(no_tag).to_string()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> io::Result<AppConfig> {
        parse_with(text, None)
    }

    fn parse_with(text: &str, profile: Option<&str>) -> io::Result<AppConfig> {
        let raw: AppRaw = toml::from_str(text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        raw.resolve(profile)
    }

    #[test]
    fn minimal_config_only_image() {
        let c = parse(r#"image = "python:3.12-slim""#).unwrap();
        assert_eq!(c.image, "python:3.12-slim");
        assert_eq!(c.name, "python"); // derived from image
        assert!(c.setup.is_empty());
        assert!(c.run.is_none());
        assert!(!c.autocommit);
        assert_eq!(c.workspace, WorkspaceMode::Rw); // unspecified → rw default
    }

    #[test]
    fn name_derivation() {
        assert_eq!(derive_name_from_image("alpine"), "alpine");
        assert_eq!(derive_name_from_image("alpine:3.19"), "alpine");
        assert_eq!(derive_name_from_image("library/alpine"), "alpine");
        assert_eq!(derive_name_from_image("ghcr.io/foo/bar"), "bar");
        assert_eq!(derive_name_from_image("ghcr.io/foo/bar:v1"), "bar");
        assert_eq!(derive_name_from_image("alpine@sha256:abc123"), "alpine");
    }

    #[test]
    fn missing_image_errors() {
        let r = parse(r#"name = "test""#);
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("image"), "error should mention image: {}", msg);
    }

    #[test]
    fn empty_image_errors() {
        let r = parse(r#"image = """#);
        assert!(r.is_err());
    }

    #[test]
    fn named_profiles_overlay_correctly() {
        let text = r#"
image = "python:3.12-slim"
name  = "myapp"
setup = ["pip install -r /workspace/requirements.txt"]
run   = "python -m myapp"
user  = "node"
env   = { LOG_LEVEL = "info" }
env_pass = ["OPENAI_API_KEY"]
autocommit = true

[dev]
workspace = "rw"

[prod]
workspace = "none"
env_pass = []
autocommit = false

[ci]
autocommit = false
"#;
        // Base config: workspace defaults to Rw, autocommit per top-level true.
        let base = parse(text).unwrap();
        assert_eq!(base.workspace, WorkspaceMode::Rw);
        assert!(base.autocommit);

        // Dev profile: same as base for these fields.
        let dev = parse_with(text, Some("dev")).unwrap();
        assert_eq!(dev.user.as_deref(), Some("node"));
        assert_eq!(dev.env_pass, vec!["OPENAI_API_KEY"]);
        assert!(dev.autocommit);
        assert_eq!(dev.workspace, WorkspaceMode::Rw);

        // Prod: workspace=none, no env_pass, no autocommit.
        let prod = parse_with(text, Some("prod")).unwrap();
        assert!(prod.env_pass.is_empty());
        assert!(!prod.autocommit);
        assert_eq!(prod.workspace, WorkspaceMode::None);

        // CI: only autocommit changed; env_pass, user, workspace all inherited.
        let ci = parse_with(text, Some("ci")).unwrap();
        assert!(!ci.autocommit);
        assert_eq!(ci.env_pass, vec!["OPENAI_API_KEY"]);
        assert_eq!(ci.workspace, WorkspaceMode::Rw);
    }

    #[test]
    fn unknown_profile_errors_with_hint() {
        let text = r#"
image = "alpine"
[dev]
workspace = "rw"
[prod]
workspace = "none"
"#;
        let r = parse_with(text, Some("nope"));
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("nope"),
            "error should name the missing profile"
        );
        assert!(msg.contains("dev"), "error should list available profiles");
        assert!(msg.contains("prod"));
    }

    #[test]
    fn unknown_profile_when_none_defined() {
        let r = parse_with(r#"image = "alpine""#, Some("dev"));
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("no profiles defined"));
    }

    #[test]
    fn legacy_nested_profiles_get_migration_hint() {
        let text = r#"
image = "alpine"

[profiles.dev]
workspace = "rw"

[profiles.prod]
workspace = "none"
"#;
        let r = parse_with(text, Some("dev"));
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("[dev]") && msg.contains("[profiles.dev]"),
            "expected migration hint suggesting [dev], got: {}",
            msg
        );
    }

    #[test]
    fn run_shell_form() {
        let c = parse(
            r#"
image = "alpine"
run = "echo hi"
"#,
        )
        .unwrap();
        match c.run {
            Some(RunSpec::Shell(s)) => assert_eq!(s, "echo hi"),
            other => panic!("expected Shell, got {:?}", other),
        }
    }

    #[test]
    fn run_exec_form() {
        let c = parse(
            r#"
image = "alpine"
run = ["echo", "hi"]
"#,
        )
        .unwrap();
        match c.run {
            Some(RunSpec::Exec(v)) => assert_eq!(v, vec!["echo", "hi"]),
            other => panic!("expected Exec, got {:?}", other),
        }
    }

    #[test]
    fn profile_overrides_setup_completely() {
        // Profile setup REPLACES base setup, not appends. This matches
        // the reference's behavior and the common Docker mental model.
        let text = r#"
image = "alpine"
setup = ["apk add bash"]

[fat]
setup = ["apk add bash python3"]
"#;
        let base = parse(text).unwrap();
        assert_eq!(base.setup, vec!["apk add bash"]);
        let fat = parse_with(text, Some("fat")).unwrap();
        assert_eq!(fat.setup, vec!["apk add bash python3"]);
    }

    #[test]
    fn overrides_apply_after_profile_resolution() {
        let text = r#"
image = "alpine"
user  = "alice"
env   = { K = "base" }
env_pass = ["A"]
autocommit = false

[dev]
user  = "bob"
"#;
        let mut app = parse_with(text, Some("dev")).unwrap();
        // Profile applied: user=bob.
        assert_eq!(app.user.as_deref(), Some("bob"));

        let ovs = AppOverrides {
            user: Some("carol".into()),
            workspace: Some(WorkspaceMode::Ro),
            env: vec![
                ("K".into(), "override".into()),
                ("NEW".into(), "added".into()),
            ],
            env_pass: vec!["B".into(), "A".into()], // A is dup; should not double
            autocommit: Some(true),
        };
        ovs.apply_to(&mut app);
        assert_eq!(app.user.as_deref(), Some("carol"));
        assert_eq!(app.workspace, WorkspaceMode::Ro);
        assert_eq!(app.env.get("K").map(String::as_str), Some("override"));
        assert_eq!(app.env.get("NEW").map(String::as_str), Some("added"));
        assert_eq!(app.env_pass, vec!["A", "B"]); // A dedup, B appended
        assert!(app.autocommit);
    }

    #[test]
    fn no_hako_toml_returns_default_config() {
        // Test via the public load path with a tempdir lacking hako.toml.
        let d = tempfile::TempDir::new().unwrap();
        let c = Config::load(d.path()).unwrap();
        assert_eq!(c.default_container, "hako");
        assert!(c.app.is_none());
    }

    #[test]
    fn loads_and_resolves_from_disk() {
        let d = tempfile::TempDir::new().unwrap();
        std::fs::write(
            d.path().join("hako.toml"),
            r#"
image = "alpine:3.19"
setup = ["echo init"]
"#,
        )
        .unwrap();
        let c = Config::load(d.path()).unwrap();
        assert_eq!(c.default_container, "alpine");
        let app = c.app.unwrap();
        assert_eq!(app.image, "alpine:3.19");
        assert_eq!(app.setup, vec!["echo init"]);
    }
}
