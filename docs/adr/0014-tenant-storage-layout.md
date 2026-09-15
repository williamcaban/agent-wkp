# ADR-0014: Per-tenant persistent storage layout (bare repo + index, one volume)

Status: accepted
Date: 2026-09-14
Design sections affected: 8.2, 8.3
Decided by: William (2026-09-14): "when running podman use a volume pointing to a local folder, when in K8s, it should have a PVC"

## Context

Issue #165 (found while fixing #159): `tenant_pod.rs`'s `repo_mount_arg` bind-mounts only `repos_root/<slug>.git` into a tenant's pod. `tenant_repo::tenant_index_path` writes `<slug>.index.db` as a *sibling* of `<slug>.git` directly under the flat, shared `repos_root` -- outside that bind mount entirely. Verified by hand: after a real push and a real `index-worker` run, `<slug>.index.db` exists only inside whichever container's own ephemeral filesystem ran it, never on the host, never visible to the other container in the same pod.

Two constraints rule out the obvious quick fixes:

- Mounting the whole `repos_root` into every pod (so every tenant's directory is visible from every other tenant's pod) is a straight cross-tenant isolation regression -- `deploy/hub/test-cross-tenant-isolation.sh` exists specifically to catch this.
- Bind-mounting `<slug>.index.db` as its own single-file mount, separate from `<slug>.git`, breaks `wkp_core::index::build_index`'s temp-file-then-rename atomicity: the temp file is created in the same directory as the destination file, and if that directory isn't itself the actual mount point, the temp file and the bind-mounted destination end up on different filesystems -- `rename(2)` across that boundary fails (`EXDEV`).
- Putting `index.db` *inside* the bare repo directory (`<slug>.git/index.db`) was already considered and rejected when this module was first written (`tenant_index_path`'s own doc comment): it conflates a derived artifact with git's own object database for no benefit.

## Decision

Each tenant gets **one persistent storage location**, mounted as a whole into that tenant's pod -- not a bare-repo mount and a separate index-file mount. On disk today: `repos_root/<slug>/` containing `repo.git/` (the bare repo, renamed from the previous `repos_root/<slug>.git` convention) and `index.db` as true siblings inside that one per-tenant directory.

- **Podman (today)**: a bind mount of that host directory (`repos_root/<slug>` -> `/srv/wkp-hub/repos/<slug>` in both the serving and indexing containers) -- the same mechanism already used for the bare repo alone, just scoped to the whole per-tenant directory instead of just the `.git` subpath.
- **Kubernetes (issue #141, not started)**: a PersistentVolumeClaim per tenant, mounted at the same container path convention (`/srv/wkp-hub/repos/<slug>`). This ADR does not implement that backend -- it fixes the shape #141's own `PodOrchestrator` implementation needs to target, so that work starts from an already-decided convention instead of re-litigating "PVC vs. hostPath" from scratch (previously an explicit open item in ADR-0012/#141's own acceptance criteria).

`crates/wkp-hub/src/http.rs`'s `serve_git_http` is adjusted to match: `GIT_PROJECT_ROOT` becomes the tenant-specific directory (`repos_root.join(tenant_slug)`) instead of the flat shared `repos_root`, and `PATH_INFO` becomes the fixed `/repo.git/<suffix>` instead of `/<slug>.git/<suffix>` -- purely a server-side implementation detail. The wire-facing URL convention a git client actually uses (`https://.../<slug>.git`) is unchanged; `git_http_path`'s parsing of the incoming request URL is untouched.

## Consequences

- `tenant_repo::tenant_repo_path`/`tenant_index_path` both change to the nested convention; every caller (`provision_tenant_repo`, `index_tenant`, `current_head`, `tenant_pod::repo_mount_arg`, `http::serve_git_http`, and this crate's own test fixtures that construct a bare repo path directly) is updated together in one PR, since a partial migration would leave some code reading/writing the old flat layout and some the new nested one.
- `index.db` is now host-persisted (podman) and visible to any future reader that mounts the same per-tenant directory -- resolves issue #165's actual complaint, not just its symptom in `deploy/hub/test-tenant-indexing-isolation.sh`.
- `deploy/hub/test-cross-tenant-isolation.sh`'s path assertions move from `/srv/wkp-hub/repos/<slug>.git` to `/srv/wkp-hub/repos/<slug>` (the whole per-tenant directory) -- the isolation property itself (a tenant's pod never has another tenant's directory mounted in at all) is unchanged, only the specific path checked.
- When issue #141's Kubernetes backend lands, its `start_pod` implementation provisions/references a per-tenant PVC and mounts it at `/srv/wkp-hub/repos/<slug>`, matching what the Podman backend already does with a bind mount -- no further path-convention decision needed at that point, only the PVC provisioning mechanism itself (storage class, size, reclaim policy -- still genuinely open, not decided here).
