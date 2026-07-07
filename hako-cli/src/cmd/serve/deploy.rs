//! Push-to-deploy reconcile (`docs/push-to-deploy.md` P1-1).
//!
//! When a peer's push advances a branch that this node's `[deploy]` config
//! tracks, the receiving daemon reconciles the running workload: stop the old
//! instance, start a new one at the **just-pushed tree** with the run shape the
//! **receiver** declared (`[deploy]` in its own `hako.toml` — never the pushed
//! tree, so a push can't dictate what code runs here). The reconcile runs after
//! the push's mutation lock is dropped (spawning a container must not hold the
//! workspace lock, #78) and its log is returned to the pusher, so
//! `hako peer push` prints a deploy summary.

use crate::cmd::Ctx;
use crate::DOT_HAKO;
use hako::DeployConfig;
use hako_runtime::{instances, Network, RestartPolicy, VolumeMount};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serializes reconciles so two near-simultaneous pushes to the same target
/// can't race two stop/start sequences (the mutation lock is already dropped
/// here by design, so nothing else covers this). One global lock is enough:
/// deploys are infrequent and a node has a single `[deploy]` target.
static DEPLOY_LOCK: Mutex<()> = Mutex::new(());

/// Whether an advance of `(container, branch)` is this node's deploy target.
pub fn matches(deploy: &DeployConfig, container: &str, branch: &str) -> bool {
    deploy.container == container && deploy.branch == branch
}

/// Reconcile the workload for a deploy target whose branch just advanced: stop
/// the old instance(s), then start a new one at the branch's new tip. Returns a
/// human-readable log (appended to the push response). Never panics — every
/// failure is reported in the log, so a botched deploy still answers the pusher.
pub fn reconcile(ctx: &Ctx<'_>, deploy: &DeployConfig) -> String {
    // Serialize against a concurrent reconcile to the same node.
    let _guard = DEPLOY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime_root = ctx.workdir.join(DOT_HAKO);
    let mut log = String::new();
    let _ = write!(log, "\ndeploy {}:{}", deploy.container, deploy.branch);

    // 1. Find the running instances of this container and ask them to stop.
    let old: Vec<_> = instances::list(&runtime_root)
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.config.container == deploy.container && i.is_running())
        .collect();
    for inst in &old {
        let _ = instances::stop(&runtime_root, &inst.id, false); // graceful SIGTERM
        let _ = write!(log, "\n  stopping {}", short(&inst.id));
    }

    // 2. Drain: wait up to grace_secs for them to exit, then SIGKILL survivors.
    if !old.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(deploy.grace_secs);
        while Instant::now() < deadline && any_running(&runtime_root, &old) {
            std::thread::sleep(Duration::from_millis(200));
        }
        for inst in &old {
            if is_running(&runtime_root, &inst.id) {
                let _ = instances::stop(&runtime_root, &inst.id, true); // SIGKILL
                let _ = write!(log, "\n  killed {} (grace expired)", short(&inst.id));
            }
            let _ = instances::remove(&runtime_root, &inst.id, true); // reap state
        }
    }

    // 3. Start the new workload at the advanced branch (its new tip is the tree
    //    the push just made durable). Supervised (restart = always) so the
    //    service stays up; the pinned-root restart means a later `revert` push
    //    reconciles it, never a silent re-resolve.
    let repo = match ctx.state.open_container(&deploy.container) {
        Ok(r) => r,
        Err(e) => {
            let _ = write!(log, "\n  FAILED to open container: {e}");
            return log;
        }
    };
    // A deploy with no `run` command has nothing meaningful to launch — starting
    // the container's default shell under restart=always would just spin an
    // instant-exit respawn loop. Stop the old workload (done above) and report it,
    // rather than boot a busy-loop.
    let command = match deploy.run.as_ref() {
        Some(r) => r.argv(),
        None => {
            let _ = write!(
                log,
                "\n  not started: [deploy] has no `run` command (set one to launch a service)"
            );
            return log;
        }
    };
    // Surface bad specs instead of silently dropping them: a typo'd volume or
    // network would otherwise quietly bring the workload up wrong (no mount, or
    // no connectivity — and with ports unpublished, `host` is the only way to
    // serve, so a downgrade to isolated makes the service unreachable).
    let mut volumes: Vec<VolumeMount> = Vec::new();
    for v in &deploy.volumes {
        match VolumeMount::parse(v) {
            Ok(m) => volumes.push(m),
            Err(e) => {
                let _ = write!(log, "\n  WARNING skipping bad volume {v:?}: {e}");
            }
        }
    }
    let network = match deploy.network.as_deref() {
        Some(s) => Network::parse(s).unwrap_or_else(|e| {
            let _ = write!(log, "\n  WARNING {e}; falling back to isolated network");
            Network::Isolated
        }),
        None => Network::Isolated,
    };
    // Port publishing (`-p`) isn't wired yet (push-to-deploy P0-1 slice 2); a
    // `--network host` deploy can still serve on host ports meanwhile.
    if !deploy.ports.is_empty() {
        let _ = write!(
            log,
            "\n  note: [deploy].ports not yet published (use network=\"host\" for now)"
        );
    }

    match hako_runtime::transform::run_container_detached(
        &repo,
        &deploy.branch,
        Some(command),
        &volumes,
        network,
        RestartPolicy::Always,
    ) {
        Ok(id) => {
            let _ = write!(log, "\n  started {} (restart=always)", short(&id));
        }
        Err(e) => {
            let _ = write!(log, "\n  FAILED to start: {e}");
        }
    }
    log
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn is_running(runtime_root: &std::path::Path, id: &str) -> bool {
    instances::get(runtime_root, id)
        .map(|i| i.is_running())
        .unwrap_or(false)
}

fn any_running(runtime_root: &std::path::Path, insts: &[instances::Instance]) -> bool {
    insts.iter().any(|i| is_running(runtime_root, &i.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako::RunSpec;

    fn cfg(container: &str, branch: &str) -> DeployConfig {
        DeployConfig {
            container: container.into(),
            branch: branch.into(),
            run: Some(RunSpec::Shell("server".into())),
            grace_secs: 10,
            network: Some("host".into()),
            ports: vec![],
            volumes: vec![],
        }
    }

    #[test]
    fn matches_only_the_configured_target() {
        let d = cfg("app", "main");
        assert!(matches(&d, "app", "main"));
        assert!(!matches(&d, "app", "dev")); // wrong branch
        assert!(!matches(&d, "other", "main")); // wrong container
    }
}
