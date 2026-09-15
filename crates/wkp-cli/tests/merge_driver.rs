//! M3-2 (issue #55) acceptance criterion: "a real git merge invokes the
//! driver". Every other merge-driver test (`crates/wkp-cli/src/main.rs`'s
//! unit tests) calls `run_merge_driver`/`wkp_core::merge::merge` directly --
//! that proves the merge *logic* is right, but not that git's own
//! merge-driver protocol (`gitattributes(5)`) actually wires up to this
//! binary. Only a genuine `git merge` across two real, diverged branches
//! can prove that, so this file drives `git` as a subprocess directly.
//!
//! This is a deliberate, narrow exception to CLAUDE.md's "no
//! `Command::new(\"git\")` outside `crates/wkp-git`" rule: there is no
//! `wkp-git` wrapper for `git merge` or plain unsigned commits (nor should
//! there be one just for this), and the entire point of this test is
//! exercising the real external git protocol end to end, not wkp's own
//! plumbing. `CARGO_BIN_EXE_wkp` (only available to genuine Cargo
//! integration tests, not unit tests inside `src/main.rs` -- verified
//! empirically while building this test) gives the path to the real
//! compiled binary that git's `merge.wkp.driver` config is pointed at.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo_with_wkp_merge_driver(dir: &Path) {
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "user.email", "test@example.com"]);

    std::fs::write(dir.join(".gitattributes"), "*.md merge=wkp\n").expect("write .gitattributes");

    let wkp_exe = env!("CARGO_BIN_EXE_wkp");
    git(
        dir,
        &[
            "config",
            "merge.wkp.name",
            "wkp OKF frontmatter merge driver",
        ],
    );
    git(
        dir,
        &[
            "config",
            "merge.wkp.driver",
            &format!("{wkp_exe} merge-driver %O %A %B"),
        ],
    );
}

fn write(dir: &Path, relative: &str, contents: &str) {
    std::fs::write(dir.join(relative), contents).expect("write file");
}

fn read(dir: &Path, relative: &str) -> String {
    std::fs::read_to_string(dir.join(relative)).expect("read file")
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", message]);
}

/// Two branches each change a different, auto-mergeable frontmatter field
/// on the same pre-existing file (`updated` on `main`, `tags` on
/// `feature`). Merging `feature` into `main` is a genuine 3-way merge --
/// `main` and `feature` both moved past their common ancestor -- so git
/// can only resolve it by invoking the configured `*.md` driver, which
/// must be the real compiled `wkp` binary.
#[test]
fn real_git_merge_of_divergent_fields_invokes_the_compiled_driver_cleanly() {
    let temp = tempfile::Builder::new()
        .prefix("wkp-cli-merge-driver-clean-")
        .tempdir()
        .expect("create temp dir");
    let dir = temp.path();
    init_repo_with_wkp_merge_driver(dir);

    write(
        dir,
        "item.md",
        "---\ntags: []\nupdated: 2026-01-01\n---\n\nbody\n",
    );
    commit_all(dir, "base");

    git(dir, &["branch", "feature"]);

    // main: advance independently by changing `updated`.
    write(
        dir,
        "item.md",
        "---\ntags: []\nupdated: 2026-02-01\n---\n\nbody\n",
    );
    commit_all(dir, "main: bump updated");

    // feature: advance independently by changing `tags`.
    git(dir, &["checkout", "--quiet", "feature"]);
    write(
        dir,
        "item.md",
        "---\ntags: [x]\nupdated: 2026-01-01\n---\n\nbody\n",
    );
    commit_all(dir, "feature: add tag");

    git(dir, &["checkout", "--quiet", "main"]);
    git(dir, &["merge", "--quiet", "--no-edit", "feature"]);

    let merged = read(dir, "item.md");
    let fm = wkp_core::frontmatter::parse(&merged).frontmatter;
    assert_eq!(fm.tags, vec!["x"]);
    assert_eq!(fm.updated.as_deref(), Some("2026-02-01"));
    assert!(
        !merged.contains(wkp_core::merge::CONFLICT_MARKER),
        "a clean structural merge must not fall back to the conflict form:\n{merged}"
    );
}

/// Two branches independently create the same new path with different
/// bodies and no common ancestor version of the file. Git still routes
/// this through the configured `*.md` driver (with an empty `%O`), and
/// `wkp_core::merge::merge` has no ancestor to reconcile against, so it
/// must fall back to the keep-both-documents conflict form rather than
/// silently picking a side.
#[test]
fn real_git_merge_of_an_add_add_conflict_keeps_both_documents() {
    let temp = tempfile::Builder::new()
        .prefix("wkp-cli-merge-driver-addadd-")
        .tempdir()
        .expect("create temp dir");
    let dir = temp.path();
    init_repo_with_wkp_merge_driver(dir);

    write(dir, "root.md", "---\n---\n\nroot\n");
    commit_all(dir, "root");

    git(dir, &["branch", "feature"]);

    write(dir, "new.md", "---\ntitle: from main\n---\n\nmain body\n");
    commit_all(dir, "main: add new.md");

    git(dir, &["checkout", "--quiet", "feature"]);
    write(
        dir,
        "new.md",
        "---\ntitle: from feature\n---\n\nfeature body\n",
    );
    commit_all(dir, "feature: add new.md");

    git(dir, &["checkout", "--quiet", "main"]);
    git(dir, &["merge", "--quiet", "--no-edit", "feature"]);

    let merged = read(dir, "new.md");
    assert!(
        merged.contains(wkp_core::merge::CONFLICT_MARKER),
        "an add/add divergence with no common ancestor must keep both documents:\n{merged}"
    );
    assert!(merged.contains("main body"));
    assert!(merged.contains("feature body"));
}
