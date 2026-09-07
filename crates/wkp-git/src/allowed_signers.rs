//! The `allowed_signers` file the store carries (design 7.3): principal ->
//! SSH public key, in the exact minimal format git's own
//! `gpg.ssh.allowedSignersFile` consumes for `git verify-commit` (a plain
//! `<principal> <key-type> <base64-key>` line -- see the format git's own
//! docs show for that config key). "Role" (human vs. agent) is not a
//! separate column: it's read directly off the principal string's own
//! `human:`/`agent:` prefix, the same convention design 5.4's
//! `provenance.actor` field already uses (`human:<id>` /
//! `agent:<harness>[:<model>]`), so this file needs no wkp-specific
//! superset of git's own format to carry it -- git can consume it exactly
//! as written, with zero translation.

use std::path::Path;

/// Read off a [`SignerEntry`]'s `principal` prefix, not stored as its own
/// column in the file (see the module doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerRole {
    Human,
    Agent,
    /// A principal matching neither `human:` nor `agent:` -- accepted
    /// (any principal string git itself would accept is valid here too),
    /// just never eligible for the tier 0/1 provenance gate (M2-6), which
    /// only recognizes [`SignerRole::Human`].
    Other,
}

impl SignerRole {
    fn from_principal(principal: &str) -> Self {
        if principal.starts_with("human:") {
            SignerRole::Human
        } else if principal.starts_with("agent:") {
            SignerRole::Agent
        } else {
            SignerRole::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerEntry {
    pub principal: String,
    pub role: SignerRole,
    pub key_type: String,
    pub key_base64: String,
}

/// A new-file header explaining the format to a human who opens it --
/// written once, when [`configure_ssh_signing`] creates the file for the
/// first time. `parse` ignores `#`-prefixed lines, so this is inert to
/// every consumer, including git itself.
const FILE_HEADER: &str = "\
# wkp allowed_signers (design 7.3). One SSH signer per line:
#   <principal> <key-type> <base64-key>
# <principal> is `human:<id>` or `agent:<harness>[:<model>]` (design 5.4's
# provenance.actor convention) -- role (human/agent) is read from that
# prefix, not a separate column. Consumed directly by git itself via
# gpg.ssh.allowedSignersFile; see `git help config` for the format.
";

/// Parses an `allowed_signers`-format file body. Blank lines and
/// `#`-comments are ignored. A line is expected to be exactly three
/// whitespace-separated fields (`principal key-type key-base64`); a line
/// with more or fewer fields (a multi-principal list, or one of
/// `ssh-keygen(1)`'s `namespaces=`/`valid-after=` options, neither of
/// which anything here ever writes) is skipped rather than mis-parsed --
/// tolerant, matching `wkp-core::frontmatter`'s own posture on malformed
/// or unrecognized input.
pub fn parse(contents: &str) -> Vec<SignerEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split_whitespace();
            let principal = fields.next()?;
            let key_type = fields.next()?;
            let key_base64 = fields.next()?;
            if fields.next().is_some() {
                return None;
            }
            Some(SignerEntry {
                principal: principal.to_string(),
                role: SignerRole::from_principal(principal),
                key_type: key_type.to_string(),
                key_base64: key_base64.to_string(),
            })
        })
        .collect()
}

fn render_line(entry: &SignerEntry) -> String {
    format!(
        "{} {} {}",
        entry.principal, entry.key_type, entry.key_base64
    )
}

/// Appends `entry` to the `allowed_signers` file at `path`, creating it
/// (with [`FILE_HEADER`]) if it doesn't exist yet. Idempotent: an entry
/// whose principal, key type and key already appear as an identical line
/// is left alone, not duplicated -- re-running `wkp init`, or a harness
/// re-registering its own key on every startup, must not grow the file
/// forever.
pub fn append(path: &Path, entry: &SignerEntry) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let new_line = render_line(entry);
    if existing.lines().any(|line| line.trim() == new_line) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut updated = if existing.is_empty() {
        FILE_HEADER.to_string()
    } else {
        existing
    };
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&new_line);
    updated.push('\n');
    std::fs::write(path, updated).map_err(|e| e.to_string())
}

/// `wkp init`'s SSH-signing wiring (design 7.3): ensures `repo_dir`'s
/// `allowed_signers` file exists (creating it with just [`FILE_HEADER`]
/// if this is the first run), then sets `gpg.format=ssh` and
/// `gpg.ssh.allowedSignersFile` (local config, matching
/// `apply_init_settings`'s existing per-repo, non-global posture) to its
/// absolute path -- absolute rather than relative because git's
/// resolution of a relative `gpg.ssh.allowedSignersFile` value is not
/// consistent enough across versions to rely on (unlike, say,
/// `core.hooksPath`, which the git docs are explicit about).
///
/// Idempotent: re-running `wkp init` against an existing store neither
/// duplicates the file's header nor errors if the config is already set
/// (`git config --local` overwrites the existing value, git's own
/// behavior, not special-cased here).
pub fn configure_ssh_signing(repo_dir: &Path) -> Result<(), String> {
    let signers_path = repo_dir.join("allowed_signers");
    if !signers_path.exists() {
        std::fs::write(&signers_path, FILE_HEADER).map_err(|e| e.to_string())?;
    }
    let absolute = std::fs::canonicalize(&signers_path).map_err(|e| e.to_string())?;
    let absolute_str = absolute.to_string_lossy();

    super::run_git(repo_dir, &["config", "--local", "gpg.format", "ssh"])
        .map_err(|e| format!("wkp: failed to set git config gpg.format=ssh: {e}"))?;
    super::run_git(
        repo_dir,
        &[
            "config",
            "--local",
            "gpg.ssh.allowedSignersFile",
            &absolute_str,
        ],
    )
    .map_err(|e| format!("wkp: failed to set git config gpg.ssh.allowedSignersFile: {e}"))?;
    Ok(())
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
    fn parse_reads_a_well_formed_line() {
        let entries = parse("human:alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].principal, "human:alice");
        assert_eq!(entries[0].role, SignerRole::Human);
        assert_eq!(entries[0].key_type, "ssh-ed25519");
        assert_eq!(entries[0].key_base64, "AAAAC3NzaC1lZDI1NTE5AAAAI...");
    }

    #[test]
    fn parse_derives_agent_role_from_principal_prefix() {
        let entries = parse("agent:claude-code@host ssh-ed25519 AAAA...\n");
        assert_eq!(entries[0].role, SignerRole::Agent);
    }

    #[test]
    fn parse_derives_other_role_for_an_unrecognized_principal_prefix() {
        let entries = parse("alice@example.com ssh-ed25519 AAAA...\n");
        assert_eq!(entries[0].role, SignerRole::Other);
    }

    #[test]
    fn parse_ignores_blank_lines_and_comments() {
        let entries =
            parse("# a comment\n\n   \nhuman:alice ssh-ed25519 AAAA...\n# trailing comment\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].principal, "human:alice");
    }

    #[test]
    fn parse_skips_a_line_with_too_few_fields_instead_of_panicking() {
        let entries = parse("human:alice ssh-ed25519\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_skips_a_line_with_extra_fields_instead_of_misparsing() {
        // A multi-principal or options-bearing line this parser doesn't
        // understand yet -- must be skipped, not silently truncated into
        // a wrong-but-plausible-looking entry.
        let entries = parse("human:alice,human:bob ssh-ed25519 AAAA... trailing\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_never_panics_on_arbitrary_bytes() {
        for sample in [
            "",
            "\0\0\0",
            "one two three four five",
            "\t\t\t",
            "🎉 emoji-principal ssh-ed25519 AAAA",
        ] {
            let _ = parse(sample);
        }
    }

    #[test]
    fn append_creates_the_file_with_a_header_and_the_entry() {
        let dir = temp_dir("append-create");
        let path = dir.path().join("allowed_signers");
        let entry = SignerEntry {
            principal: "human:alice".to_string(),
            role: SignerRole::Human,
            key_type: "ssh-ed25519".to_string(),
            key_base64: "AAAA...".to_string(),
        };
        append(&path, &entry).expect("append");

        let contents = std::fs::read_to_string(&path).expect("read allowed_signers");
        assert!(contents.contains("wkp allowed_signers"));
        assert!(contents.contains("human:alice ssh-ed25519 AAAA..."));

        let parsed = parse(&contents);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], entry);
    }

    #[test]
    fn append_is_idempotent_for_an_identical_entry() {
        let dir = temp_dir("append-idempotent");
        let path = dir.path().join("allowed_signers");
        let entry = SignerEntry {
            principal: "human:alice".to_string(),
            role: SignerRole::Human,
            key_type: "ssh-ed25519".to_string(),
            key_base64: "AAAA...".to_string(),
        };
        append(&path, &entry).expect("first append");
        append(&path, &entry).expect("second append");

        let contents = std::fs::read_to_string(&path).expect("read allowed_signers");
        assert_eq!(
            contents.matches("human:alice ssh-ed25519 AAAA...").count(),
            1,
            "re-appending an identical entry must not duplicate the line"
        );
    }

    #[test]
    fn append_adds_a_second_distinct_entry_without_touching_the_first() {
        let dir = temp_dir("append-second");
        let path = dir.path().join("allowed_signers");
        let alice = SignerEntry {
            principal: "human:alice".to_string(),
            role: SignerRole::Human,
            key_type: "ssh-ed25519".to_string(),
            key_base64: "AAAA...".to_string(),
        };
        let bot = SignerEntry {
            principal: "agent:claude-code@host".to_string(),
            role: SignerRole::Agent,
            key_type: "ssh-ed25519".to_string(),
            key_base64: "BBBB...".to_string(),
        };
        append(&path, &alice).expect("append alice");
        append(&path, &bot).expect("append bot");

        let contents = std::fs::read_to_string(&path).expect("read allowed_signers");
        let parsed = parse(&contents);
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(&alice));
        assert!(parsed.contains(&bot));
    }

    #[test]
    fn append_creates_missing_parent_directories() {
        let dir = temp_dir("append-parent");
        let path = dir.path().join("nested/deep/allowed_signers");
        let entry = SignerEntry {
            principal: "human:alice".to_string(),
            role: SignerRole::Human,
            key_type: "ssh-ed25519".to_string(),
            key_base64: "AAAA...".to_string(),
        };
        append(&path, &entry).expect("append with missing parent dirs");
        assert!(path.is_file());
    }

    #[test]
    fn configure_ssh_signing_creates_the_file_and_sets_git_config() {
        let dir = temp_dir("configure-ssh-signing");
        let repo = dir.path();
        crate::init_repo(repo).expect("init_repo");

        configure_ssh_signing(repo).expect("configure_ssh_signing");

        assert!(repo.join("allowed_signers").is_file());

        let format = crate::run_git_stdout(repo, &["config", "--local", "gpg.format"])
            .expect("read gpg.format");
        assert_eq!(format.trim(), "ssh");

        let signers_file =
            crate::run_git_stdout(repo, &["config", "--local", "gpg.ssh.allowedSignersFile"])
                .expect("read gpg.ssh.allowedSignersFile");
        let expected = std::fs::canonicalize(repo.join("allowed_signers")).expect("canonicalize");
        assert_eq!(signers_file.trim(), expected.to_string_lossy());
    }

    #[test]
    fn configure_ssh_signing_is_idempotent() {
        let dir = temp_dir("configure-ssh-signing-idempotent");
        let repo = dir.path();
        crate::init_repo(repo).expect("init_repo");

        configure_ssh_signing(repo).expect("first configure_ssh_signing");
        let first_contents =
            std::fs::read_to_string(repo.join("allowed_signers")).expect("read after first");
        configure_ssh_signing(repo).expect("second configure_ssh_signing");
        let second_contents =
            std::fs::read_to_string(repo.join("allowed_signers")).expect("read after second");

        assert_eq!(
            first_contents, second_contents,
            "re-running configure_ssh_signing must not rewrite an existing file"
        );
    }
}
