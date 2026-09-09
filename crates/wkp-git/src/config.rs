//! Generic local git config get/set, plus `core.fsmonitor` introspection.

use std::path::Path;

use crate::plumbing::{run_git, run_git_stdout};

/// Sets a single local (per-repository, not global) git config key.
/// `apply_init_settings`/`allowed_signers::configure_ssh_signing` already
/// set config this same way internally; this is the generic, public
/// version other crates need for a one-off key -- the merge-driver
/// wiring (M3-2: `merge.wkp.name`/`merge.wkp.driver`), so far the only
/// external caller.
pub fn set_local_config(repo_dir: &Path, key: &str, value: &str) -> Result<(), String> {
    run_git(repo_dir, &["config", "--local", key, value])
}

/// Reads a single local git config key, `None` if it isn't set at all
/// (not an error -- an absent key is a completely normal, expected
/// state for optional configuration like the merge driver's own
/// settings before `wkp init` has run).
pub fn get_local_config(repo_dir: &Path, key: &str) -> Option<String> {
    run_git_stdout(repo_dir, &["config", "--local", "--get", key])
        .ok()
        .map(|s| s.trim().to_string())
}

/// Whether `core.fsmonitor` is enabled for `repo_dir`. Purely
/// informational (e.g. for tests or diagnostics): [`crate::detect_changes`]
/// behaves identically either way, since `git status` consults fsmonitor
/// internally when configured (ADR-0001).
pub fn fsmonitor_enabled(repo_dir: &Path) -> bool {
    matches!(
        run_git_stdout(repo_dir, &["config", "--get", "core.fsmonitor"]),
        Ok(value) if value.trim() == "true"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempGitRepo;

    #[test]
    fn fsmonitor_enabled_reflects_repo_config() {
        let repo = TempGitRepo::new("fsmonitor");
        assert!(!fsmonitor_enabled(repo.path()));

        crate::plumbing::run_git(repo.path(), &["config", "core.fsmonitor", "true"])
            .expect("enable fsmonitor");
        assert!(fsmonitor_enabled(repo.path()));

        // detect_changes must behave identically either way -- fsmonitor
        // is a transparent optimization inside `git status`, not a
        // separate code path here.
        repo.write("a.md", "hello\n");
        repo.commit_all("initial");
        repo.write("a.md", "edited\n");
        let changes = crate::detect_changes(repo.path()).expect("detect_changes");
        assert_eq!(changes.modified, vec![std::path::PathBuf::from("a.md")]);
    }
}
