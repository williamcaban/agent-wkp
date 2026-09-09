//! M5-4's own explicit acceptance criterion: "a real bare repo + hook
//! fixture, a real `git push` ... confirming the hook ran and only the
//! shared item landed in the index" -- proven here against the real,
//! compiled `wkp-hub` binary (`env!("CARGO_BIN_EXE_wkp-hub")`), not by
//! calling `index_tenant` directly the way `tenant_repo`'s own unit
//! tests do. Those unit tests exercise the indexing *logic*; this test
//! exercises the *wiring* -- that `wkp-hub provision-repo` really
//! installs a hook that really runs on a real push and really produces
//! the tenant's `index.db`.
//!
//! Needs no Postgres: `provision-repo`/`index-tenant` are both
//! deliberately DB-free (see `main.rs`'s own doc comment on
//! `provision-repo`), so this test has no `DATABASE_URL` dependency
//! unlike this crate's `control_plane`/`http`/`wkp_shell` tests.
//!
//! Uses `Command::new("git")` directly to drive a throwaway client
//! clone -- the same narrow, documented exception to CLAUDE.md's "no
//! `Command::new(\"git\")` outside `wkp-git`" that
//! `wkp-cli/tests/encryption_filter.rs` already establishes for
//! integration tests that need to prove real git wiring end to end.

use std::path::{Path, PathBuf};
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

fn temp_repos_root() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("wkp-hub-post-receive-hook-test-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp repos root");
    dir
}

#[test]
fn a_real_push_triggers_the_installed_hook_and_indexes_only_the_shared_item() {
    let repos_root = temp_repos_root();

    let provision = run_wkp_hub(&repos_root, &["provision-repo", "acme"]);
    assert!(
        provision.status.success(),
        "provision-repo failed: {}",
        String::from_utf8_lossy(&provision.stderr)
    );

    // The installed hook invokes bare `wkp-hub` (relies on `PATH` --
    // this task's own deliberate choice, real deployment wiring is
    // M5-5's job). Only the test's own subprocess PATH needs the real
    // compiled binary made reachable under that exact name; production
    // `provision_tenant_repo` is unchanged.
    let bin_dir = Path::new(wkp_hub_bin())
        .parent()
        .expect("CARGO_BIN_EXE_wkp-hub has a parent dir");
    let path_with_bin = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let repo_path = repos_root.join("acme.git");
    let clone_dir = repos_root.join("clone");
    run_git(
        &repos_root,
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

    // `WKP_HUB_REPOS_ROOT` must reach the hook's own `wkp-hub
    // index-tenant` subprocess, which git spawns as a *child of this
    // `git push`*, not of the earlier `provision-repo` invocation --
    // env vars don't reach sideways between unrelated subprocesses.
    let push_status = Command::new("git")
        .current_dir(&clone_dir)
        .env("PATH", &path_with_bin)
        .env("WKP_HUB_REPOS_ROOT", &repos_root)
        .args(["push", "--quiet", "origin", "main"])
        .status()
        .expect("run git push");
    assert!(push_status.success(), "git push failed: {push_status:?}");

    let index_path = repos_root.join("acme.index.db");
    assert!(
        index_path.exists(),
        "post-receive hook must have run `wkp-hub index-tenant` and produced index.db, \
         but {} does not exist",
        index_path.display()
    );

    let conn = wkp_core::index::open_index(&index_path).expect("open index");
    let known = wkp_core::index::known_paths(&conn).expect("known_paths");
    assert_eq!(
        known,
        std::collections::HashSet::from(["shared.md".to_string()]),
        "the hook must have indexed only the shared item, never the private one"
    );
}
