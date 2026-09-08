//! `wkp merge-driver`: git's merge-driver protocol implementation
//! (design 6.2, M3-2).

use std::path::Path;

/// `wkp merge-driver <ancestor> <ours> <theirs>` (design 6.2, M3-2): git's
/// own merge-driver protocol (`gitattributes(5)`, "defining a custom
/// merge driver") -- git substitutes `%O %A %B` with these three temp
/// file paths before invoking the configured driver command, and treats
/// whatever is left in the `%A` (`ours`) file afterward as the merge
/// result. `wkp_core::merge::merge` always succeeds (see its own doc
/// comment on why there is no conflicted-exit-code case left for this
/// driver), so the only way this returns `Err` is a real I/O failure
/// reading the three inputs or writing the result back.
pub(crate) fn run_merge_driver(ancestor: &Path, ours: &Path, theirs: &Path) -> Result<(), String> {
    let base_content = std::fs::read_to_string(ancestor)
        .map_err(|e| format!("reading ancestor file {}: {e}", ancestor.display()))?;
    let ours_content = std::fs::read_to_string(ours)
        .map_err(|e| format!("reading ours file {}: {e}", ours.display()))?;
    let theirs_content = std::fs::read_to_string(theirs)
        .map_err(|e| format!("reading theirs file {}: {e}", theirs.display()))?;

    let merged = wkp_core::merge::merge(&base_content, &ours_content, &theirs_content);

    std::fs::write(ours, merged)
        .map_err(|e| format!("writing merged result to {}: {e}", ours.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    #[test]
    fn run_merge_driver_writes_the_merged_result_to_the_ours_path() {
        let temp = temp_dir("merge-driver-file-io");
        let dir = temp.path();
        let ancestor = dir.join("ancestor.md");
        let ours = dir.join("ours.md");
        let theirs = dir.join("theirs.md");
        std::fs::write(&ancestor, "---\ntags: []\n---\n\nbody\n").expect("write ancestor");
        std::fs::write(&ours, "---\ntags: [a]\n---\n\nbody\n").expect("write ours");
        std::fs::write(&theirs, "---\ntags: [b]\n---\n\nbody\n").expect("write theirs");

        run_merge_driver(&ancestor, &ours, &theirs).expect("run_merge_driver");

        let merged = std::fs::read_to_string(&ours).expect("read merged ours file");
        let fm = wkp_core::frontmatter::parse(&merged).frontmatter;
        assert_eq!(fm.tags, vec!["a", "b"]);
    }
}
