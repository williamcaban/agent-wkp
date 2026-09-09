//! A device's persistent ed25519 SSH signing identity (design 6.4, 8.1,
//! M5-2): generated once per hub registration, stored in the OS
//! keystore with a `0600` file fallback -- the exact same
//! keystore-or-file pattern [`crate::device_identity`] already
//! established, applied here to a different key (ed25519, not X25519),
//! a different job (SSH transport authentication to a hub, per design
//! 8.1's `AuthorizedKeysCommand` model), and deliberately a *different*
//! identity from both `device_identity`'s age encryption key and M2's
//! `allowed_signers` commit-signing key -- three separate keys for
//! three separate jobs (design 7.2's encryption identity, 7.3's
//! commit-signing identity, 8.1's SSH transport identity), matching
//! the design's own separation rather than conflating them because
//! they happen to share a key algorithm family.
//!
//! Real, standard OpenSSH key material throughout (`ssh-key`'s own
//! `PrivateKey`/public-key encoding), not a bespoke format: the public
//! half this module produces (`ssh-ed25519 AAAA...`) is exactly what
//! gets uploaded to the hub during registration and, eventually
//! (M5-3), exactly what `sshd`'s `AuthorizedKeysCommand` compares a
//! real, live SSH connection's presented key against -- no translation
//! layer to keep in sync between "the key this module generates" and
//! "the key a real `ssh`/`git` client actually presents on the wire."

use crate::Error;
use rand_core::OsRng;
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::path::Path;

/// A device's ed25519 SSH signing identity -- the private half. See
/// the module doc comment for what this is (and isn't) used for.
pub struct SigningIdentity(PrivateKey);

impl SigningIdentity {
    /// Generates a fresh, random ed25519 keypair.
    pub fn generate() -> Result<Self, Error> {
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).map_err(Error::SshKey)?;
        Ok(SigningIdentity(key))
    }

    /// The real OpenSSH public-key line (`ssh-ed25519 AAAA...`), with
    /// no trailing comment or newline -- what a caller (`wkp hub
    /// register`, M5-2) uploads to the hub's `/device/code` grant.
    pub fn public_key_openssh(&self) -> Result<String, Error> {
        self.0
            .public_key()
            .to_openssh()
            .map_err(Error::SshKey)
            .map(|s| s.trim().to_string())
    }

    fn to_secret_string(&self) -> Result<String, Error> {
        self.0
            .to_openssh(LineEnding::LF)
            .map(|doc| doc.to_string())
            .map_err(Error::SshKey)
    }

    fn from_secret_string(s: &str) -> Result<Self, Error> {
        PrivateKey::from_openssh(s)
            .map(SigningIdentity)
            .map_err(Error::SshKey)
    }
}

/// The keystore service name every wkp SSH signing identity is filed
/// under -- deliberately distinct from `device_identity`'s own service
/// name, so the two key types (and any future third one) never
/// collide in the same keystore namespace even when a caller reuses
/// the same `key_id`.
const KEYSTORE_SERVICE: &str = "wkp-hub-signing-identity";

/// Get this device's persistent SSH signing identity for `key_id`,
/// generating and persisting a fresh one on first use. Same
/// keystore-then-file-fallback behavior as
/// [`crate::device_identity::ensure`] -- see that function's doc
/// comment for the full reasoning, unchanged here.
pub fn ensure(key_id: &str, fallback_path: &Path) -> Result<SigningIdentity, Error> {
    match keystore::ensure(key_id) {
        Ok(identity) => Ok(identity),
        Err(_keystore_err) => file_fallback::ensure(fallback_path),
    }
}

fn get_or_create(entry: &keyring_core::Entry) -> Result<SigningIdentity, Error> {
    match entry.get_secret() {
        Ok(secret) => {
            let secret_str = String::from_utf8(secret)
                .map_err(|_| Error::InvalidIdentity("keystore entry was not UTF-8".to_string()))?;
            SigningIdentity::from_secret_string(&secret_str)
        }
        Err(keyring_core::Error::NoEntry) => {
            let identity = SigningIdentity::generate()?;
            entry
                .set_secret(identity.to_secret_string()?.as_bytes())
                .map_err(Error::Keystore)?;
            Ok(identity)
        }
        Err(e) => Err(Error::Keystore(e)),
    }
}

mod file_fallback {
    use super::SigningIdentity;
    use crate::Error;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub(super) fn ensure(path: &Path) -> Result<SigningIdentity, Error> {
        if let Ok(mut file) = fs::File::open(path) {
            let mut contents = String::new();
            file.read_to_string(&mut contents).map_err(Error::Io)?;
            return SigningIdentity::from_secret_string(&contents);
        }

        let identity = SigningIdentity::generate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(Error::Io)?;
        file.write_all(identity.to_secret_string()?.as_bytes())
            .map_err(Error::Io)?;
        Ok(identity)
    }
}

#[cfg(target_os = "macos")]
mod keystore {
    use super::SigningIdentity;
    use crate::Error;
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

    pub(super) fn ensure(key_id: &str) -> Result<SigningIdentity, Error> {
        ensure_default_store();
        let entry =
            keyring_core::Entry::new(super::KEYSTORE_SERVICE, key_id).map_err(Error::Keystore)?;
        super::get_or_create(&entry)
    }
}

#[cfg(target_os = "linux")]
mod keystore {
    use super::SigningIdentity;
    use crate::Error;
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

    pub(super) fn ensure(key_id: &str) -> Result<SigningIdentity, Error> {
        ensure_default_store();
        let entry =
            keyring_core::Entry::new(super::KEYSTORE_SERVICE, key_id).map_err(Error::Keystore)?;
        super::get_or_create(&entry)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod keystore {
    use super::SigningIdentity;
    use crate::Error;

    pub(super) fn ensure(_key_id: &str) -> Result<SigningIdentity, Error> {
        Err(Error::Keystore(keyring_core::Error::NoDefaultStore))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_a_real_openssh_ed25519_public_key_line() {
        let identity = SigningIdentity::generate().expect("generate");
        let public = identity.public_key_openssh().expect("public_key_openssh");
        assert!(
            public.starts_with("ssh-ed25519 "),
            "expected a real OpenSSH ed25519 public key line, got: {public}"
        );
        assert!(!public.contains('\n'));
    }

    #[test]
    fn secret_string_round_trips_to_the_same_public_key() {
        let identity = SigningIdentity::generate().expect("generate");
        let public_before = identity.public_key_openssh().expect("public before");

        let secret = identity.to_secret_string().expect("to_secret_string");
        let restored = SigningIdentity::from_secret_string(&secret).expect("from_secret_string");
        let public_after = restored.public_key_openssh().expect("public after");

        assert_eq!(public_before, public_after);
    }

    #[test]
    fn file_fallback_generates_once_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let first = file_fallback::ensure(&path).unwrap();
        let second = file_fallback::ensure(&path).unwrap();
        assert_eq!(
            first.public_key_openssh().unwrap(),
            second.public_key_openssh().unwrap()
        );
    }

    #[test]
    fn file_fallback_writes_a_0600_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        file_fallback::ensure(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn ensure_falls_back_to_file_when_no_keystore_is_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let identity = ensure("test-device", &path).unwrap();
        assert!(path.exists());
        let persisted = file_fallback::ensure(&path).unwrap();
        assert_eq!(
            identity.public_key_openssh().unwrap(),
            persisted.public_key_openssh().unwrap()
        );
    }
}
