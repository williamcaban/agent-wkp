#![forbid(unsafe_code)]

//! Git plumbing wrapper, bundles, and sync.
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! No `Command::new("git")` is allowed outside this crate (CLAUDE.md hard rule).
//! Implementation lands starting in M2; see `docs/plan/milestones.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod allowed_signers;
pub mod provenance;
pub mod signed_commit;
pub mod sync;

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

/// Which side deleted a path in a `CONFLICT (modify/delete)` (design 6.2,
/// M3-3). The *other* side's content is what a stopped `git merge` already
/// left in the working tree at that path -- confirmed against a real merge
/// in both directions before writing this function: git never leaves the
/// deleted side's (i.e. nothing's) content there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedBy {
    /// `git status --porcelain=v2` reports `DU` for this path: our side
    /// (`HEAD`) deleted it, the side being merged in modified it.
    Ours,
    /// `git status --porcelain=v2` reports `UD` for this path: the side
    /// being merged in deleted it, our side (`HEAD`) modified it.
    Theirs,
}

/// One path a stopped `git merge` left as an unmerged modify/delete
/// conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifyDeleteConflict {
    pub path: PathBuf,
    pub deleted_by: DeletedBy,
}

/// Scans `repo_dir` (expected to be mid-merge, i.e. `MERGE_HEAD` present
/// and some paths left unmerged) for modify/delete conflicts specifically
/// -- design 6.2's "a deletion always loses to a modification" needs its
/// own detection step because git's own three-way merge machinery stops
/// with `CONFLICT (modify/delete)` for this class *without* ever invoking
/// a content merge driver (there is no "theirs" -- or "ours" -- content to
/// hand one). This is why `wkp merge-driver` (M3-2) never sees these
/// paths: they never reach that protocol at all.
///
/// Every other unmerged class (`AA`/`DD`/`AU`/`UA`/`UU`) is out of this
/// function's scope and left alone rather than misclassified --
/// `wkp merge-driver`'s `.gitattributes` wiring already resolves add/add
/// and modify/modify frontmatter conflicts, and (per `wkp_core::merge`'s
/// own doc comment) that driver always succeeds, so those classes never
/// remain unmerged after a `git merge` returns in the first place.
pub fn modify_delete_conflicts(repo_dir: &Path) -> Result<Vec<ModifyDeleteConflict>, String> {
    let stdout = run_git_stdout(repo_dir, &["status", "--porcelain=v2"])?;
    let mut conflicts = Vec::new();
    for line in stdout.lines() {
        let Some((kind, rest)) = line.split_once(' ') else {
            continue;
        };
        if kind != "u" {
            continue;
        }
        // Unmerged entry: `<XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`.
        let fields: Vec<&str> = rest.splitn(10, ' ').collect();
        let (Some(xy), Some(path)) = (fields.first(), fields.get(9)) else {
            continue;
        };
        let deleted_by = match *xy {
            "DU" => DeletedBy::Ours,
            "UD" => DeletedBy::Theirs,
            _ => continue,
        };
        conflicts.push(ModifyDeleteConflict {
            path: PathBuf::from(*path),
            deleted_by,
        });
    }
    Ok(conflicts)
}

/// Stages `path` at whatever content is currently in the working tree
/// (`git add -- <path>`), resolving one [`ModifyDeleteConflict`] entry by
/// keeping the modification, or staging a brand-new file (e.g. an
/// inbox note re-proposing the deletion). Plain plumbing -- callers decide
/// what "the modification" or "the new file" actually is; this function
/// only touches the index.
pub fn stage_path(repo_dir: &Path, path: &str) -> Result<(), String> {
    run_git(repo_dir, &["add", "--", path])
}

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
/// index/working tree (inspect with e.g. [`modify_delete_conflicts`]) --
/// not an `Err`, since a conflicted merge is an expected, ordinary outcome
/// a caller needs to detect and resolve, not a plumbing failure.
///
/// Passes a placeholder `-c user.name`/`-c user.email` the same way
/// [`commit_all`] does: found the hard way (a CI run failed where a local
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
/// remaining conflict for (M3-3's [`modify_delete_conflicts`]/[`stage_path`]
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

/// Like [`run_git_stdout`], but feeds `stdin_data` to the child process's
/// stdin rather than assuming the command needs none -- `git
/// interpret-trailers` (design 5.4/7.4's provenance-trailer machinery,
/// M2-3) reads the message it formats or parses from stdin when given no
/// `<file>` argument, and this crate would rather use git's own trailer
/// logic than re-derive the subject/trailer-block formatting rules itself.
fn run_git_with_stdin(repo_dir: &Path, args: &[&str], stdin_data: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .expect("child spawned with Stdio::piped() stdin")
        .write_all(stdin_data.as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
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

/// Removes `path` from `repo_dir`'s git index (`git update-index
/// --remove -- <path>`), without touching the working tree itself --
/// `wkp promote` (M2-7, design 7.4) uses this to stage the "old"
/// half of a move (the caller deletes the file on disk first; `--remove`
/// is what makes `update-index` accept a path that no longer exists
/// there, rather than erroring on a missing file). Followed by
/// [`signed_commit::signed_commit`] for the "new" half, so both halves
/// of the move land in one commit -- `write-tree` inside that call
/// serializes whatever the index holds at that point, picking up this
/// removal and the new path's addition together.
pub fn remove_from_index(repo_dir: &Path, path: &str) -> Result<(), String> {
    run_git(repo_dir, &["update-index", "--remove", "--", path])
}

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

        /// The name of whichever branch `git init` chose (this environment's
        /// `init.defaultBranch`) -- valid even before the first commit, since
        /// `HEAD` is already a symbolic ref to it. Modify/delete tests need
        /// two real, named branches to diverge and merge, so they read this
        /// rather than assuming `main`/`master`.
        fn current_branch(&self) -> String {
            current_branch(self.path()).expect("current_branch")
        }

        fn checkout_new_branch(&self, name: &str) {
            checkout_branch(self.path(), name, true).expect("checkout_branch (create)");
        }

        fn checkout(&self, name: &str) {
            checkout_branch(self.path(), name, false).expect("checkout_branch");
        }

        fn remove_and_commit(&self, relative_path: &str, message: &str) {
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
        fn merge(&self, branch: &str) -> bool {
            merge_branch(self.path(), branch).expect("merge_branch")
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
    fn init_repo_always_names_the_initial_branch_main() {
        let temp = tempfile::Builder::new()
            .prefix("wkp-git-test-init-repo-branch-name-")
            .tempdir()
            .expect("create temp dir");
        init_repo(temp.path()).expect("init_repo");
        assert_eq!(
            current_branch(temp.path()).expect("current_branch"),
            "main",
            "wkp sync depends on `main` being a fixed name, not the host's init.defaultBranch"
        );
    }

    /// `remove_from_index` staged alone (no accompanying commit) is
    /// still observable via `write-tree`: the path disappears from the
    /// tree it produces. The real "one commit for both halves of a
    /// move" behavior is exercised by `wkp-cli`'s own `run_promote`
    /// tests, the actual consumer of this function.
    #[test]
    fn remove_from_index_drops_a_path_from_the_next_write_tree() {
        let temp = tempfile::Builder::new()
            .prefix("wkp-git-test-remove-from-index-")
            .tempdir()
            .expect("create temp dir");
        let dir = temp.path();
        init_repo(dir).expect("init_repo");
        std::fs::write(dir.join("a.md"), "hello\n").expect("write a.md");
        run_git(dir, &["add", "a.md"]).expect("git add");
        let tree_with = run_git_stdout(dir, &["write-tree"]).expect("write-tree with a.md");
        assert!(!tree_with.trim().is_empty());

        std::fs::remove_file(dir.join("a.md")).expect("delete a.md from disk");
        remove_from_index(dir, "a.md").expect("remove_from_index");
        let tree_without = run_git_stdout(dir, &["write-tree"]).expect("write-tree without a.md");

        assert_ne!(
            tree_with.trim(),
            tree_without.trim(),
            "the tree must change once a.md is removed from the index"
        );
        let ls_tree = run_git_stdout(dir, &["ls-tree", "-r", "--name-only", tree_without.trim()])
            .expect("ls-tree");
        assert!(!ls_tree.contains("a.md"));
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

    /// Sets up two branches that both diverge from a shared base commit --
    /// one modifies `item.md`, the other deletes it -- and merges the
    /// second into the repo's current branch, which must stop with a real
    /// `CONFLICT (modify/delete)` rather than auto-resolving. Shared setup
    /// for both directions of the conflict, since only which branch does
    /// which differs between them.
    fn set_up_modify_delete_conflict(name: &str, main_deletes: bool) -> TempGitRepo {
        let repo = TempGitRepo::new(name);
        let main = repo.current_branch();
        repo.write("item.md", "base content\n");
        repo.commit_all("base");
        repo.checkout_new_branch("feature");

        if main_deletes {
            repo.checkout(&main);
            repo.remove_and_commit("item.md", "main deletes");
            repo.checkout("feature");
            repo.write("item.md", "modified by feature\n");
            repo.commit_all("feature modifies");
        } else {
            repo.write("item.md", "modified by feature\n");
            repo.commit_all("feature modifies");
            repo.checkout(&main);
            repo.remove_and_commit("item.md", "main deletes");
        }

        repo.checkout(&main);
        assert!(
            !repo.merge("feature"),
            "a modify/delete conflict must stop the merge, not auto-resolve it"
        );
        repo
    }

    #[test]
    fn modify_delete_conflicts_detects_deleted_by_theirs_and_leaves_the_modification_in_place() {
        // main modifies, feature deletes, feature is merged in: from main's
        // point of view the deletion came from "them".
        let repo = TempGitRepo::new("modify-delete-theirs");
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

        let conflicts = modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert_eq!(
            conflicts,
            vec![ModifyDeleteConflict {
                path: PathBuf::from("item.md"),
                deleted_by: DeletedBy::Theirs,
            }]
        );

        let on_disk = std::fs::read_to_string(repo.path().join("item.md")).expect("read item.md");
        assert_eq!(
            on_disk, "modified by main\n",
            "git itself already keeps the modification in the working tree"
        );
    }

    #[test]
    fn modify_delete_conflicts_detects_deleted_by_ours() {
        // main deletes, feature modifies, feature is merged in: from main's
        // point of view the deletion came from "us".
        let repo = set_up_modify_delete_conflict("modify-delete-ours", true);

        let conflicts = modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert_eq!(
            conflicts,
            vec![ModifyDeleteConflict {
                path: PathBuf::from("item.md"),
                deleted_by: DeletedBy::Ours,
            }]
        );

        let on_disk = std::fs::read_to_string(repo.path().join("item.md")).expect("read item.md");
        assert_eq!(on_disk, "modified by feature\n");
    }

    #[test]
    fn modify_delete_conflicts_is_empty_on_a_clean_repo() {
        let repo = TempGitRepo::new("modify-delete-clean");
        repo.write("item.md", "hello\n");
        repo.commit_all("initial");

        let conflicts = modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    #[test]
    fn stage_path_resolves_a_modify_delete_conflict() {
        let repo = set_up_modify_delete_conflict("modify-delete-stage", false);

        stage_path(repo.path(), "item.md").expect("stage_path");

        let remaining = modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert!(
            remaining.is_empty(),
            "item.md should no longer be unmerged after staging: {remaining:?}"
        );
    }

    /// M3-2's `wkp merge-driver`/`.gitattributes` wiring never sees a
    /// modify/delete conflict at all -- git's own merge machinery stops
    /// before handing this class to any content driver. This test proves
    /// that boundary rather than assuming it: `merge.wkp.driver` is
    /// configured to a command that would leave a detectable side effect
    /// if it ever actually ran, and after a real modify/delete merge stops,
    /// that side effect must be absent.
    #[test]
    fn merge_driver_is_never_invoked_for_a_modify_delete_conflict() {
        let repo = TempGitRepo::new("modify-delete-driver-boundary");
        repo.write(".gitattributes", "*.md merge=wkp\n");
        let sentinel = repo.path().join("driver-was-invoked");
        set_local_config(repo.path(), "merge.wkp.name", "test sentinel driver")
            .expect("set merge.wkp.name");
        set_local_config(
            repo.path(),
            "merge.wkp.driver",
            &format!("touch {}", sentinel.display()),
        )
        .expect("set merge.wkp.driver");

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
        assert!(
            !sentinel.exists(),
            "the configured merge driver must never run for a modify/delete conflict"
        );

        let conflicts = modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert_eq!(conflicts.len(), 1);
    }

    /// A bare repo standing in for "the hub, or any git server the user
    /// already trusts" (design 6.1) -- the target of [`fetch`]/[`push_branch`]
    /// in these tests, never opened as a working tree itself.
    fn bare_remote(name: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("wkp-git-test-bare-{name}-"))
            .tempdir()
            .expect("create temp dir");
        init_bare_repo(dir.path()).expect("init_bare_repo");
        dir
    }

    impl TempGitRepo {
        fn add_remote(&self, name: &str, url: &Path) {
            set_local_config(
                self.path(),
                &format!("remote.{name}.url"),
                &url.to_string_lossy(),
            )
            .expect("set remote url");
            set_local_config(
                self.path(),
                &format!("remote.{name}.fetch"),
                &format!("+refs/heads/*:refs/remotes/{name}/*"),
            )
            .expect("set remote fetch refspec");
        }
    }

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
        let conflicts = modify_delete_conflicts(repo.path()).expect("modify_delete_conflicts");
        assert_eq!(conflicts.len(), 1);
        stage_path(repo.path(), "item.md").expect("stage_path");

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
}
