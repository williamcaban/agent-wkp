//! A device's persistent X25519 encryption identity (design 7.2, 7.5):
//! generated once, then stored in the OS keystore (macOS Keychain / Linux
//! secret-service) with a `0600` file as the documented fallback for an
//! environment with neither keystore available.
//!
//! **Resolves a design/reality gap, recorded in
//! `docs/adr/0005-device-encryption-identity-storage.md`**: design 7.2/7.5
//! describe "keys live in macOS Keychain or Linux secret-service with
//! ssh-agent fallback", but that sentence describes SSH *signing* key
//! storage (design 7.3) applied to an age X25519 identity by analogy --
//! `ssh-agent` holds and uses signing keys, it cannot hold or produce
//! output from an age decryption identity. There is no ssh-agent-shaped
//! fallback here; per the ADR, the fallback is the keystore-or-a-`0600`-file
//! rule CLAUDE.md already states for all secrets.
//!
//! This module has no git or store involvement -- callers decide what
//! `key_id` and `fallback_path` mean for their store (the recipients file
//! wiring is M4-3, the git filter that actually calls this is M4-4).

use crate::Identity;
use std::path::Path;

/// The keystore service name under which every wkp device identity is
/// filed, regardless of `key_id`. A single, fixed constant (not derived
/// from anything caller-supplied) so every wkp installation on a machine
/// looks in the same keystore namespace.
const KEYSTORE_SERVICE: &str = "wkp-device-identity";

/// Get this device's persistent encryption identity for `key_id`,
/// generating and persisting a fresh one on first use. Tries the platform
/// keystore first (macOS Keychain via `apple-native-keyring-store`, Linux
/// secret-service via `zbus-secret-service-keyring-store`); on **any**
/// keystore error -- most commonly "no keystore reachable at all" (no
/// D-Bus session, no Keychain), verified in this crate's own tests, which
/// run in an environment with neither -- falls back to a `0600` file at
/// `fallback_path`.
///
/// `key_id` scopes the keystore entry (e.g. a caller-chosen per-store
/// device id); it is not a secret and is stored/looked-up as plaintext
/// keystore metadata (the keyring account name).
///
/// **Accepted limitation, not silently swallowed**: if the keystore is
/// reachable but a lookup fails for some other reason (corrupted entry,
/// permission denied on an *existing* entry), this still falls through to
/// the file path and may create a second, different identity there rather
/// than surfacing the keystore error. Precise error-cause discrimination
/// is deferred; every keystore error is currently treated the same way
/// CLAUDE.md treats "OS keystore or a 0600 file" -- as two equally valid
/// places to find the secret, tried in preference order.
pub fn ensure(key_id: &str, fallback_path: &Path) -> Result<Identity, crate::Error> {
    match keystore::ensure(key_id) {
        Ok(identity) => Ok(identity),
        Err(_keystore_err) => file_fallback::ensure(fallback_path),
    }
}

/// Shared by every platform's `keystore` submodule: read an existing
/// entry, or generate and store a new one -- **only** when the entry is
/// confirmed absent (`keyring_core::Error::NoEntry`). Any other read
/// error (permission denied, a transient platform failure, ...) is
/// propagated rather than treated as "absent", so a hiccup reading an
/// existing entry can never fall through into silently generating and
/// overwriting it -- that would destroy access to whatever was already
/// encrypted to the real key (design 7.2's own "loss of device keys is
/// loss of private content" failure mode, just self-inflicted).
fn get_or_create(entry: &keyring_core::Entry) -> Result<Identity, crate::Error> {
    match entry.get_secret() {
        Ok(secret) => {
            let secret_str = String::from_utf8(secret).map_err(|_| {
                crate::Error::InvalidIdentity("keystore entry was not UTF-8".to_string())
            })?;
            Identity::from_secret_string(&secret_str)
        }
        Err(keyring_core::Error::NoEntry) => {
            let identity = Identity::generate();
            entry
                .set_secret(identity.to_secret_string().as_bytes())
                .map_err(crate::Error::Keystore)?;
            Ok(identity)
        }
        Err(e) => Err(crate::Error::Keystore(e)),
    }
}

mod file_fallback {
    use crate::{Error, Identity};
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    /// Read an existing `0600` identity file at `path`, or generate one and
    /// write it (creating parent directories as needed, per the same
    /// pattern `wkp remember`'s `inbox/` bootstrap already established).
    pub(super) fn ensure(path: &Path) -> Result<Identity, Error> {
        if let Ok(mut file) = fs::File::open(path) {
            let mut contents = String::new();
            file.read_to_string(&mut contents).map_err(Error::Io)?;
            return Identity::from_secret_string(contents.trim());
        }

        let identity = Identity::generate();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(Error::Io)?;
        file.write_all(identity.to_secret_string().as_bytes())
            .map_err(Error::Io)?;
        Ok(identity)
    }
}

#[cfg(target_os = "macos")]
mod keystore {
    use crate::{Error, Identity};
    use std::sync::OnceLock;

    static STORE_INIT: OnceLock<()> = OnceLock::new();

    fn ensure_default_store() {
        STORE_INIT.get_or_init(|| {
            if keyring_core::get_default_store().is_none() {
                if let Ok(store) = apple_native_keyring_store::keychain::Store::new() {
                    keyring_core::set_default_store(store);
                }
            }
        });
    }

    pub(super) fn ensure(key_id: &str) -> Result<Identity, Error> {
        ensure_default_store();
        let entry =
            keyring_core::Entry::new(super::KEYSTORE_SERVICE, key_id).map_err(Error::Keystore)?;
        super::get_or_create(&entry)
    }
}

#[cfg(target_os = "linux")]
mod keystore {
    use crate::{Error, Identity};
    use std::sync::OnceLock;

    static STORE_INIT: OnceLock<()> = OnceLock::new();

    fn ensure_default_store() {
        STORE_INIT.get_or_init(|| {
            if keyring_core::get_default_store().is_none() {
                if let Ok(store) = zbus_secret_service_keyring_store::Store::new() {
                    keyring_core::set_default_store(store);
                }
            }
        });
    }

    pub(super) fn ensure(key_id: &str) -> Result<Identity, Error> {
        ensure_default_store();
        let entry =
            keyring_core::Entry::new(super::KEYSTORE_SERVICE, key_id).map_err(Error::Keystore)?;
        super::get_or_create(&entry)
    }
}

/// No native keystore integration on any other platform (design 7.2/7.5
/// only name macOS and Linux; M3-1's device-id doc comment already flags
/// Windows as out of scope for this whole milestone). Fails closed into
/// the caller's `ensure`, which then always uses the file fallback --
/// matching `wkpd`'s `peer_is_self` precedent (ADR-0004) of an explicit,
/// trivial stub rather than a platform-conditional call site.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod keystore {
    use crate::{Error, Identity};

    pub(super) fn ensure(_key_id: &str) -> Result<Identity, Error> {
        Err(Error::Keystore(keyring_core::Error::NoDefaultStore))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn file_fallback_generates_once_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let first = file_fallback::ensure(&path).unwrap();
        let second = file_fallback::ensure(&path).unwrap();
        assert!(first.to_recipient() == second.to_recipient());
    }

    #[test]
    fn file_fallback_writes_a_0600_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        file_fallback::ensure(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn ensure_falls_back_to_file_when_no_keystore_is_reachable() {
        // This test's own CI/sandbox environment has no D-Bus session and
        // no Keychain -- exercising the real "keystore unavailable" path,
        // not a mock of it. See the module doc comment.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let identity = ensure("test-device", &path).unwrap();
        assert!(path.exists());
        let persisted = file_fallback::ensure(&path).unwrap();
        assert!(identity.to_recipient() == persisted.to_recipient());
    }
}
