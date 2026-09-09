//! `wkp resolve-conflicts`: M3-3's modify-vs-delete safe-mode handling
//! (design 6.2's "a deletion always loses to a modification").

use std::path::{Path, PathBuf};

/// `wkp resolve-conflicts [path]`: only flag is an optional store path
/// (default: cwd), matching `wkp index`/`wkp materialize`'s convention.
pub(crate) fn parse_resolve_conflicts_args(
    args: impl Iterator<Item = String>,
) -> Result<PathBuf, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    for arg in args {
        match arg.as_str() {
            other if !other.starts_with('-') => path = PathBuf::from(other),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(path)
}

/// M3-3 (design 6.2's "a deletion always loses to a modification"):
/// resolves every unmerged modify/delete conflict a stopped `git merge`
/// left in `repo_dir`. `wkp merge-driver` never sees this conflict class
/// at all -- git's own merge machinery stops with `CONFLICT (modify/delete)`
/// before handing it to any content driver -- so it needs this separate
/// step. Git itself already leaves the modification's content in the
/// working tree at each conflicted path (`wkp_git::modify_delete_conflicts`'s
/// own doc comment: confirmed against a real merge in both directions), so
/// "keep the modification" here means staging it (`git add`), not
/// reconstructing anything. The deleted side is never silently dropped:
/// each conflict gets a new `inbox/<slug>-<nanos>.md` item (CLAUDE.md:
/// agent-written memory always lands in `inbox/` with `confidence:
/// proposed`) noting what was deleted and by which side, for a human to
/// confirm before the deletion is ever re-applied.
///
/// Returns the repo-relative path of each inbox item created, one per
/// resolved conflict. Deliberately does not commit anything -- finishing
/// the merge commit is `wkp sync`'s job (M3-4): a modify/delete conflict
/// can coexist with other paths a device-branch merge is still assembling,
/// and this function only knows how to resolve this one conflict class.
pub(crate) fn resolve_modify_delete_conflicts(repo_dir: &Path) -> Result<Vec<String>, String> {
    let conflicts = wkp_git::modify_delete_conflicts(repo_dir)?;
    let mut inbox_paths = Vec::new();

    for conflict in conflicts {
        let path_str = conflict.path.to_string_lossy().into_owned();
        wkp_git::stage_path(repo_dir, &path_str)?;

        let (deleter, survivor) = match conflict.deleted_by {
            wkp_git::DeletedBy::Ours => ("our side", "the other side's"),
            wkp_git::DeletedBy::Theirs => ("the other side", "our"),
        };
        let title = format!("deletion of {path_str} needs confirmation");
        let slug = crate::remember::slugify(&title);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let relative_path = format!("inbox/{slug}-{nanos}.md");

        let frontmatter = crate::remember::remember_frontmatter_block(
            &title,
            "project-state",
            None,
            "agent:wkp-merge-resolver",
            None,
        );
        let body = format!(
            "\n`{path_str}` was deleted by {deleter} during a sync merge, but \
             {survivor} modification survived and was kept in place (design \
             6.2: a deletion always loses to a modification). Confirm \
             whether `{path_str}` should still be deleted.\n"
        );

        let full_path = repo_dir.join(&relative_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&full_path, format!("{frontmatter}{body}")).map_err(|e| e.to_string())?;
        wkp_git::stage_path(repo_dir, &relative_path)?;

        inbox_paths.push(relative_path);
    }

    Ok(inbox_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{temp_dir, test_init};

    #[test]
    fn resolve_modify_delete_conflicts_keeps_the_modification_and_creates_an_inbox_item() {
        let temp = temp_dir("resolve-modify-delete");
        let dir = temp.path();
        // `test_init` wires up the M3-2 merge driver too -- part of the
        // point of this test is that a real modify/delete conflict stops
        // the merge even with that driver fully configured, since git
        // never hands this conflict class to any content driver at all.
        test_init(dir).expect("run_init");

        std::fs::write(dir.join("item.md"), "---\n---\n\nbase content\n").expect("write base");
        wkp_git::commit_all(dir, "base").expect("commit base");
        let main_branch = wkp_git::current_branch(dir).expect("current_branch");
        wkp_git::checkout_branch(dir, "feature", true).expect("branch feature");

        wkp_git::checkout_branch(dir, &main_branch, false).expect("checkout main branch");
        std::fs::write(dir.join("item.md"), "---\n---\n\nmodified on main\n")
            .expect("write main change");
        wkp_git::commit_all(dir, "main modifies").expect("commit main change");

        wkp_git::checkout_branch(dir, "feature", false).expect("checkout feature");
        std::fs::remove_file(dir.join("item.md")).expect("remove item.md");
        wkp_git::commit_all(dir, "feature deletes").expect("commit feature delete");

        wkp_git::checkout_branch(dir, &main_branch, false).expect("checkout main branch");
        let clean = wkp_git::merge_branch(dir, "feature").expect("merge_branch");
        assert!(!clean, "a modify/delete conflict must stop the merge");

        let inbox_paths =
            resolve_modify_delete_conflicts(dir).expect("resolve_modify_delete_conflicts");
        assert_eq!(inbox_paths.len(), 1, "{inbox_paths:?}");

        let remaining =
            wkp_git::modify_delete_conflicts(dir).expect("modify_delete_conflicts after resolve");
        assert!(remaining.is_empty(), "{remaining:?}");

        let survivor = std::fs::read_to_string(dir.join("item.md")).expect("read item.md");
        assert!(survivor.contains("modified on main"));

        let inbox_content =
            std::fs::read_to_string(dir.join(&inbox_paths[0])).expect("read inbox item");
        assert!(inbox_content.contains("confidence: proposed"));
        assert!(inbox_content.contains("item.md"));
        assert!(inbox_content.to_lowercase().contains("delet"));
    }
}
