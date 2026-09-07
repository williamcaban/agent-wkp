#![forbid(unsafe_code)]

//! Git plumbing wrapper, bundles, and sync.
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! No `Command::new("git")` is allowed outside this crate (CLAUDE.md hard rule).
//! Implementation lands starting in M2; see `docs/plan/milestones.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod allowed_signers;

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

/// Which store files changed in the working tree, relative to git's own
/// index (design 5.1: "which files changed" from cached stat metadata,
/// not a full-corpus content read). Renames are reported distinctly so a
/// caller can move an index row instead of deleting and re-inserting it,
/// but treating a rename as delete-then-add is also correct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub added: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub renamed: Vec<Renamed>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renamed {
    pub from: PathBuf,
    pub to: PathBuf,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
            && self.renamed.is_empty()
    }
}

/// Detects which tracked or untracked files changed in `repo_dir`'s working
/// tree, using git's own stat cache rather than reading every file's
/// content (design 5.1). `git update-index --refresh` updates that cache
/// from cheap stat metadata (size, mtime, inode) before `git status
/// --porcelain=v2` reports the result; a repo with `core.fsmonitor`
/// enabled skips even the stat walk transparently to this function, since
/// fsmonitor is consulted internally by `git status` itself; there is no
/// separate code path here for it (ADR-0001: fsmonitor stays opportunistic).
///
/// A known limitation: paths containing characters `core.quotePath` would
/// quote (non-ASCII, embedded quotes/tabs/newlines) are not unquoted here.
/// Store paths are expected to be ordinary filenames; revisit if that
/// stops being true.
pub fn detect_changes(repo_dir: &Path) -> Result<ChangeSet, String> {
    // Best-effort: `--refresh` can exit non-zero for a file it can't
    // confirm clean from stat alone (rare), which isn't fatal here --
    // `status` below still produces a correct answer either way, just
    // possibly slower for that one file.
    let _ = run_git(repo_dir, &["update-index", "-q", "--refresh"]);

    let stdout = run_git_stdout(
        repo_dir,
        &["status", "--porcelain=v2", "--untracked-files=all"],
    )?;
    Ok(parse_porcelain_v2(&stdout))
}

fn parse_porcelain_v2(output: &str) -> ChangeSet {
    let mut changes = ChangeSet::default();
    for line in output.lines() {
        let Some((kind, rest)) = line.split_once(' ') else {
            continue;
        };
        match kind {
            // Ordinary changed entry: `<XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
            "1" => {
                let fields: Vec<&str> = rest.splitn(8, ' ').collect();
                let (Some(xy), Some(path)) = (fields.first(), fields.get(7)) else {
                    continue;
                };
                classify_ordinary(xy, PathBuf::from(*path), &mut changes);
            }
            // Renamed/copied entry:
            // `<XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<origPath>`
            "2" => {
                let fields: Vec<&str> = rest.splitn(9, ' ').collect();
                let Some(tail) = fields.get(8) else {
                    continue;
                };
                if let Some((to, from)) = tail.split_once('\t') {
                    changes.renamed.push(Renamed {
                        from: PathBuf::from(from),
                        to: PathBuf::from(to),
                    });
                }
            }
            // Untracked file.
            "?" => changes.added.push(PathBuf::from(rest)),
            // Unmerged entry: `<XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`.
            // Conservative: treat a conflict as needing reindexing rather
            // than skipping it.
            "u" => {
                let fields: Vec<&str> = rest.splitn(10, ' ').collect();
                if let Some(path) = fields.get(9) {
                    changes.modified.push(PathBuf::from(*path));
                }
            }
            // Ignored entries only appear with `--ignored`, which isn't
            // passed; header lines only appear with `--branch`, also not
            // passed. Anything else is unrecognized and skipped rather
            // than guessed at.
            _ => {}
        }
    }
    changes
}

fn classify_ordinary(xy: &str, path: PathBuf, changes: &mut ChangeSet) {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    if x == 'D' || y == 'D' {
        changes.deleted.push(path);
    } else if x == 'A' {
        changes.added.push(path);
    } else {
        // M (modified) or T (typechange) on either side; anything else
        // unrecognized is still treated as "needs reindexing" rather than
        // silently skipped.
        changes.modified.push(path);
    }
}

fn run_git_stdout(repo_dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Every path git currently tracks in `repo_dir` (its index/staging area,
/// which for a repo with nothing staged is the same as HEAD's tree).
/// Cheap: reads git's own index metadata, not file content. Combined with
/// [`ChangeSet::added`]'s untracked entries, this gives the full current
/// set of store paths -- needed alongside [`detect_changes`] because a
/// file can be "in the working tree but never indexed" (a fresh clone, or
/// the first `wkp index` run after `wkp init`) without git considering it
/// changed at all, since it may already be fully committed.
pub fn list_tracked_files(repo_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let stdout = run_git_stdout(repo_dir, &["ls-files"])?;
    Ok(stdout.lines().map(PathBuf::from).collect())
}

/// Whether `core.fsmonitor` is enabled for `repo_dir`. Purely
/// informational (e.g. for tests or diagnostics): [`detect_changes`]
/// behaves identically either way, since `git status` consults fsmonitor
/// internally when configured (ADR-0001).
pub fn fsmonitor_enabled(repo_dir: &Path) -> bool {
    matches!(
        run_git_stdout(repo_dir, &["config", "--get", "core.fsmonitor"]),
        Ok(value) if value.trim() == "true"
    )
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

    /// `tempfile` rather than `std::env::temp_dir()` + a predictable name:
    /// the latter is flagged by this repo's semgrep gate as an
    /// insecure-temp-file pattern (a shared temp directory with a
    /// guessable name invites symlink/TOCTOU races).
    struct TempGitRepo {
        dir: tempfile::TempDir,
    }

    impl TempGitRepo {
        fn new(name: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("wkp-git-test-{name}-"))
                .tempdir()
                .expect("create temp dir");
            let status = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["init", "--quiet"])
                .status()
                .expect("run git init");
            assert!(status.success(), "git init failed");
            Self { dir }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, relative_path: &str, contents: &str) {
            let full = self.path().join(relative_path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(full, contents).expect("write file");
        }

        fn commit_all(&self, message: &str) {
            commit_all(self.path(), message).expect("commit_all");
        }
    }

    #[test]
    fn detect_changes_reports_nothing_on_a_clean_repo() {
        let repo = TempGitRepo::new("clean");
        repo.write("a.md", "hello\n");
        repo.commit_all("initial");

        let changes = detect_changes(repo.path()).expect("detect_changes");
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn detect_changes_reports_an_untracked_added_file() {
        let repo = TempGitRepo::new("added");
        repo.write("a.md", "hello\n");
        repo.commit_all("initial");
        repo.write("b.md", "new file\n");

        let changes = detect_changes(repo.path()).expect("detect_changes");
        assert_eq!(changes.added, vec![PathBuf::from("b.md")]);
        assert!(changes.modified.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn detect_changes_reports_a_modified_tracked_file() {
        let repo = TempGitRepo::new("modified");
        repo.write("a.md", "hello\n");
        repo.commit_all("initial");
        repo.write("a.md", "hello, edited\n");

        let changes = detect_changes(repo.path()).expect("detect_changes");
        assert_eq!(changes.modified, vec![PathBuf::from("a.md")]);
        assert!(changes.added.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn detect_changes_reports_a_deleted_tracked_file() {
        let repo = TempGitRepo::new("deleted");
        repo.write("a.md", "hello\n");
        repo.commit_all("initial");
        std::fs::remove_file(repo.path().join("a.md")).expect("remove file");

        let changes = detect_changes(repo.path()).expect("detect_changes");
        assert_eq!(changes.deleted, vec![PathBuf::from("a.md")]);
        assert!(changes.added.is_empty());
        assert!(changes.modified.is_empty());
    }

    #[test]
    fn detect_changes_reports_a_renamed_tracked_file() {
        let repo = TempGitRepo::new("renamed");
        // Content long/distinctive enough that git's rename heuristic
        // (similarity index) reliably detects the rename rather than
        // reporting a plain delete+add.
        let body = "hello world, this is a fairly long body of text that git's \
                     similarity-index rename detector should recognize as the \
                     same content under a new name.\n";
        repo.write("a.md", body);
        repo.commit_all("initial");
        std::fs::rename(repo.path().join("a.md"), repo.path().join("b.md")).expect("rename file");
        run_git(repo.path(), &["add", "-A"]).expect("stage the rename");

        let changes = detect_changes(repo.path()).expect("detect_changes");
        assert_eq!(
            changes.renamed,
            vec![Renamed {
                from: PathBuf::from("a.md"),
                to: PathBuf::from("b.md"),
            }]
        );
        assert!(changes.added.is_empty());
        assert!(changes.modified.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn fsmonitor_enabled_reflects_repo_config() {
        let repo = TempGitRepo::new("fsmonitor");
        assert!(!fsmonitor_enabled(repo.path()));

        run_git(repo.path(), &["config", "core.fsmonitor", "true"]).expect("enable fsmonitor");
        assert!(fsmonitor_enabled(repo.path()));

        // detect_changes must behave identically either way -- fsmonitor
        // is a transparent optimization inside `git status`, not a
        // separate code path here.
        repo.write("a.md", "hello\n");
        repo.commit_all("initial");
        repo.write("a.md", "edited\n");
        let changes = detect_changes(repo.path()).expect("detect_changes");
        assert_eq!(changes.modified, vec![PathBuf::from("a.md")]);
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
}
