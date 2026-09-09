//! `git filter-repo` wrapper for true history erasure (design 7.6, M4-6).
//!
//! Distinct from `wkp forget` (M4-5), which only ever affects the
//! current tree/HEAD -- a real, ordinary commit that removes or
//! re-encrypts something, indistinguishable in shape from any other
//! commit -- this rewrites *every* commit in history that ever touched
//! the given path, producing entirely new hashes for the whole commit
//! graph from that point forward. There is no way to make this look
//! like "just another commit": every existing clone's history diverges
//! from this repository's the moment this runs, which is exactly why
//! `wkp purge`'s caller (`crates/wkp-cli/src/purge.rs`) prints the
//! force-push-and-re-clone instructions this module's own doc comment
//! keeps mentioning -- they are not optional decoration.
//!
//! `git-filter-repo` is an external tool (a single Python script, git's
//! own documented replacement for `filter-branch`/BFG) this crate shells
//! out to via git's own subcommand dispatch (`git filter-repo ...`) --
//! not vendored into this repo (CLAUDE.md: "Vendoring a Python or Node
//! dependency" is an explicit non-shortcut), so it must already be
//! installed and on `PATH` wherever `wkp purge` runs. A missing
//! installation surfaces as a clear, actionable error instead of git's
//! own raw "is not a git command" dispatch failure.

use std::path::Path;

/// What [`purge_path`] actually did, for the caller's own user-facing
/// summary and required follow-up instructions.
pub struct PurgeSummary {
    pub path: String,
}

/// Removes `path` from every commit in `repo_dir`'s history
/// (`git filter-repo --invert-paths --path <path> --force`).
///
/// `--force` bypasses `git-filter-repo`'s own default "refuse unless
/// this looks like a fresh clone" safety check -- appropriate here
/// because `wkp purge` *is* the explicit, heavier operation design 7.6
/// reserves for a caller who has already decided to rewrite history, not
/// an accidental invocation on a repo someone forgot was their real
/// working copy. `git-filter-repo` still performs its own other default
/// safety behavior unconditionally: it removes the `origin` remote from
/// the rewritten repository (so a careless `git push` can't happen
/// without deliberately re-adding it first) and expires the reflog.
pub fn purge_path(repo_dir: &Path, path: &str) -> Result<PurgeSummary, String> {
    let result = super::run_git(
        repo_dir,
        &["filter-repo", "--invert-paths", "--path", path, "--force"],
    );
    match result {
        Ok(()) => Ok(PurgeSummary {
            path: path.to_string(),
        }),
        Err(stderr) if stderr.contains("is not a git command") => Err(format!(
            "git-filter-repo is not installed (or not on PATH) -- wkp purge wraps the \
             upstream `git-filter-repo` tool (https://github.com/newren/git-filter-repo), \
             which this project deliberately does not vendor (CLAUDE.md: no vendoring a \
             Python dependency). Install it via your OS package manager (e.g. `apt install \
             git-filter-repo`, `brew install git-filter-repo`, or `pip install \
             git-filter-repo`), then re-run `wkp purge {path}`. git's own error: {stderr}"
        )),
        Err(stderr) => Err(stderr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempGitRepo;

    /// M4-6's own acceptance criterion: a path committed across several
    /// revisions is genuinely gone from `git log --all --full-history`
    /// afterward, not just missing from the current tree (which `wkp
    /// forget`'s own tests already cover for the non-erasure case).
    #[test]
    fn purge_path_removes_a_path_from_every_revision_in_history() {
        let repo = TempGitRepo::new("purge-removes-from-history");
        repo.write("secret.md", "revision one\n");
        repo.commit_all("add secret.md");
        repo.write("secret.md", "revision two\n");
        repo.commit_all("update secret.md");
        repo.write("keep.md", "unrelated file\n");
        repo.commit_all("add keep.md");

        let before = super::super::run_git_stdout(
            repo.path(),
            &[
                "log",
                "--all",
                "--full-history",
                "--oneline",
                "--",
                "secret.md",
            ],
        )
        .expect("git log before purge");
        assert!(
            !before.trim().is_empty(),
            "sanity check: secret.md must have history before purging"
        );

        let summary = purge_path(repo.path(), "secret.md").expect("purge_path");
        assert_eq!(summary.path, "secret.md");

        let after = super::super::run_git_stdout(
            repo.path(),
            &[
                "log",
                "--all",
                "--full-history",
                "--oneline",
                "--",
                "secret.md",
            ],
        )
        .expect("git log after purge");
        assert!(
            after.trim().is_empty(),
            "secret.md must have zero history after purge, found: {after}"
        );

        assert!(
            repo.path().join("keep.md").is_file(),
            "an unrelated file must survive the purge"
        );
        let keep_history = super::super::run_git_stdout(
            repo.path(),
            &["log", "--all", "--oneline", "--", "keep.md"],
        )
        .expect("git log for keep.md");
        assert!(
            !keep_history.trim().is_empty(),
            "keep.md's own history must survive the purge"
        );
    }

    #[test]
    fn purge_path_on_a_never_committed_path_is_a_harmless_no_op() {
        let repo = TempGitRepo::new("purge-never-committed");
        repo.write("a.md", "hello\n");
        repo.commit_all("add a.md");

        purge_path(repo.path(), "never-existed.md")
            .expect("purging a path with no history at all must not be an error");
        assert!(repo.path().join("a.md").is_file());
    }
}
