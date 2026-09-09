//! M4-4 (issue #76) acceptance criteria: real `git add`/`git commit`/
//! `git checkout` round trips through the compiled `wkp filter
//! clean`/`smudge` subcommands, driving `git` as a subprocess directly --
//! the same deliberate, narrow exception to "no `Command::new(\"git\")`
//! outside `crates/wkp-git`" that `tests/merge_driver.rs` already
//! established, for the same reason: only a real git filter invocation
//! proves the protocol wiring (`.gitattributes` + `filter.wkp-crypt.*`
//! config) actually works, not just the filter *logic*
//! (`crates/wkp-cli/src/filter.rs`'s own unit tests already cover that).

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
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
    output
}

fn wkp(dir: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_wkp"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run wkp");
    assert!(
        output.status.success(),
        "wkp {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(dir: &Path, relative: &str, contents: &str) {
    if let Some(parent) = Path::new(relative).parent() {
        std::fs::create_dir_all(dir.join(parent)).expect("create parent dirs");
    }
    std::fs::write(dir.join(relative), contents).expect("write file");
}

fn read(dir: &Path, relative: &str) -> Vec<u8> {
    std::fs::read(dir.join(relative)).expect("read file")
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", message]);
}

fn init_store(dir: &Path) {
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    wkp(dir, &["init"]);
}

fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("create temp dir")
}

const PRIVATE_CONTENT: &str = "---\nvisibility: private\n---\n\nsecret body\n";
const SHARED_CONTENT: &str = "---\nvisibility: shared\n---\n\nshared body\n";
const AGE_HEADER: &[u8] = b"age-encryption.org/v1";

/// A real `git add`/`git commit` of a `visibility: private` file produces
/// an encrypted (age ciphertext) blob in the object database, not the
/// plaintext -- `wkp init` registered this device as a recipient, so the
/// clean filter had a real recipients file to encrypt to.
#[test]
fn committing_a_private_file_stores_an_encrypted_blob() {
    let temp = temp_dir("wkp-cli-filter-private-blob-");
    let dir = temp.path();
    init_store(dir);

    write(dir, "secret.md", PRIVATE_CONTENT);
    commit_all(dir, "add private item");

    let blob = git(dir, &["cat-file", "-p", "HEAD:secret.md"]).stdout;
    assert!(
        blob.starts_with(AGE_HEADER),
        "blob should be age ciphertext, got: {:?}",
        String::from_utf8_lossy(&blob[..blob.len().min(80)])
    );
    assert_ne!(blob, PRIVATE_CONTENT.as_bytes());
}

/// A `shared`-visibility file's blob is untouched plaintext through the
/// same round trip -- confirms the pass-through path, not just the
/// encrypted path (M4-4's own acceptance criterion, worded exactly this
/// way).
#[test]
fn committing_a_shared_file_stores_the_plaintext_blob_untouched() {
    let temp = temp_dir("wkp-cli-filter-shared-blob-");
    let dir = temp.path();
    init_store(dir);

    write(dir, "note.md", SHARED_CONTENT);
    commit_all(dir, "add shared item");

    let blob = git(dir, &["cat-file", "-p", "HEAD:note.md"]).stdout;
    assert_eq!(blob, SHARED_CONTENT.as_bytes());
}

/// A real `git checkout` of a private path, in a fresh clone with the
/// right device identity available, produces the original plaintext.
/// "The right device identity available" is simulated by copying this
/// device's own un-synced local state (`.wkp/device-id`,
/// `.wkp/device-identity`) into the clone before its own `wkp init` runs
/// -- that local state is deliberately never part of what `git clone`
/// transfers (gitignored), so a literal fresh clone with no such copy
/// would correctly be unable to decrypt yet (a genuinely new, unregistered
/// device -- out of scope here; device-to-device recipient distribution
/// is deferred past this task).
#[test]
fn checking_out_a_private_file_in_a_fresh_clone_with_the_right_identity_produces_plaintext() {
    let temp = temp_dir("wkp-cli-filter-clone-checkout-");
    let origin = temp.path().join("origin");
    std::fs::create_dir_all(&origin).expect("mkdir origin");
    init_store(&origin);

    write(&origin, "secret.md", PRIVATE_CONTENT);
    commit_all(&origin, "add private item");

    let clone = temp.path().join("clone");
    git(
        temp.path(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );

    // Carry this device's real identity into the clone before `wkp init`
    // runs there, so `configure_encryption_filter` recognizes it as
    // already-registered instead of minting (and registering) a
    // different one.
    std::fs::create_dir_all(clone.join(".wkp")).expect("mkdir clone .wkp");
    std::fs::copy(origin.join(".wkp/device-id"), clone.join(".wkp/device-id"))
        .expect("copy device-id");
    let identity_fallback = origin.join(".wkp/device-identity");
    if identity_fallback.exists() {
        std::fs::copy(&identity_fallback, clone.join(".wkp/device-identity"))
            .expect("copy device-identity fallback");
    }

    git(&clone, &["config", "user.name", "Test User"]);
    git(&clone, &["config", "user.email", "test@example.com"]);

    // `git clone` populates the working tree *before* this point -- the
    // `.gitattributes` `filter=wkp-crypt` attribute is already tracked
    // and in effect, but the local git config naming an actual filter
    // *command* for `wkp-crypt` (set by `wkp init`, below) doesn't exist
    // yet. Confirmed by hand: with no filter command configured, git
    // just writes the raw (ciphertext) blob into the working tree during
    // that initial checkout, no error or warning. So `secret.md` on disk
    // right now is still ciphertext, and a plain `git checkout -- secret.md`
    // *after* `wkp init` is a no-op: git considers the working-tree file
    // already "clean" relative to the index's cached stat and skips
    // re-smudging it, even though the filter command is now configured.
    // Removing the file first forces a real (re-)checkout that actually
    // runs the now-configured smudge filter -- exactly what a real user
    // would need to do too (e.g. `rm` + `git checkout`, or a second
    // clone) to see plaintext after registering a device on a repo they
    // already cloned.
    wkp(&clone, &["init"]);
    std::fs::remove_file(clone.join("secret.md")).expect("remove pre-filter working copy");

    git(&clone, &["checkout", "--quiet", "--", "secret.md"]);
    let checked_out = read(&clone, "secret.md");
    assert_eq!(
        checked_out,
        PRIVATE_CONTENT.as_bytes(),
        "checkout with the right device identity available must produce the original plaintext"
    );
}

/// Unchanged private files eventually stop being re-encrypted (and so
/// eventually stop being falsely reported as modified) once repeated
/// `add`s land outside git's own "racy git" window -- design 7.2 notes
/// the indexer's own blob-SHA cache already avoids gratuitous
/// re-filtering; this verifies the *filter* path's behavior independently.
///
/// **Real finding, not an assumption**: age's clean output is genuinely
/// nondeterministic (a fresh random nonce every run), and confirmed by
/// hand (with a real repo, typing commands with natural pauses between
/// them) that this is fine in ordinary use. But driven back-to-back with
/// no delay -- as any test, or any script/agent calling `wkp remember`
/// in a tight loop, would -- every `git add` lands inside git's "racy
/// git" window relative to the previous one (same-second mtime), so git
/// cannot trust its cached stat and must re-run the clean filter to
/// check for a real change; age's nondeterminism then makes that
/// recheck *always* look like a change, which git accepts as the new
/// index content with a freshly racy stat of its own -- repeating on the
/// next add, too. It takes a handful of adds, each separated by enough
/// real wall-clock time to clear the previous one's racy window, before
/// the cached stat is finally trustworthy and the blob stabilizes. This
/// loop models exactly that: real elapsed time between adds, polling for
/// convergence rather than asserting it happens on any specific one.
#[test]
fn an_unchanged_private_file_is_not_re_encrypted_on_repeated_add() {
    let temp = temp_dir("wkp-cli-filter-unchanged-");
    let dir = temp.path();
    init_store(dir);

    write(dir, "secret.md", PRIVATE_CONTENT);
    commit_all(dir, "add private item");

    // Touch the file's mtime without changing its content -- this alone
    // forces at least one real re-clean (a fresh mtime always requires
    // git to recheck), so what matters is convergence afterward, not
    // this specific blob.
    let contents = read(dir, "secret.md");
    std::fs::write(dir.join("secret.md"), &contents).expect("rewrite unchanged content");

    let rev_parse_index_sha = |dir: &Path| {
        String::from_utf8(git(dir, &["rev-parse", ":secret.md"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };

    const RACY_GIT_WINDOW: std::time::Duration = std::time::Duration::from_millis(1100);
    const MAX_ADDS: u32 = 10;

    git(dir, &["add", "-A"]);
    let mut previous_sha = rev_parse_index_sha(dir);
    let mut converged = false;
    for _ in 0..MAX_ADDS {
        std::thread::sleep(RACY_GIT_WINDOW);
        git(dir, &["add", "-A"]);
        let sha = rev_parse_index_sha(dir);
        if sha == previous_sha {
            converged = true;
            break;
        }
        previous_sha = sha;
    }

    assert!(
        converged,
        "blob sha for an unchanged file never stabilized across {MAX_ADDS} real-time-spaced \
         `git add`s -- either the clean filter is genuinely being re-trusted on every add \
         (a real regression), or the racy-git window is wider than {RACY_GIT_WINDOW:?} in \
         this environment and MAX_ADDS/RACY_GIT_WINDOW need to grow"
    );

    // One more add, still spaced past the racy window, must reproduce
    // the same blob -- confirming this is real convergence, not two
    // values that coincidentally matched once.
    std::thread::sleep(RACY_GIT_WINDOW);
    git(dir, &["add", "-A"]);
    assert_eq!(rev_parse_index_sha(dir), previous_sha);
}
