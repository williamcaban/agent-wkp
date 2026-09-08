//! Per-device identity and `sync/<device-id>` branches (design 4.2, 6.2
//! point 1, M3-1): the "avoid" layer of the safe-mode conflict strategy
//! -- unique branch names so two machines never write the same ref, and
//! a remote never sees a non-fast-forward push from concurrent devices.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::provenance::Provenance;
use crate::signed_commit::{self, CommitId};

/// Reads `store_path`'s persisted device ID (`.wkp/device-id`),
/// generating and persisting a new one on first use. Stable across runs
/// -- regenerated only if the file is explicitly removed. `.wkp/` is
/// this store's derived, gitignored directory (`index.db`, `tier*.md`):
/// a device ID belongs there for the same reason those do, but for the
/// opposite direction -- it must never be committed at all, since
/// cloning the store onto a second device must not inherit the first
/// device's ID (that would defeat the whole point of "per device").
pub fn device_id(store_path: &Path) -> Result<String, String> {
    let path = store_path.join(".wkp/device-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let generated = generate_device_id()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, format!("{generated}\n")).map_err(|e| e.to_string())?;
    Ok(generated)
}

/// 32 hex characters from 16 bytes read off `/dev/urandom` -- the OS's
/// own CSPRNG, not a new dependency, for a one-time, non-sensitive
/// identifier (CLAUDE.md's slim-core rule: this is exactly the kind of
/// thing a `uuid`/`rand` crate would otherwise exist only to provide).
/// Unix-only (`/dev/urandom`), matching this design's own platform list
/// (macOS, Linux; Windows is explicitly out of scope elsewhere in this
/// milestone).
fn generate_device_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    let mut urandom = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("opening /dev/urandom for a device id: {e}"))?;
    urandom
        .read_exact(&mut bytes)
        .map_err(|e| format!("reading /dev/urandom for a device id: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `sync/<device_id>` -- the per-device branch name (design 4.2, 6.2).
pub fn device_branch_name(device_id: &str) -> String {
    format!("sync/{device_id}")
}

/// Ensures `repo_dir` has a `sync/<device_id>` branch, creating it from
/// the current `HEAD` if it doesn't exist yet. Idempotent: a second call
/// is a no-op (does not reset an existing branch back to `HEAD`).
/// Requires at least one commit to already exist in `repo_dir` -- a
/// branch cannot point at nothing; callers making the very first commit
/// in a fresh store do so on whatever branch is already checked out
/// (git's own default), and only need this once that commit exists.
pub fn ensure_device_branch(repo_dir: &Path, device_id: &str) -> Result<(), String> {
    let branch = device_branch_name(device_id);
    let exists = super::run_git(
        repo_dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    if exists {
        return Ok(());
    }
    super::run_git(repo_dir, &["branch", &branch])
}

/// Whichever ref was checked out before [`commit_to_device_branch`]
/// switched away from it, so it can restore exactly that afterward --
/// a named branch, or (rarer, but must not be dropped) a detached
/// `HEAD` at a specific commit.
enum PreviousRef {
    Branch(String),
    DetachedAt(String),
}

fn capture_current_ref(repo_dir: &Path) -> Result<PreviousRef, String> {
    let symbolic = super::run_git_stdout(repo_dir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(branch) = symbolic {
        return Ok(PreviousRef::Branch(branch));
    }
    let sha = super::run_git_stdout(repo_dir, &["rev-parse", "HEAD"])?;
    Ok(PreviousRef::DetachedAt(sha.trim().to_string()))
}

fn restore_ref(repo_dir: &Path, previous: &PreviousRef) -> Result<(), String> {
    let target = match previous {
        PreviousRef::Branch(b) => b.as_str(),
        PreviousRef::DetachedAt(sha) => sha.as_str(),
    };
    super::run_git(repo_dir, &["checkout", "--quiet", target])
}

/// Commits `paths` onto `repo_dir`'s `sync/<device_id>` branch
/// specifically (signed, via [`signed_commit`]), restoring whichever
/// ref was checked out before as a last step -- a caller working on
/// `main` (or anywhere else) is not left switched onto the device
/// branch as a side effect.
///
/// Scoped to the realistic case this exists for (`wkp remember`-style
/// writes of a brand-new, uniquely-named path): `git checkout` only
/// touches paths *tracked and different* on the target branch, so a
/// genuinely new, not-yet-tracked-anywhere path written just before this
/// call survives the checkout untouched. This is not a general
/// "move arbitrary already-committed content to another branch"
/// primitive.
///
/// Handles the store's very first commit specially: [`ensure_device_branch`]
/// needs an existing commit to point a branch at, and there is no
/// previous branch worth restoring either (an unborn branch -- whatever
/// `git init`/`init.defaultBranch` happened to name it -- never held
/// anything). Retargeting `HEAD`'s symbolic ref directly at the device
/// branch before that first commit means every write a fresh store ever
/// makes lands on its device branch from the start, not just the second
/// one onward.
pub fn commit_to_device_branch(
    repo_dir: &Path,
    device_id: &str,
    paths: &[PathBuf],
    subject: &str,
    principal: &str,
    signing_key_path: &Path,
    provenance: &Provenance,
) -> Result<CommitId, String> {
    let branch = device_branch_name(device_id);
    let has_any_commit = super::run_git(repo_dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok();

    if !has_any_commit {
        super::run_git(
            repo_dir,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        )?;
        return signed_commit::signed_commit(
            repo_dir,
            paths,
            subject,
            principal,
            signing_key_path,
            provenance,
        );
    }

    ensure_device_branch(repo_dir, device_id)?;

    let previous = capture_current_ref(repo_dir)?;
    super::run_git(repo_dir, &["checkout", "--quiet", &branch])?;

    let result = signed_commit::signed_commit(
        repo_dir,
        paths,
        subject,
        principal,
        signing_key_path,
        provenance,
    );

    // Always attempt to restore, even if the commit itself failed --
    // a caller must not be left on the device branch just because the
    // write it was trying to make didn't go through.
    let restore_result = restore_ref(repo_dir, &previous);

    match (result, restore_result) {
        (Ok(commit), Ok(())) => Ok(commit),
        (Ok(_), Err(restore_err)) => Err(format!(
            "commit succeeded but failed to restore the previous branch: {restore_err}"
        )),
        (Err(commit_err), _) => Err(commit_err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("wkp-git-test-{name}-"))
            .tempdir()
            .expect("create temp dir")
    }

    #[test]
    fn device_id_is_generated_once_and_stable_across_calls() {
        let dir = temp_dir("device-id-stable");
        let first = device_id(dir.path()).expect("first device_id");
        let second = device_id(dir.path()).expect("second device_id");
        assert_eq!(first, second);
        assert_eq!(first.len(), 32, "expected 32 hex chars: {first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn device_id_differs_across_different_stores() {
        let dir_a = temp_dir("device-id-a");
        let dir_b = temp_dir("device-id-b");
        let a = device_id(dir_a.path()).expect("device_id a");
        let b = device_id(dir_b.path()).expect("device_id b");
        assert_ne!(a, b);
    }

    #[test]
    fn device_branch_name_formats_as_sync_prefixed() {
        assert_eq!(device_branch_name("abc123"), "sync/abc123");
    }

    fn init_with_one_commit(dir: &Path) {
        crate::init_repo(dir).expect("init_repo");
        crate::allowed_signers::configure_ssh_signing(dir).expect("configure_ssh_signing");
        std::fs::write(dir.join("seed.md"), "seed\n").expect("write seed.md");
        crate::commit_all(dir, "seed").expect("commit_all");
    }

    #[test]
    fn ensure_device_branch_creates_it_from_head() {
        let dir = temp_dir("ensure-branch-create");
        init_with_one_commit(dir.path());

        ensure_device_branch(dir.path(), "device-a").expect("ensure_device_branch");

        let head = crate::run_git_stdout(dir.path(), &["rev-parse", "HEAD"])
            .expect("rev-parse HEAD")
            .trim()
            .to_string();
        let branch_tip = crate::run_git_stdout(dir.path(), &["rev-parse", "sync/device-a"])
            .expect("rev-parse sync/device-a")
            .trim()
            .to_string();
        assert_eq!(head, branch_tip);
    }

    #[test]
    fn ensure_device_branch_is_idempotent() {
        let dir = temp_dir("ensure-branch-idempotent");
        init_with_one_commit(dir.path());

        ensure_device_branch(dir.path(), "device-a").expect("first ensure_device_branch");
        ensure_device_branch(dir.path(), "device-a").expect("second ensure_device_branch");

        let branch_tip = crate::run_git_stdout(dir.path(), &["rev-parse", "sync/device-a"])
            .expect("rev-parse sync/device-a");
        assert!(!branch_tip.trim().is_empty());
    }

    fn generate_test_key(dir: &Path, principal: &str) -> PathBuf {
        let private_path = dir.join("test-key");
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
        let public_line = std::fs::read_to_string(dir.join("test-key.pub"))
            .expect("read generated public key")
            .trim()
            .to_string();
        let mut fields = public_line.split_whitespace();
        let key_type = fields.next().expect("key type field").to_string();
        let key_base64 = fields.next().expect("base64 field").to_string();
        crate::allowed_signers::append(
            &dir.join("allowed_signers"),
            &crate::allowed_signers::SignerEntry {
                principal: principal.to_string(),
                role: crate::allowed_signers::SignerRole::from_principal(principal),
                key_type,
                key_base64,
            },
        )
        .expect("append signer entry");
        private_path
    }

    #[test]
    fn commit_to_device_branch_lands_only_on_the_device_branch_and_restores_main() {
        let dir = temp_dir("commit-device-branch");
        let repo = dir.path();
        init_with_one_commit(repo);
        let key = generate_test_key(repo, "agent:claude-code@host");

        let starting_branch = crate::run_git_stdout(repo, &["symbolic-ref", "--short", "HEAD"])
            .expect("starting branch")
            .trim()
            .to_string();

        std::fs::create_dir_all(repo.join("inbox")).expect("create inbox dir");
        std::fs::write(repo.join("inbox/new-item.md"), "new content\n")
            .expect("write new inbox item");

        commit_to_device_branch(
            repo,
            "device-a",
            &[PathBuf::from("inbox/new-item.md")],
            "remember: new item",
            "agent:claude-code@host",
            &key,
            &Provenance::default(),
        )
        .expect("commit_to_device_branch");

        let ending_branch = crate::run_git_stdout(repo, &["symbolic-ref", "--short", "HEAD"])
            .expect("ending branch")
            .trim()
            .to_string();
        assert_eq!(
            starting_branch, ending_branch,
            "must restore the originally checked-out branch"
        );

        let main_has_it =
            crate::run_git_stdout(repo, &["cat-file", "-e", "HEAD:inbox/new-item.md"]).is_ok();
        assert!(
            !main_has_it,
            "the new item must not have landed on the original branch's history"
        );

        let device_branch_has_it =
            crate::run_git_stdout(repo, &["cat-file", "-e", "sync/device-a:inbox/new-item.md"])
                .is_ok();
        assert!(
            device_branch_has_it,
            "the new item must be committed on the device branch"
        );
    }

    #[test]
    fn commit_to_device_branch_bootstraps_a_fresh_stores_very_first_commit_onto_the_device_branch()
    {
        let dir = temp_dir("commit-device-branch-bootstrap");
        let repo = dir.path();
        crate::init_repo(repo).expect("init_repo");
        crate::allowed_signers::configure_ssh_signing(repo).expect("configure_ssh_signing");
        let key = generate_test_key(repo, "agent:claude-code@host");

        std::fs::create_dir_all(repo.join("inbox")).expect("create inbox dir");
        std::fs::write(repo.join("inbox/first-item.md"), "first content\n")
            .expect("write first inbox item");

        commit_to_device_branch(
            repo,
            "device-a",
            &[PathBuf::from("inbox/first-item.md")],
            "remember: first item",
            "agent:claude-code@host",
            &key,
            &Provenance::default(),
        )
        .expect("commit_to_device_branch on an empty store");

        let current_branch = crate::run_git_stdout(repo, &["symbolic-ref", "--short", "HEAD"])
            .expect("current branch")
            .trim()
            .to_string();
        assert_eq!(
            current_branch, "sync/device-a",
            "the store's very first commit must land directly on the device branch"
        );

        let has_it =
            crate::run_git_stdout(repo, &["cat-file", "-e", "HEAD:inbox/first-item.md"]).is_ok();
        assert!(has_it);
    }
}
