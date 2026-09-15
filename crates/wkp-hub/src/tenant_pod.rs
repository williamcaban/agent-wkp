//! Per-tenant `podman pod` provisioning and lifecycle (ADR-0009,
//! ADR-0010, M5-7). No `wkp-git`-style "no `Command::new`" rule applies
//! here -- that rule is specifically about `git`; `podman` is this
//! module's own, equally deliberate, subprocess boundary instead.
//!
//! A tenant's pod is one `podman pod` (named `wkp-tenant-<slug>`)
//! holding **two containers**, both with the tenant's *entire* storage
//! directory (`repos_root/<slug>`, ADR-0014/issue #165 -- bare repo
//! *and* `index.db`, not the bare repo alone) bind-mounted in at the
//! same `/srv/wkp-hub/repos/<slug>` path the front door's HTTPS path
//! already uses (M5-3/M5-4) -- the pod's own container filesystem is
//! otherwise ephemeral, so this has to live outside it. Mounting the
//! whole per-tenant directory rather than just the bare repo is what
//! lets both containers see the same `index.db`, and keeps
//! `index_tenant`'s temp-file-then-rename atomicity intact (temp file
//! and rename target on the one filesystem this single mount
//! provides, not split across a mount boundary):
//!
//! - The **serving** container (`wkp-hub serve-tenant --tenant <slug>`,
//!   M5-7 PR 2/4): the only one reachable over the network, via the
//!   pod's shared network-alias address.
//! - The **indexing** container (`wkp-hub index-worker --tenant
//!   <slug>`, issue #159): polls the same bind-mounted repo for a
//!   changed `HEAD` and re-indexes on change, but can never itself make
//!   or accept a network connection -- see below.
//!
//! **Why two containers, not one (issue #159, correcting ADR-0009's own
//! assumption):** ADR-0009 originally assumed that putting the indexing
//! step in its own container in the same pod would satisfy design 8.3's
//! "indexing worker... with no network" line "for free," because the
//! pod's network namespace would do the sandboxing. That assumption is
//! wrong, not just unconfirmed: every container in a pod shares exactly
//! one network namespace (this is *why* `--network-alias` below has to
//! be a pod-create flag, not a per-container one -- podman rejects it
//! outright on `podman run --pod ...`, "network cannot be configured
//! when it is shared with a pod"), so a sibling container in the same
//! pod is, by default, exactly as network-reachable as the one serving
//! git traffic. The property this module actually needs comes from two
//! independent mechanisms applied to the indexing container alone,
//! both verified by hand against real podman (a second, unrestricted
//! sibling container in the same pod could still reach a third
//! container's listener; the indexing container, with both of these
//! applied, could not even call `socket()`):
//!
//! - `--network none`: gives that one container its own network
//!   namespace (loopback only) instead of joining the pod's shared one.
//!   Podman-specific -- there is no Kubernetes equivalent (the
//!   Kubernetes Pod API has no per-container opt-out of the Pod's
//!   shared network namespace at all), so this is defense-in-depth on
//!   today's backend, not something a future `KubernetesOrchestrator`
//!   (issue #141) can rely on.
//! - [`SECCOMP_NO_NETWORK_PROFILE`]: a seccomp filter denying every
//!   syscall that creates or uses a socket. This one *does* have a
//!   direct Kubernetes equivalent (`securityContext.seccompProfile`,
//!   per-container, GA since 1.19) -- it is the mechanism a future
//!   Kubernetes backend must carry over for this property to hold
//!   there too, not `--network none`.
//!
//! Every pod joins the same user-defined network ([`NETWORK_NAME`]),
//! with the tenant's own slug as the *pod's* network alias -- podman's
//! built-in DNS for user-defined networks resolves
//! `http://<slug>:8080/...` directly from anything else on the same
//! network (the front door, once M5-7 PR 4/4 joins it too), with no
//! per-tenant host-port allocation to track (ADR-0010's whole point).
//! [`SERVE_PORT`] can be the same fixed value for every tenant because
//! each pod has its own isolated network namespace -- tenant A's 8080
//! and tenant B's 8080 never collide, only reachable via each pod's
//! own alias hostname, never a shared host port. The indexing
//! container never listens on anything, so it needs no alias and no
//! port at all.
//!
//! **Verified by hand against real podman, with one gap**: pod
//! creation, the `--entrypoint` override (the image's own default
//! entrypoint, `deploy/hub/entrypoint.sh`, execs `wkp-hub serve` --
//! the front door's own mode -- unconditionally, silently ignoring
//! whatever command a `podman run` passes after the image name
//! otherwise, ADR-0011), the bind mount, and the resulting containers
//! actually serving the real git smart-HTTP protocol (serving
//! container) and picking up a real push (indexing container) all
//! confirmed working end to end. **Not verified in this sandbox**:
//! [`NETWORK_NAME`]'s user-defined network and `--network-alias`
//! resolution -- this sandbox's rootless podman has no systemd user
//! session/D-Bus for `aardvark-dns` (the same class of limitation
//! `deploy/hub/test-ssh-integration.sh`'s own comments already
//! document for `--network host`), so `start_pod` reliably fails here
//! specifically at the network-alias step. A real CI runner or
//! production host is expected not to have this constraint; M5-7 PR
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

/// Denies every syscall that creates or uses a network socket (see the
/// module doc comment, issue #159); applied to the indexing container
/// only, via `podman run --security-opt seccomp=<path>`. The JSON lives
/// in `deploy/hub/seccomp-no-network.json` (reviewable as a real file,
/// not a Rust string literal) and is embedded at compile time so
/// [`ensure_seccomp_profile`] can write it out without a separate
/// deployment/image-baking step.
const SECCOMP_NO_NETWORK_PROFILE: &str =
    include_str!("../../../deploy/hub/seccomp-no-network.json");

fn pod_name(tenant_slug: &str) -> String {
    format!("wkp-tenant-{tenant_slug}")
}

fn serve_container_name(tenant_slug: &str) -> String {
    format!("{}-serve", pod_name(tenant_slug))
}

fn index_container_name(tenant_slug: &str) -> String {
    format!("{}-index", pod_name(tenant_slug))
}

/// Writes [`SECCOMP_NO_NETWORK_PROFILE`] out to a path under
/// `repos_root` -- the one host-side directory this module already
/// knows is reachable by whatever `podman` instance actually creates
/// tenant pods (the same one `repo_mount_arg`'s bind-mount sources
/// already rely on being resolvable there), so this needs no separate
/// configuration surface for where the profile file lives. Rewritten
/// unconditionally on every call: this only runs at pod-start, not a
/// hot path, and guards against a stale or hand-edited copy drifting
/// from what this binary actually embeds.
fn ensure_seccomp_profile(repos_root: &Path) -> Result<std::path::PathBuf, String> {
    let path = repos_root.join(".wkp-hub-seccomp-no-network.json");
    std::fs::write(&path, SECCOMP_NO_NETWORK_PROFILE)
        .map_err(|e| format!("failed to write seccomp profile to {}: {e}", path.display()))?;
    Ok(path)
}

/// The bind-mount argument for a tenant's bare repo -- extracted as its
/// own pure function so a test can assert on the exact string without
/// actually running `podman` (this module's own tests have no podman
/// dependency at all; a real pod's actual behavior is proven by
/// `deploy/hub/test-pod-lifecycle.sh`, the same documented split M5-5's
/// container work already established between fast unit tests and one
/// real end-to-end script).
/// Binds a tenant's *entire* storage directory (ADR-0014, issue #165) --
/// not just its bare repo -- so both this tenant's serving and indexing
/// containers see the same `index.db`, not only the same repo. Mounting
/// a whole per-tenant directory this way (rather than one mount for the
/// repo and a second for the index file) also keeps `index_tenant`'s
/// temp-file-then-rename atomicity intact: both the temp file and its
/// rename target stay on the one filesystem this single mount provides.
fn repo_mount_arg(repos_root: &Path, tenant_slug: &str) -> String {
    format!(
        "{}:/srv/wkp-hub/repos/{tenant_slug}:Z",
        repos_root.join(tenant_slug).display()
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
        &serve_container_name(tenant_slug),
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
    ])?;

    // The indexing container: same repo bind-mount, but no listener, no
    // alias, and -- see the module doc comment (issue #159) -- no way
    // to touch the network at all, applied two independent ways since
    // only one of them (the seccomp profile) has a Kubernetes
    // equivalent for a future `KubernetesOrchestrator` to carry over.
    let seccomp_path = ensure_seccomp_profile(repos_root)?;
    run_podman(&[
        "run",
        "-d",
        "--pod",
        &pod,
        "--name",
        &index_container_name(tenant_slug),
        "--network",
        "none",
        "--security-opt",
        &format!("seccomp={}", seccomp_path.display()),
        "--entrypoint",
        "/usr/local/bin/wkp-hub",
        "-v",
        &mount,
        image,
        "index-worker",
        "--tenant",
        tenant_slug,
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
        assert_eq!(serve_container_name("acme"), "wkp-tenant-acme-serve");
        assert_eq!(index_container_name("acme"), "wkp-tenant-acme-index");
    }

    #[test]
    fn repo_mount_arg_binds_the_whole_per_tenant_storage_directory() {
        let mount = repo_mount_arg(Path::new("/srv/wkp-hub/repos"), "acme");
        assert_eq!(mount, "/srv/wkp-hub/repos/acme:/srv/wkp-hub/repos/acme:Z");
    }

    #[test]
    fn seccomp_profile_is_valid_json_and_denies_the_full_socket_syscall_surface() {
        // Parsed here (not just embedded) so a syntax error in
        // deploy/hub/seccomp-no-network.json fails a fast unit test
        // instead of only surfacing when `start_pod` runs against real
        // podman. Real network-blocking behavior is verified by hand
        // against podman (see the module doc comment) and by
        // deploy/hub/test-tenant-indexing-isolation.sh, not by this
        // test -- this only locks down the profile's own shape.
        let parsed: serde_json::Value =
            serde_json::from_str(SECCOMP_NO_NETWORK_PROFILE).expect("profile must be valid JSON");
        let denied: Vec<&str> = parsed["syscalls"][0]["names"]
            .as_array()
            .expect("syscalls[0].names must be an array")
            .iter()
            .map(|v| v.as_str().expect("syscall name must be a string"))
            .collect();
        for must_deny in [
            "socket", "connect", "bind", "listen", "accept", "sendto", "recvfrom",
        ] {
            assert!(
                denied.contains(&must_deny),
                "seccomp profile must deny {must_deny}, denies: {denied:?}"
            );
        }
        assert_eq!(parsed["syscalls"][0]["action"], "SCMP_ACT_ERRNO");
        assert_eq!(parsed["defaultAction"], "SCMP_ACT_ALLOW");
    }

    #[test]
    fn ensure_seccomp_profile_writes_the_embedded_profile_under_repos_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = ensure_seccomp_profile(dir.path()).expect("write profile");
        assert_eq!(path, dir.path().join(".wkp-hub-seccomp-no-network.json"));
        let written = std::fs::read_to_string(&path).expect("read written profile");
        assert_eq!(written, SECCOMP_NO_NETWORK_PROFILE);
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
