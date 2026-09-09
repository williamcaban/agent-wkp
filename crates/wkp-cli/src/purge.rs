//! `wkp purge <path>`: true history erasure, wrapping
//! `wkp_git::purge::purge_path` (design 7.6, M4-6).
//!
//! Distinct from `wkp forget` (M4-5): forgetting removes or re-encrypts
//! something as of one new commit, leaving every prior commit touching
//! it untouched in history. Purging rewrites *every* commit that ever
//! touched the path, which unavoidably changes the hash of that commit
//! and every commit after it -- there is no way to purge something
//! without the whole repository's history from that point diverging from
//! every other existing clone. That is why this command's own success
//! output is, deliberately, mostly a wall of required follow-up
//! instructions rather than a one-line summary: design 7.6 asks for this
//! to be "stated plainly to users rather than hidden," and a caller who
//! doesn't force-push and get every other clone to discard its own copy
//! has not actually finished the operation they asked for.

use std::path::PathBuf;

pub(crate) struct PurgeOptions {
    pub(crate) path: PathBuf,
    pub(crate) item_path: String,
}

/// Parses `wkp purge <path> [--path <dir>]`.
pub(crate) fn parse_purge_args(
    mut args: impl Iterator<Item = String>,
) -> Result<PurgeOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut item_path: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other if item_path.is_none() && !other.starts_with('-') => {
                item_path = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let item_path =
        item_path.ok_or_else(|| "purge requires a path, e.g. `wkp purge secret.md`".to_string())?;

    Ok(PurgeOptions { path, item_path })
}

pub(crate) struct PurgeSummary {
    path: String,
}

impl std::fmt::Display for PurgeSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: purged {} from every commit in this repository's local history.\n\n\
             This rewrote every commit that touched {} -- their hashes, and every commit \
             after them, are now different from what any other clone of this repository \
             still has. Required next steps, not optional:\n\
             \n\
             1. Force-push every branch this repository shares with a remote:\n\
             \x20  git push --force --all && git push --force --tags\n\
             2. Every OTHER clone of this repository (every other device, any bare remote \
             a hub or NAS holds) is now diverged history, not just \"behind\" -- pulling \
             into one will not converge and may resurrect the purged content from that \
             clone's own copy. Each one must be discarded and re-cloned fresh from the \
             repository you just purged, never merged or rebased onto.\n\
             3. {} is still recoverable by anyone who already has a copy of this \
             repository's history from before this purge (a stale clone, a backup, a \
             bundle) -- purging this repository does not reach into those. If that is a \
             real concern, treat every such copy as compromised, not just this one.",
            self.path, self.path, self.path
        )
    }
}

/// `wkp purge <path>`: see the module doc comment. No identity/signing
/// arguments, unlike `wkp forget`/`wkp promote` -- there is no single
/// new commit here to attach a principal to; `git-filter-repo` rewrites
/// the entire graph, and whatever signatures existing commits carried
/// are invalidated by that rewrite regardless of who runs this.
pub(crate) fn run_purge(opts: &PurgeOptions) -> Result<PurgeSummary, String> {
    let summary = wkp_git::purge::purge_path(&opts.path, &opts.item_path)?;
    Ok(PurgeSummary { path: summary.path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use std::path::Path;

    #[test]
    fn parse_purge_args_requires_a_path() {
        assert!(parse_purge_args(args(&[])).is_err());
    }

    #[test]
    fn parse_purge_args_reads_the_path_and_store_dir() {
        let opts = parse_purge_args(args(&["secret.md", "--path", "/tmp/store"]))
            .expect("parse_purge_args");
        assert_eq!(opts.item_path, "secret.md");
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn parse_purge_args_rejects_unrecognized_flags() {
        assert!(parse_purge_args(args(&["secret.md", "--bogus"])).is_err());
    }

    fn write_and_commit(dir: &Path, key: &TestKey, principal: &str, relative: &str, content: &str) {
        let full = dir.join(relative);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&full, content).expect("write file");
        wkp_git::signed_commit::signed_commit(
            dir,
            &[PathBuf::from(relative)],
            &format!("add {relative}"),
            principal,
            &key.private_path,
            &wkp_git::provenance::Provenance::default(),
        )
        .expect("signed_commit seeding fixture");
    }

    #[test]
    fn run_purge_removes_the_path_from_every_revision_and_reports_it() {
        let temp = temp_dir("purge-run");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let human_key = generate_test_key_and_register(dir, "human:alice");
        write_and_commit(
            dir,
            &human_key,
            "human:alice",
            "secret.md",
            "revision one\n",
        );
        write_and_commit(
            dir,
            &human_key,
            "human:alice",
            "secret.md",
            "revision two\n",
        );
        write_and_commit(dir, &human_key, "human:alice", "keep.md", "unrelated\n");

        let opts = PurgeOptions {
            path: dir.to_path_buf(),
            item_path: "secret.md".to_string(),
        };
        let summary = run_purge(&opts).expect("run_purge");
        let rendered = summary.to_string();
        assert!(rendered.contains("secret.md"));
        assert!(
            rendered.to_lowercase().contains("force-push") || rendered.contains("--force"),
            "must mention force-push: {rendered}"
        );
        assert!(
            rendered.to_lowercase().contains("re-clone")
                || rendered.to_lowercase().contains("re-cloned"),
            "must instruct other clones to re-clone, not pull: {rendered}"
        );

        // Full-history erasure itself (every prior revision, not just the
        // current tree) is `wkp_git::purge`'s own test coverage
        // (`purge_path_removes_a_path_from_every_revision_in_history`) --
        // this layer only needs to confirm the current tree and the
        // required-instructions message, not re-invoke git directly
        // (CLAUDE.md: no `Command::new("git")` outside `crates/wkp-git`).
        assert!(!dir.join("secret.md").exists());
        let tracked = wkp_git::list_tracked_files(dir).expect("list_tracked_files");
        assert!(!tracked.iter().any(|p| p == Path::new("secret.md")));
        assert!(dir.join("keep.md").is_file(), "unrelated file must survive");
    }
}
