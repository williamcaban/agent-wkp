//! Issue #159's own wiring test: a real `git push` against a real bare
//! repo, followed by a real, separately-invoked `wkp-hub index-worker
//! --tenant <slug> --once` subprocess, proving the *new* trigger
//! mechanism actually works end to end -- that command notices `HEAD`
//! moved and produces the tenant's `index.db` -- not by calling
//! `index_tenant`/`current_head` directly the way `tenant_repo`'s own
//! unit tests do (those exercise the indexing *logic*; this exercises
//! the *wiring* a real push and a real second process depend on).
//!
//! Replaces `post_receive_hook.rs` (deleted): there is no `post-receive`
//! hook installed any more (see `tenant_repo.rs`'s module doc) -- the
//! indexing container polls on its own instead, since a hook running in
//! the *serving* container has no network-free way to reach across to
//! the separate, no-network *indexing* container that issue #159 splits
//! it into (`tenant_pod.rs`'s module doc). `--once` makes that poll loop
//! deterministic for a test: run exactly one check-and-maybe-index
//! cycle and exit, rather than looping forever the way the real
//! container's foreground process does.
//!
//! Needs no Postgres: `provision-repo`/`index-worker` are both
//! deliberately DB-free (see `main.rs`'s own doc comment on
//! `provision-repo`), so this test has no `DATABASE_URL` dependency
//! unlike this crate's `control_plane`/`http`/`front_door` tests.
//!
//! Uses `Command::new("git")` directly to drive a throwaway client
//! clone -- the same narrow, documented exception to CLAUDE.md's "no
//! `Command::new(\"git\")` outside `wkp-git`" that
//! `wkp-cli/tests/encryption_filter.rs` already establishes for
//! integration tests that need to prove real git wiring end to end.

use std::path::Path;
use std::process::Command;

fn wkp_hub_bin() -> &'static str {
    env!("CARGO_BIN_EXE_wkp-hub")
}

fn run_git(dir: &Path, args: &[&str]) {
    // nosemgrep: rust.lang.security.command-injection.command-injection
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    assert!(status.success(), "git {args:?} failed: {status:?}");
}

fn run_wkp_hub(repos_root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wkp_hub_bin())
        .env("WKP_HUB_REPOS_ROOT", repos_root)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run wkp-hub {args:?}: {e}"))
}

fn temp_repos_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("wkp-hub-index-worker-test-")
        .tempdir()
        .expect("create temp repos root")
}

#[test]
fn index_worker_once_notices_a_real_push_and_indexes_only_the_shared_item() {
    let temp = temp_repos_root();
    let repos_root = temp.path();

    let provision = run_wkp_hub(repos_root, &["provision-repo", "acme"]);
    assert!(
        provision.status.success(),
        "provision-repo failed: {}",
        String::from_utf8_lossy(&provision.stderr)
    );

    // Before any push: nothing to index yet, and `index-worker --once`
    // must reflect that rather than erroring on an unborn HEAD.
    let before = run_wkp_hub(repos_root, &["index-worker", "--tenant", "acme", "--once"]);
    assert!(
        before.status.success(),
        "index-worker --once must succeed against an unborn repo: {}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert!(
        !repos_root.join("acme").join("index.db").exists(),
        "index-worker must not produce index.db before anything has been pushed"
    );

    let repo_path = repos_root.join("acme").join("repo.git");
    let clone_dir = repos_root.join("clone");
    run_git(
        repos_root,
        &["clone", "--quiet", repo_path.to_str().unwrap(), "clone"],
    );
    run_git(&clone_dir, &["checkout", "--quiet", "-b", "main"]);
    std::fs::write(
        clone_dir.join("shared.md"),
        "---\nvisibility: shared\ntitle: shared item\n---\n\nshared body\n",
    )
    .expect("write shared.md");
    std::fs::write(
        clone_dir.join("private.md"),
        "---\nvisibility: private\ntitle: private item\n---\n\nprivate body\n",
    )
    .expect("write private.md");
    run_git(&clone_dir, &["add", "-A"]);
    run_git(
        &clone_dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "--quiet",
            "-m",
            "add shared and private items",
        ],
    );
    run_git(&clone_dir, &["push", "--quiet", "origin", "main"]);

    // No hook fired -- `index.db` must not exist yet purely from the
    // push itself. This is the behavior change issue #159's fix
    // deliberately makes: indexing is now the index-worker's own job,
    // triggered by it noticing HEAD moved, not the push's.
    assert!(
        !repos_root.join("acme").join("index.db").exists(),
        "a push alone must not produce index.db any more -- that's index-worker's job now, \
         not a post-receive hook's"
    );

    let after = run_wkp_hub(repos_root, &["index-worker", "--tenant", "acme", "--once"]);
    assert!(
        after.status.success(),
        "index-worker --once failed: {}",
        String::from_utf8_lossy(&after.stderr)
    );

    let index_path = repos_root.join("acme").join("index.db");
    assert!(
        index_path.exists(),
        "index-worker --once must have noticed the push and produced index.db, but {} does not exist",
        index_path.display()
    );

    let conn = wkp_core::index::open_index(&index_path).expect("open index");
    let known = wkp_core::index::known_paths(&conn).expect("known_paths");
    assert_eq!(
        known,
        std::collections::HashSet::from(["shared.md".to_string()]),
        "index-worker must have indexed only the shared item, never the private one"
    );
}
