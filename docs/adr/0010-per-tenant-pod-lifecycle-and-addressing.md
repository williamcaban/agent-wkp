# ADR-0010: Per-tenant pod lifecycle, addressing, and repo persistence

Status: accepted
Date: 2026-09-10
Design sections affected: 8.1, 8.2, 8.3
Depends on: ADR-0009 (decided *that* isolation is a pod per tenant, reached over `git http-backend`; this ADR decides how those pods are started, stopped, found, and kept from losing data)

## Context

ADR-0009 decided per-tenant hard isolation is a container-runtime pod per tenant, with the front door proxying git's smart-HTTP protocol to that pod's own `git http-backend` over the network, and explicitly deferred "the pod's actual lifecycle policy... and its provisioning path" to a follow-up (issue #110, M5-7). This ADR is that follow-up's design pass, required by the issue itself before any implementation.

Three questions had no existing answer to build on: how long a tenant's pod runs relative to its actual traffic; what code the pod itself runs; and how the front door finds a specific tenant's pod on the network without relying on a mechanism (like `podman exec`) ADR-0009 already ruled out for the same reason (no Kubernetes equivalent).

## Decisions

**1. Lifecycle: on-demand by default, with a per-tenant override to stay always-warm.** A tenant's pod is not created at `tenant create` time and does not run continuously by default -- the front door starts it on that tenant's first connection since the pod last stopped, and a reaper stops it again after a configurable idle period. A new `tenants.always_warm` column (`BOOLEAN NOT NULL DEFAULT false`) lets an operator (or, later, a plan tier) pin a specific tenant's pod to run continuously instead, skipping both the cold start and the reaper. This matches the design doc's own "micro-SaaS scale" framing (most tenants inactive most of the time) while leaving room for a tenant that can't tolerate a cold start.

- No existing benchmark gate constrains this: design 4.3's latency targets are all local-CLI operations (`wkp search`, `wkp index`, session-start injection) that never touch the hub. Cold-start latency is still a real cost worth measuring once this lands, just not one CI currently enforces.
- Not decided here: the idle-timeout default value, or whether it's configurable per-tenant vs. one fixed value hub-wide. Pick a conservative starting value (a few minutes) when implementing and revisit with real traffic data.
- Not decided here: what a request does while its tenant's pod is cold-starting (queue and wait vs. an immediate retryable error). Whichever is simpler to implement correctly first; this is exactly the kind of thing worth an ADR of its own if it turns out to matter, not a guess baked in now.

**2. Each pod runs `wkp-hub` itself, in a new single-tenant serving mode.** Not a second, separately-maintained program. The same image already built for M5-5/M5-6 runs inside each tenant's pod, invoked in a mode that serves exactly one fixed repo via `wkp_git::http_backend` and skips the bearer-token/tenant-match check the front door already performed -- that check does not need repeating once a request has been routed to the one pod it's already been authorized for. One codebase, one container image, one thing to keep correct as the git-http logic evolves.

**3. Addressing: front door and tenant pods share a user-defined podman network; the front door reaches a tenant's pod by name, not by a per-tenant host-port mapping.** Podman (like Docker) resolves container/pod names via built-in DNS for any user-defined bridge network every member joins -- naming each pod after its tenant's slug means the front door can always reach `http://<tenant-slug>:<port>/...` directly, with no per-tenant port allocation to track or exhaust. This is the same shape Kubernetes uses for the same problem (a `Service`/`Pod` reachable by cluster DNS name, not a host port) -- not byte-identical, but the same *idea*, which is what ADR-0009's "must work the same way under Kubernetes later" requirement actually asked for. The front door container itself needs to join that same network too, which M5-5's current standalone container does not do yet; this is new deployment surface that task's own container didn't need.

**4. Bare repos persist outside the pod, on a shared, bind-mounted path.** A pod's own container filesystem is otherwise ephemeral -- stopped and started again by the reaper/cold-start logic above, which must never mean losing a tenant's history. `wkp-hub tenant create`'s existing behavior (a bare repo under `repos_root`, M5-4, unchanged) continues to be the actual source of truth; each tenant's pod bind-mounts that same fixed path (`repos_root/<slug>.git`) into itself at start time rather than holding its own copy. A `PersistentVolume`/`hostPath`-equivalent mount is the natural Kubernetes analog, so this doesn't need to change shape later, only its exact mount mechanism.

## Options considered and rejected

- **Always-warm for every tenant.** Rejected as the *default* (kept as an explicit per-tenant override, decision 1): resource cost scales with tenant count rather than activity, forcing real capacity planning far earlier than actual usage would otherwise require.
- **A second, minimal purpose-built binary per pod** (e.g. a thin wrapper directly around `git http-backend`, smaller than shipping the whole `wkp-hub` image). Rejected: a second codebase to keep in sync with `wkp-hub`'s own git-http logic as M5-6 and later work evolve it is a maintenance cost this milestone's own slim-core priority (CLAUDE.md) weighs against, for an image-size saving that hasn't been shown to matter yet.
- **Per-tenant host-port mapping** (`podman pod create -p <unique-host-port>:<container-port>` per tenant, front door tracks which port belongs to which tenant). Rejected: a real, growing allocation table to manage (and free correctly on teardown) for a problem container-runtime DNS already solves for free, and no analog at all in a Kubernetes deployment, which addresses pods by name/IP, never a per-workload host port.

## Consequences

- The front door gains real new responsibility: deciding whether a tenant's pod is running, starting it if not, and waiting for it to become ready before proxying -- none of which M5-5/M5-6's single-shared-filesystem model needed at all. This is genuinely more failure-prone surface (a pod that fails to start, or times out coming up) than "run a local subprocess," and needs its own error handling designed carefully, not bolted on.
- `wkp-hub tenant create` needs to grow from "make a directory" (M5-4) to also registering (not necessarily starting, per decision 1) a pod definition -- the exact provisioning command shape is implementation work, not decided here.
- The reaper is a new, standing background responsibility (something has to periodically check idle tenants and stop their pods) that does not fit today's `wkp-hub` invocation model (a `serve` process and a handful of one-shot admin commands) without adding one -- likely a periodic sweep, either as a thread inside `serve` or a separate scheduled invocation. Decide which when implementing; this ADR does not pick one.
- Local single-host development and testing (as done for M5-5/M5-6) gets meaningfully more involved: a developer now needs a user-defined podman network and at least one running tenant pod to exercise the full path, not just one standalone container. Expect the M5-7 implementation to need its own local-testing writeup, the same way M5-5's work produced one.
