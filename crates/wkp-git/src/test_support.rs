//! Shared test-only fixtures used across this crate's module test suites.
//! `tempfile` rather than `std::env::temp_dir()` + a predictable name: the
//! latter is flagged by this repo's semgrep gate as an insecure-temp-file
//! pattern (a shared temp directory with a guessable name invites
//! symlink/TOCTOU races).

use std::path::Path;
use std::process::Command;

pub(crate) struct TempGitRepo {
    dir: tempfile::TempDir,
}

impl TempGitRepo {
    pub(crate) fn new(name: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("wkp-git-test-{name}-"))
            .tempdir()
            .expect("create temp dir");
        // `init_repo` itself, not a raw `git init` -- found the hard
        // way (a bundle test failed with a `main`/`master` mismatch)
        // that this helper predates `init_repo`'s `--initial-branch=main`
        // fix and had drifted from it by duplicating the plumbing
        // instead of calling it.
        crate::init_repo(dir.path()).expect("init_repo");
        Self { dir }
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    pub(crate) fn write(&self, relative_path: &str, contents: &str) {
        let full = self.path().join(relative_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(full, contents).expect("write file");
    }

    pub(crate) fn commit_all(&self, message: &str) {
        crate::commit_all(self.path(), message).expect("commit_all");
    }

    /// The name of whichever branch `git init` chose (this environment's
    /// `init.defaultBranch`) -- valid even before the first commit, since
    /// `HEAD` is already a symbolic ref to it. Modify/delete tests need
    /// two real, named branches to diverge and merge, so they read this
    /// rather than assuming `main`/`master`.
    pub(crate) fn current_branch(&self) -> String {
        crate::current_branch(self.path()).expect("current_branch")
    }

    pub(crate) fn checkout_new_branch(&self, name: &str) {
        crate::checkout_branch(self.path(), name, true).expect("checkout_branch (create)");
    }

    pub(crate) fn checkout(&self, name: &str) {
        crate::checkout_branch(self.path(), name, false).expect("checkout_branch");
    }

    pub(crate) fn remove_and_commit(&self, relative_path: &str, message: &str) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(["rm", "--quiet", relative_path])
            .status()
            .expect("run git rm");
        assert!(status.success(), "git rm {relative_path} failed");
        self.commit_all(message);
    }

    /// Attempts to merge `branch` into this repo's current branch,
    /// returning whether it succeeded -- for the modify/delete tests,
    /// which need the merge to *stop* (a real `CONFLICT
    /// (modify/delete)`, not silently auto-resolved) before they can
    /// exercise the detection/resolution step.
    pub(crate) fn merge(&self, branch: &str) -> bool {
        crate::merge_branch(self.path(), branch).expect("merge_branch")
    }

    pub(crate) fn add_remote(&self, name: &str, url: &Path) {
        crate::set_local_config(
            self.path(),
            &format!("remote.{name}.url"),
            &url.to_string_lossy(),
        )
        .expect("set remote url");
        crate::set_local_config(
            self.path(),
            &format!("remote.{name}.fetch"),
            &format!("+refs/heads/*:refs/remotes/{name}/*"),
        )
        .expect("set remote fetch refspec");
    }
}

/// A bare repo standing in for "the hub, or any git server the user
/// already trusts" (design 6.1) -- the target of `fetch`/`push_branch`
/// in these tests, never opened as a working tree itself.
pub(crate) fn bare_remote(name: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("wkp-git-test-bare-{name}-"))
        .tempdir()
        .expect("create temp dir");
    crate::init_bare_repo(dir.path()).expect("init_bare_repo");
    dir
}
