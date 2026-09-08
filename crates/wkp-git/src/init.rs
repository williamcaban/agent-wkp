//! `wkp init`'s git-level bootstrap: `git init`, the design-5.1 config
//! settings, and the plain (unsigned) commit helper test/CLI-simple-flow
//! code paths use.

use std::path::Path;

use crate::allowed_signers;
use crate::plumbing::run_git;

/// Git settings `wkp init` applies to a store repository (design 5.1):
/// `protocol.version=2` (no full ref advertisement on fetch),
/// `receive.fsckObjects`/`transfer.fsckObjects` (reject malformed objects),
/// `core.untrackedCache` (faster status), and enabling the `commit-graph`
/// maintenance task so `git log` stays fast on long audit histories.
const INIT_CONFIG_SETTINGS: &[(&str, &str)] = &[
    ("protocol.version", "2"),
    ("receive.fsckObjects", "true"),
    ("transfer.fsckObjects", "true"),
    ("core.untrackedCache", "true"),
    ("maintenance.commit-graph.enabled", "true"),
];

/// Ensures `path` exists and is a git repository, running `git init`
/// (idempotent: re-running it against an existing repository is a no-op
/// git already supports) if needed.
///
/// `--initial-branch=main` fixes the store's shared/promoted branch name
/// regardless of the host's own `init.defaultBranch` (found while building
/// `wkp sync`, M3-4: both this session's dev machine and CI default a bare
/// `git init` to `master`, unset -- `wkp sync`'s "merge the remote's
/// `main` in too" step needs that name to be a fixed convention, not
/// whatever a given machine happens to default to).
pub fn init_repo(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    run_git(path, &["init", "--quiet", "--initial-branch=main"])
}

/// Creates a bare git repository at `path` -- a plain "server" repo with
/// no working tree, standing in for the hub, a NAS, or any git host a
/// real `wkp sync` remote points at (design 6.1). `wkp` itself only ever
/// `fetch`/`push`es against a remote like this; nothing in this crate
/// opens one as a working tree.
pub fn init_bare_repo(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    run_git(path, &["init", "--quiet", "--bare"])
}

/// Stages every change in `repo_dir` and commits it with `message`.
/// Porcelain, not plumbing, and no signing -- for test setup and simple
/// CLI flows (change detection needs a committed base state to diff
/// against). The write path's actual audit-trail commits go through
/// signed plumbing instead (design 5.1); that lands in M2. A per-call
/// identity override (`-c user.*`) means this works standalone in a
/// fresh environment with no global git identity configured, without
/// mutating that environment's global config as a side effect.
///
/// CLAUDE.md's "no `Command::new(\"git\")` outside this crate" rule means
/// other crates' tests that need to commit a fixture file must go
/// through this rather than shelling out themselves.
pub fn commit_all(repo_dir: &Path, message: &str) -> Result<(), String> {
    run_git(repo_dir, &["add", "-A"])?;
    commit_staged(repo_dir, message)
}

/// Commits whatever is currently staged in `repo_dir`'s index, without
/// first running `git add -A` -- for a caller that already staged
/// exactly the path(s) it wants (e.g. via [`stage_path`]) and needs a
/// plain, unsigned commit without also sweeping in every other untracked
/// change sitting in the working tree the way [`commit_all`] does. Same
/// per-call identity override, for the same reason.
pub fn commit_staged(repo_dir: &Path, message: &str) -> Result<(), String> {
    run_git(
        repo_dir,
        &[
            "-c",
            "user.email=wkp-test@example.com",
            "-c",
            "user.name=wkp test",
            "commit",
            "-q",
            "-m",
            message,
        ],
    )
}

/// Applies the design-5.1 git settings to the repository at `repo_dir`,
/// then best-effort registers `git maintenance start`'s background
/// schedule. Registration needs cron or systemd/launchd, which isn't
/// available in every environment (containers, CI, sandboxes); a failure
/// there is not fatal to `wkp init`, the same opportunistic posture ADR-0001
/// takes for the builtin fsmonitor.
///
/// All config values here are static and known at compile time, not
/// user input, but plumbing (`git config`, not the porcelain command) is
/// used throughout per design 5.1: deterministic, non-interactive, and
/// immune to a user's global hooks or aliases.
///
/// Also wires up SSH commit signing (design 7.3, M2-1): see
/// [`allowed_signers::configure_ssh_signing`] for what that adds --
/// `gpg.format`/`gpg.ssh.allowedSignersFile` aren't in
/// [`INIT_CONFIG_SETTINGS`] because the signers-file path is derived
/// from `repo_dir`, not a static value.
pub fn apply_init_settings(repo_dir: &Path) -> Result<(), String> {
    for (key, value) in INIT_CONFIG_SETTINGS {
        run_git(repo_dir, &["config", "--local", key, value])
            .map_err(|e| format!("wkp: failed to set git config {key}={value}: {e}"))?;
    }
    allowed_signers::configure_ssh_signing(repo_dir)?;
    // Best-effort only; see doc comment above.
    let _ = run_git(repo_dir, &["maintenance", "start"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempGitRepo;
    use std::process::Command;

    fn git_config_get(repo: &Path, key: &str) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "--local", "--get", key])
            .output()
            .expect("run git config");
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }

    #[test]
    fn init_repo_creates_a_working_git_repository() {
        let temp = tempfile::Builder::new()
            .prefix("wkp-git-test-init-repo-")
            .tempdir()
            .expect("create temp dir");
        // Point at a not-yet-existing subdirectory; init_repo must create it.
        let dir = temp.path().join("store");
        init_repo(&dir).expect("init_repo");
        assert!(dir.join(".git").is_dir());
        // Idempotent: a second call on the same path must not error.
        init_repo(&dir).expect("init_repo again");
    }

    #[test]
    fn init_repo_always_names_the_initial_branch_main() {
        let temp = tempfile::Builder::new()
            .prefix("wkp-git-test-init-repo-branch-name-")
            .tempdir()
            .expect("create temp dir");
        init_repo(temp.path()).expect("init_repo");
        assert_eq!(
            crate::current_branch(temp.path()).expect("current_branch"),
            "main",
            "wkp sync depends on `main` being a fixed name, not the host's init.defaultBranch"
        );
    }

    #[test]
    fn apply_init_settings_writes_expected_git_config() {
        let repo = TempGitRepo::new("init-settings");
        apply_init_settings(repo.path()).expect("apply_init_settings");

        for (key, expected) in INIT_CONFIG_SETTINGS {
            assert_eq!(
                git_config_get(repo.path(), key).as_deref(),
                Some(*expected),
                "unexpected value for {key}"
            );
        }
    }

    #[test]
    fn apply_init_settings_is_idempotent() {
        let repo = TempGitRepo::new("init-settings-idempotent");
        apply_init_settings(repo.path()).expect("first apply");
        apply_init_settings(repo.path()).expect("second apply");

        for (key, expected) in INIT_CONFIG_SETTINGS {
            assert_eq!(git_config_get(repo.path(), key).as_deref(), Some(*expected));
        }
    }

    #[test]
    fn apply_init_settings_fails_clearly_outside_a_git_repo() {
        let dir = tempfile::Builder::new()
            .prefix("wkp-git-test-not-a-repo-")
            .tempdir()
            .expect("create temp dir");

        let result = apply_init_settings(dir.path());
        assert!(result.is_err(), "expected an error outside a git repo");
    }

    #[test]
    fn commit_staged_commits_only_what_was_explicitly_staged() {
        let repo = TempGitRepo::new("commit-staged");
        repo.write("item.md", "hello\n");
        repo.commit_all("initial");
        repo.write("staged.md", "staged\n");
        repo.write("not-staged.md", "not staged\n");
        crate::conflicts::stage_path(repo.path(), "staged.md").expect("stage_path");

        commit_staged(repo.path(), "only the staged file").expect("commit_staged");

        assert!(crate::plumbing::run_git_stdout(repo.path(), &["cat-file", "-e", "HEAD:staged.md"])
            .is_ok());
        assert!(crate::plumbing::run_git_stdout(
            repo.path(),
            &["cat-file", "-e", "HEAD:not-staged.md"]
        )
        .is_err());
    }
}
