#![forbid(unsafe_code)]

//! Git plumbing wrapper, bundles, and sync.
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! No `Command::new("git")` is allowed outside this crate (CLAUDE.md hard rule).
//! Implementation lands starting in M2; see `docs/plan/milestones.md`.

use std::path::Path;
use std::process::Command;

/// Minimum git version `wkp` requires. Decided in `docs/adr/0001-git-minimum-version.md`:
/// SSH commit signing (`gpg.format = ssh`) needs git >= 2.34, and that is the
/// only hard floor. The builtin fsmonitor daemon is opportunistic (wired up
/// in M1), not part of this gate.
pub const MIN_GIT_VERSION: (u32, u32, u32) = (2, 34, 0);

#[derive(Debug, PartialEq, Eq)]
pub enum GitVersionCheck {
    Ok { found: (u32, u32, u32) },
    TooOld { found: (u32, u32, u32) },
    Unparseable { raw: String },
    NotFound,
}

/// Runs `git --version` and checks it against [`MIN_GIT_VERSION`].
pub fn check_git_version() -> GitVersionCheck {
    let output = match Command::new("git").arg("--version").output() {
        Ok(o) => o,
        Err(_) => return GitVersionCheck::NotFound,
    };
    if !output.status.success() {
        return GitVersionCheck::NotFound;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match parse_git_version(&raw) {
        Some(found) if found >= MIN_GIT_VERSION => GitVersionCheck::Ok { found },
        Some(found) => GitVersionCheck::TooOld { found },
        None => GitVersionCheck::Unparseable { raw },
    }
}

/// Checks the installed git against [`MIN_GIT_VERSION`], returning a
/// human-readable error naming the found and required versions if it does
/// not meet the floor, is missing, or can't be parsed.
pub fn ensure_min_git_version() -> Result<(), String> {
    let (req_major, req_minor, req_patch) = MIN_GIT_VERSION;
    match check_git_version() {
        GitVersionCheck::Ok { .. } => Ok(()),
        GitVersionCheck::TooOld {
            found: (major, minor, patch),
        } => Err(format!(
            "wkp: git {major}.{minor}.{patch} found, but wkp requires git >= \
             {req_major}.{req_minor}.{req_patch} (needed for SSH commit signing; see \
             docs/adr/0001-git-minimum-version.md). Upgrade git and try again."
        )),
        GitVersionCheck::Unparseable { raw } => Err(format!(
            "wkp: could not parse a version from `git --version` output {raw:?}; wkp requires \
             git >= {req_major}.{req_minor}.{req_patch}."
        )),
        GitVersionCheck::NotFound => Err(format!(
            "wkp: git not found on PATH; wkp requires git >= {req_major}.{req_minor}.{req_patch}."
        )),
    }
}

/// Parses `git version X.Y[.Z][ (platform suffix)]` into a `(major, minor, patch)`
/// triple. Missing patch defaults to 0; a non-numeric trailing platform
/// suffix (e.g. Apple Git's `2.39.3 (Apple Git-146)`) is dropped.
fn parse_git_version(raw: &str) -> Option<(u32, u32, u32)> {
    let version_str = raw.strip_prefix("git version ")?;
    let core = version_str.split_whitespace().next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        None => 0,
        Some(p) => {
            let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                0
            } else {
                digits.parse().ok()?
            }
        }
    };
    Some((major, minor, patch))
}

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
pub fn init_repo(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    run_git(path, &["init", "--quiet"])
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
pub fn apply_init_settings(repo_dir: &Path) -> Result<(), String> {
    for (key, value) in INIT_CONFIG_SETTINGS {
        run_git(repo_dir, &["config", "--local", key, value])
            .map_err(|e| format!("wkp: failed to set git config {key}={value}: {e}"))?;
    }
    // Best-effort only; see doc comment above.
    let _ = run_git(repo_dir, &["maintenance", "start"]);
    Ok(())
}

fn run_git(repo_dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_version() {
        assert_eq!(parse_git_version("git version 2.55.0"), Some((2, 55, 0)));
    }

    #[test]
    fn parses_platform_suffixed_version() {
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parses_two_component_version() {
        assert_eq!(parse_git_version("git version 2.34"), Some((2, 34, 0)));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_git_version("not git at all"), None);
    }

    #[test]
    fn min_version_ordering() {
        assert!(MIN_GIT_VERSION == (2, 34, 0));
        assert!((2, 55, 0) >= MIN_GIT_VERSION);
        assert!((2, 33, 9) < MIN_GIT_VERSION);
    }

    #[test]
    fn ensure_min_git_version_passes_on_this_dev_machine() {
        // This crate's own CI and dev environments run a git new enough to
        // sign commits over SSH (see rust-toolchain.toml neighbors: CI baseline
        // in M0 task 2 runs on runners with git 2.55). If this ever fails in
        // CI, the runner's git dropped below our floor, which is itself
        // worth knowing about.
        assert!(ensure_min_git_version().is_ok());
    }

    struct TempGitRepo {
        path: std::path::PathBuf,
    }

    impl TempGitRepo {
        fn new(name: &str) -> Self {
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("wkp-git-test-{name}-{pid}-{nanos}"));
            std::fs::create_dir_all(&path).expect("create temp dir");
            let status = Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["init", "--quiet"])
                .status()
                .expect("run git init");
            assert!(status.success(), "git init failed");
            Self { path }
        }
    }

    impl Drop for TempGitRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

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
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("wkp-git-test-init-repo-{pid}-{nanos}"));
        // Deliberately does not exist yet; init_repo must create it.
        init_repo(&dir).expect("init_repo");
        assert!(dir.join(".git").is_dir());
        // Idempotent: a second call on the same path must not error.
        init_repo(&dir).expect("init_repo again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_init_settings_writes_expected_git_config() {
        let repo = TempGitRepo::new("init-settings");
        apply_init_settings(&repo.path).expect("apply_init_settings");

        for (key, expected) in INIT_CONFIG_SETTINGS {
            assert_eq!(
                git_config_get(&repo.path, key).as_deref(),
                Some(*expected),
                "unexpected value for {key}"
            );
        }
    }

    #[test]
    fn apply_init_settings_is_idempotent() {
        let repo = TempGitRepo::new("init-settings-idempotent");
        apply_init_settings(&repo.path).expect("first apply");
        apply_init_settings(&repo.path).expect("second apply");

        for (key, expected) in INIT_CONFIG_SETTINGS {
            assert_eq!(git_config_get(&repo.path, key).as_deref(), Some(*expected));
        }
    }

    #[test]
    fn apply_init_settings_fails_clearly_outside_a_git_repo() {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("wkp-git-test-not-a-repo-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let result = apply_init_settings(&dir);
        assert!(result.is_err(), "expected an error outside a git repo");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
