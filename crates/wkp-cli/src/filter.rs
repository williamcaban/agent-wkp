//! `wkp filter clean`/`wkp filter smudge`: git's clean/smudge filter
//! protocol (design 7.2, 5.1, M4-4), registered by
//! `init::configure_encryption_filter` via `.gitattributes: *.md
//! filter=wkp-crypt` plus local git config -- the same self-invocation
//! pattern `wkp merge-driver` (M3-2) already established for a different
//! git protocol.
//!
//! **Asymmetric failure policy, recorded in
//! `docs/adr/0006-encryption-filter-failure-policy.md`**: `clean`
//! propagates every real failure so `filter.wkp-crypt.required=true`
//! (set by `configure_encryption_filter`) makes git refuse to stage or
//! commit rather than silently fall back to raw plaintext -- verified by
//! hand that this is git's actual default behavior for a failing clean
//! filter otherwise, a severe finding for this feature specifically.
//! `smudge` never returns an error: every failure path here degrades to
//! "leave the content as ciphertext, warn on stderr", so git's much
//! harsher `required`-mode smudge failure (which aborts the *entire*
//! checkout, not just one file, also verified by hand) is never
//! triggered by this side.

use std::path::Path;

pub(crate) const RECIPIENTS_FILENAME: &str = "recipients";
const DEVICE_IDENTITY_FALLBACK: &str = ".wkp/device-identity";

/// Clean direction (working tree -> repo object): encrypts `content` to
/// every recipient in `store_root`'s `recipients` file (M4-3) when its
/// frontmatter says `visibility: private`; every other file (`shared`,
/// unspecified, or content that isn't valid UTF-8 text at all -- clean
/// filters only ever apply to `*.md`, but content this parser can't even
/// read as text is treated the same as "not marked private") passes
/// through byte-for-byte unchanged.
///
/// Every `Err` here must reach the caller as a real process failure: see
/// the module doc comment and ADR-0006 for why silently falling back to
/// `content` unmodified on a real error would be a serious regression,
/// not a convenience.
pub(crate) fn run_filter_clean(store_root: &Path, content: &[u8]) -> Result<Vec<u8>, String> {
    let Ok(text) = std::str::from_utf8(content) else {
        return Ok(content.to_vec());
    };
    let parsed = wkp_core::frontmatter::parse(text);
    if parsed.frontmatter.visibility != Some(wkp_core::frontmatter::Visibility::Private) {
        return Ok(content.to_vec());
    }

    let recipients_path = store_root.join(RECIPIENTS_FILENAME);
    let recipients_contents = std::fs::read_to_string(&recipients_path).map_err(|e| {
        format!(
            "reading recipients file {}: {e} (a visibility: private file cannot be committed \
             until this store has at least one registered recipient -- run `wkp init` to \
             register this device)",
            recipients_path.display()
        )
    })?;
    let recipients: Vec<wkp_crypto::Recipient> =
        wkp_crypto::recipients::parse(&recipients_contents)
            .into_iter()
            .map(|entry| entry.recipient)
            .collect();

    wkp_crypto::encrypt(content, &recipients).map_err(|e| e.to_string())
}

/// Smudge direction (repo object -> working tree): decrypts `content`
/// when it's recognizably age ciphertext (starts with age's own
/// `age-encryption.org/v1` header -- design 7.2's own suggested
/// detection) and a usable local device identity can both be obtained
/// and actually decrypt it; every other case, including every failure,
/// returns `content` unchanged. Never fails the process -- see the
/// module doc comment and ADR-0006.
pub(crate) fn run_filter_smudge(store_root: &Path, content: &[u8]) -> Vec<u8> {
    if !content.starts_with(wkp_crypto::AGE_HEADER) {
        return content.to_vec();
    }

    let device_id = match wkp_git::sync::device_id(store_root) {
        Ok(id) => id,
        Err(e) => {
            eprintln!(
                "wkp: filter smudge: could not resolve this device's id ({e}); \
                 leaving content encrypted"
            );
            return content.to_vec();
        }
    };
    let fallback_path = store_root.join(DEVICE_IDENTITY_FALLBACK);
    let identity = match wkp_crypto::device_identity::ensure(&device_id, &fallback_path) {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!(
                "wkp: filter smudge: could not obtain this device's encryption identity ({e}); \
                 leaving content encrypted"
            );
            return content.to_vec();
        }
    };
    match wkp_crypto::decrypt(content, &identity) {
        Ok(plaintext) => plaintext,
        Err(e) => {
            eprintln!(
                "wkp: filter smudge: could not decrypt ({e}) -- this device may not be a \
                 registered recipient for this file; leaving content encrypted"
            );
            content.to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    fn register_device_and_get_identity(store_root: &Path) -> wkp_crypto::Identity {
        let device_id = wkp_git::sync::device_id(store_root).expect("device_id");
        let fallback = store_root.join(DEVICE_IDENTITY_FALLBACK);
        let identity =
            wkp_crypto::device_identity::ensure(&device_id, &fallback).expect("device identity");
        wkp_crypto::recipients::append(
            &store_root.join(RECIPIENTS_FILENAME),
            &wkp_crypto::recipients::RecipientEntry {
                label: format!("device:{device_id}"),
                kind: wkp_crypto::recipients::RecipientKind::Device,
                recipient: identity.to_recipient(),
            },
        )
        .expect("append recipient");
        identity
    }

    #[test]
    fn clean_passes_through_a_shared_file_unchanged() {
        let temp = temp_dir("filter-clean-shared");
        let content = b"---\nvisibility: shared\n---\n\nbody\n";
        let output = run_filter_clean(temp.path(), content).expect("run_filter_clean");
        assert_eq!(output, content);
    }

    #[test]
    fn clean_passes_through_a_file_with_no_visibility_field_unchanged() {
        let temp = temp_dir("filter-clean-no-visibility");
        let content = b"---\ntitle: no visibility field\n---\n\nbody\n";
        let output = run_filter_clean(temp.path(), content).expect("run_filter_clean");
        assert_eq!(output, content);
    }

    #[test]
    fn clean_passes_through_non_utf8_content_unchanged() {
        let temp = temp_dir("filter-clean-non-utf8");
        let content: &[u8] = &[0xff, 0xfe, 0x00, 0x01];
        let output = run_filter_clean(temp.path(), content).expect("run_filter_clean");
        assert_eq!(output, content);
    }

    #[test]
    fn clean_encrypts_a_private_file_to_the_registered_recipient() {
        let temp = temp_dir("filter-clean-private");
        let identity = register_device_and_get_identity(temp.path());
        let content = b"---\nvisibility: private\n---\n\nsecret body\n";

        let ciphertext = run_filter_clean(temp.path(), content).expect("run_filter_clean");
        assert!(ciphertext.starts_with(wkp_crypto::AGE_HEADER));
        assert_ne!(ciphertext, content);

        let plaintext = wkp_crypto::decrypt(&ciphertext, &identity).expect("decrypt");
        assert_eq!(plaintext, content);
    }

    #[test]
    fn clean_fails_loudly_when_no_recipients_file_exists() {
        let temp = temp_dir("filter-clean-no-recipients-file");
        let content = b"---\nvisibility: private\n---\n\nsecret body\n";
        let result = run_filter_clean(temp.path(), content);
        assert!(
            result.is_err(),
            "clean must fail, not silently pass through plaintext, when there is no \
             recipients file yet (see ADR-0006)"
        );
    }

    #[test]
    fn clean_fails_loudly_when_the_recipients_file_is_empty() {
        let temp = temp_dir("filter-clean-empty-recipients-file");
        std::fs::write(
            temp.path().join(RECIPIENTS_FILENAME),
            "# no recipients yet\n",
        )
        .expect("seed empty recipients file");
        let content = b"---\nvisibility: private\n---\n\nsecret body\n";
        assert!(run_filter_clean(temp.path(), content).is_err());
    }

    #[test]
    fn smudge_passes_through_plaintext_unchanged() {
        let temp = temp_dir("filter-smudge-plaintext");
        let content = b"---\nvisibility: shared\n---\n\nbody\n";
        assert_eq!(run_filter_smudge(temp.path(), content), content);
    }

    #[test]
    fn smudge_decrypts_ciphertext_when_this_device_is_a_recipient() {
        let temp = temp_dir("filter-smudge-success");
        register_device_and_get_identity(temp.path());
        let content = b"---\nvisibility: private\n---\n\nsecret body\n";
        let ciphertext = run_filter_clean(temp.path(), content).expect("run_filter_clean");

        let plaintext = run_filter_smudge(temp.path(), &ciphertext);
        assert_eq!(plaintext, content);
    }

    #[test]
    fn smudge_leaves_ciphertext_unchanged_and_does_not_panic_when_no_identity_is_registered() {
        let temp = temp_dir("filter-smudge-no-identity");
        // A recipient that is NOT this test's own device -- decryptable
        // by nobody this test can produce an identity for, simulating a
        // fresh device with no local identity registered as a recipient
        // yet.
        let stranger_recipient = wkp_crypto::Identity::generate().to_recipient();
        wkp_crypto::recipients::append(
            &temp.path().join(RECIPIENTS_FILENAME),
            &wkp_crypto::recipients::RecipientEntry {
                label: "device:someone-else".to_string(),
                kind: wkp_crypto::recipients::RecipientKind::Device,
                recipient: stranger_recipient,
            },
        )
        .expect("append recipient");
        let content = b"---\nvisibility: private\n---\n\nsecret body\n";
        let ciphertext = run_filter_clean(temp.path(), content).expect("run_filter_clean");

        let output = run_filter_smudge(temp.path(), &ciphertext);
        assert_eq!(
            output, ciphertext,
            "smudge must leave undecryptable content as-is, not panic or corrupt it"
        );
    }
}
