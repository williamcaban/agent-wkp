//! `wkp init`: makes a directory a store (design 5.1, 4.2, 5.2).

use std::path::Path;

/// `wkp init [path]`: makes `path` (default: cwd) a store — a git
/// repository with the settings design 5.1 wants, and a `.wkp/` directory
/// holding the derived, gitignored `index.db` (design 4.2, 5.2).
pub(crate) fn run_init(path: &Path) -> Result<(), String> {
    run_init_with_claude_home(path, crate::import::claude_home_from_env().as_deref())
}

/// The actual body of `wkp init`, with `claude_home` injectable so tests
/// can pass a controlled, empty directory instead of silently scanning
/// whatever `$HOME/.claude` really contains on the machine running the
/// test suite -- exactly the mistake `claude_home_from_env`'s doc comment
/// warns about, this time on the *test* side rather than the CLI wiring.
pub(crate) fn run_init_with_claude_home(
    path: &Path,
    claude_home: Option<&Path>,
) -> Result<(), String> {
    wkp_git::init_repo(path)?;
    wkp_git::apply_init_settings(path)?;
    configure_merge_driver(path)?;

    let wkp_dir = path.join(".wkp");
    std::fs::create_dir_all(&wkp_dir).map_err(|e| e.to_string())?;

    // Real bug found auditing M2-7: `.wkp/tier1.md` (M1-6's own
    // `wkp materialize --tier 1` output) was never added here, only
    // `tier0.md` -- an untracked, non-gitignored `.md` file is exactly
    // what `wkp index`'s change detection treats as ordinary store
    // content, so it would get indexed as if it were a real item. A glob
    // covers any future tier's materialized file, not just today's two.
    //
    // Another real bug found the same way while building M3-7:
    // `.wkp/device-id` (M3-1) was never added here either -- its own doc
    // comment (`wkp_git::sync::device_id`) says it "must never be
    // committed at all" (cloning onto a second device must not inherit
    // the first device's ID), but nothing enforced that, so a plain `git
    // add -A` anywhere in this store's history would silently sweep it
    // into a real commit.
    ensure_gitignored(path, &[".wkp/index.db", ".wkp/tier*.md", ".wkp/device-id"])?;

    // An empty index at this point; `wkp index` (M1-3) populates it from
    // the store's markdown files.
    wkp_core::index::build_index(&wkp_dir.join("index.db"), &[]).map_err(|e| e.to_string())?;

    // Import runs once at `wkp init` (design 10). Best-effort: a store
    // with nothing to import (no CLAUDE.md/AGENTS.md, no
    // ~/.claude/projects/*/memory/) must initialize cleanly with zero
    // imported items, not fail (M1-7's "net-new" acceptance criterion).
    crate::import::run_import(path, claude_home)?;

    Ok(())
}

/// Appends `patterns` to `<path>/.gitignore`, skipping any pattern already
/// present as an exact line. `index.db` and `tier0.md` are derived
/// artifacts a harness reads (CLAUDE.md: written via temp-file-then-rename,
/// never synced) and must never be committed to the store.
fn ensure_gitignored(path: &Path, patterns: &[&str]) -> Result<(), String> {
    ensure_lines_present(&path.join(".gitignore"), patterns)
}

/// Appends any of `lines` not already present verbatim in `file_path`,
/// creating the file if it doesn't exist. Idempotent (a line already
/// there is left alone, never duplicated) -- the shared implementation
/// behind [`ensure_gitignored`] and [`configure_merge_driver`]'s
/// `.gitattributes` entry.
fn ensure_lines_present(file_path: &Path, lines: &[&str]) -> Result<(), String> {
    let existing = std::fs::read_to_string(file_path).unwrap_or_default();
    let existing_lines: std::collections::HashSet<&str> = existing.lines().collect();

    let missing: Vec<&&str> = lines
        .iter()
        .filter(|p| !existing_lines.contains(*p))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    for line in missing {
        updated.push_str(line);
        updated.push('\n');
    }
    std::fs::write(file_path, updated).map_err(|e| e.to_string())
}

/// `wkp init`'s merge-driver wiring (design 6.2, M3-2): `.gitattributes`
/// (`*.md merge=wkp`, tracked -- declares *which* driver `*.md` files use)
/// plus local git config (`merge.wkp.name`/`merge.wkp.driver`, not
/// tracked -- git deliberately never reads the driver *command* itself
/// from repo content, since a merge driver runs arbitrary code; each
/// clone configures that part for itself, the same reasoning
/// `apply_init_settings`'s other settings already follow). The driver
/// command uses this process's own absolute path
/// (`std::env::current_exe`) rather than a bare `wkp`, so it works
/// correctly even when the binary running `wkp init` isn't the one a
/// later `git merge` would find first on `PATH` (as in this crate's own
/// tests, which run `target/debug/wkp`, not an installed copy).
fn configure_merge_driver(path: &Path) -> Result<(), String> {
    ensure_lines_present(&path.join(".gitattributes"), &["*.md merge=wkp"])?;

    let wkp_exe = std::env::current_exe() // nosemgrep: rust.lang.security.current-exe.current-exe -- not a security decision: this path is a same-user, same-process convenience written to *this clone's own* local git config, read back only by a later `git merge` run by that same user on that same machine; nothing crosses a trust boundary, and a hostile actor who could already redirect this process's own binary path controls the machine outright.
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "wkp".to_string());
    wkp_git::set_local_config(
        path,
        "merge.wkp.name",
        "wkp OKF frontmatter merge driver (design 6.2)",
    )?;
    wkp_git::set_local_config(
        path,
        "merge.wkp.driver",
        &format!("{wkp_exe} merge-driver %O %A %B"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::test_support::temp_dir;

    #[test]
    fn run_init_creates_store_with_gitignored_index() {
        let temp = temp_dir("run-init");
        let dir = temp.path();
        crate::test_support::test_init(dir).expect("run_init");

        assert!(dir.join(".git").is_dir());
        assert!(dir.join(".wkp/index.db").is_file());

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert!(gitignore.contains(".wkp/index.db"));
        assert!(gitignore.contains(".wkp/tier*.md"));

        // The freshly built index is a valid, queryable database.
        let conn =
            wkp_core::index::open_index(&dir.join(".wkp/index.db")).expect("open fresh index");
        let hits =
            wkp_core::index::search(&conn, "anything", &wkp_core::index::SearchFilter::default())
                .expect("search empty index");
        assert!(hits.is_empty());
    }

    /// M2-1 acceptance criterion: `wkp init` wires up SSH commit signing --
    /// the store carries an `allowed_signers` file (not gitignored, unlike
    /// `.wkp/index.db`/`tier0.md`: this one needs to sync with the store)
    /// and local git config points `gpg.ssh.allowedSignersFile` at it.
    #[test]
    fn run_init_wires_ssh_signing_config() {
        let temp = temp_dir("run-init-ssh-signing");
        let dir = temp.path();
        crate::test_support::test_init(dir).expect("run_init");

        assert!(dir.join("allowed_signers").is_file());

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert!(
            !gitignore.contains("allowed_signers"),
            "allowed_signers must be tracked, not gitignored -- the store carries it"
        );
    }

    #[test]
    fn run_init_is_idempotent_and_preserves_existing_gitignore_entries() {
        let temp = temp_dir("run-init-idempotent");
        let dir = temp.path();
        std::fs::write(dir.join(".gitignore"), "target/\n").expect("seed .gitignore");

        crate::test_support::test_init(dir).expect("first run_init");
        crate::test_support::test_init(dir).expect("second run_init");

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert_eq!(gitignore.matches(".wkp/index.db").count(), 1);
        assert!(gitignore.contains("target/"));
    }

    #[test]
    fn configure_merge_driver_writes_gitattributes_and_git_config() {
        let temp = temp_dir("configure-merge-driver");
        let dir = temp.path();
        crate::test_support::test_init(dir).expect("run_init");

        let gitattributes =
            std::fs::read_to_string(dir.join(".gitattributes")).expect("read .gitattributes");
        assert!(gitattributes.contains("*.md merge=wkp"));

        let driver = wkp_git::get_local_config(dir, "merge.wkp.driver")
            .expect("merge.wkp.driver config must be set");
        assert!(driver.contains("merge-driver %O %A %B"));
    }

    #[test]
    fn configure_merge_driver_is_idempotent() {
        let temp = temp_dir("configure-merge-driver-idempotent");
        let dir = temp.path();
        crate::test_support::test_init(dir).expect("first run_init");
        let first = std::fs::read_to_string(dir.join(".gitattributes")).expect("read first");
        crate::test_support::test_init(dir).expect("second run_init");
        let second = std::fs::read_to_string(dir.join(".gitattributes")).expect("read second");
        assert_eq!(
            first, second,
            "re-running init must not duplicate the .gitattributes entry"
        );
    }
}
