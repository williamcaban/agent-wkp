//! Architectural lint, not a functional test: guards against a real
//! incident's exact class of bug (see `crates/wkp-cli/src/init.rs`'s
//! `resolve_wkp_exe` doc comment for the full account). In short:
//! `std::env::current_exe()`, called directly to build a command this
//! process registers for git to invoke later (a merge driver, a filter),
//! resolves to *this crate's own unit-test binary* when the calling code
//! runs in-process from a `#[cfg(test)]` module rather than the compiled
//! `wkp` CLI. Git then spawns that test-harness binary as a subcommand
//! (e.g. `<it> filter clean some.md`) -- but that binary's real entry
//! point is libtest, which reinterprets those arguments as test-name
//! filters and reruns every matching test, several of which performed
//! their *own* real git operations against the same self-registered,
//! self-referential command, repeating the mistake inside the recursive
//! run without bound. That is what actually happened: thousands of
//! orphaned processes, each stuck forever on `main.rs`'s `filter`
//! dispatch blocking on a stdin read that would never see EOF, exhausted
//! the host's process table and memory before anyone stopped it by hand.
//!
//! `resolve_wkp_exe` (`crates/wkp-cli/src/init.rs`) is the one place in
//! this workspace allowed to call `current_exe()` for this purpose: it
//! detects the test-harness case (its own binary living in a `deps/`
//! directory alongside every other compiled test/bench artifact) and
//! substitutes the real, sibling plain `wkp` binary instead. Every other
//! call site that wants "this process's own binary path, for a command
//! git (or anything else) will run later" must route through it -- this
//! test fails the build the moment a second, unguarded call site
//! appears, rather than waiting for another incident to find it.

use std::path::{Path, PathBuf};

/// The one call site `resolve_wkp_exe` itself is allowed to use, relative
/// to the workspace root.
const ALLOWED_CALL_SITE: &str = "crates/wkp-cli/src/init.rs";

/// Every workspace member with source that could plausibly grow this
/// pattern. Deliberately not "walk the whole repo" -- `tests/`,
/// `fuzz/`, `benches/` etc. are a different concern (harness code, not
/// something git will ever be told to invoke), so keeping this list
/// explicit avoids the lint drifting into unrelated code just because
/// some future top-level directory happens to contain the substring.
const WORKSPACE_MEMBERS: &[&str] = &[
    "wkp-core",
    "wkp-crypto",
    "wkp-git",
    "wkp-cli",
    "wkp-hub",
    "wkp-sys",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .expect("wkp-cli's parent directory")
        .parent() // workspace root
        .expect("crates/'s parent directory")
        .to_path_buf()
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return, // a member with no such subdirectory yet
    };
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn current_exe_self_invocation_is_confined_to_the_sanctioned_resolver() {
    let root = workspace_root();
    let mut files = Vec::new();
    for member in WORKSPACE_MEMBERS {
        collect_rust_files(&root.join("crates").join(member).join("src"), &mut files);
    }
    assert!(
        files.len() > 10,
        "found suspiciously few source files ({}) -- this lint's file-walk is probably \
         broken (wrong root, wrong member list) rather than the workspace actually \
         shrinking that much",
        files.len()
    );

    let mut offending = Vec::new();
    for path in &files {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let relative_str = relative.to_string_lossy().replace('\\', "/");
        if relative_str == ALLOWED_CALL_SITE {
            continue;
        }
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if content.contains("current_exe(") {
            offending.push(relative_str);
        }
    }

    assert!(
        offending.is_empty(),
        "std::env::current_exe() must only be called from {ALLOWED_CALL_SITE}'s \
         resolve_wkp_exe(), not directly -- a direct call anywhere else breaks the moment \
         it runs in-process from a unit test instead of the real compiled CLI binary (see \
         this file's module doc comment for the real incident this guards against). If the \
         new call site genuinely needs this process's own binary path for something git (or \
         anything else) will later invoke as a subprocess, route it through \
         `resolve_wkp_exe()` instead; if it needs current_exe() for an unrelated reason, \
         update this lint's allowlist deliberately, with a comment saying why that call site \
         is safe under a unit-test harness. Found direct call(s) in: {offending:?}"
    );
}
