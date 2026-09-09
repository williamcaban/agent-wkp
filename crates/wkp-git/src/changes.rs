//! Working-tree change detection (design 5.1): what `wkp index` needs to
//! know changed since the last incremental index build.

use std::path::{Path, PathBuf};

use crate::plumbing::{run_git, run_git_stdout, run_git_stdout_bytes};

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

/// Reads the exact bytes of the object at `rev_path` (e.g.
/// `HEAD:secret.md`, `<sha>:path`) via `git cat-file -p` -- the object
/// database's content exactly as committed, before any smudge filter a
/// real checkout would apply. Raw bytes, not a lossy-UTF8 `String`: a
/// `visibility: private` blob's content is age ciphertext, arbitrary
/// binary data a lossy conversion would corrupt.
pub fn read_blob(repo_dir: &Path, rev_path: &str) -> Result<Vec<u8>, String> {
    run_git_stdout_bytes(repo_dir, &["cat-file", "-p", rev_path])
}

/// Every path tracked at `rev` (e.g. `HEAD`, a branch, a sha), via `git
/// ls-tree -r --name-only` -- unlike [`list_tracked_files`], this walks a
/// commit's tree object directly rather than reading the index/working
/// tree, so it also works against a bare repo (design 8.2, M5-4's tenant
/// indexer: a freshly pushed-to bare repo has no working tree or index to
/// read at all).
pub fn list_files_at_ref(repo_dir: &Path, rev: &str) -> Result<Vec<PathBuf>, String> {
    let stdout = run_git_stdout(repo_dir, &["ls-tree", "-r", "--name-only", rev])?;
    Ok(stdout.lines().map(PathBuf::from).collect())
}

/// Removes `path` from `repo_dir`'s git index (`git update-index
/// --remove -- <path>`), without touching the working tree itself --
/// `wkp promote` (M2-7, design 7.4) uses this to stage the "old"
/// half of a move (the caller deletes the file on disk first; `--remove`
/// is what makes `update-index` accept a path that no longer exists
/// there, rather than erroring on a missing file). Followed by
/// [`crate::signed_commit::signed_commit`] for the "new" half, so both halves
/// of the move land in one commit -- `write-tree` inside that call
/// serializes whatever the index holds at that point, picking up this
/// removal and the new path's addition together.
pub fn remove_from_index(repo_dir: &Path, path: &str) -> Result<(), String> {
    run_git(repo_dir, &["update-index", "--remove", "--", path])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plumbing::{run_git, run_git_stdout};
    use crate::test_support::TempGitRepo;

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
        crate::init_repo(dir).expect("init_repo");
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
}
