//! Branch/remote plumbing `wkp sync` (M3-4) is built on: checkout, merge,
//! fetch, push, and ref enumeration.

use std::path::Path;
use std::process::Command;

use crate::plumbing::{run_git, run_git_stdout};
use crate::sync;

/// Checks out `branch_name` in `repo_dir`, creating it from the current
/// `HEAD` first if `create` is true. Thin plumbing -- `wkp sync` (M3-4)
/// needs this to move onto each device/`main` branch it merges; M3-3's own
/// tests need it to construct a real, two-branch modify/delete conflict to
/// resolve rather than faking one.
pub fn checkout_branch(repo_dir: &Path, branch_name: &str, create: bool) -> Result<(), String> {
    if create {
        run_git(repo_dir, &["checkout", "--quiet", "-b", branch_name])
    } else {
        run_git(repo_dir, &["checkout", "--quiet", branch_name])
    }
}

/// The name of the branch `repo_dir`'s `HEAD` currently points to. Valid
/// even before the first commit (`HEAD` is already a symbolic ref to
/// whichever branch `git init`/`init.defaultBranch` chose) -- callers that
/// need to return to "whatever branch we started on" after checking out
/// others (as `wkp sync`, M3-4, will) read this rather than assuming
/// `main`/`master`.
pub fn current_branch(repo_dir: &Path) -> Result<String, String> {
    run_git_stdout(repo_dir, &["symbolic-ref", "--short", "HEAD"]).map(|s| s.trim().to_string())
}

/// Merges `branch_name` into `repo_dir`'s current branch (`git merge
/// --no-edit <branch_name>`), returning whether it completed cleanly.
/// `Ok(false)` means git stopped with one or more conflicts left in the
/// index/working tree (inspect with e.g. [`crate::modify_delete_conflicts`]) --
/// not an `Err`, since a conflicted merge is an expected, ordinary outcome
/// a caller needs to detect and resolve, not a plumbing failure.
///
/// Passes a placeholder `-c user.name`/`-c user.email` the same way
/// [`crate::commit_all`] does: found the hard way (a CI run failed where a local
/// dev machine with a real global git identity configured did not) that
/// `git merge` refuses to even *attempt* a merge -- conflicted or not --
/// without a committer identity available from somewhere, since it has to
/// be ready to write an auto-merge commit before it knows whether one will
/// be needed. This identity is never what actually lands in history for a
/// real device-branch sync: a clean auto-merge here still isn't a
/// provenance-bearing commit (M3-4's job, layered on top, the same way a
/// conflicted merge here produces no commit at all until a caller resolves
/// and finishes it).
///
/// Also always passes `--allow-unrelated-histories`: `wkp sync`'s whole
/// job (M3-4) is reconciling devices that may never have shared a single
/// commit before (two independent `wkp init`s pushing to the same remote
/// for the first time) -- git refuses that by default as a safety check
/// against merging the wrong repository by accident, which does not apply
/// here since the branch name itself (`sync/<device-id>` or `main`) is
/// exactly what the caller already chose to merge. Harmless when a real
/// common ancestor does exist (every merge after the first cross-device
/// one): the flag only changes behavior when git would otherwise refuse
/// to proceed at all.
pub fn merge_branch(repo_dir: &Path, branch_name: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args([
            "-c",
            "user.email=wkp@localhost",
            "-c",
            "user.name=wkp",
            "merge",
            "--no-edit",
            "--allow-unrelated-histories",
            branch_name,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(output.status.success())
}

/// Completes a merge `wkp sync` (M3-4) has already resolved every
/// remaining conflict for (M3-3's [`crate::modify_delete_conflicts`]/[`crate::stage_path`]
/// for the modify/delete class; M3-2's `wkp merge-driver` auto-stages
/// every other class as part of the merge itself) -- `git commit --no-edit`
/// picks up `MERGE_HEAD` and the already-fully-staged index to write the
/// merge commit. Same placeholder identity as [`merge_branch`], for the
/// same reason: this never needs to be the commit's real provenance, since
/// `wkp_core::index::compute_tier`'s signature check already keeps
/// anything from an unsigned merge like this one out of tier 0/1
/// regardless of which branch it lands on.
pub fn finish_merge(repo_dir: &Path) -> Result<(), String> {
    run_git(
        repo_dir,
        &[
            "-c",
            "user.email=wkp@localhost",
            "-c",
            "user.name=wkp",
            "commit",
            "--no-edit",
        ],
    )
}

/// `git fetch <remote>` -- updates `repo_dir`'s remote-tracking refs
/// (`refs/remotes/<remote>/*`) without touching the working tree or any
/// local branch. The first step of `wkp sync` (M3-4, design 6.1: "git
/// fetch/push against a remote is the sync protocol").
pub fn fetch(repo_dir: &Path, remote: &str) -> Result<(), String> {
    run_git(repo_dir, &["fetch", "--quiet", remote])
}

/// Pushes `branch_name` to `remote` (`git push <remote> <branch_name>`) --
/// deliberately never `--force`: a rejected (non-fast-forward) push means
/// something else landed on that ref since the last fetch, which is
/// exactly the "avoid" layer's job to prevent by giving every device its
/// own branch (design 6.2 point 1) -- if it happens anyway, the right
/// response is another `wkp sync` (fetch, merge, retry), never clobbering
/// history.
pub fn push_branch(repo_dir: &Path, remote: &str, branch_name: &str) -> Result<(), String> {
    run_git(repo_dir, &["push", "--quiet", remote, branch_name])
}

/// Every other device's `sync/<device-id>` branch visible on `remote`
/// after a [`fetch`], as full remote-tracking ref names
/// (`refs/remotes/<remote>/sync/<device-id>`) ready to pass straight to
/// [`merge_branch`] -- excludes `own_device_id`'s own branch, since
/// `wkp sync` merges *other* devices' work in, never its own.
pub fn other_device_sync_refs(
    repo_dir: &Path,
    remote: &str,
    own_device_id: &str,
) -> Result<Vec<String>, String> {
    let own_branch = format!("{remote}/{}", sync::device_branch_name(own_device_id));
    let stdout = run_git_stdout(
        repo_dir,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            &format!("refs/remotes/{remote}/sync/"),
        ],
    )?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != own_branch)
        .map(String::from)
        .collect())
}

/// Whether `remote/branch_name` exists as a remote-tracking ref after a
/// [`fetch`] -- `wkp sync` uses this to check for `<remote>/main` before
/// trying to merge it in, since a brand-new remote (nothing ever pushed
/// to `main` yet) legitimately has none.
pub fn remote_branch_exists(repo_dir: &Path, remote: &str, branch_name: &str) -> bool {
    run_git(
        repo_dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/remotes/{remote}/{branch_name}"),
        ],
    )
    .is_ok()
}

/// Whether `repo_dir` has a *local* branch named `branch_name`
/// (`refs/heads/<branch_name>`) -- [`remote_branch_exists`]'s counterpart
/// for a ref this repo owns itself, e.g. checking for a local `main`
/// before including it in a `wkp bundle export` (M3-5).
pub fn local_branch_exists(repo_dir: &Path, branch_name: &str) -> bool {
    run_git(
        repo_dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch_name}"),
        ],
    )
    .is_ok()
}

/// Every ref whose name starts with `prefix` (e.g. `refs/remotes/bundle/`),
/// as full ref names via `git for-each-ref --format=%(refname)`. General
/// enough for `wkp bundle import` (M3-5) to enumerate whatever a bundle's
/// refspec happened to land under, without hardcoding branch names.
pub fn refs_matching(repo_dir: &Path, prefix: &str) -> Result<Vec<String>, String> {
    let stdout = run_git_stdout(repo_dir, &["for-each-ref", "--format=%(refname)", prefix])?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{bare_remote, TempGitRepo};

    #[test]
    fn fetch_populates_remote_tracking_refs_after_a_push_from_another_clone() {
        let remote = bare_remote("fetch");
        let publisher = TempGitRepo::new("fetch-publisher");
        publisher.write("item.md", "hello\n");
        publisher.commit_all("initial");
        publisher.checkout_new_branch("sync/device-a");
        publisher.add_remote("origin", remote.path());
        push_branch(publisher.path(), "origin", "sync/device-a").expect("push_branch");

        let subscriber = TempGitRepo::new("fetch-subscriber");
        subscriber.add_remote("origin", remote.path());
        fetch(subscriber.path(), "origin").expect("fetch");

        let tip = run_git_stdout(
            subscriber.path(),
            &["rev-parse", "refs/remotes/origin/sync/device-a"],
        );
        assert!(tip.is_ok(), "expected origin/sync/device-a after fetch");
    }

    #[test]
    fn push_branch_makes_the_branch_visible_on_the_bare_remote() {
        let remote = bare_remote("push");
        let repo = TempGitRepo::new("push-source");
        repo.write("item.md", "hello\n");
        repo.commit_all("initial");
        repo.checkout_new_branch("sync/device-a");
        repo.add_remote("origin", remote.path());

        push_branch(repo.path(), "origin", "sync/device-a").expect("push_branch");

        let status = Command::new("git")
            .arg("-C")
            .arg(remote.path())
            .args(["rev-parse", "--verify", "-q", "refs/heads/sync/device-a"])
            .status()
            .expect("run git rev-parse on the bare remote");
        assert!(
            status.success(),
            "expected sync/device-a on the bare remote"
        );
    }

    #[test]
    fn other_device_sync_refs_excludes_the_local_devices_own_branch() {
        let remote = bare_remote("other-devices");
        let publisher = TempGitRepo::new("other-devices-publisher");
        publisher.write("item.md", "hello\n");
        publisher.commit_all("initial");
        publisher.add_remote("origin", remote.path());
        for device in ["device-a", "device-b"] {
            checkout_branch(publisher.path(), &format!("sync/{device}"), true)
                .expect("checkout_branch (create)");
            push_branch(publisher.path(), "origin", &format!("sync/{device}"))
                .expect("push_branch");
        }

        let subscriber = TempGitRepo::new("other-devices-subscriber");
        subscriber.add_remote("origin", remote.path());
        fetch(subscriber.path(), "origin").expect("fetch");

        let refs = other_device_sync_refs(subscriber.path(), "origin", "device-b")
            .expect("other_device_sync_refs");
        assert_eq!(refs, vec!["origin/sync/device-a".to_string()]);
    }

    #[test]
    fn remote_branch_exists_is_false_for_a_branch_never_pushed() {
        let remote = bare_remote("branch-exists");
        let publisher = TempGitRepo::new("branch-exists-publisher");
        publisher.write("item.md", "hello\n");
        publisher.commit_all("initial");
        publisher.add_remote("origin", remote.path());
        push_branch(publisher.path(), "origin", &publisher.current_branch()).expect("push_branch");

        let subscriber = TempGitRepo::new("branch-exists-subscriber");
        subscriber.add_remote("origin", remote.path());
        fetch(subscriber.path(), "origin").expect("fetch");

        assert!(remote_branch_exists(
            subscriber.path(),
            "origin",
            &publisher.current_branch()
        ));
        assert!(!remote_branch_exists(
            subscriber.path(),
            "origin",
            "no-such-branch"
        ));
    }

    #[test]
    fn finish_merge_completes_a_resolved_modify_delete_conflict_as_a_real_merge_commit() {
        let repo = TempGitRepo::new("finish-merge");
        let main = repo.current_branch();
        repo.write("item.md", "base content\n");
        repo.commit_all("base");
        repo.checkout_new_branch("feature");
        repo.checkout(&main);
        repo.write("item.md", "modified by main\n");
        repo.commit_all("main modifies");
        repo.checkout("feature");
        repo.remove_and_commit("item.md", "feature deletes");
        repo.checkout(&main);

        assert!(!repo.merge("feature"));
        let conflicts =
            crate::modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert_eq!(conflicts.len(), 1);
        crate::stage_path(repo.path(), "item.md").expect("stage_path");

        finish_merge(repo.path()).expect("finish_merge");

        let parent_count =
            run_git_stdout(repo.path(), &["rev-list", "--parents", "-n", "1", "HEAD"])
                .expect("rev-list --parents")
                .split_whitespace()
                .count()
                - 1;
        assert_eq!(parent_count, 2, "expected a real two-parent merge commit");
        assert!(
            run_git(repo.path(), &["rev-parse", "--verify", "-q", "MERGE_HEAD"]).is_err(),
            "MERGE_HEAD must be cleared once the merge commit lands"
        );
    }

    #[test]
    fn local_branch_exists_reflects_real_local_branches_only() {
        let repo = TempGitRepo::new("local-branch-exists");
        repo.write("item.md", "hello\n");
        repo.commit_all("initial");
        assert!(local_branch_exists(repo.path(), &repo.current_branch()));
        assert!(!local_branch_exists(repo.path(), "no-such-branch"));
    }
}
