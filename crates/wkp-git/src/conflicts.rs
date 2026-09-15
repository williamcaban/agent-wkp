//! Merge-conflict detection and resolution primitives (design 6.2's
//! safe-mode strategy): the general [`conflicts`] scan `wkp sync status`
//! (M3-6) reports from, and [`modify_delete_conflicts`] (M3-3), the
//! narrower view the merge loop itself needs.

use std::path::{Path, PathBuf};

use crate::plumbing::{run_git, run_git_stdout};

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

/// Which class of unmerged entry `git status --porcelain=v2` reports a
/// path as (M3-6, `wkp sync status`'s "ask" layer of design 6.2's
/// strategy: report whatever `wkp merge-driver` (M3-2) and
/// [`modify_delete_conflicts`] (M3-3) didn't already auto-resolve, rather
/// than assume only the modify/delete class can ever remain -- a content
/// conflict on a path `.gitattributes` doesn't scope to the custom driver
/// (e.g. any non-`.md` path) genuinely can stay unresolved this way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// `UD`/`DU`: design 6.2's "deletion always loses to a modification"
    /// class -- `wkp sync`/`wkp bundle import` already resolve this
    /// automatically via [`modify_delete_conflicts`]; seeing it here means
    /// something interrupted that resolution before it finished.
    ModifyDelete { deleted_by: DeletedBy },
    /// `UU`: both sides modified the path's content. Auto-resolved by
    /// `wkp merge-driver` for any `*.md` path; a real, still-open conflict
    /// here means the path is outside that scope.
    Content,
    /// `AA`: both sides independently created the path.
    AddAdd,
    /// `DD`: both sides deleted the path (nothing to keep either way).
    BothDeleted,
    /// `AU`/`UA`: added on one side only, alongside an unrelated change
    /// that still needs reconciling on the other -- rare in practice, not
    /// one of the named classes above.
    Other,
}

/// One path currently left in an unmerged (conflicted) state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: PathBuf,
    pub kind: ConflictKind,
}

/// Scans `repo_dir` for every path `git status --porcelain=v2` currently
/// reports as unmerged, classified by [`ConflictKind`] -- the general
/// form [`modify_delete_conflicts`] is now built on. `wkp sync status`
/// (M3-6, design 6.2 point 3's "ask") calls this directly to report
/// *anything* left open, not just the modify/delete class; `wkp
/// sync`/`wkp bundle import`'s own merge loop only ever needs the
/// modify/delete-scoped view.
pub fn conflicts(repo_dir: &Path) -> Result<Vec<Conflict>, String> {
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
        let kind = match *xy {
            "DU" => ConflictKind::ModifyDelete {
                deleted_by: DeletedBy::Ours,
            },
            "UD" => ConflictKind::ModifyDelete {
                deleted_by: DeletedBy::Theirs,
            },
            "UU" => ConflictKind::Content,
            "AA" => ConflictKind::AddAdd,
            "DD" => ConflictKind::BothDeleted,
            _ => ConflictKind::Other,
        };
        conflicts.push(Conflict {
            path: PathBuf::from(*path),
            kind,
        });
    }
    Ok(conflicts)
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
/// remain unmerged after a `git merge` returns in the first place. See
/// [`conflicts`] for the general form covering every class, used by `wkp
/// sync status` (M3-6).
pub fn modify_delete_conflicts(repo_dir: &Path) -> Result<Vec<ModifyDeleteConflict>, String> {
    Ok(conflicts(repo_dir)?
        .into_iter()
        .filter_map(|c| match c.kind {
            ConflictKind::ModifyDelete { deleted_by } => Some(ModifyDeleteConflict {
                path: c.path,
                deleted_by,
            }),
            _ => None,
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempGitRepo;

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
        crate::set_local_config(repo.path(), "merge.wkp.name", "test sentinel driver")
            .expect("set merge.wkp.name");
        crate::set_local_config(
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

    #[test]
    fn conflicts_is_empty_on_a_clean_repo() {
        let repo = TempGitRepo::new("conflicts-clean");
        repo.write("item.md", "hello\n");
        repo.commit_all("initial");
        assert!(conflicts(repo.path()).expect("conflicts").is_empty());
    }

    #[test]
    fn conflicts_reports_a_real_content_conflict_on_a_non_md_path() {
        // `.gitattributes: *.md merge=wkp` only scopes the custom driver
        // to `.md` paths (M3-2) -- a plain text file that both branches
        // genuinely change differently is exactly the class `wkp merge
        // -driver` can't touch and `modify_delete_conflicts` doesn't
        // cover either, so it must stay unmerged for `wkp sync status`
        // (M3-6) to have something real to report.
        let repo = TempGitRepo::new("conflicts-content");
        repo.write(".gitattributes", "*.md merge=wkp\n");
        crate::set_local_config(repo.path(), "merge.wkp.name", "test driver").expect("set config");
        crate::set_local_config(repo.path(), "merge.wkp.driver", "true %A").expect("set config");
        let main = repo.current_branch();
        repo.write("notes.txt", "base\n");
        repo.commit_all("base");
        repo.checkout_new_branch("feature");
        repo.checkout(&main);
        repo.write("notes.txt", "changed by main\n");
        repo.commit_all("main changes");
        repo.checkout("feature");
        repo.write("notes.txt", "changed by feature\n");
        repo.commit_all("feature changes");
        repo.checkout(&main);

        assert!(!repo.merge("feature"), "expected a real content conflict");

        let found = conflicts(repo.path()).expect("conflicts");
        assert_eq!(
            found,
            vec![Conflict {
                path: PathBuf::from("notes.txt"),
                kind: ConflictKind::Content,
            }]
        );
    }

    #[test]
    fn conflicts_still_reports_modify_delete_paths_alongside_the_general_view() {
        let repo = TempGitRepo::new("conflicts-modify-delete");
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

        let found = conflicts(repo.path()).expect("conflicts");
        assert_eq!(
            found,
            vec![Conflict {
                path: PathBuf::from("item.md"),
                kind: ConflictKind::ModifyDelete {
                    deleted_by: DeletedBy::Theirs
                },
            }]
        );
    }
}
