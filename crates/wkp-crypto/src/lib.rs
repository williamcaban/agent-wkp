#![forbid(unsafe_code)]

//! age filter, key handling, signing, and allowed_signers.
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//!
//! This module (M4-1) is the encryption primitive layer: X25519 identity
//! generation, recipient derivation, and multi-recipient encrypt/decrypt of
//! a byte buffer, using the `age` crate in-process (design 7.2). It has no
//! git or keystore involvement -- that lands in later M4 tasks (keystore
//! storage in M4-2, a recipients-file format in M4-3, the git clean/smudge
//! filter in M4-4).

use age::secrecy::ExposeSecret;
use std::fmt;
use std::io::{Read, Write};

pub mod device_identity;

/// An X25519 decryption identity (a device's private key).
pub struct Identity(age::x25519::Identity);

/// An X25519 encryption recipient (a device's public key).
#[derive(Clone, PartialEq, Eq)]
pub struct Recipient(age::x25519::Recipient);

/// Error type for this crate's encrypt/decrypt/parse operations.
#[derive(Debug)]
pub enum Error {
    /// The identity string was not a valid age X25519 identity.
    InvalidIdentity(String),
    /// The recipient string was not a valid age X25519 recipient.
    InvalidRecipient(String),
    /// `encrypt` was called with an empty recipient list.
    NoRecipients,
    /// The underlying age encryption operation failed.
    Encrypt(age::EncryptError),
    /// The underlying age decryption operation failed. This is the error
    /// path exercised when none of the supplied identities can decrypt the
    /// ciphertext (e.g. decrypting with an identity that was not among the
    /// original recipients).
    Decrypt(age::DecryptError),
    /// An I/O error while streaming plaintext/ciphertext through age's
    /// `Read`/`Write` adapters, or while reading/writing the file-fallback
    /// identity in [`device_identity`].
    Io(std::io::Error),
    /// The OS keystore (macOS Keychain / Linux secret-service) rejected the
    /// operation, or -- far more commonly in practice -- is not reachable
    /// at all (no D-Bus session, no Keychain). See [`device_identity`].
    Keystore(keyring_core::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidIdentity(s) => write!(f, "invalid age identity: {s}"),
            Error::InvalidRecipient(s) => write!(f, "invalid age recipient: {s}"),
            Error::NoRecipients => write!(f, "encrypt requires at least one recipient"),
            Error::Encrypt(e) => write!(f, "age encryption failed: {e}"),
            Error::Decrypt(e) => write!(f, "age decryption failed: {e}"),
            Error::Io(e) => write!(f, "I/O error during age streaming: {e}"),
            Error::Keystore(e) => write!(f, "OS keystore error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl Identity {
    /// Generate a fresh, random X25519 identity (a new device key).
    pub fn generate() -> Self {
        Identity(age::x25519::Identity::generate())
    }

    /// The recipient (public key) derived from this identity.
    pub fn to_recipient(&self) -> Recipient {
        Recipient(self.0.to_public())
    }

    /// Serialize to age's `AGE-SECRET-KEY-1...` text encoding. The returned
    /// string is secret material: callers must not let it touch argv or an
    /// environment variable (CLAUDE.md's secrets rule) -- write it to a
    /// `0600` file or the OS keystore.
    pub fn to_secret_string(&self) -> String {
        self.0.to_string().expose_secret().to_string()
    }

    /// Parse an identity from its `AGE-SECRET-KEY-1...` text encoding.
    pub fn from_secret_string(s: &str) -> Result<Self, Error> {
        s.parse::<age::x25519::Identity>()
            .map(Identity)
            .map_err(|_| Error::InvalidIdentity(s.to_string()))
    }
}

impl Recipient {
    /// Parse a recipient from its `age1...` text encoding.
    pub fn from_string(s: &str) -> Result<Self, Error> {
        s.parse::<age::x25519::Recipient>()
            .map(Recipient)
            .map_err(|_| Error::InvalidRecipient(s.to_string()))
    }
}

impl fmt::Display for Recipient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Encrypt `plaintext` to all of `recipients`. Any of the corresponding
/// identities can later decrypt the result via [`decrypt`]. Fails clearly
/// (`Error::NoRecipients`) rather than silently producing an unreadable
/// ciphertext when `recipients` is empty.
pub fn encrypt(plaintext: &[u8], recipients: &[Recipient]) -> Result<Vec<u8>, Error> {
    if recipients.is_empty() {
        return Err(Error::NoRecipients);
    }
    let age_recipients: Vec<Box<dyn age::Recipient>> = recipients
        .iter()
        .map(|r| Box::new(r.0.clone()) as Box<dyn age::Recipient>)
        .collect();
    let encryptor = age::Encryptor::with_recipients(
        age_recipients
            .iter()
            .map(|r| r.as_ref() as &dyn age::Recipient),
    )
    .map_err(Error::Encrypt)?;

    let mut ciphertext = vec![];
    let mut writer = encryptor.wrap_output(&mut ciphertext).map_err(Error::Io)?;
    writer.write_all(plaintext).map_err(Error::Io)?;
    writer.finish().map_err(Error::Io)?;
    Ok(ciphertext)
}

/// Decrypt `ciphertext` with `identity`. Returns `Error::Decrypt` (wrapping
/// age's `NoMatchingKeys`) when `identity` was not among the recipients
/// `encrypt` was called with -- this fails clearly, not silently.
pub fn decrypt(ciphertext: &[u8], identity: &Identity) -> Result<Vec<u8>, Error> {
    let decryptor = age::Decryptor::new(ciphertext).map_err(Error::Decrypt)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity.0 as &dyn age::Identity))
        .map_err(Error::Decrypt)?;
    let mut plaintext = vec![];
    reader.read_to_end(&mut plaintext).map_err(Error::Io)?;
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_recipient_string_roundtrip() {
        let identity = Identity::generate();
        let recipient = identity.to_recipient();

        let recipient_str = recipient.to_string();
        let parsed_recipient = Recipient::from_string(&recipient_str).unwrap();
        assert!(parsed_recipient == recipient);

        let identity_str = identity.to_secret_string();
        assert!(identity_str.starts_with("AGE-SECRET-KEY-1"));
        let parsed_identity = Identity::from_secret_string(&identity_str).unwrap();
        assert!(parsed_identity.to_recipient() == recipient);
    }

    #[test]
    fn invalid_identity_and_recipient_strings_are_rejected() {
        assert!(matches!(
            Identity::from_secret_string("not-an-identity"),
            Err(Error::InvalidIdentity(_))
        ));
        assert!(matches!(
            Recipient::from_string("not-a-recipient"),
            Err(Error::InvalidRecipient(_))
        ));
    }

    #[test]
    fn round_trips_a_single_recipient() {
        let identity = Identity::generate();
        let recipient = identity.to_recipient();
        let plaintext = b"single recipient round trip";

        let ciphertext = encrypt(plaintext, &[recipient]).unwrap();
        let decrypted = decrypt(&ciphertext, &identity).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn round_trips_two_device_recipients() {
        // Simulates "my devices": encrypt once, either device key opens it.
        let device_a = Identity::generate();
        let device_b = Identity::generate();
        let recipients = vec![device_a.to_recipient(), device_b.to_recipient()];
        let plaintext = b"multi-recipient private memory item";

        let ciphertext = encrypt(plaintext, &recipients).unwrap();

        let decrypted_a = decrypt(&ciphertext, &device_a).unwrap();
        assert_eq!(decrypted_a, plaintext);

        let decrypted_b = decrypt(&ciphertext, &device_b).unwrap();
        assert_eq!(decrypted_b, plaintext);
    }

    #[test]
    fn decrypting_with_a_non_recipient_identity_fails_clearly() {
        let device_a = Identity::generate();
        let device_b = Identity::generate();
        let stranger = Identity::generate();
        let recipients = vec![device_a.to_recipient(), device_b.to_recipient()];
        let plaintext = b"not for the stranger's eyes";

        let ciphertext = encrypt(plaintext, &recipients).unwrap();

        let result = decrypt(&ciphertext, &stranger);
        assert!(matches!(result, Err(Error::Decrypt(_))));
    }

    #[test]
    fn encrypt_with_no_recipients_fails_clearly() {
        let plaintext = b"nobody to encrypt this to";
        let result = encrypt(plaintext, &[]);
        assert!(matches!(result, Err(Error::NoRecipients)));
    }

    #[test]
    fn ciphertext_is_nondeterministic_across_encryptions() {
        // design 7.2: age uses random nonces, unlike git-crypt's
        // deterministic scheme -- re-encrypting identical plaintext must
        // not leak equality via identical ciphertext.
        let identity = Identity::generate();
        let recipient = identity.to_recipient();
        let plaintext = b"same plaintext, twice";

        let ciphertext_1 = encrypt(plaintext, std::slice::from_ref(&recipient)).unwrap();
        let ciphertext_2 = encrypt(plaintext, &[recipient]).unwrap();
        assert_ne!(ciphertext_1, ciphertext_2);

        assert_eq!(decrypt(&ciphertext_1, &identity).unwrap(), plaintext);
        assert_eq!(decrypt(&ciphertext_2, &identity).unwrap(), plaintext);
    }
}
