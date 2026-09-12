//! Per-tenant `podman pod` provisioning and lifecycle (ADR-0009,
//! ADR-0010, M5-7). No `wkp-git`-style "no `Command::new`" rule applies
//! here -- that rule is specifically about `git`; `podman` is this
//! module's own, equally deliberate, subprocess boundary instead.
//!
//! A tenant's pod is one `podman pod` (named `wkp-tenant-<slug>`)
//! holding one container that runs `wkp-hub serve-tenant --tenant
//! <slug>` (M5-7 PR 2/4) against that tenant's bare repo, bind-mounted
//! in from the same fixed `repos_root/<slug>.git` path `wkp-shell`'s
//! SSH-path git-shell and the front door's HTTPS path both already use
//! (M5-3/M5-4) -- the pod's own container filesystem is otherwise
//! ephemeral, so the repo has to live outside it.
//!
//! Every pod joins the same user-defined network ([`NETWORK_NAME`]),
//! with the tenant's own slug as that container's network alias --
//! podman's built-in DNS for user-defined networks resolves
//! `http://<slug>:8080/...` directly from anything else on the same
//! network (the front door, once M5-7 PR 4/4 joins it too), with no
//! per-tenant host-port allocation to track (ADR-0010's whole point).
//! [`SERVE_PORT`] can be the same fixed value for every tenant because
//! each pod has its own isolated network namespace -- tenant A's 8080
//! and tenant B's 8080 never collide, only reachable via each pod's
//! own alias hostname, never a shared host port.
//!
//! **Verified by hand against real podman, with one gap**: pod
//! creation, the `--entrypoint` override (the image's own default
//! entrypoint, `deploy/hub/entrypoint.sh`, unconditionally starts
//! `sshd` -- it silently ignores whatever command a `podman run`
//! passes after the image name otherwise), the bind mount, and the
//! resulting container actually serving the real git smart-HTTP
//! protocol all confirmed working end to end. **Not verified in this
//! sandbox**: [`NETWORK_NAME`]'s user-defined network and
//! `--network-alias` resolution -- this sandbox's rootless podman has
//! no systemd user session/D-Bus for `aardvark-dns` (the same class of
//! limitation `deploy/hub/test-ssh-integration.sh`'s own comments
//! already document for `--network host`), so `start_pod` reliably
//! fails here specifically at the network-alias step. A real CI runner
//! or production host is expected not to have this constraint; M5-7 PR
//! 4/4's own end-to-end test is what actually proves the alias-based
//! addressing works, not this module's unit tests.
//!
//! **Pluggable since ADR-0012.** Every call site (`handle_git_http`'s
//! on-demand cold start, the `start-pod`/`stop-pod`/`reap-idle-pods`
//! CLI subcommands, the reaper) depends on the [`PodOrchestrator`]
//! trait, not on `podman` directly -- ADR-0009 required the mechanism
//! for reaching a tenant's pod to work the same way under Kubernetes
//! later, without a rewrite, and a direct `Command::new("podman")` at
//! every call site would have meant touching all of them again to add
//! a second backend. [`PodmanOrchestrator`] is the only implementation
//! today; see [`orchestrator`]'s own doc comment for how a Kubernetes
//! backend slots in later.

use std::path::Path;
use std::process::Command;

/// The mechanism a tenant's pod is actually started, stopped, and
/// queried through -- see the module doc comment (ADR-0012) for why
/// this exists instead of every call site running `podman` directly.
pub trait PodOrchestrator: Send + Sync {
    fn start_pod(&self, image: &str, repos_root: &Path, tenant_slug: &str) -> Result<(), String>;
    fn stop_pod(&self, tenant_slug: &str) -> Result<(), String>;
}

/// Selects this process's [`PodOrchestrator`] from
/// `WKP_HUB_POD_ORCHESTRATOR` (default `"podman"`, ADR-0012). Every
/// production call site resolves this once and holds onto the result,
/// rather than re-reading the environment variable per call.
///
/// `"kubernetes"` is a reserved name, not a working backend -- there is
/// no Kubernetes client code in this repo yet (ADR-0012 deliberately
/// left that implementation for later). Selecting it fails fast at
/// startup with a clear message; it does not silently fall back to
/// podman, and there is no stub implementation pretending to work.
pub fn orchestrator() -> Result<Box<dyn PodOrchestrator>, String> {
    match std::env::var("WKP_HUB_POD_ORCHESTRATOR") {
        Err(std::env::VarError::NotPresent) => Ok(Box::new(PodmanOrchestrator)),
        Ok(v) if v == "podman" => Ok(Box::new(PodmanOrchestrator)),
        Ok(v) if v == "kubernetes" => Err(
            "WKP_HUB_POD_ORCHESTRATOR=kubernetes is reserved (ADR-0012) but has no \
             implementation yet"
                .to_string(),
        ),
        Ok(other) => Err(format!(
            "unknown WKP_HUB_POD_ORCHESTRATOR {other:?} (expected \"podman\")"
        )),
        Err(e) => Err(format!("WKP_HUB_POD_ORCHESTRATOR: {e}")),
    }
}

/// The only [`PodOrchestrator`] implemented so far: shells out to
/// `podman` directly, exactly as this module did before ADR-0012.
pub struct PodmanOrchestrator;

impl PodOrchestrator for PodmanOrchestrator {
    fn start_pod(&self, image: &str, repos_root: &Path, tenant_slug: &str) -> Result<(), String> {
        start_pod(image, repos_root, tenant_slug)
    }

    fn stop_pod(&self, tenant_slug: &str) -> Result<(), String> {
        stop_pod(tenant_slug)
    }
}

/// The shared user-defined network every tenant pod (and, from M5-7 PR
/// 4/4 on, the front door itself) joins.
pub const NETWORK_NAME: &str = "wkp-hub-tenants";

/// Fixed for every tenant's own pod -- safe precisely because each pod
/// has its own network namespace; see the module doc comment. `pub`:
/// `http.rs`'s own proxy (M5-7 PR 4) needs the exact same value to
/// address a tenant's pod.
pub const SERVE_PORT: u16 = 8080;

fn pod_name(tenant_slug: &str) -> String {
    format!("wkp-tenant-{tenant_slug}")
}

fn container_name(tenant_slug: &str) -> String {
    format!("{}-serve", pod_name(tenant_slug))
}

/// The bind-mount argument for a tenant's bare repo -- extracted as its
/// own pure function so a test can assert on the exact string without
/// actually running `podman` (this module's own tests have no podman
/// dependency at all; a real pod's actual behavior is proven by
/// `deploy/hub/test-pod-lifecycle.sh`, the same documented split M5-5's
/// container work already established between fast unit tests and one
/// real end-to-end script).
fn repo_mount_arg(repos_root: &Path, tenant_slug: &str) -> String {
    format!(
        "{}:/srv/wkp-hub/repos/{tenant_slug}.git:Z",
        repos_root.join(format!("{tenant_slug}.git")).display()
    )
}

fn run_podman(args: &[&str]) -> Result<(), String> {
    let output = Command::new("podman")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run podman {args:?}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "podman {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Idempotent: a network that already exists is not an error.
fn ensure_network() -> Result<(), String> {
    let exists = Command::new("podman")
        .args(["network", "exists", NETWORK_NAME])
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if exists {
        return Ok(());
    }
    run_podman(&["network", "create", NETWORK_NAME])
}

/// `Ok(true)` only for a pod podman itself reports as actually
/// running -- a stopped, paused, or nonexistent pod (podman's own
/// `pod inspect` fails outright for a name it doesn't know) are both
/// `Ok(false)`, since [`start_pod`]'s caller only cares about "is
/// there already a live one to reuse."
fn pod_is_running(pod: &str) -> Result<bool, String> {
    let output = Command::new("podman")
        .args(["pod", "inspect", pod, "--format", "{{.State}}"])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "Running")
}

/// Starts a tenant's pod if it isn't already running -- safe to call
/// against a tenant that's already up (a no-op) or one that crashed
/// with a stale, stopped pod left behind under the same name (removed
/// and recreated fresh; podman refuses to create a pod whose name is
/// already taken, even by a stopped one).
///
/// Does not touch the control plane's own `pod_running` bookkeeping --
/// the caller (the front door's cold-start path, M5-7 PR 4/4, or the
/// admin CLI) calls [`crate::control_plane::record_pod_started`]
/// itself once this returns `Ok`, the same separation of concerns
/// `crate::control_plane`'s own doc comments already establish (this
/// module talks to the container runtime, that one only ever records
/// what happened).
fn start_pod(image: &str, repos_root: &Path, tenant_slug: &str) -> Result<(), String> {
    let pod = pod_name(tenant_slug);
    if pod_is_running(&pod)? {
        return Ok(());
    }
    ensure_network()?;
    let _ = run_podman(&["pod", "rm", "-f", &pod]);
    // `--network-alias` belongs on the *pod*, not the container joining
    // it -- found by hand: podman rejects `--network-alias` on `podman
    // run --pod ...` outright ("network cannot be configured when it
    // is shared with a pod"), since every container in a pod shares
    // one network namespace, so the alias is the pod's own property.
    run_podman(&[
        "pod",
        "create",
        "--name",
        &pod,
        "--network",
        NETWORK_NAME,
        "--network-alias",
        tenant_slug,
    ])?;

    let mount = repo_mount_arg(repos_root, tenant_slug);
    let port = SERVE_PORT.to_string();
    // `--entrypoint`: the image's own default entrypoint
    // (`deploy/hub/entrypoint.sh`) unconditionally execs `wkp-hub serve`
    // (the *front-door* server mode) -- found by hand while testing this
    // against the real image, it silently ignores whatever command a
    // `podman run` passes after the image name instead of running it.
    // A tenant's own pod needs `wkp-hub` invoked directly instead, in
    // its single-tenant serving mode, not the front door's.
    run_podman(&[
        "run",
        "-d",
        "--pod",
        &pod,
        "--name",
        &container_name(tenant_slug),
        "--entrypoint",
        "/usr/local/bin/wkp-hub",
        "-v",
        &mount,
        image,
        "serve-tenant",
        "--tenant",
        tenant_slug,
        "--port",
        &port,
    ])
}

/// Tears a tenant's pod down entirely (not just stopped -- removed, so
/// a later [`start_pod`] recreates it fresh rather than reusing
/// anything). A pod that was never started, or already gone, is not an
/// error: `podman pod rm -f` on an unknown name is itself already a
/// no-op-shaped success from this function's own caller's point of
/// view (the reaper sweeping a tenant whose pod-state row says
/// "running" but which crashed out from under it, say).
fn stop_pod(tenant_slug: &str) -> Result<(), String> {
    let pod = pod_name(tenant_slug);
    // `pod rm -f` on a pod that doesn't exist at all returns a nonzero
    // exit and a "no such pod" stderr line -- treated the same as
    // success here, per this function's own doc comment.
    let _ = run_podman(&["pod", "rm", "-f", &pod]);
    Ok(())
}

/// One reaper sweep: stops the pod for every tenant
/// [`crate::control_plane::find_idle_running_tenants`] reports idle,
/// recording each in the control plane once actually stopped. Returns
/// the slugs it reaped. One tenant's pod failing to stop is logged and
/// skipped, not fatal to the sweep -- a stuck pod for tenant A must
/// never stop the reaper from freeing an idle tenant B.
pub fn reap_idle(
    client: &mut postgres::Client,
    idle_for: time::Duration,
    orchestrator: &dyn PodOrchestrator,
) -> Result<Vec<String>, String> {
    let idle_tenants = crate::control_plane::find_idle_running_tenants(client, idle_for)
        .map_err(|e| e.to_string())?;
    let mut reaped = Vec::new();
    for tenant in idle_tenants {
        if let Err(e) = orchestrator.stop_pod(&tenant.slug) {
            eprintln!(
                "wkp-hub: reap: failed to stop pod for tenant {}: {e}",
                tenant.slug
            );
            continue;
        }
        if let Err(e) = crate::control_plane::record_pod_stopped(client, tenant.id) {
            eprintln!(
                "wkp-hub: reap: pod stopped but failed to record it for tenant {}: {e}",
                tenant.slug
            );
            continue;
        }
        reaped.push(tenant.slug);
    }
    Ok(reaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_and_container_names_are_derived_consistently() {
        assert_eq!(pod_name("acme"), "wkp-tenant-acme");
        assert_eq!(container_name("acme"), "wkp-tenant-acme-serve");
    }

    #[test]
    fn repo_mount_arg_binds_the_fixed_per_tenant_repo_path() {
        let mount = repo_mount_arg(Path::new("/srv/wkp-hub/repos"), "acme");
        assert_eq!(
            mount,
            "/srv/wkp-hub/repos/acme.git:/srv/wkp-hub/repos/acme.git:Z"
        );
    }

    // No other test in this crate reads or writes
    // `WKP_HUB_POD_ORCHESTRATOR`, so mutating it here (one test, one
    // sequential block) doesn't race with anything else `cargo test`
    // runs in parallel.
    #[test]
    fn orchestrator_selects_podman_by_default_and_rejects_unknown_values() {
        std::env::remove_var("WKP_HUB_POD_ORCHESTRATOR");
        assert!(orchestrator().is_ok(), "unset must default to podman");

        std::env::set_var("WKP_HUB_POD_ORCHESTRATOR", "podman");
        assert!(orchestrator().is_ok(), "explicit podman must be accepted");

        std::env::set_var("WKP_HUB_POD_ORCHESTRATOR", "kubernetes");
        // `Box<dyn PodOrchestrator>` isn't `Debug`, so `unwrap_err`
        // (which needs the `Ok` side to be `Debug` for its own panic
        // message) doesn't work here -- match instead.
        match orchestrator() {
            Err(err) => assert!(
                err.contains("kubernetes") && err.contains("ADR-0012"),
                "kubernetes must fail fast with a clear reserved-name message, got: {err}"
            ),
            Ok(_) => panic!("kubernetes must not silently select a working orchestrator"),
        }

        std::env::set_var("WKP_HUB_POD_ORCHESTRATOR", "nonsense");
        assert!(
            orchestrator().is_err(),
            "an unrecognized value must not silently fall back to podman"
        );

        std::env::remove_var("WKP_HUB_POD_ORCHESTRATOR");
    }
}
