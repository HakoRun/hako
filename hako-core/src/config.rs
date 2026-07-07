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
    /// Node-local deploy config: what this node (re)launches when a tracked
    /// branch advances. Present only if hako.toml has a `[deploy]` table.
    /// Deliberately *not* profile-overlaid — it describes the receiving node,
    /// not an app profile.
    pub deploy: Option<DeployConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_container: "hako".into(),
            app: None,
            deploy: None,
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

/// Node-local deploy target (`[deploy]` in hako.toml). Read by the receiving
/// node's serve daemon to reconcile a running workload when the tracked branch
/// advances (push-to-deploy). Its `run`/network/volume shape is declared
/// receiver-side on purpose, so a push can never dictate what code runs here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployConfig {
    /// Container whose branch this node deploys.
    pub container: String,
    /// Branch to watch and (re)launch on advance.
    pub branch: String,
    /// Command the deployed workload runs. Receiver-declared (never from the
    /// pushed tree) — the push supplies the filesystem, this supplies what runs
    /// on it. `None` launches the container's default shell (rarely what a
    /// service wants; set `run` for a real deploy).
    pub run: Option<RunSpec>,
    /// Graceful-stop drain + health-gate window, seconds (default 10). The
    /// reconcile runs on the push's response path and can take up to *twice* this
    /// (drain the old + health-gate the new), so keep it well under the ~30s wire
    /// timeout — a larger value still deploys, but `hako peer push` may report a
    /// read timeout while the deploy completes server-side (check `status`).
    pub grace_secs: u64,
    /// Networking for the workload (matches `run --network`); `None` = isolated.
    pub network: Option<String>,
    /// Published ports, `host:container`.
    pub ports: Vec<String>,
    /// Volume mounts, `host:container[:ro]`.
    pub volumes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunSpec {
    /// `run = "python -m myapp"` — interpreted as `/bin/sh -c "..."`.
    Shell(String),
    /// `run = ["python", "-m", "myapp"]` — direct exec, no shell.
    Exec(Vec<String>),
}

impl RunSpec {
    /// The argv to exec. A shell string becomes `["/bin/sh", "-c", cmd]`; an
    /// exec-form array is passed through verbatim.
    pub fn argv(&self) -> Vec<String> {
        match self {
            RunSpec::Shell(cmd) => vec!["/bin/sh".into(), "-c".into(), cmd.clone()],
            RunSpec::Exec(v) => v.clone(),
        }
    }
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
        let mut raw: AppRaw = toml::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("hako.toml: {}", e)))?;
        // `[deploy]` is node-local, not profile-overlaid — resolve it before the
        // profile merge consumes `raw`.
        let deploy = raw.deploy.take().map(DeployRaw::resolve).transpose()?;
        // A file that configures no local app — e.g. a deploy-only node whose
        // hako.toml is just a `[deploy]` table — has no image to require. Only
        // resolve (and demand an image) when the file actually describes an app,
        // or a profile was explicitly requested.
        let app = if profile.is_some() || raw.has_app_content() {
            Some(raw.resolve(profile)?)
        } else {
            None
        };
        let default_container = app
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| Self::default().default_container);
        Ok(Self {
            default_container,
            app,
            deploy,
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

    /// Reserved: a `[deploy]` table is the node's deploy target, NOT a profile.
    /// This explicit field must precede the `flatten` below, or serde would sweep
    /// `[deploy]` into `profiles` and silently drop its keys.
    deploy: Option<DeployRaw>,

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

#[derive(Deserialize, Debug, Default, Clone)]
struct DeployRaw {
    container: Option<String>,
    branch: Option<String>,
    run: Option<RunRaw>,
    grace_secs: Option<u64>,
    network: Option<String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    volumes: Vec<String>,
}

impl DeployRaw {
    fn resolve(self) -> io::Result<DeployConfig> {
        let required = |field: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("hako.toml [deploy]: `{field}` is required"),
            )
        };
        Ok(DeployConfig {
            container: self
                .container
                .filter(|s| !s.is_empty())
                .ok_or_else(|| required("container"))?,
            branch: self
                .branch
                .filter(|s| !s.is_empty())
                .ok_or_else(|| required("branch"))?,
            run: self.run.map(RunSpec::from).map(validate_run).transpose()?,
            grace_secs: self.grace_secs.unwrap_or(10),
            network: self.network,
            ports: self.ports,
            volumes: self.volumes,
        })
    }
}

/// Reject a `[deploy].run` that would exec nothing (`run = ""` or `run = []`),
/// which under `restart = always` would spin an instant-exit respawn loop.
fn validate_run(run: RunSpec) -> io::Result<RunSpec> {
    let empty = match &run {
        RunSpec::Shell(s) => s.trim().is_empty(),
        RunSpec::Exec(v) => v.is_empty() || v[0].trim().is_empty(),
    };
    if empty {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hako.toml [deploy]: `run` must not be empty",
        ));
    }
    Ok(run)
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
    /// Whether this file describes a local app (so an `image` is expected).
    /// False only for a file with NO app fields at all — an empty file or a
    /// node-local `[deploy]`-only config. Any app field being set means the user
    /// intended an app, so resolution proceeds and a missing `image` still errors
    /// (rather than silently dropping the partial config). `[deploy]` and profiles
    /// are excluded on purpose: they aren't base-app content.
    fn has_app_content(&self) -> bool {
        self.image.is_some()
            || self.name.is_some()
            || !self.setup.is_empty()
            || self.run.is_some()
            || self.bin.is_some()
            || self.user.is_some()
            || !self.env.is_empty()
            || !self.env_pass.is_empty()
            || self.autocommit.is_some()
            || self.workspace.is_some()
            || self.display.is_some()
    }

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

    fn parse_deploy(text: &str) -> io::Result<Option<DeployConfig>> {
        let raw: AppRaw = toml::from_str(text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        raw.deploy.map(DeployRaw::resolve).transpose()
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
    fn deploy_table_parses() {
        let d = parse_deploy(
            r#"
image = "x"
[deploy]
container = "app"
branch = "main"
ports = ["8080:80"]
"#,
        )
        .unwrap()
        .expect("deploy present");
        assert_eq!(d.container, "app");
        assert_eq!(d.branch, "main");
        assert_eq!(d.grace_secs, 10); // default
        assert_eq!(d.ports, vec!["8080:80"]);
        assert!(d.volumes.is_empty());
        assert!(d.network.is_none());
        assert!(d.run.is_none()); // absent → falls back to the container shell
    }

    #[test]
    fn deploy_run_command_parses_both_forms() {
        let shell =
            parse_deploy("[deploy]\ncontainer=\"app\"\nbranch=\"main\"\nrun=\"python -m app\"\n")
                .unwrap()
                .unwrap();
        assert_eq!(
            shell.run.unwrap().argv(),
            vec!["/bin/sh", "-c", "python -m app"]
        );

        let exec = parse_deploy(
            "[deploy]\ncontainer=\"app\"\nbranch=\"main\"\nrun=[\"python\",\"-m\",\"app\"]\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(exec.run.unwrap().argv(), vec!["python", "-m", "app"]);
    }

    #[test]
    fn deploy_requires_container_and_branch() {
        assert!(parse_deploy("[deploy]\ncontainer = \"app\"").is_err()); // no branch
        assert!(parse_deploy("[deploy]\nbranch = \"main\"").is_err()); // no container
    }

    #[test]
    fn deploy_is_reserved_not_a_selectable_profile() {
        // `deploy` is an explicit field, so `[deploy]` is NOT swept into the
        // profile catch-all and can't be selected as `--profile deploy`.
        let text = "image = \"x\"\n[deploy]\ncontainer = \"app\"\nbranch = \"main\"";
        let err = parse_with(text, Some("deploy")).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("profile"),
            "expected an unknown-profile error, got: {err}"
        );
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
    fn deploy_only_hako_toml_needs_no_image() {
        // A push-to-deploy receiver's hako.toml is just a `[deploy]` table — it
        // configures no local app, so `image` must not be required.
        let d = tempfile::TempDir::new().unwrap();
        std::fs::write(
            d.path().join("hako.toml"),
            "[deploy]\ncontainer=\"app\"\nbranch=\"main\"\nrun=\"serve\"\n",
        )
        .unwrap();
        let c = Config::load(d.path()).unwrap();
        assert!(
            c.app.is_none(),
            "deploy-only file must not synthesize an app"
        );
        assert_eq!(c.default_container, "hako");
        let deploy = c.deploy.expect("deploy table present");
        assert_eq!(deploy.container, "app");
        assert_eq!(deploy.run.unwrap().argv(), vec!["/bin/sh", "-c", "serve"]);
    }

    #[test]
    fn app_fields_without_image_still_error() {
        // A file that expresses app intent through ANY app field but omits
        // `image` is a user mistake — surface it, don't silently drop the config.
        for body in [
            "run = \"echo hi\"\n",
            "name = \"myproj\"\n",
            "env = { FOO = \"bar\" }\n",
            "user = \"bob\"\n",
        ] {
            let d = tempfile::TempDir::new().unwrap();
            std::fs::write(d.path().join("hako.toml"), body).unwrap();
            let err = Config::load(d.path()).expect_err(body);
            assert!(err.to_string().contains("image"), "for {body:?}: {err}");
        }
    }

    #[test]
    fn deploy_run_must_not_be_empty() {
        // `run = ""` / `run = []` would exec nothing and, under restart=always,
        // spin an instant-exit respawn loop — reject at parse time.
        for run in ["run = \"\"", "run = \"   \"", "run = []"] {
            let toml = format!("[deploy]\ncontainer=\"app\"\nbranch=\"main\"\n{run}\n");
            let err = parse_deploy(&toml).unwrap_err();
            assert!(err.to_string().contains("run"), "for {run:?}: {err}");
        }
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
