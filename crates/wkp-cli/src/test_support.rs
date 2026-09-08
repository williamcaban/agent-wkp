//! Shared test-only fixtures used across this crate's module test suites.

use std::path::{Path, PathBuf};

/// `tempfile` rather than `std::env::temp_dir()` + a predictable name:
/// the latter is flagged by this repo's semgrep gate as an
/// insecure-temp-file pattern (a shared temp directory with a
/// guessable name invites symlink/TOCTOU races).
pub(crate) fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("wkp-cli-test-{name}-"))
        .tempdir()
        .expect("create temp dir")
}

/// `run_init`, but with an empty, guaranteed-`projects/`-less
/// `claude_home` instead of the real `$HOME/.claude` -- otherwise
/// every test calling this would non-deterministically import
/// whatever real Claude Code memory files happen to exist on the
/// machine running the test suite (this repo's own memory files,
/// found the hard way when `claude_home_from_env`'s `$HOME` vs.
/// `$HOME/.claude` bug was fixed and previously-passing tests started
/// failing because real content suddenly showed up in `inbox/import/`).
pub(crate) fn test_init(dir: &Path) -> Result<(), String> {
    let empty_claude_home = temp_dir("test-init-empty-claude-home");
    crate::init::run_init_with_claude_home(dir, Some(empty_claude_home.path()))
}

pub(crate) fn args(parts: &[&str]) -> impl Iterator<Item = String> {
    parts
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

pub(crate) fn search_paths(dir: &Path, query: &str) -> Vec<String> {
    let conn = wkp_core::index::open_index(&dir.join(".wkp/index.db")).expect("open index");
    wkp_core::index::search(&conn, query, &wkp_core::index::SearchFilter::default())
        .expect("search")
        .into_iter()
        .map(|h| h.path)
        .collect()
}

pub(crate) fn search_opts(
    dir: &Path,
    query: &str,
    format: crate::search::SearchFormat,
) -> crate::search::SearchOptions {
    crate::search::SearchOptions {
        path: dir.to_path_buf(),
        query: query.to_string(),
        tier: None,
        budget: None,
        limit: None,
        format,
        embed_url: None,
        embed_model: None,
        embed_key_file: None,
    }
}

/// A throwaway, passphrase-less ed25519 keypair for tests -- same
/// approach `wkp-git`'s own `signed_commit` tests use, duplicated
/// here rather than shared: there is no common test-utility crate,
/// and this is a handful of lines.
pub(crate) struct TestKey {
    pub(crate) private_path: PathBuf,
}

pub(crate) fn generate_test_key_and_register(dir: &Path, principal: &str) -> TestKey {
    // A key name unique per principal (and per call, via a
    // nanosecond suffix) -- a fixed name would collide the second
    // time this is called against the same `dir` (every `run_promote`
    // test registers at least two principals, e.g. an agent and a
    // human), and `ssh-keygen` prompts interactively to overwrite an
    // existing file rather than erroring, which just hangs under
    // `cargo test`.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key_name = format!("test-key-{}-{nanos}", crate::remember::slugify(principal));
    let private_path = dir.join(&key_name);
    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "test",
            "-f",
            &private_path.to_string_lossy(),
            "-q",
        ])
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
    let public_line = std::fs::read_to_string(dir.join(format!("{key_name}.pub")))
        .expect("read generated public key")
        .trim()
        .to_string();
    let mut fields = public_line.split_whitespace();
    let key_type = fields.next().expect("key type field").to_string();
    let key_base64 = fields.next().expect("base64 field").to_string();

    wkp_git::allowed_signers::append(
        &dir.join("allowed_signers"),
        &wkp_git::allowed_signers::SignerEntry {
            principal: principal.to_string(),
            role: wkp_git::allowed_signers::SignerRole::from_principal(principal),
            key_type,
            key_base64,
        },
    )
    .expect("append signer entry");

    TestKey { private_path }
}

pub(crate) fn remember_opts(
    dir: &Path,
    key: &TestKey,
    item_type: &str,
    title: &str,
) -> crate::remember::RememberOptions {
    crate::remember::RememberOptions {
        path: dir.to_path_buf(),
        item_type: item_type.to_string(),
        scope: Some("project".to_string()),
        title: Some(title.to_string()),
        principal: "agent:claude-code@host".to_string(),
        signing_key_file: key.private_path.clone(),
        session: Some("sess-abc".to_string()),
    }
}

/// Seeds a real `inbox/` item via `run_remember_with_body`, using the
/// same `key`/agent principal `remember_opts` already sets up --
/// `wkp promote`'s own tests promote what an actual `wkp remember`
/// call would have produced, not a hand-crafted fixture.
pub(crate) fn seed_inbox_item(dir: &Path, key: &TestKey, scope: &str, body: &str) -> String {
    let mut opts = remember_opts(dir, key, "knowledge", "A Remembered Thing");
    opts.scope = Some(scope.to_string());
    crate::remember::run_remember_with_body(&opts, body)
        .expect("seed run_remember_with_body")
        .relative_path
}

/// Registers the same signer entry (one real keypair, generated once)
/// in two separate stores' `allowed_signers` files. M3-4's sync test
/// needs two genuinely independent `wkp init`s (never sharing a single
/// commit) to reconcile cleanly on their very first cross-device
/// merge: `allowed_signers` is tracked, non-`.md` content, so
/// `wkp merge-driver`'s `.gitattributes` scoping does not apply to it
/// and an add/add divergence there would leave real conflict markers.
/// Giving both stores byte-identical `allowed_signers` content up
/// front (the same header, the same one entry) sidesteps that
/// entirely -- a realistic stand-in for "the org already distributed
/// a shared signer roster before either device made its first
/// commit", not a workaround for a real gap.
pub(crate) fn generate_test_key_and_register_in_both(
    dir_a: &Path,
    dir_b: &Path,
    principal: &str,
) -> TestKey {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key_name = format!("test-key-{}-{nanos}", crate::remember::slugify(principal));
    let private_path = dir_a.join(&key_name);
    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "test",
            "-f",
            &private_path.to_string_lossy(),
            "-q",
        ])
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
    let public_line = std::fs::read_to_string(dir_a.join(format!("{key_name}.pub")))
        .expect("read generated public key")
        .trim()
        .to_string();
    let mut fields = public_line.split_whitespace();
    let key_type = fields.next().expect("key type field").to_string();
    let key_base64 = fields.next().expect("base64 field").to_string();

    for dir in [dir_a, dir_b] {
        wkp_git::allowed_signers::append(
            &dir.join("allowed_signers"),
            &wkp_git::allowed_signers::SignerEntry {
                principal: principal.to_string(),
                role: wkp_git::allowed_signers::SignerRole::from_principal(principal),
                key_type: key_type.clone(),
                key_base64: key_base64.clone(),
            },
        )
        .expect("append signer entry");
    }

    TestKey { private_path }
}
