//! The real, SSH-signed commit primitive (design 5.1, 7.3, M2-2) --
//! replaces `commit_all`'s explicit "porcelain, not plumbing, and no
//! signing" placeholder for the write path's actual audit-trail commits.
//!
//! True plumbing (`hash-object` / `update-index --cacheinfo` /
//! `write-tree` / `commit-tree` / `update-ref`), not `git add -A` +
//! `git commit`: a caller like `wkp remember` (M2-5) must commit *exactly*
//! the paths it just wrote, never anything else a user or another
//! in-progress `wkp` invocation happens to have sitting unstaged in the
//! same store. `git add -A`-style staging has no way to express that
//! scoping; individually hashing and `--cacheinfo`-staging each path
//! does.

use std::path::{Path, PathBuf};

use crate::provenance::Provenance;

/// The object ID (40-hex-char SHA-1, or 64-char SHA-256 on a
/// `--object-format=sha256` repository) of a commit `signed_commit`
/// produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitId(pub String);

/// Creates an SSH-signed commit containing exactly `paths` (each must
/// already exist on disk at `repo_dir.join(path)`, relative to
/// `repo_dir`, with the content the caller wants committed -- this
/// function only stages and commits, it does not write file content
/// itself) on top of the current `HEAD` (or as a root commit, if `HEAD`
/// doesn't resolve yet -- a fresh store's first commit).
///
/// `principal` becomes both the commit's author and committer identity
/// (design 7.3's `human:<id>` / `agent:<harness>[:<model>]` convention --
/// the same string [`crate::allowed_signers`] keys signer entries by).
/// `signing_key_path` is passed as `user.signingkey`; per `ssh-keygen(1)`'s
/// own `-f` semantics this can be a private key file directly (as tests
/// here do, for a self-contained throwaway key with no `ssh-agent`
/// involved) or a public key file whose matching private half
/// `ssh-agent` already holds.
///
/// `subject` is the commit message's first line; `provenance`'s non-`None`
/// fields (design 5.4, 7.4, M2-3) are appended as `Wkp-*` trailers via
/// [`Provenance::format_message`] -- an all-`None` `Provenance` leaves
/// `subject` unchanged, no empty trailer block.
///
/// All three of `gpg.format`, `user.signingkey`, `user.name`/`user.email`
/// are passed as per-invocation `-c` overrides (matching `commit_all`'s
/// existing pattern) rather than written to the repo's persisted config:
/// a store's committed history is meant to carry commits from many
/// different principals (a human, and one `agent:` identity per
/// harness), so there is no single "the" signing identity to persist
/// into local config the way `wkp init`'s other settings are.
///
/// Signing failure (a missing or invalid `signing_key_path`, an
/// `ssh-agent` that doesn't hold the matching private key, ...) is fatal
/// -- `git commit-tree -S`'s own default behavior -- never a silent
/// fallback to an unsigned commit; the `Err` this function returns in
/// that case is git's own stderr, unmodified.
pub fn signed_commit(
    repo_dir: &Path,
    paths: &[PathBuf],
    subject: &str,
    principal: &str,
    signing_key_path: &Path,
    provenance: &Provenance,
) -> Result<CommitId, String> {
    if paths.is_empty() {
        return Err("signed_commit: no paths given".to_string());
    }

    for path in paths {
        let full_path = repo_dir.join(path);
        let full_path_str = full_path.to_string_lossy();
        let blob = super::run_git_stdout(repo_dir, &["hash-object", "-w", "--", &full_path_str])?;
        let blob = blob.trim();
        let path_str = path.to_string_lossy();
        let cacheinfo = format!("100644,{blob},{path_str}");
        super::run_git(
            repo_dir,
            &["update-index", "--add", "--cacheinfo", &cacheinfo],
        )?;
    }

    let tree = super::run_git_stdout(repo_dir, &["write-tree"])?;
    let tree = tree.trim();

    // No parent on a fresh store's very first commit -- `rev-parse
    // --verify -q HEAD` exits non-zero with empty output when `HEAD`
    // doesn't resolve to a commit yet, which this treats as "no parent"
    // rather than propagating as an error.
    let parent = super::run_git_stdout(repo_dir, &["rev-parse", "--verify", "-q", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let full_message = provenance.format_message(repo_dir, subject)?;

    let signing_key_str = signing_key_path.to_string_lossy();
    let user_name_arg = format!("user.name={principal}");
    let user_email_arg = format!("user.email={principal}");
    let signing_key_arg = format!("user.signingkey={signing_key_str}");
    let mut args: Vec<&str> = vec![
        "-c",
        "gpg.format=ssh",
        "-c",
        &signing_key_arg,
        "-c",
        &user_name_arg,
        "-c",
        &user_email_arg,
        "commit-tree",
        "-S",
        tree,
        "-m",
        &full_message,
    ];
    if let Some(parent) = &parent {
        args.push("-p");
        args.push(parent);
    }
    let commit = super::run_git_stdout(repo_dir, &args)?;
    let commit = commit.trim().to_string();

    super::run_git(repo_dir, &["update-ref", "HEAD", &commit])?;

    Ok(CommitId(commit))
}

/// Checks `commit`'s SSH signature against `repo_dir`'s configured
/// `gpg.ssh.allowedSignersFile` (the file [`crate::allowed_signers`]
/// manages) via `git verify-commit`. `Ok(())` for a good signature from a
/// principal present in that file; `Err` (git's own stderr) for anything
/// else -- unsigned, signed by an unknown key, or a signature that
/// doesn't verify.
pub fn verify_commit(repo_dir: &Path, commit: &CommitId) -> Result<(), String> {
    super::run_git(repo_dir, &["verify-commit", &commit.0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowed_signers::{self, SignerEntry, SignerRole};
    use std::process::Command;

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("wkp-git-test-{name}-"))
            .tempdir()
            .expect("create temp dir")
    }

    /// A throwaway, passphrase-less ed25519 keypair for tests -- shelling
    /// to `ssh-keygen` (not `git`, so CLAUDE.md's "no `Command::new(\"git\")`
    /// outside this crate" rule doesn't apply) is the minimal way to get a
    /// real key `git`'s own SSH-signing code path can use, without adding
    /// a crypto dependency this crate would otherwise have no reason to
    /// carry.
    struct TestKey {
        private_path: PathBuf,
        public_line: String,
    }

    fn generate_test_key(dir: &Path, comment: &str) -> TestKey {
        let private_path = dir.join(format!("{comment}-key"));
        let status = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                comment,
                "-f",
                &private_path.to_string_lossy(),
                "-q",
            ])
            .status()
            .expect("run ssh-keygen");
        assert!(status.success(), "ssh-keygen failed");
        let public_line = std::fs::read_to_string(dir.join(format!("{comment}-key.pub")))
            .expect("read generated public key")
            .trim()
            .to_string();
        TestKey {
            private_path,
            public_line,
        }
    }

    /// `<key-type> <base64>` from a full `ssh-keygen`-format public key
    /// line, which also carries a trailing ` comment` this module's
    /// `<principal> <key-type> <base64-key>` format has no room for.
    fn key_type_and_base64(public_line: &str) -> (String, String) {
        let mut fields = public_line.split_whitespace();
        let key_type = fields.next().expect("key type field").to_string();
        let key_base64 = fields.next().expect("base64 field").to_string();
        (key_type, key_base64)
    }

    fn setup_signed_repo(dir: &Path, principal: &str) -> TestKey {
        crate::init_repo(dir).expect("init_repo");
        allowed_signers::configure_ssh_signing(dir).expect("configure_ssh_signing");

        let key = generate_test_key(dir, "test");
        let (key_type, key_base64) = key_type_and_base64(&key.public_line);
        allowed_signers::append(
            &dir.join("allowed_signers"),
            &SignerEntry {
                principal: principal.to_string(),
                role: SignerRole::Human,
                key_type,
                key_base64,
            },
        )
        .expect("append signer entry");
        key
    }

    #[test]
    fn signed_commit_produces_a_commit_that_verify_commit_accepts() {
        let temp = temp_dir("signed-commit-verify");
        let dir = temp.path();
        let key = setup_signed_repo(dir, "human:alice");

        std::fs::write(dir.join("a.md"), "hello world\n").expect("write a.md");
        let commit = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "first commit",
            "human:alice",
            &key.private_path,
            &Provenance::default(),
        )
        .expect("signed_commit");

        verify_commit(dir, &commit).expect("verify_commit should accept a good signature");
    }

    #[test]
    fn signed_commit_message_and_content_round_trip() {
        let temp = temp_dir("signed-commit-content");
        let dir = temp.path();
        let key = setup_signed_repo(dir, "human:alice");

        std::fs::write(dir.join("a.md"), "distinctive body\n").expect("write a.md");
        let commit = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "a distinctive commit message",
            "human:alice",
            &key.private_path,
            &Provenance::default(),
        )
        .expect("signed_commit");

        let log = crate::run_git_stdout(dir, &["log", "-1", "--format=%B", &commit.0])
            .expect("read commit message");
        assert_eq!(log.trim(), "a distinctive commit message");

        let show = crate::run_git_stdout(dir, &["show", &format!("{}:a.md", commit.0)])
            .expect("read committed content");
        assert_eq!(show, "distinctive body\n");
    }

    #[test]
    fn signed_commit_second_commit_has_the_first_as_its_parent() {
        let temp = temp_dir("signed-commit-parent");
        let dir = temp.path();
        let key = setup_signed_repo(dir, "human:alice");

        std::fs::write(dir.join("a.md"), "first\n").expect("write a.md");
        let first = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "first",
            "human:alice",
            &key.private_path,
            &Provenance::default(),
        )
        .expect("first signed_commit");

        std::fs::write(dir.join("a.md"), "second\n").expect("rewrite a.md");
        let second = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "second",
            "human:alice",
            &key.private_path,
            &Provenance::default(),
        )
        .expect("second signed_commit");

        let parent = crate::run_git_stdout(dir, &["rev-parse", &format!("{}^", second.0)])
            .expect("resolve parent");
        assert_eq!(parent.trim(), first.0);
    }

    #[test]
    fn signed_commit_fails_clearly_on_a_missing_signing_key_and_creates_no_commit() {
        let temp = temp_dir("signed-commit-missing-key");
        let dir = temp.path();
        crate::init_repo(dir).expect("init_repo");
        allowed_signers::configure_ssh_signing(dir).expect("configure_ssh_signing");

        std::fs::write(dir.join("a.md"), "hello\n").expect("write a.md");
        let result = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "should fail",
            "human:alice",
            &dir.join("does-not-exist-key"),
            &Provenance::default(),
        );

        assert!(
            result.is_err(),
            "expected signing with a missing key to fail"
        );
        let head = crate::run_git_stdout(dir, &["rev-parse", "--verify", "-q", "HEAD"]);
        assert!(
            head.is_err(),
            "a failed signed_commit must not have created any commit at all"
        );
    }

    #[test]
    fn signed_commit_rejects_an_empty_path_list() {
        let temp = temp_dir("signed-commit-empty-paths");
        let dir = temp.path();
        let key = setup_signed_repo(dir, "human:alice");

        let result = signed_commit(
            dir,
            &[],
            "empty",
            "human:alice",
            &key.private_path,
            &Provenance::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_commit_rejects_an_unsigned_commit() {
        let temp = temp_dir("verify-commit-unsigned");
        let dir = temp.path();
        crate::init_repo(dir).expect("init_repo");
        allowed_signers::configure_ssh_signing(dir).expect("configure_ssh_signing");

        std::fs::write(dir.join("a.md"), "hello\n").expect("write a.md");
        crate::commit_all(dir, "unsigned commit").expect("commit_all");
        let sha = crate::run_git_stdout(dir, &["rev-parse", "HEAD"])
            .expect("rev-parse HEAD")
            .trim()
            .to_string();

        let result = verify_commit(dir, &CommitId(sha));
        assert!(result.is_err(), "an unsigned commit must not verify");
    }

    /// M2-3 acceptance criterion: a commit written via `signed_commit`
    /// with real provenance round-trips exactly through
    /// `read_provenance_trailers`.
    #[test]
    fn signed_commit_provenance_round_trips_through_read_provenance_trailers() {
        let temp = temp_dir("signed-commit-provenance-roundtrip");
        let dir = temp.path();
        let key = setup_signed_repo(dir, "agent:claude-code@host");

        std::fs::write(dir.join("a.md"), "hello\n").expect("write a.md");
        let provenance = Provenance {
            actor: Some("agent:claude-code@host".to_string()),
            session: Some("7f3c".to_string()),
            source: Some("conversation".to_string()),
            confidence: Some("proposed".to_string()),
        };
        let commit = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "remembered something",
            "agent:claude-code@host",
            &key.private_path,
            &provenance,
        )
        .expect("signed_commit");

        let read_back = crate::provenance::read_provenance_trailers(dir, &commit)
            .expect("read_provenance_trailers")
            .expect("expected Some(Provenance), got None");
        assert_eq!(read_back, provenance);
    }

    #[test]
    fn signed_commit_with_default_provenance_round_trips_to_none() {
        let temp = temp_dir("signed-commit-provenance-none");
        let dir = temp.path();
        let key = setup_signed_repo(dir, "human:alice");

        std::fs::write(dir.join("a.md"), "hello\n").expect("write a.md");
        let commit = signed_commit(
            dir,
            &[PathBuf::from("a.md")],
            "no provenance here",
            "human:alice",
            &key.private_path,
            &Provenance::default(),
        )
        .expect("signed_commit");

        let read_back = crate::provenance::read_provenance_trailers(dir, &commit).expect("read");
        assert_eq!(read_back, None);
    }
}
