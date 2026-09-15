//! Per-tenant bare repo provisioning and indexing (design 8.2, 8.3,
//! M5-4; indexing's trigger mechanism rewritten by issue #159; on-disk
//! layout rewritten by ADR-0014/issue #165).
//!
//! Three parts:
//!
//! - [`provision_tenant_repo`]: creates the bare repo at
//!   [`tenant_repo_path`]'s fixed convention and hardens it
//!   (`receive.fsckObjects`, `transfer.fsckObjects` -- design 8.3's own
//!   hardening table).
//! - [`index_tenant`]: reads every tracked path at `HEAD` straight from
//!   the object database (no working tree exists in a bare repo to
//!   read from instead), and builds this tenant's own `index.db` from
//!   the *shared* subset only.
//! - [`current_head`]: what `wkp-hub index-worker` (`main.rs`, running
//!   as its own no-network container per `tenant_pod.rs`, issue #159)
//!   polls to decide whether to call [`index_tenant`] again. There is
//!   no `post-receive` hook installed here any more -- a hook running
//!   inside the *serving* container would need to reach across to the
//!   separate indexing container to trigger it, and the only ways to
//!   do that are either a network call (defeats the point: the whole
//!   reason for two containers is that the indexing one can't make or
//!   accept one) or a shared-filesystem signal file, which is no
//!   simpler or more reliable than the indexing container simply
//!   noticing `HEAD` moved on its own. Bounded staleness (up to one
//!   poll interval after a push) replaces the old hook's synchronous
//!   guarantee; `wkp-hub index-worker`'s own doc comment states the
//!   default interval.
//!
//! **Never touches `visibility: private` content.** The hub holds no
//! device's decryption key at all (design 8's whole point), so a
//! private item's blob is age ciphertext this process could not read
//! even if it tried. This module still checks explicitly and skips
//! before ever calling `wkp_core::frontmatter::parse` on such a blob --
//! belt and suspenders, not reliance on the parser happening to fail
//! safely on binary ciphertext bytes. Two independent signals, either
//! one enough to skip a file: the blob starts with
//! [`wkp_crypto::AGE_HEADER`] (the same detection `wkp-cli`'s clean/smudge
//! filter uses), or its frontmatter says `visibility: private` (a file a
//! client chose not to encrypt yet, or couldn't -- still not this hub's
//! content to index).

use std::path::{Path, PathBuf};

/// `repos_root/<slug>`'s own directory -- the one persistent storage
/// location a tenant's entire pod (both the serving and indexing
/// containers, `tenant_pod.rs`) gets mounted into as a whole (ADR-0014,
/// issue #165): a bind mount of this host directory today (podman), a
/// PersistentVolumeClaim mounted at the same container path once
/// issue #141's Kubernetes backend exists. [`tenant_repo_path`] and
/// [`tenant_index_path`] are the two fixed things that live inside it.
fn tenant_storage_dir(repos_root: &Path, tenant_slug: &str) -> PathBuf {
    repos_root.join(tenant_slug)
}

/// The one, fixed bare-repo path a tenant's git operations ever
/// operate on -- never derived from anything a connecting client says.
/// Originally `wkp_shell`'s own contract (M5-3, SSH `git-shell`
/// invocations honored it too); moved here when #125 (ADR-0011)
/// removed the SSH transport, since this module -- provisioning and
/// indexing a tenant's repo -- is where it actually belongs. Named
/// `repo.git`, not `<slug>.git`, since the slug is now the enclosing
/// directory's own name ([`tenant_storage_dir`], ADR-0014) -- repeating
/// it in the bare repo's own name would be redundant.
pub fn tenant_repo_path(repos_root: &Path, tenant_slug: &str) -> PathBuf {
    tenant_storage_dir(repos_root, tenant_slug).join("repo.git")
}

/// Where a tenant's derived index lives -- a sibling of its bare repo
/// inside [`tenant_storage_dir`] (ADR-0014), not inside the bare repo
/// itself (an `index.db` living inside `repo.git/` would need its own
/// `.gitignore`-equivalent carve-out from git's own object database,
/// for no benefit) and not a sibling directly under the flat, shared
/// `repos_root` either (issue #165: that isn't part of the same mount
/// a tenant's pod gets, so it was never actually host-persisted or
/// visible outside whichever container most recently wrote it).
pub fn tenant_index_path(repos_root: &Path, tenant_slug: &str) -> PathBuf {
    tenant_storage_dir(repos_root, tenant_slug).join("index.db")
}

/// Creates `tenant_slug`'s bare repo (idempotent: safe to call again
/// against an already-provisioned tenant, `git init --bare` on an
/// existing bare repo is a no-op). Called from `wkp-hub tenant create`
/// so one command leaves a tenant both registered in the control plane
/// and actually push/pull-able.
pub fn provision_tenant_repo(repos_root: &Path, tenant_slug: &str) -> Result<PathBuf, String> {
    let repo_path = tenant_repo_path(repos_root, tenant_slug);
    wkp_git::init_bare_repo(&repo_path)?;
    wkp_git::set_local_config(&repo_path, "receive.fsckObjects", "true")?;
    wkp_git::set_local_config(&repo_path, "transfer.fsckObjects", "true")?;
    Ok(repo_path)
}

/// The commit `tenant_slug`'s bare repo's `HEAD` currently resolves to,
/// or `None` for an unborn repo (nothing pushed yet). What `wkp-hub
/// index-worker`'s poll loop (`main.rs`, issue #159) compares against
/// its own last-seen value to decide whether to call [`index_tenant`]
/// again -- cheap (`git rev-parse HEAD` via `wkp_git::current_commit`,
/// no working tree needed for a bare repo) and, like everything else in
/// this module, entirely filesystem-based.
pub fn current_head(repos_root: &Path, tenant_slug: &str) -> Option<String> {
    let repo_path = tenant_repo_path(repos_root, tenant_slug);
    let sha = wkp_git::current_commit(&repo_path).ok()?;
    // `wkp_git::current_commit` runs `git rev-parse HEAD`, not `--verify
    // HEAD` -- for an unborn repo (nothing pushed yet) that doesn't
    // error, it echoes the literal ref name `"HEAD"` back as if it were
    // a resolved value. Filter that out rather than treating it as a
    // real, indexable commit.
    if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(sha)
    } else {
        None
    }
}

/// Summary of one `index_tenant` run, returned so both the CLI and this
/// module's own tests can assert on outcomes without re-reading the
/// index back out of SQLite.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct IndexSummary {
    pub indexed: Vec<String>,
    pub skipped_private: Vec<String>,
}

/// Rebuilds `tenant_slug`'s `index.db` from its bare repo's `HEAD`,
/// including only markdown items whose stored blob is genuinely shared
/// (plaintext) content -- see the module doc for the two independent
/// skip signals. Every non-`.md` path is skipped outright: this
/// indexer, like `wkp index` itself, only ever indexes the store's own
/// item format.
///
/// An empty or unborn repo (nothing pushed yet, `HEAD` doesn't resolve)
/// is not an error: it indexes to an empty `index.db`, the correct
/// state for a tenant that has not received its first push.
pub fn index_tenant(repos_root: &Path, tenant_slug: &str) -> Result<IndexSummary, String> {
    let repo_path = tenant_repo_path(repos_root, tenant_slug);
    let index_path = tenant_index_path(repos_root, tenant_slug);

    // An unborn HEAD (no commits pushed yet) is not a failure -- just
    // nothing to index yet.
    let paths = wkp_git::list_files_at_ref(&repo_path, "HEAD").unwrap_or_default();

    let mut summary = IndexSummary::default();
    let mut items = Vec::new();

    for path in paths {
        let Some(path_str) = path.to_str() else {
            continue;
        };
        if !path_str.ends_with(".md") {
            continue;
        }

        let rev_path = format!("HEAD:{path_str}");
        let blob = wkp_git::read_blob(&repo_path, &rev_path)?;

        if blob.starts_with(wkp_crypto::AGE_HEADER) {
            summary.skipped_private.push(path_str.to_string());
            continue;
        }
        let Ok(text) = std::str::from_utf8(&blob) else {
            // Not valid UTF-8 and not age-ciphertext-shaped either: not
            // a markdown item this indexer understands. Skip rather
            // than guess.
            continue;
        };
        let parsed = wkp_core::frontmatter::parse(text);
        if parsed.frontmatter.visibility == Some(wkp_core::frontmatter::Visibility::Private) {
            summary.skipped_private.push(path_str.to_string());
            continue;
        }

        summary.indexed.push(path_str.to_string());
        items.push(wkp_core::index::Item {
            path: path_str.to_string(),
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            embedding: None,
            // The hub has no allowed_signers verification wired up for
            // tenant repos yet (that's the client-side write path,
            // design 7.3/7.4) -- never claim human-signed provenance
            // this process did not itself verify.
            human_signed: false,
        });
    }

    wkp_core::index::build_index(&index_path, &items).map_err(|e| e.to_string())?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repos_root(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("wkp-hub-tenant-repo-{label}-"))
            .tempdir()
            .expect("create temp repos root")
    }

    #[test]
    fn provision_tenant_repo_creates_a_hardened_bare_repo() {
        let temp = temp_repos_root("provision");
        let repos_root = temp.path();
        let repo_path = provision_tenant_repo(repos_root, "acme").expect("provision");

        assert_eq!(repo_path, repos_root.join("acme").join("repo.git"));
        assert!(repo_path.join("HEAD").exists(), "must be a real bare repo");
        assert_eq!(
            wkp_git::get_local_config(&repo_path, "receive.fsckObjects").as_deref(),
            Some("true")
        );
        assert_eq!(
            wkp_git::get_local_config(&repo_path, "transfer.fsckObjects").as_deref(),
            Some("true")
        );

        // Deliberately no hook (issue #159): indexing is triggered by
        // `wkp-hub index-worker` polling `current_head`, not by
        // anything running inside the serving container/process.
        assert!(
            !repo_path.join("hooks/post-receive").exists(),
            "no post-receive hook should be installed any more"
        );
    }

    #[test]
    fn provision_tenant_repo_is_idempotent() {
        let temp = temp_repos_root("idempotent");
        let repos_root = temp.path();
        provision_tenant_repo(repos_root, "acme").expect("first provision");
        provision_tenant_repo(repos_root, "acme").expect("second provision must not fail");
    }

    #[test]
    fn current_head_is_none_for_an_unborn_repo_and_some_after_a_commit() {
        let temp = temp_repos_root("current-head");
        let repos_root = temp.path();
        provision_tenant_repo(repos_root, "acme").expect("provision");

        assert_eq!(
            current_head(repos_root, "acme"),
            None,
            "an unborn repo has no HEAD commit yet"
        );

        let clone_dir = repos_root.join("clone");
        run_git(
            repos_root,
            &[
                "clone",
                "--quiet",
                repo_path_str(repos_root, "acme").as_str(),
                "clone",
            ],
        );
        run_git(&clone_dir, &["checkout", "--quiet", "-b", "main"]);
        std::fs::write(
            clone_dir.join("a.md"),
            "---\nvisibility: shared\n---\n\nbody\n",
        )
        .expect("write a.md");
        run_git(&clone_dir, &["add", "-A"]);
        run_git(
            &clone_dir,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "first commit",
            ],
        );
        run_git(&clone_dir, &["push", "--quiet", "origin", "main"]);

        let first = current_head(repos_root, "acme").expect("HEAD must resolve after a push");
        assert_eq!(first.len(), 40, "expected a full SHA-1 hex commit id");

        std::fs::write(
            clone_dir.join("b.md"),
            "---\nvisibility: shared\n---\n\nmore\n",
        )
        .expect("write b.md");
        run_git(&clone_dir, &["add", "-A"]);
        run_git(
            &clone_dir,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "second commit",
            ],
        );
        run_git(&clone_dir, &["push", "--quiet", "origin", "main"]);

        let second = current_head(repos_root, "acme").expect("HEAD must resolve after a push");
        assert_ne!(
            first, second,
            "HEAD must change after a second push -- this is what the index-worker poll loop diffs against"
        );
    }

    fn repo_path_str(repos_root: &Path, tenant_slug: &str) -> String {
        tenant_repo_path(repos_root, tenant_slug)
            .to_str()
            .expect("repo path must be valid UTF-8")
            .to_string()
    }

    #[test]
    fn index_tenant_on_an_unborn_repo_produces_an_empty_index() {
        let temp = temp_repos_root("empty");
        let repos_root = temp.path();
        provision_tenant_repo(repos_root, "acme").expect("provision");

        let summary = index_tenant(repos_root, "acme").expect("index_tenant");
        assert_eq!(summary, IndexSummary::default());
        assert!(tenant_index_path(repos_root, "acme").exists());
    }

    /// M5-4's own core acceptance criterion: a push containing both a
    /// shared and a private item indexes only the shared one.
    #[test]
    fn index_tenant_indexes_shared_content_and_skips_private_frontmatter() {
        let temp = temp_repos_root("shared-and-private");
        let repos_root = temp.path();
        let repo_path = provision_tenant_repo(repos_root, "acme").expect("provision");

        // Simulate a push by committing directly into the bare repo via
        // a throwaway non-bare clone -- this crate's own test-only use
        // of git plumbing, the same documented exception `wkp-cli`'s
        // integration tests already use for real end-to-end git wiring.
        let clone_dir = repos_root.join("clone");
        run_git(
            repos_root,
            &["clone", "--quiet", repo_path.to_str().unwrap(), "clone"],
        );
        run_git(&clone_dir, &["checkout", "--quiet", "-b", "main"]);
        std::fs::write(
            clone_dir.join("shared.md"),
            "---\nvisibility: shared\ntitle: shared item\n---\n\nshared body\n",
        )
        .expect("write shared.md");
        std::fs::write(
            clone_dir.join("private.md"),
            "---\nvisibility: private\ntitle: private item\n---\n\nprivate body\n",
        )
        .expect("write private.md");
        std::fs::write(clone_dir.join("notes.txt"), "not markdown, ignored")
            .expect("write notes.txt");
        run_git(&clone_dir, &["add", "-A"]);
        run_git(
            &clone_dir,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "add shared and private items",
            ],
        );
        run_git(&clone_dir, &["push", "--quiet", "origin", "main"]);

        let summary = index_tenant(repos_root, "acme").expect("index_tenant");
        assert_eq!(summary.indexed, vec!["shared.md".to_string()]);
        assert_eq!(summary.skipped_private, vec!["private.md".to_string()]);

        let index_path = tenant_index_path(repos_root, "acme");
        let conn = wkp_core::index::open_index(&index_path).expect("open index");
        let known = wkp_core::index::known_paths(&conn).expect("known_paths");
        assert_eq!(
            known,
            std::collections::HashSet::from(["shared.md".to_string()])
        );
    }

    #[test]
    fn index_tenant_skips_content_that_is_recognizably_age_ciphertext_even_without_frontmatter() {
        let temp = temp_repos_root("ciphertext");
        let repos_root = temp.path();
        let repo_path = provision_tenant_repo(repos_root, "acme").expect("provision");

        let clone_dir = repos_root.join("clone");
        run_git(
            repos_root,
            &["clone", "--quiet", repo_path.to_str().unwrap(), "clone"],
        );
        run_git(&clone_dir, &["checkout", "--quiet", "-b", "main"]);
        // Fake ciphertext-shaped bytes: not real age output, but this
        // check only ever inspects the header prefix, never decrypts.
        std::fs::write(
            clone_dir.join("secret.md"),
            b"age-encryption.org/v1\nnot-real-ciphertext",
        )
        .expect("write secret.md");
        run_git(&clone_dir, &["add", "-A"]);
        run_git(
            &clone_dir,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "add ciphertext-shaped item",
            ],
        );
        run_git(&clone_dir, &["push", "--quiet", "origin", "main"]);

        let summary = index_tenant(repos_root, "acme").expect("index_tenant");
        assert!(summary.indexed.is_empty());
        assert_eq!(summary.skipped_private, vec!["secret.md".to_string()]);
    }

    /// Test-only, narrow exception to CLAUDE.md's "no `Command::new(\"git\")`
    /// outside `wkp-git`" -- proving a real push against a real bare repo
    /// needs a real `git clone`/`push` from a second working copy,
    /// which `wkp-git`'s own plumbing (built for the client's
    /// single-store use, not driving a second throwaway clone in a
    /// test) has no call for otherwise. Mirrors the same documented
    /// exception in `wkp-cli/tests/encryption_filter.rs`.
    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git") // nosemgrep: rust.lang.security.command-injection.command-injection
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
        assert!(status.success(), "git {args:?} failed: {status:?}");
    }
}
