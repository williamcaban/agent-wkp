//! M5-8 (ADR-0011): the hub's own private certificate authority.
//!
//! The hub is its own root of trust -- never the public web PKI. There
//! is no `webpki-roots` dependency anywhere in this crate and there
//! must never be one: every certificate this system trusts is one this
//! CA issued, and the only trust anchor is the root generated here.
//! That is also why ADR-0011 could drop M5-6's `webpki-roots` licensing
//! constraint entirely -- it was a constraint on trusting the *public*
//! web PKI, which mTLS against a private CA was never doing.
//!
//! **Key storage.** The root private key is the single highest-value
//! secret in the system: anyone holding it can mint a certificate for
//! any tenant. It lives in a `0600` file on the hub's persistent
//! volume, written exactly the way `wkp_crypto::device_identity`'s
//! `file_fallback::ensure` writes a device identity
//! (`create_new(true).mode(0o600)`) -- never in Postgres, never in an
//! environment variable, never in argv. ADR-0011's 2026-09-11 addendum
//! records why the database was considered and rejected: CLAUDE.md's
//! hard rules name only the OS keystore, a `0600` file, or stdin as
//! approved secret sources, and this is not a rule worth a quiet
//! exception for. Unlike a device, a hub has no OS keystore to lean
//! on, so the file fallback is the whole story here rather than a
//! fallback.
//!
//! The root *certificate* is public information and is written as
//! plain PEM with ordinary permissions: a device needs it to verify
//! the front door, and #123's CSR enrollment will hand it out.
//!
//! Scope: this module stands up the CA and the TLS server certificate
//! the front door presents. Signing device CSRs (#123) and verifying
//! device certificates at the handshake (#124) are separate tasks --
//! [`HubCa::issuer`] is the seam they build on, kept here so neither
//! needs to redesign how the root is loaded.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

/// The CA's own certificate -- public information, ordinary
/// permissions.
pub const CA_CERT_FILE: &str = "ca-cert.pem";
/// The CA's private key -- `0600`, see the module doc.
pub const CA_KEY_FILE: &str = "ca-key.pem";

/// Ten years. A root that has to be rotated often is a root whose
/// rotation story has to exist before anything else works; ADR-0011
/// deliberately left rotation out of this milestone, so the root
/// outlives it comfortably rather than expiring mid-M5.
const CA_VALIDITY_DAYS: i64 = 3650;

/// The TLS server leaf is minted fresh on every startup and never
/// persisted, so its validity only has to cover one process's
/// lifetime with a wide margin.
const SERVER_LEAF_VALIDITY_DAYS: i64 = 397;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Rcgen(rcgen::Error),
    Rustls(rustls::Error),
    /// The state directory holds one of the two CA files but not the
    /// other. Deliberately fatal rather than "regenerate the missing
    /// half": a half-present CA means either a partial restore or a
    /// partial deletion, and silently minting a *new* root would
    /// invalidate every certificate already issued from the old one --
    /// the same never-overwrite-an-existing-secret-on-a-read-hiccup
    /// posture `device_identity::get_or_create` takes.
    Incomplete(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "hub CA: {e}"),
            Error::Rcgen(e) => write!(f, "hub CA: certificate generation failed: {e}"),
            Error::Rustls(e) => write!(f, "hub CA: TLS configuration failed: {e}"),
            Error::Incomplete(m) => write!(f, "hub CA: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rcgen::Error> for Error {
    fn from(e: rcgen::Error) -> Self {
        Error::Rcgen(e)
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Self {
        Error::Rustls(e)
    }
}

/// The hub's loaded (or freshly generated) root CA.
///
/// Holds the root's certificate and private key in memory for the
/// process's lifetime -- the key never leaves this struct except
/// through [`HubCa::issuer`], and is never rendered by `Debug` (this
/// type deliberately does not derive it) or logged.
pub struct HubCa {
    cert_pem: String,
    /// PKCS#8 PEM. Secret; see the module doc.
    key_pem: String,
}

impl HubCa {
    /// Load the root CA from `state_dir`, or generate and persist a new
    /// one if it isn't there yet.
    ///
    /// Idempotent: a second call against the same directory loads the
    /// same root rather than minting a second one, which matters
    /// because every device certificate already issued chains to
    /// whichever root is on disk.
    pub fn ensure(state_dir: &Path) -> Result<Self, Error> {
        let cert_path = state_dir.join(CA_CERT_FILE);
        let key_path = state_dir.join(CA_KEY_FILE);

        match (cert_path.exists(), key_path.exists()) {
            (true, true) => Ok(HubCa {
                cert_pem: fs::read_to_string(&cert_path)?,
                key_pem: fs::read_to_string(&key_path)?,
            }),
            (false, false) => Self::generate(state_dir, &cert_path, &key_path),
            (true, false) => Err(Error::Incomplete(format!(
                "{} exists but {} does not -- refusing to mint a new root that would \
                 invalidate every certificate already issued from the old one",
                cert_path.display(),
                key_path.display()
            ))),
            (false, true) => Err(Error::Incomplete(format!(
                "{} exists but {} does not -- refusing to guess at a root certificate \
                 for an existing private key",
                key_path.display(),
                cert_path.display()
            ))),
        }
    }

    fn generate(state_dir: &Path, cert_path: &Path, key_path: &Path) -> Result<Self, Error> {
        let key = KeyPair::generate()?;

        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "wkp hub CA");
        dn.push(DnType::OrganizationName, "wkp");
        params.distinguished_name = dn;
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(CA_VALIDITY_DAYS);

        let cert = params.self_signed(&key)?;
        let cert_pem = cert.pem();
        let key_pem = key.serialize_pem();

        fs::create_dir_all(state_dir)?;
        // The certificate first, the key second: if the process dies
        // between the two, `ensure` reports the half-written state
        // rather than loading a key whose certificate never made it to
        // disk.
        let mut cert_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(cert_path)?;
        cert_file.write_all(cert_pem.as_bytes())?;

        // `create_new(true).mode(0o600)`: the mode is applied by
        // `open(2)` itself, so the key's bytes are never reachable
        // through a world-readable file, not even for the instant a
        // create-then-chmod would leave open. Same construction as
        // `wkp_crypto::device_identity`'s `file_fallback::ensure`.
        let mut key_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(key_path)?;
        key_file.write_all(key_pem.as_bytes())?;

        Ok(HubCa { cert_pem, key_pem })
    }

    /// The root certificate, PEM -- public information. What a device
    /// pins as its trust anchor, and what #123's enrollment response
    /// hands back.
    pub fn root_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The root as an `rcgen::Issuer`, rebuilt from the certificate
    /// actually on disk (`from_ca_cert_pem`) rather than from a
    /// reconstructed copy of the parameters it was generated with --
    /// the reconstruction would silently start minting leaves with a
    /// mismatched issuer DN the day someone edited a constant in this
    /// file, and every already-issued certificate would stop chaining.
    ///
    /// This is the seam #123 (CSR signing) builds on: signing a device
    /// certificate is `params.signed_by(public_key, &ca.issuer()?)`,
    /// with no change needed to how the root is loaded.
    pub fn issuer(&self) -> Result<Issuer<'static, KeyPair>, Error> {
        let key = KeyPair::from_pem(&self.key_pem)?;
        Ok(Issuer::from_ca_cert_pem(&self.cert_pem, key)?)
    }

    /// The `rustls::ServerConfig` the front door's TLS acceptor
    /// presents: a freshly minted server leaf, signed by this root,
    /// with SANs for `localhost` and `127.0.0.1`.
    ///
    /// **Client certificates are deliberately not verified here**
    /// (`with_no_client_auth`). mTLS client-certificate verification --
    /// a custom `ClientCertVerifier` checking both chain validity
    /// against this same root and the control plane's `revoked_at`
    /// state -- is issue #124's job, not this one's; ADR-0011 sequences
    /// it after TLS termination exists at all. This is an explicit
    /// decision, not a forgotten check.
    ///
    /// The leaf is minted per call and never written to disk: it is
    /// derivable from the root at any time, and one less private key
    /// at rest is one less thing to protect.
    pub fn server_tls_config(&self) -> Result<Arc<rustls::ServerConfig>, Error> {
        let (leaf, key) = self.mint_server_leaf()?;
        // Only the leaf goes on the wire, not the root: a client that
        // does not already hold this root as a trust anchor must not be
        // able to complete the handshake, and sending the root would
        // never help one that does.
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![leaf], key)?;

        Ok(Arc::new(config))
    }

    /// The TLS server certificate itself, split out from
    /// [`HubCa::server_tls_config`] so the chain can be inspected
    /// (notably: verified against this same CA's root) without
    /// reaching inside a built `ServerConfig`, which `rustls`
    /// deliberately does not expose.
    fn mint_server_leaf(&self) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), Error> {
        let issuer = self.issuer()?;
        let leaf_key = KeyPair::generate()?;

        let mut params = CertificateParams::new(Vec::<String>::new())?;
        // Not a configurable-hostname mechanism: the front door is
        // reached over the shared podman network and, in tests, over
        // loopback. A real public hostname is deployment work
        // (milestones.md's "out of scope for M5"), and adding a knob
        // nobody has a value for yet would just be a knob to get wrong.
        params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into()?),
            SanType::IpAddress(std::net::IpAddr::from([127, 0, 0, 1])),
        ];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "wkp hub front door");
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after =
            time::OffsetDateTime::now_utc() + time::Duration::days(SERVER_LEAF_VALIDITY_DAYS);

        let leaf = params.signed_by(&leaf_key, &issuer)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        Ok((leaf.der().clone(), key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls_pki_types::{ServerName, UnixTime};
    use std::os::unix::fs::PermissionsExt;

    fn root_store(ca: &HubCa) -> Arc<rustls::RootCertStore> {
        let mut roots = rustls::RootCertStore::empty();
        for pem in rustls_pemfile_certs(ca.root_cert_pem()) {
            roots.add(pem).expect("add the hub's own root");
        }
        Arc::new(roots)
    }

    /// A deliberately tiny PEM certificate decoder rather than a new
    /// dependency for test-only use: this only ever reads a PEM this
    /// same module just wrote.
    fn rustls_pemfile_certs(pem: &str) -> Vec<CertificateDer<'static>> {
        let mut out = Vec::new();
        let mut current: Option<String> = None;
        for line in pem.lines() {
            if line.starts_with("-----BEGIN CERTIFICATE-----") {
                current = Some(String::new());
            } else if line.starts_with("-----END CERTIFICATE-----") {
                if let Some(b64) = current.take() {
                    out.push(CertificateDer::from(base64_decode(&b64)));
                }
            } else if let Some(buf) = current.as_mut() {
                buf.push_str(line.trim());
            }
        }
        out
    }

    fn base64_decode(s: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut bits = 0;
        for byte in s.bytes() {
            if byte == b'=' {
                break;
            }
            let Some(value) = ALPHABET.iter().position(|c| *c == byte) else {
                continue;
            };
            acc = (acc << 6) | value as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }

    #[test]
    fn ensure_generates_and_persists_both_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ca = HubCa::ensure(dir.path()).expect("ensure");

        assert!(dir.path().join(CA_CERT_FILE).exists());
        assert!(dir.path().join(CA_KEY_FILE).exists());
        assert!(ca.root_cert_pem().contains("BEGIN CERTIFICATE"));
    }

    /// `ensure` must load the existing root, not mint a second one --
    /// every certificate already issued chains to whichever root is on
    /// disk. Compares the actual root DER, not just "no error".
    #[test]
    fn ensure_loads_the_same_ca_on_a_second_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = HubCa::ensure(dir.path()).expect("first ensure");
        let second = HubCa::ensure(dir.path()).expect("second ensure");

        let first_der = rustls_pemfile_certs(first.root_cert_pem());
        let second_der = rustls_pemfile_certs(second.root_cert_pem());
        assert_eq!(first_der.len(), 1);
        assert_eq!(
            first_der, second_der,
            "a second ensure must load the same root, not generate a new one"
        );
    }

    /// The module's whole storage rule, asserted the same way
    /// `device_identity`'s `file_fallback_writes_a_0600_file` asserts
    /// it.
    #[test]
    fn ca_private_key_is_written_0600() {
        let dir = tempfile::tempdir().expect("temp dir");
        HubCa::ensure(dir.path()).expect("ensure");
        let mode = std::fs::metadata(dir.path().join(CA_KEY_FILE))
            .expect("stat the key file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A half-present CA is fatal, never silently re-minted.
    #[test]
    fn ensure_refuses_a_half_present_ca() {
        let dir = tempfile::tempdir().expect("temp dir");
        HubCa::ensure(dir.path()).expect("ensure");
        std::fs::remove_file(dir.path().join(CA_KEY_FILE)).expect("remove the key");
        assert!(matches!(
            HubCa::ensure(dir.path()),
            Err(Error::Incomplete(_))
        ));
    }

    /// The load-bearing assertion for TLS termination: the leaf
    /// `server_tls_config` mints actually verifies against a
    /// `RootCertStore` built from this same CA's root, for the
    /// `localhost` SAN -- a real chain-and-name check by rustls' own
    /// verifier, not just "a certificate was produced".
    #[test]
    fn server_leaf_verifies_against_the_hubs_own_root() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ca = HubCa::ensure(dir.path()).expect("ensure");
        let (leaf, _key) = ca.mint_server_leaf().expect("mint the TLS server leaf");

        let verifier = WebPkiServerVerifier::builder_with_provider(
            root_store(&ca),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .expect("build the verifier");

        verifier
            .verify_server_cert(
                &leaf,
                &[],
                &ServerName::try_from("localhost").expect("server name"),
                &[],
                UnixTime::now(),
            )
            .expect("the hub's own leaf must verify against the hub's own root, for localhost");
    }

    /// The other half of the trust story: a leaf from an unrelated CA
    /// must not verify against the hub's root. Without this, the test
    /// above would still pass against a verifier that accepted
    /// anything.
    #[test]
    fn a_leaf_from_an_unrelated_ca_does_not_verify() {
        let hub_dir = tempfile::tempdir().expect("temp dir");
        let ca = HubCa::ensure(hub_dir.path()).expect("ensure");

        let other_dir = tempfile::tempdir().expect("temp dir");
        let unrelated = HubCa::ensure(other_dir.path()).expect("ensure an unrelated CA");
        let (foreign_leaf, _key) = unrelated
            .mint_server_leaf()
            .expect("mint a leaf from an unrelated CA");

        let verifier = WebPkiServerVerifier::builder_with_provider(
            root_store(&ca),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .expect("build the verifier");

        assert!(
            verifier
                .verify_server_cert(
                    &foreign_leaf,
                    &[],
                    &ServerName::try_from("localhost").expect("server name"),
                    &[],
                    UnixTime::now(),
                )
                .is_err(),
            "a certificate signed by an unrelated CA must be rejected"
        );
    }
}
