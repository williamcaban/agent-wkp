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
    configure_encryption_filter(path)?;

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
    ensure_gitignored(
        path,
        &[
            ".wkp/index.db",
            ".wkp/tier*.md",
            ".wkp/device-id",
            // M4-4's file-fallback device encryption identity
            // (wkp_crypto::device_identity) -- a private key, must
            // never be committed, added here from the start rather
            // than repeating M3-1's device-id oversight (that one
            // wasn't gitignored until M3-7 found it the hard way).
            ".wkp/device-identity",
        ],
    )?;

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

/// The absolute path this process's own binary should be invoked as by a
/// later `git merge`/filter run -- shared by [`configure_merge_driver`]
/// and `configure_encryption_filter`.
///
/// Same executable as plain `std::env::current_exe()` in every real case:
/// the compiled `wkp` binary itself running `wkp init`, or an integration
/// test driving it via `Command::new(CARGO_BIN_EXE_wkp)` (`tests/*.rs`).
/// It differs only when this crate's own *unit* tests call `run_init`
/// in-process (`test_support::test_init`, not a subprocess) and then run
/// a real git operation that fires the filter/driver -- there,
/// `current_exe()` resolves to the unit-test harness binary itself
/// (`<profile-dir>/deps/<crate>-<hash>`), not a `wkp` CLI at all. Git
/// would then spawn that harness binary as e.g. `<it> filter clean
/// some.md`, whose actual entry point is libtest, not `main.rs`'s
/// subcommand dispatch: libtest treats "filter"/"clean"/the filename as
/// test-name filters and reruns every matching test, several of which do
/// their *own* real git commits against the same self-registered filter
/// -- a real incident where this recursed without bound, leaking
/// thousands of orphaned processes stuck reading stdin (`main.rs`'s
/// `filter` dispatch blocks on `read_to_end`) before anything stopped it.
/// Cargo always builds a package's plain `[[bin]]` executable at
/// `<profile-dir>/<name>` alongside its test binaries in
/// `<profile-dir>/deps/`, so when `current_exe()`'s immediate parent
/// directory is literally named `deps`, this rewrites to that sibling
/// plain binary -- a real, correctly-dispatching `wkp` -- instead.
fn resolve_wkp_exe() -> String {
    let current = match std::env::current_exe() {
        // nosemgrep: rust.lang.security.current-exe.current-exe -- not a security decision: this path is a same-user, same-process convenience written to *this clone's own* local git config, read back only by a later `git merge`/filter run by that same user on that same machine; nothing crosses a trust boundary, and a hostile actor who could already redirect this process's own binary path controls the machine outright.
        Ok(p) => p,
        Err(_) => return "wkp".to_string(),
    };
    let plain_bin_sibling = (|| {
        let deps_dir = current.parent()?;
        if deps_dir.file_name()?.to_str()? != "deps" {
            return None;
        }
        let candidate = deps_dir
            .parent()?
            .join(format!("wkp{}", std::env::consts::EXE_SUFFIX));
        candidate.is_file().then_some(candidate)
    })();
    plain_bin_sibling
        .unwrap_or(current)
        .to_string_lossy()
        .into_owned()
}

/// `wkp init`'s merge-driver wiring (design 6.2, M3-2): `.gitattributes`
/// (`*.md merge=wkp`, tracked -- declares *which* driver `*.md` files use)
/// plus local git config (`merge.wkp.name`/`merge.wkp.driver`, not
/// tracked -- git deliberately never reads the driver *command* itself
/// from repo content, since a merge driver runs arbitrary code; each
/// clone configures that part for itself, the same reasoning
/// `apply_init_settings`'s other settings already follow). The driver
/// command uses this process's own absolute path (see
/// [`resolve_wkp_exe`]) rather than a bare `wkp`, so it works correctly
/// even when the binary running `wkp init` isn't the one a later
/// `git merge` would find first on `PATH`.
fn configure_merge_driver(path: &Path) -> Result<(), String> {
    ensure_lines_present(&path.join(".gitattributes"), &["*.md merge=wkp"])?;

    let wkp_exe = resolve_wkp_exe();
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

/// `wkp init`'s encryption filter wiring (design 7.2, 5.1, M4-4):
/// `.gitattributes` (`*.md filter=wkp-crypt`, tracked) plus local git
/// config (`filter.wkp-crypt.clean`/`.smudge`/`.required`, not tracked --
/// same reasoning as `configure_merge_driver`'s driver command: never
/// read from repo content, each clone configures its own). Also this
/// device's own "first use" registration (M4-3's issue text): obtains
/// this device's encryption identity (M4-2, keyed by the same device id
/// M3-1's sync branch already uses -- one "device" concept, not two) and
/// appends its public half to the store's `recipients` file, so a device
/// can always decrypt whatever it itself just encrypted. Idempotent both
/// ways: `ensure_lines_present`/`device_identity::ensure`/
/// `recipients::append` are all already idempotent on their own.
///
/// `filter.wkp-crypt.required = true` is essential, not a nice-to-have:
/// see `docs/adr/0006-encryption-filter-failure-policy.md` for the
/// by-hand-verified finding that git's *default* behavior for a failing
/// clean filter is to silently commit the raw, unencrypted input.
fn configure_encryption_filter(path: &Path) -> Result<(), String> {
    ensure_lines_present(&path.join(".gitattributes"), &["*.md filter=wkp-crypt"])?;

    let device_id = wkp_git::sync::device_id(path)?;
    let fallback_path = path.join(".wkp/device-identity");
    let identity = wkp_crypto::device_identity::ensure(&device_id, &fallback_path)
        .map_err(|e| e.to_string())?;
    wkp_crypto::recipients::append(
        &path.join("recipients"),
        &wkp_crypto::recipients::RecipientEntry {
            label: format!("device:{device_id}"),
            kind: wkp_crypto::recipients::RecipientKind::Device,
            recipient: identity.to_recipient(),
        },
    )
    .map_err(|e| e.to_string())?;

    let wkp_exe = resolve_wkp_exe();
    wkp_git::set_local_config(
        path,
        "filter.wkp-crypt.clean",
        &format!("{wkp_exe} filter clean %f"),
    )?;
    wkp_git::set_local_config(
        path,
        "filter.wkp-crypt.smudge",
        &format!("{wkp_exe} filter smudge %f"),
    )?;
    wkp_git::set_local_config(path, "filter.wkp-crypt.required", "true")?;
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

    /// Regression test for a real incident: `test_init` runs `run_init`
    /// in-process (not via `CARGO_BIN_EXE_wkp`), so a naive
    /// `std::env::current_exe()` here resolves to *this unit-test binary*
    /// (`target/debug/deps/<crate>-<hash>`), not a real `wkp` CLI. Any
    /// later real `git add`/`commit`/`merge` in a test then makes git
    /// spawn that test-harness binary as e.g. `<it> filter clean some.md`
    /// -- libtest, not `main.rs`'s dispatch, is what actually runs, and it
    /// treats those arguments as test-name filters, re-running (and so
    /// re-triggering) every matching test without bound. This asserts the
    /// registered commands point at a real binary path -- specifically,
    /// not one living in a `deps/` directory alongside every other
    /// compiled test/bench artifact.
    #[test]
    fn configured_driver_and_filter_commands_do_not_point_at_the_test_harness_binary() {
        let temp = temp_dir("configure-resolve-wkp-exe");
        let dir = temp.path();
        crate::test_support::test_init(dir).expect("run_init");

        let merge_driver = wkp_git::get_local_config(dir, "merge.wkp.driver")
            .expect("merge.wkp.driver config must be set");
        let filter_clean = wkp_git::get_local_config(dir, "filter.wkp-crypt.clean")
            .expect("filter.wkp-crypt.clean config must be set");

        for command in [&merge_driver, &filter_clean] {
            assert!(
                !command.contains("/deps/"),
                "command must not invoke a test-harness binary from target/*/deps/, \
                 which cannot dispatch `merge-driver`/`filter` subcommands: {command}"
            );
        }
    }
}
