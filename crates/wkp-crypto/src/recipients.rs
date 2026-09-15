//! The `recipients` file a store carries (design 7.2): every registered
//! device's X25519 public key plus an optional recovery public key,
//! parallel in spirit to `wkp-git`'s `allowed_signers` (M2-1) -- tracked
//! in git, not itself encrypted (it only ever carries public keys), and
//! consumed directly by the encryption filter (M4-4, out of scope here)
//! to know who private content is encrypted to.
//!
//! Deliberately lives in `wkp-crypto`, not `wkp-git`: unlike
//! `allowed_signers` (which wires into git's own `gpg.ssh.allowedSignersFile`
//! config and needs `wkp-git`'s `run_git` plumbing), nothing here calls
//! git at all -- `parse`/`append` are plain file I/O, and recovery-key
//! generation reuses this crate's own [`Identity`]/[`Recipient`]. Keeping
//! it here needs no new dependency edge between the two crates. Whichever
//! caller actually commits this file into a store's history is later,
//! CLI-level wiring (M4-4), same division `allowed_signers::append`
//! itself already has from `wkp_git::allowed_signers::configure_ssh_signing`.

use crate::{Error, Identity, Recipient};
use std::path::Path;

/// Read off a [`RecipientEntry`]'s `label` prefix, not stored as its own
/// column (mirrors `allowed_signers::SignerRole`'s `human:`/`agent:`
/// convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientKind {
    Device,
    Recovery,
    /// A label matching neither `device:` nor `recovery:` -- accepted (any
    /// label is a valid line here), just not one M4-4's filter has a
    /// defined reason to expect.
    Other,
}

impl RecipientKind {
    pub fn from_label(label: &str) -> Self {
        if label.starts_with("device:") {
            RecipientKind::Device
        } else if label.starts_with("recovery:") {
            RecipientKind::Recovery
        } else {
            RecipientKind::Other
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RecipientEntry {
    pub label: String,
    pub kind: RecipientKind,
    pub recipient: Recipient,
}

const FILE_HEADER: &str = "\
# wkp recipients (design 7.2). One age recipient per line:
#   <label> <recipient>
# <label> is `device:<name>` or `recovery:<name>` -- kind is read from
# that prefix, not a separate column (mirrors allowed_signers' human:/
# agent: convention). Not itself encrypted: every line here is a public
# key. Consumed by the encryption filter to decide who private content
# is encrypted to.
";

/// Parses a `recipients`-format file body. Blank lines and `#`-comments
/// are ignored. A line is expected to be exactly two whitespace-separated
/// fields (`label recipient`); a malformed line (wrong field count, or a
/// `recipient` field that doesn't parse as a valid age recipient) is
/// skipped rather than mis-parsed -- same tolerant posture as
/// `allowed_signers::parse` and `wkp-core`'s frontmatter parser.
pub fn parse(contents: &str) -> Vec<RecipientEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split_whitespace();
            let label = fields.next()?;
            let recipient_str = fields.next()?;
            if fields.next().is_some() {
                return None;
            }
            let recipient = Recipient::from_string(recipient_str).ok()?;
            Some(RecipientEntry {
                label: label.to_string(),
                kind: RecipientKind::from_label(label),
                recipient,
            })
        })
        .collect()
}

fn render_line(entry: &RecipientEntry) -> String {
    format!("{} {}", entry.label, entry.recipient)
}

/// Appends `entry` to the `recipients` file at `path`, creating it (with
/// [`FILE_HEADER`]) if it doesn't exist yet. Idempotent: an entry whose
/// label and recipient already appear as an identical line is left alone,
/// not duplicated -- matching `allowed_signers::append`'s exact-line-match
/// convention (re-running a device's registration on every startup must
/// not grow the file forever; a *different* recipient later registered
/// under the same label is a second, distinct line, same as
/// `allowed_signers` allows a principal to have more than one valid key).
pub fn append(path: &Path, entry: &RecipientEntry) -> Result<(), Error> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let new_line = render_line(entry);
    if existing.lines().any(|line| line.trim() == new_line) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
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
    std::fs::write(path, updated).map_err(Error::Io)
}

/// Removes every line whose `label` matches `label` exactly, rewriting
/// the file in place. Returns whether any line matched -- callers that
/// need "this device was actually registered" (M4-5's device revocation)
/// treat `Ok(false)` as an error condition themselves; this function
/// stays a plain, unconditional removal, matching `append`'s own
/// "exact label/line match" granularity rather than guessing intent.
/// A missing file is treated as "nothing to remove" (`Ok(false)`), not
/// an error -- symmetric with `append`'s "creates it if it doesn't exist"
/// posture at the other end of this file's lifecycle.
pub fn remove(path: &Path, label: &str) -> Result<bool, Error> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };

    let mut removed = false;
    let mut kept_lines = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        let matches = !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && trimmed.split_whitespace().next() == Some(label);
        if matches {
            removed = true;
        } else {
            kept_lines.push(line);
        }
    }

    if !removed {
        return Ok(false);
    }

    let mut updated = kept_lines.join("\n");
    if !updated.is_empty() {
        updated.push('\n');
    }
    std::fs::write(path, updated).map_err(Error::Io)?;
    Ok(true)
}

/// Generates a fresh recovery identity, appends its public half to the
/// `recipients` file at `path` under `recovery:<label>`, and returns the
/// *private* identity to the caller. This is the only code path that ever
/// produces that private key: it is not written to disk here, not cached,
/// and not retrievable again through any function in this module --
/// storage of the returned value is the caller's own responsibility
/// (design 7.2's explicit posture, "same as SSH keys").
///
/// Deliberately does not check whether a recovery entry already exists:
/// enforcing "at most one recovery key" is rotation policy, out of scope
/// for this task (M4-5). Calling this twice appends two distinct
/// `recovery:` entries; callers that want single-recovery-key semantics
/// enforce that themselves.
pub fn generate_recovery_key(path: &Path, label: &str) -> Result<Identity, Error> {
    let identity = Identity::generate();
    let entry = RecipientEntry {
        label: format!("recovery:{label}"),
        kind: RecipientKind::Recovery,
        recipient: identity.to_recipient(),
    };
    append(path, &entry)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_recipient() -> Recipient {
        Identity::generate().to_recipient()
    }

    #[test]
    fn parse_reads_a_device_entry() {
        let recipient = a_recipient();
        let contents = format!("device:alice-laptop {recipient}\n");
        let entries = parse(&contents);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "device:alice-laptop");
        assert_eq!(entries[0].kind, RecipientKind::Device);
        assert!(entries[0].recipient == recipient);
    }

    #[test]
    fn parse_derives_recovery_kind_from_label_prefix() {
        let recipient = a_recipient();
        let contents = format!("recovery:default {recipient}\n");
        assert_eq!(parse(&contents)[0].kind, RecipientKind::Recovery);
    }

    #[test]
    fn parse_derives_other_kind_for_an_unrecognized_label_prefix() {
        let recipient = a_recipient();
        let contents = format!("laptop {recipient}\n");
        assert_eq!(parse(&contents)[0].kind, RecipientKind::Other);
    }

    #[test]
    fn parse_ignores_blank_lines_and_comments() {
        let recipient = a_recipient();
        let contents = format!("# a comment\n\n   \ndevice:alice {recipient}\n# trailing\n");
        let entries = parse(&contents);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "device:alice");
    }

    #[test]
    fn parse_skips_a_line_with_too_few_fields() {
        assert!(parse("device:alice\n").is_empty());
    }

    #[test]
    fn parse_skips_a_line_with_extra_fields() {
        let recipient = a_recipient();
        let contents = format!("device:alice {recipient} trailing\n");
        assert!(parse(&contents).is_empty());
    }

    #[test]
    fn parse_skips_a_line_with_an_invalid_recipient_string() {
        assert!(parse("device:alice not-a-valid-age-recipient\n").is_empty());
    }

    #[test]
    fn parse_never_panics_on_arbitrary_bytes() {
        for sample in [
            "",
            "\0\0\0",
            "one two three four five",
            "\t\t\t",
            "🎉 emoji-label age1notreal",
        ] {
            let _ = parse(sample);
        }
    }

    #[test]
    fn append_creates_the_file_with_a_header_and_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        let recipient = a_recipient();
        let entry = RecipientEntry {
            label: "device:alice-laptop".to_string(),
            kind: RecipientKind::Device,
            recipient: recipient.clone(),
        };
        append(&path, &entry).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("wkp recipients"));
        assert!(contents.contains(&format!("device:alice-laptop {recipient}")));

        let parsed = parse(&contents);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, entry.label);
    }

    #[test]
    fn append_is_idempotent_for_an_identical_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        let recipient = a_recipient();
        let entry = RecipientEntry {
            label: "device:alice-laptop".to_string(),
            kind: RecipientKind::Device,
            recipient,
        };
        append(&path, &entry).unwrap();
        append(&path, &entry).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let line = render_line(&entry);
        assert_eq!(
            contents.matches(&line).count(),
            1,
            "re-appending an identical entry must not duplicate the line"
        );
    }

    #[test]
    fn append_adds_a_second_distinct_entry_without_touching_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        let alice = RecipientEntry {
            label: "device:alice-laptop".to_string(),
            kind: RecipientKind::Device,
            recipient: a_recipient(),
        };
        let bob = RecipientEntry {
            label: "device:bob-desktop".to_string(),
            kind: RecipientKind::Device,
            recipient: a_recipient(),
        };
        append(&path, &alice).unwrap();
        append(&path, &bob).unwrap();

        let parsed = parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(parsed.len(), 2);
        assert!(parsed.iter().any(|e| e.label == alice.label));
        assert!(parsed.iter().any(|e| e.label == bob.label));
    }

    #[test]
    fn append_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deep/recipients");
        let entry = RecipientEntry {
            label: "device:alice".to_string(),
            kind: RecipientKind::Device,
            recipient: a_recipient(),
        };
        append(&path, &entry).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn remove_drops_only_the_matching_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        let alice = RecipientEntry {
            label: "device:alice-laptop".to_string(),
            kind: RecipientKind::Device,
            recipient: a_recipient(),
        };
        let bob = RecipientEntry {
            label: "device:bob-desktop".to_string(),
            kind: RecipientKind::Device,
            recipient: a_recipient(),
        };
        append(&path, &alice).unwrap();
        append(&path, &bob).unwrap();

        let removed = remove(&path, "device:alice-laptop").unwrap();
        assert!(removed);

        let parsed = parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "device:bob-desktop");
    }

    #[test]
    fn remove_returns_false_for_an_unknown_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        append(
            &path,
            &RecipientEntry {
                label: "device:alice-laptop".to_string(),
                kind: RecipientKind::Device,
                recipient: a_recipient(),
            },
        )
        .unwrap();

        assert!(!remove(&path, "device:nonexistent").unwrap());
        assert_eq!(parse(&std::fs::read_to_string(&path).unwrap()).len(), 1);
    }

    #[test]
    fn remove_on_a_missing_file_returns_false_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        assert!(!remove(&path, "device:anything").unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn generate_recovery_key_appends_the_public_half_and_returns_the_private_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");

        let recovery_identity = generate_recovery_key(&path, "default").unwrap();

        let parsed = parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "recovery:default");
        assert_eq!(parsed[0].kind, RecipientKind::Recovery);
        assert!(parsed[0].recipient == recovery_identity.to_recipient());
    }

    #[test]
    fn generate_recovery_key_round_trips_with_encrypt_and_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");
        let recovery_identity = generate_recovery_key(&path, "default").unwrap();

        let ciphertext = crate::encrypt(
            b"lost every device, recovery saves the day",
            &[recovery_identity.to_recipient()],
        )
        .unwrap();
        let plaintext = crate::decrypt(&ciphertext, &recovery_identity).unwrap();
        assert_eq!(plaintext, b"lost every device, recovery saves the day");
    }

    #[test]
    fn recipients_file_supports_a_device_and_a_recovery_entry_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipients");

        let device_identity = Identity::generate();
        append(
            &path,
            &RecipientEntry {
                label: "device:alice-laptop".to_string(),
                kind: RecipientKind::Device,
                recipient: device_identity.to_recipient(),
            },
        )
        .unwrap();
        let recovery_identity = generate_recovery_key(&path, "default").unwrap();

        let parsed = parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(parsed.len(), 2);

        let recipients: Vec<Recipient> = parsed.into_iter().map(|e| e.recipient).collect();
        let ciphertext = crate::encrypt(b"multi-recipient recovery scenario", &recipients).unwrap();
        assert_eq!(
            crate::decrypt(&ciphertext, &device_identity).unwrap(),
            b"multi-recipient recovery scenario"
        );
        assert_eq!(
            crate::decrypt(&ciphertext, &recovery_identity).unwrap(),
            b"multi-recipient recovery scenario"
        );
    }
}
