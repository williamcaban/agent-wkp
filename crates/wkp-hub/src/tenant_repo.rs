//! Per-tenant bare repo provisioning and post-receive indexing (design
//! 8.2, 8.3, M5-4).
//!
//! Two halves:
//!
//! - [`provision_tenant_repo`]: creates the bare repo at
//!   [`crate::wkp_shell::tenant_repo_path`]'s fixed convention (the
//!   contract M5-3 already defined), hardens it (`receive.fsckObjects`,
//!   `transfer.fsckObjects` -- design 8.3's own hardening table), and
//!   installs a `post-receive` hook that indexes on every push.
//! - [`index_tenant`]: what that hook actually runs. Reads every
//!   tracked path at `HEAD` straight from the object database (no
//!   working tree exists in a bare repo to read from instead), and
//!   builds this tenant's own `index.db` from the *shared* subset only.
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

use crate::wkp_shell::tenant_repo_path;
use std::path::{Path, PathBuf};

/// `hooks/post-receive`'s own content: hardcodes this tenant's slug (a
/// bare repo is dedicated to exactly one tenant, per
/// [`crate::wkp_shell::tenant_repo_path`]'s convention) rather than
/// trying to pass it as an argument -- git invokes post-receive hooks
/// with no arguments at all, ref-update info arrives on stdin instead,
/// which indexing has no use for (it always (re)indexes the whole tree
/// at `HEAD`, not just what changed).
///
/// Bare command name, not an absolute path: the same deliberate choice
/// `wkp-shell`'s own `command=` line makes (M5-3) -- relies on
/// `wkp-hub` being on `PATH` in the deployment environment, real
/// wiring of which is M5-5's container job, not this one's.
fn post_receive_hook_script(tenant_slug: &str) -> String {
    format!("#!/bin/sh\nexec wkp-hub index-tenant {tenant_slug}\n")
}

/// Where a tenant's derived index lives -- a sibling of its bare repo,
/// not inside it (an `index.db` living inside `<slug>.git/` would need
/// its own `.gitignore`-equivalent carve-out from git's own object
/// database, for no benefit).
pub fn tenant_index_path(repos_root: &Path, tenant_slug: &str) -> PathBuf {
    repos_root.join(format!("{tenant_slug}.index.db"))
}

/// Creates `tenant_slug`'s bare repo (idempotent: safe to call again
/// against an already-provisioned tenant, `git init --bare` on an
/// existing bare repo is a no-op) and installs its post-receive hook.
/// Called from `wkp-hub tenant create` so one command leaves a tenant
/// both registered in the control plane and actually push/pull-able.
pub fn provision_tenant_repo(repos_root: &Path, tenant_slug: &str) -> Result<PathBuf, String> {
    let repo_path = tenant_repo_path(repos_root, tenant_slug);
    wkp_git::init_bare_repo(&repo_path)?;
    wkp_git::set_local_config(&repo_path, "receive.fsckObjects", "true")?;
    wkp_git::set_local_config(&repo_path, "transfer.fsckObjects", "true")?;

    let hooks_dir = repo_path.join("hooks");
    std::fs::create_dir_all(&hooks_dir).map_err(|e| e.to_string())?;
    let hook_path = hooks_dir.join("post-receive");
    std::fs::write(&hook_path, post_receive_hook_script(tenant_slug)).map_err(|e| e.to_string())?;
    set_executable(&hook_path)?;

    Ok(repo_path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| e.to_string())
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
    fn provision_tenant_repo_creates_a_hardened_bare_repo_with_a_hook() {
        let temp = temp_repos_root("provision");
        let repos_root = temp.path();
        let repo_path = provision_tenant_repo(repos_root, "acme").expect("provision");

        assert_eq!(repo_path, repos_root.join("acme.git"));
        assert!(repo_path.join("HEAD").exists(), "must be a real bare repo");
        assert_eq!(
            wkp_git::get_local_config(&repo_path, "receive.fsckObjects").as_deref(),
            Some("true")
        );
        assert_eq!(
            wkp_git::get_local_config(&repo_path, "transfer.fsckObjects").as_deref(),
            Some("true")
        );

        let hook = std::fs::read_to_string(repo_path.join("hooks/post-receive")).expect("hook");
        assert!(hook.contains("wkp-hub index-tenant acme"));
    }

    #[test]
    fn provision_tenant_repo_is_idempotent() {
        let temp = temp_repos_root("idempotent");
        let repos_root = temp.path();
        provision_tenant_repo(repos_root, "acme").expect("first provision");
        provision_tenant_repo(repos_root, "acme").expect("second provision must not fail");
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
    /// outside `wkp-git`" -- proving a real post-receive-triggering push
    /// against a real bare repo needs a real `git clone`/`push` from a
    /// second working copy, which `wkp-git`'s own plumbing (built for
    /// the client's single-store use, not driving a second throwaway
    /// clone in a test) has no call for otherwise. Mirrors the same
    /// documented exception in `wkp-cli/tests/encryption_filter.rs`.
    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git") // nosemgrep: rust.lang.security.command-injection.command-injection
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
        assert!(status.success(), "git {args:?} failed: {status:?}");
    }
}
