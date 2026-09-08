//! `git bundle` wrapping for air-gapped sync (design 6.1, M3-5).

use std::path::Path;

use crate::plumbing::run_git;

/// Creates a git bundle at `bundle_path` containing everything reachable
/// from `refs` (e.g. the local device branch, and `main` if it exists) --
/// design 6.1's "air-gapped sync is `git bundle`" (M3-5). `since` scopes
/// it to an incremental range (`<since>..<ref>` for each of `refs`)
/// instead of each ref's complete history.
pub fn bundle_create(
    repo_dir: &Path,
    bundle_path: &Path,
    refs: &[&str],
    since: Option<&str>,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "bundle".to_string(),
        "create".to_string(),
        bundle_path.to_string_lossy().into_owned(),
    ];
    for r in refs {
        args.push(match since {
            Some(since_ref) => format!("{since_ref}..{r}"),
            None => (*r).to_string(),
        });
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_git(repo_dir, &arg_refs)
}

/// Verifies `bundle_path` is a well-formed bundle whose prerequisites (if
/// it's an incremental bundle) are satisfiable in `repo_dir`, returning a
/// clear `Err` otherwise -- `wkp bundle import` (M3-5) calls this before
/// ever fetching from a bundle, so a corrupted or incompatible file is
/// refused up front rather than partially imported.
pub fn bundle_verify(repo_dir: &Path, bundle_path: &Path) -> Result<(), String> {
    run_git(
        repo_dir,
        &[
            "bundle",
            "verify",
            "--quiet",
            &bundle_path.to_string_lossy(),
        ],
    )
}

/// Fetches every `refs/heads/*` ref a bundle contains into `repo_dir`'s
/// `refs/remotes/bundle/*` remote-tracking namespace -- git treats a
/// bundle file path exactly like any other fetch source, no persistent
/// remote configuration needed. `wkp bundle import` (M3-5) calls this
/// only after [`bundle_verify`] has already accepted the file.
pub fn bundle_fetch(repo_dir: &Path, bundle_path: &Path) -> Result<(), String> {
    run_git(
        repo_dir,
        &[
            "fetch",
            "--quiet",
            &bundle_path.to_string_lossy(),
            "refs/heads/*:refs/remotes/bundle/*",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plumbing::run_git_stdout;
    use crate::test_support::TempGitRepo;

    #[test]
    fn bundle_create_and_verify_round_trip_in_a_fresh_unrelated_repo() {
        let publisher = TempGitRepo::new("bundle-publisher");
        publisher.write("item.md", "hello\n");
        publisher.commit_all("initial");
        let main = publisher.current_branch();

        let bundle_path = tempfile::Builder::new()
            .prefix("wkp-git-test-bundle-")
            .suffix(".bundle")
            .tempfile()
            .expect("create temp bundle file")
            .path()
            .to_path_buf();
        bundle_create(publisher.path(), &bundle_path, &[&main], None).expect("bundle_create");

        // A full (non-incremental) bundle carries its own complete
        // history, so it must verify cleanly even in a repo that never
        // shared a single commit with the publisher.
        let subscriber = TempGitRepo::new("bundle-subscriber");
        bundle_verify(subscriber.path(), &bundle_path).expect("bundle_verify");
    }

    #[test]
    fn bundle_verify_rejects_a_corrupted_file_with_a_clear_error() {
        let repo = TempGitRepo::new("bundle-verify-corrupt");
        repo.write("item.md", "hello\n");
        repo.commit_all("initial");

        let bad_bundle = tempfile::Builder::new()
            .prefix("wkp-git-test-bad-bundle-")
            .suffix(".bundle")
            .tempfile()
            .expect("create temp bundle file");
        std::fs::write(bad_bundle.path(), "not a bundle\n").expect("write bad bundle");

        let result = bundle_verify(repo.path(), bad_bundle.path());
        assert!(result.is_err(), "expected a clear error, not Ok");
    }

    #[test]
    fn bundle_fetch_populates_the_bundle_remote_tracking_namespace() {
        let publisher = TempGitRepo::new("bundle-fetch-publisher");
        publisher.write("item.md", "hello\n");
        publisher.commit_all("initial");
        let main = publisher.current_branch();

        let bundle_path = tempfile::Builder::new()
            .prefix("wkp-git-test-bundle-fetch-")
            .suffix(".bundle")
            .tempfile()
            .expect("create temp bundle file")
            .path()
            .to_path_buf();
        bundle_create(publisher.path(), &bundle_path, &[&main], None).expect("bundle_create");

        let subscriber = TempGitRepo::new("bundle-fetch-subscriber");
        bundle_verify(subscriber.path(), &bundle_path).expect("bundle_verify");
        bundle_fetch(subscriber.path(), &bundle_path).expect("bundle_fetch");

        let refs =
            crate::refs_matching(subscriber.path(), "refs/remotes/bundle/").expect("refs_matching");
        assert_eq!(refs, vec![format!("refs/remotes/bundle/{main}")]);

        let has_it = run_git_stdout(
            subscriber.path(),
            &[
                "cat-file",
                "-e",
                &format!("refs/remotes/bundle/{main}:item.md"),
            ],
        )
        .is_ok();
        assert!(
            has_it,
            "expected item.md reachable from the fetched bundle ref"
        );
    }

    #[test]
    fn bundle_create_with_since_produces_an_incremental_bundle() {
        let publisher = TempGitRepo::new("bundle-incremental-publisher");
        publisher.write("item.md", "base\n");
        publisher.commit_all("base");
        let base_sha = run_git_stdout(publisher.path(), &["rev-parse", "HEAD"])
            .expect("rev-parse HEAD")
            .trim()
            .to_string();
        publisher.write("item.md", "updated\n");
        publisher.commit_all("update");
        let main = publisher.current_branch();

        let bundle_path = tempfile::Builder::new()
            .prefix("wkp-git-test-bundle-incremental-")
            .suffix(".bundle")
            .tempfile()
            .expect("create temp bundle file")
            .path()
            .to_path_buf();
        bundle_create(publisher.path(), &bundle_path, &[&main], Some(&base_sha))
            .expect("bundle_create with --since");

        // An incremental bundle records a prerequisite, not a complete
        // history -- verifying it against the very repo whose history it
        // was cut from (which already has that prerequisite commit) must
        // still succeed.
        bundle_verify(publisher.path(), &bundle_path).expect("bundle_verify (incremental)");
    }
}
