//! `wkp forget`: two distinct operations under one verb (design 7.6,
//! M4-5, `docs/adr/0007-wkp-forget-scope.md`).
//!
//! `wkp forget <path>` removes one item from the current tree with a
//! human-signed commit. `wkp forget --device <id>` revokes one device's
//! recipient entry and re-encrypts every currently-tracked
//! `visibility: private` file to the resulting recipient set, in one
//! commit alongside the updated `recipients` file. Both always require
//! `role: human` -- unlike `wkp promote`, there is no auto-forget escape
//! hatch for agent principals; see the ADR for why.

use std::path::{Path, PathBuf};

pub(crate) enum ForgetTarget {
    Item(String),
    Device(String),
}

pub(crate) struct ForgetOptions {
    pub(crate) path: PathBuf,
    pub(crate) target: ForgetTarget,
    pub(crate) principal: String,
    pub(crate) signing_key_file: PathBuf,
}

/// Parses `wkp forget <path> --principal <p> --signing-key-file <f>` or
/// `wkp forget --device <id> --principal <p> --signing-key-file <f>`
/// (either form also accepts `--path <dir>`). The positional path and
/// `--device` are mutually exclusive -- exactly one must be given.
pub(crate) fn parse_forget_args(
    mut args: impl Iterator<Item = String>,
) -> Result<ForgetOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut item_path: Option<String> = None;
    let mut device: Option<String> = None;
    let mut principal: Option<String> = None;
    let mut signing_key_file: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => device = Some(args.next().ok_or("--device requires a value")?),
            "--principal" => principal = Some(args.next().ok_or("--principal requires a value")?),
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(
                    args.next().ok_or("--signing-key-file requires a value")?,
                ));
            }
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other if item_path.is_none() && !other.starts_with('-') => {
                item_path = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let principal =
        principal.ok_or_else(|| "forget requires --principal <principal>".to_string())?;
    let signing_key_file =
        signing_key_file.ok_or_else(|| "forget requires --signing-key-file <path>".to_string())?;

    let target = match (item_path, device) {
        (Some(_), Some(_)) => {
            return Err(
                "forget takes either a path (item removal) or --device <id> (device \
                 revocation), not both"
                    .to_string(),
            )
        }
        (Some(item_path), None) => ForgetTarget::Item(item_path),
        (None, Some(device)) => ForgetTarget::Device(device),
        (None, None) => {
            return Err(
                "forget requires either a path (`wkp forget <path>`) or --device <id> \
                 (`wkp forget --device <id>`)"
                    .to_string(),
            )
        }
    };

    Ok(ForgetOptions {
        path,
        target,
        principal,
        signing_key_file,
    })
}

fn require_human(principal: &str) -> Result<(), String> {
    let role = wkp_git::allowed_signers::SignerRole::from_principal(principal);
    if !matches!(role, wkp_git::allowed_signers::SignerRole::Human) {
        return Err(format!(
            "principal {principal} is not role:human -- wkp forget always requires a \
             human-signed commit, with no auto-forget escape hatch (see \
             docs/adr/0007-wkp-forget-scope.md)"
        ));
    }
    Ok(())
}

fn provenance_for(principal: &str) -> wkp_git::provenance::Provenance {
    wkp_git::provenance::Provenance {
        actor: Some(principal.to_string()),
        session: None,
        source: None,
        confidence: None,
    }
}

/// Rejects an item path that isn't a plain, store-relative path -- an
/// absolute path or one with a `..` component could name something
/// outside the store entirely, which `wkp forget` (unlike `wkp promote`,
/// which only ever moves an already-validated `inbox/` path) has no
/// other structural reason to refuse.
fn reject_path_escape(item_path: &str) -> Result<(), String> {
    let path = Path::new(item_path);
    if path.is_absolute()
        || path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "{item_path} is not a store-relative path -- wkp forget only removes paths inside \
             the store"
        ));
    }
    Ok(())
}

pub(crate) struct ForgetItemSummary {
    pub(crate) removed: String,
    pub(crate) commit: wkp_git::signed_commit::CommitId,
}

impl std::fmt::Display for ForgetItemSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wkp: forgot {} ({})", self.removed, self.commit.0)
    }
}

/// `wkp forget <path>`: removes one tracked item with a human-signed
/// commit (design 7.6, M4-5, ADR-0007). Refuses a principal that isn't
/// `role: human`, a path outside the store, or a path that isn't
/// currently tracked (an untracked path's `update-index --remove` is a
/// silent no-op in git itself, which would otherwise let a caller
/// believe it forgot something it never touched).
pub(crate) fn run_forget_item(
    opts: &ForgetOptions,
    item_path: &str,
) -> Result<ForgetItemSummary, String> {
    require_human(&opts.principal)?;
    reject_path_escape(item_path)?;

    let tracked = wkp_git::list_tracked_files(&opts.path)?;
    if !tracked.iter().any(|p| p == Path::new(item_path)) {
        return Err(format!("{item_path} is not tracked in this store"));
    }

    let full_path = opts.path.join(item_path);
    std::fs::remove_file(&full_path).map_err(|e| format!("removing {item_path}: {e}"))?;

    let subject = format!("forget: {item_path}");
    let commit = wkp_git::signed_commit::signed_removal_commit(
        &opts.path,
        &[PathBuf::from(item_path)],
        &subject,
        &opts.principal,
        &opts.signing_key_file,
        &provenance_for(&opts.principal),
    )?;

    Ok(ForgetItemSummary {
        removed: item_path.to_string(),
        commit,
    })
}

pub(crate) struct ForgetDeviceSummary {
    pub(crate) device_label: String,
    pub(crate) reencrypted: Vec<String>,
    pub(crate) commit: wkp_git::signed_commit::CommitId,
}

impl std::fmt::Display for ForgetDeviceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: revoked {}, re-encrypted {} private item(s) ({})",
            self.device_label,
            self.reencrypted.len(),
            self.commit.0
        )
    }
}

/// `wkp forget --device <id>`: revokes `device:<id>`'s recipient entry
/// and re-encrypts every currently-tracked `visibility: private` file to
/// the resulting recipient set, in one commit alongside the updated
/// `recipients` file (design 7.6, M4-5, ADR-0007).
///
/// Re-encryption reuses the existing clean filter (M4-4) rather than
/// calling `wkp-crypto`'s `encrypt`/`decrypt` directly: the `recipients`
/// file is rewritten on disk *before* [`wkp_git::signed_commit::signed_commit`]
/// stages each private path, and that function's own `git hash-object`
/// staging step applies `*.md filter=wkp-crypt`'s clean filter exactly
/// the way a real `git add` would -- confirmed by hand (repeated
/// `git hash-object -w` on the same plaintext file produces a different,
/// real age-ciphertext blob each time) rather than assumed. No new
/// crypto call site to review here.
///
/// Refuses if revoking would leave the store with zero registered
/// recipients (device or recovery combined) -- a real foot-gun (a
/// single-device store revoking its only device), not a hypothetical:
/// every future `visibility: private` write would be unencryptable to
/// anyone.
pub(crate) fn run_forget_device(
    opts: &ForgetOptions,
    device_id: &str,
) -> Result<ForgetDeviceSummary, String> {
    require_human(&opts.principal)?;

    let recipients_path = opts.path.join(crate::filter::RECIPIENTS_FILENAME);
    let label = format!("device:{device_id}");

    let existing_contents = std::fs::read_to_string(&recipients_path)
        .map_err(|e| format!("reading recipients file: {e}"))?;
    let entries = wkp_crypto::recipients::parse(&existing_contents);
    if !entries.iter().any(|e| e.label == label) {
        return Err(format!(
            "{label} is not a registered recipient in this store"
        ));
    }
    let remaining = entries.iter().filter(|e| e.label != label).count();
    if remaining == 0 {
        return Err(format!(
            "revoking {label} would leave this store with zero registered recipients -- \
             register another device or recovery key before revoking this one"
        ));
    }

    let tracked = wkp_git::list_tracked_files(&opts.path)?;
    let mut private_paths = Vec::new();
    for relative in &tracked {
        if relative.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let full = opts.path.join(relative);
        let Ok(contents) = std::fs::read_to_string(&full) else {
            continue; // non-UTF8 content: filter.rs's own clean pass treats this the same as "not private", so skip it here too
        };
        let parsed = wkp_core::frontmatter::parse(&contents);
        if parsed.frontmatter.visibility == Some(wkp_core::frontmatter::Visibility::Private) {
            private_paths.push(relative.clone());
        }
    }

    let removed =
        wkp_crypto::recipients::remove(&recipients_path, &label).map_err(|e| e.to_string())?;
    if !removed {
        return Err(format!(
            "{label} disappeared from the recipients file mid-operation -- concurrent modification?"
        ));
    }

    let mut commit_paths = vec![PathBuf::from(crate::filter::RECIPIENTS_FILENAME)];
    commit_paths.extend(private_paths.iter().cloned());

    let subject = format!(
        "forget --device {device_id}: revoke and re-encrypt {} item(s)",
        private_paths.len()
    );
    let commit = wkp_git::signed_commit::signed_commit(
        &opts.path,
        &commit_paths,
        &subject,
        &opts.principal,
        &opts.signing_key_file,
        &provenance_for(&opts.principal),
    )?;

    Ok(ForgetDeviceSummary {
        device_label: label,
        reencrypted: private_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn parse_forget_args_requires_principal_and_signing_key_file() {
        assert!(parse_forget_args(args(&["user/a.md"])).is_err());
        assert!(parse_forget_args(args(&["user/a.md", "--principal", "human:alice"])).is_err());
    }

    #[test]
    fn parse_forget_args_reads_the_item_path_form() {
        let opts = parse_forget_args(args(&[
            "user/a.md",
            "--principal",
            "human:alice",
            "--signing-key-file",
            "/tmp/key",
        ]))
        .expect("parse_forget_args");
        assert!(matches!(opts.target, ForgetTarget::Item(p) if p == "user/a.md"));
        assert_eq!(opts.principal, "human:alice");
    }

    #[test]
    fn parse_forget_args_reads_the_device_form() {
        let opts = parse_forget_args(args(&[
            "--device",
            "laptop-1",
            "--principal",
            "human:alice",
            "--signing-key-file",
            "/tmp/key",
        ]))
        .expect("parse_forget_args");
        assert!(matches!(opts.target, ForgetTarget::Device(d) if d == "laptop-1"));
    }

    #[test]
    fn parse_forget_args_rejects_both_a_path_and_device() {
        let result = parse_forget_args(args(&[
            "user/a.md",
            "--device",
            "laptop-1",
            "--principal",
            "human:alice",
            "--signing-key-file",
            "/tmp/key",
        ]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_forget_args_rejects_neither_a_path_nor_device() {
        let result = parse_forget_args(args(&[
            "--principal",
            "human:alice",
            "--signing-key-file",
            "/tmp/key",
        ]));
        assert!(result.is_err());
    }

    fn write_and_commit(dir: &Path, key: &TestKey, principal: &str, relative: &str, content: &str) {
        let full = dir.join(relative);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&full, content).expect("write file");
        wkp_git::signed_commit::signed_commit(
            dir,
            &[PathBuf::from(relative)],
            &format!("add {relative}"),
            principal,
            &key.private_path,
            &wkp_git::provenance::Provenance::default(),
        )
        .expect("signed_commit seeding fixture");
    }

    #[test]
    fn run_forget_item_removes_a_tracked_file_with_a_human_signed_commit() {
        let temp = temp_dir("forget-item");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let human_key = generate_test_key_and_register(dir, "human:alice");
        write_and_commit(
            dir,
            &human_key,
            "human:alice",
            "user/note.md",
            "some content\n",
        );

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Item("user/note.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        let summary = run_forget_item(&opts, "user/note.md").expect("run_forget_item");
        assert_eq!(summary.removed, "user/note.md");
        wkp_git::signed_commit::verify_commit(dir, &summary.commit)
            .expect("forget commit must be validly signed");

        assert!(!dir.join("user/note.md").exists());
        let tracked = wkp_git::list_tracked_files(dir).expect("list_tracked_files");
        assert!(!tracked.iter().any(|p| p == Path::new("user/note.md")));
    }

    #[test]
    fn run_forget_item_refuses_an_agent_only_principal() {
        let temp = temp_dir("forget-item-agent-refused");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");
        write_and_commit(
            dir,
            &agent_key,
            "agent:claude-code@host",
            "user/note.md",
            "content\n",
        );

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Item("user/note.md".to_string()),
            principal: "agent:claude-code@host".to_string(),
            signing_key_file: agent_key.private_path,
        };
        assert!(run_forget_item(&opts, "user/note.md").is_err());
        assert!(dir.join("user/note.md").is_file(), "file must be untouched");
    }

    #[test]
    fn run_forget_item_refuses_an_untracked_path() {
        let temp = temp_dir("forget-item-untracked");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let human_key = generate_test_key_and_register(dir, "human:alice");
        std::fs::write(dir.join("stray.md"), "never committed\n").expect("write stray.md");

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Item("stray.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        assert!(run_forget_item(&opts, "stray.md").is_err());
    }

    #[test]
    fn run_forget_item_refuses_a_path_escaping_the_store() {
        let temp = temp_dir("forget-item-path-escape");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let human_key = generate_test_key_and_register(dir, "human:alice");

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Item("../outside.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        assert!(run_forget_item(&opts, "../outside.md").is_err());
    }

    fn register_device_and_get_identity(dir: &Path) -> (String, wkp_crypto::Identity) {
        let device_id = wkp_git::sync::device_id(dir).expect("device_id");
        let fallback = dir.join(".wkp/device-identity");
        let identity =
            wkp_crypto::device_identity::ensure(&device_id, &fallback).expect("device identity");
        wkp_crypto::recipients::append(
            &dir.join("recipients"),
            &wkp_crypto::recipients::RecipientEntry {
                label: format!("device:{device_id}"),
                kind: wkp_crypto::recipients::RecipientKind::Device,
                recipient: identity.to_recipient(),
            },
        )
        .expect("append recipient");
        (device_id, identity)
    }

    /// M4-5's core acceptance criterion: after revoking a device, its
    /// identity can no longer decrypt the *new* blob of a private item,
    /// while a still-registered device's identity still can.
    #[test]
    fn run_forget_device_revokes_and_reencrypts_so_old_identity_cannot_decrypt_the_new_blob() {
        let temp = temp_dir("forget-device-revoke");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        // `wkp init` already registered this process's own device; treat
        // it as "device A" and register a second, independent identity
        // as "device B" to revoke.
        let device_a_id = wkp_git::sync::device_id(dir).expect("device_id");
        let device_a_identity =
            wkp_crypto::device_identity::ensure(&device_a_id, &dir.join(".wkp/device-identity"))
                .expect("device A identity");
        let device_b_identity = wkp_crypto::Identity::generate();
        let device_b_id = "device-b";
        wkp_crypto::recipients::append(
            &dir.join("recipients"),
            &wkp_crypto::recipients::RecipientEntry {
                label: format!("device:{device_b_id}"),
                kind: wkp_crypto::recipients::RecipientKind::Device,
                recipient: device_b_identity.to_recipient(),
            },
        )
        .expect("append device B recipient");

        let human_key = generate_test_key_and_register(dir, "human:alice");
        write_and_commit(
            dir,
            &human_key,
            "human:alice",
            "secret.md",
            "---\nvisibility: private\n---\n\nsecret body\n",
        );
        let old_ciphertext =
            wkp_git::read_blob(dir, "HEAD:secret.md").expect("read_blob old content");
        assert!(
            wkp_crypto::decrypt(&old_ciphertext, &device_b_identity).is_ok(),
            "sanity check: device B must be able to decrypt the pre-revocation blob"
        );

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Device(device_b_id.to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        let summary = run_forget_device(&opts, device_b_id).expect("run_forget_device");
        assert_eq!(summary.device_label, format!("device:{device_b_id}"));
        assert_eq!(summary.reencrypted, vec!["secret.md".to_string()]);
        wkp_git::signed_commit::verify_commit(dir, &summary.commit)
            .expect("revocation commit must be validly signed");

        let new_ciphertext =
            wkp_git::read_blob(dir, "HEAD:secret.md").expect("read_blob new content");
        assert_ne!(
            old_ciphertext, new_ciphertext,
            "the blob must actually change"
        );

        assert!(
            wkp_crypto::decrypt(&new_ciphertext, &device_b_identity).is_err(),
            "revoked device B must not be able to decrypt the new blob"
        );
        assert!(
            wkp_crypto::decrypt(&new_ciphertext, &device_a_identity).is_ok(),
            "still-registered device A must still be able to decrypt the new blob"
        );

        let recipients_contents =
            std::fs::read_to_string(dir.join("recipients")).expect("read recipients");
        assert!(!recipients_contents.contains(&format!("device:{device_b_id} ")));
    }

    #[test]
    fn run_forget_device_refuses_an_unknown_device() {
        let temp = temp_dir("forget-device-unknown");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let human_key = generate_test_key_and_register(dir, "human:alice");

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Device("nonexistent".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        assert!(run_forget_device(&opts, "nonexistent").is_err());
    }

    #[test]
    fn run_forget_device_refuses_to_leave_zero_recipients() {
        let temp = temp_dir("forget-device-last-one");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let device_id = wkp_git::sync::device_id(dir).expect("device_id");
        let human_key = generate_test_key_and_register(dir, "human:alice");

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Device(device_id.clone()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        let result = run_forget_device(&opts, &device_id);
        assert!(
            result.is_err(),
            "revoking the only registered device must be refused"
        );
        let recipients_contents =
            std::fs::read_to_string(dir.join("recipients")).expect("read recipients");
        assert!(
            recipients_contents.contains(&format!("device:{device_id} ")),
            "the sole recipient must be untouched after a refused revocation"
        );
    }

    #[test]
    fn run_forget_device_refuses_an_agent_only_principal() {
        let temp = temp_dir("forget-device-agent-refused");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let (device_id, _identity) = register_device_and_get_identity(dir);
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");

        // A second recipient so "zero recipients left" doesn't mask the
        // role check this test actually wants to exercise.
        wkp_crypto::recipients::append(
            &dir.join("recipients"),
            &wkp_crypto::recipients::RecipientEntry {
                label: "device:other".to_string(),
                kind: wkp_crypto::recipients::RecipientKind::Device,
                recipient: wkp_crypto::Identity::generate().to_recipient(),
            },
        )
        .expect("append second recipient");

        let opts = ForgetOptions {
            path: dir.to_path_buf(),
            target: ForgetTarget::Device(device_id.clone()),
            principal: "agent:claude-code@host".to_string(),
            signing_key_file: agent_key.private_path,
        };
        assert!(run_forget_device(&opts, &device_id).is_err());
    }
}
