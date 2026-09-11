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
//! Scope: this module stands up the CA, the TLS server certificate the
//! front door presents, signs device CSRs (#123), and -- M5-10,
//! ADR-0011 -- verifies device certificates at the handshake via
//! [`RevocationAwareClientCertVerifier`]. [`HubCa::issuer`] is the seam
//! [`HubCa::sign_device_csr`] builds on; [`HubCa::server_tls_config`] is
//! where the verifier gets wired into the listener's `ServerConfig`.

use crate::control_plane;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData,
    SanType, SerialNumber,
};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{CertificateError, DistinguishedName as RustlsDistinguishedName, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use x509_parser::prelude::{FromDer, X509Certificate};

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

/// A device's client certificate (M5-9, ADR-0011), unlike the server
/// leaf above, is persisted (in `devices.certificate_pem`) and handed
/// back to the device to keep using -- renewal is out of scope for this
/// milestone (`docs/plan/milestones.md`), so this matches the server
/// leaf's own validity window rather than inventing a second number
/// with no renewal story behind it either.
const DEVICE_CERT_VALIDITY_DAYS: i64 = 397;

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
    /// [`HubCa::sign_device_csr`] (M5-9, ADR-0011) was handed a CSR
    /// whose key algorithm isn't ed25519, or whose embedded public key
    /// isn't the 32 bytes an ed25519 key must be -- untrusted input
    /// from a device, rejected rather than signed.
    UnsupportedKeyAlgorithm,
    /// [`HubCa::server_tls_config`] (M5-10, ADR-0011) could not build
    /// the client-certificate verifier -- either the root PEM this
    /// struct already holds in memory failed to re-parse as DER (should
    /// be unreachable: it is the exact PEM [`HubCa::ensure`] loaded or
    /// generated), or `rustls`'s own `WebPkiClientVerifier` builder
    /// rejected the resulting root store.
    ClientVerifierSetup(String),
    /// [`HubCa::sign_device_csr`] (M5-9) could not re-parse the
    /// certificate it just signed -- should be unreachable, since the
    /// input is this same function's own freshly minted DER.
    CertificateReparse(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "hub CA: {e}"),
            Error::Rcgen(e) => write!(f, "hub CA: certificate generation failed: {e}"),
            Error::Rustls(e) => write!(f, "hub CA: TLS configuration failed: {e}"),
            Error::Incomplete(m) => write!(f, "hub CA: {m}"),
            Error::UnsupportedKeyAlgorithm => {
                write!(f, "hub CA: CSR key algorithm must be ed25519")
            }
            Error::ClientVerifierSetup(m) => {
                write!(
                    f,
                    "hub CA: could not set up the client-certificate verifier: {m}"
                )
            }
            Error::CertificateReparse(m) => {
                write!(
                    f,
                    "hub CA: could not re-parse a just-signed certificate: {m}"
                )
            }
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
/// type deliberately does not derive it) or logged. `Clone` is
/// deliberately derived, though: [`crate::front_door`]'s router needs
/// an owned copy in its shared `axum` state (M5-9, ADR-0011, for
/// [`HubCa::sign_device_csr`]), and cloning two `String`s is cheap --
/// it does not change how many copies of the key exist on disk or who
/// can reach them, only how many equivalent in-memory copies one
/// already-trusted process holds.
#[derive(Clone)]
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
    /// with SANs for `localhost` and `127.0.0.1`, plus (M5-10,
    /// ADR-0011) a [`RevocationAwareClientCertVerifier`] that requests
    /// but does not require a client certificate -- see that type's own
    /// doc comment for why client auth is optional at the TLS layer
    /// rather than mandatory.
    ///
    /// The leaf is minted per call and never written to disk: it is
    /// derivable from the root at any time, and one less private key
    /// at rest is one less thing to protect.
    pub fn server_tls_config(&self) -> Result<Arc<rustls::ServerConfig>, Error> {
        let (leaf, key) = self.mint_server_leaf()?;
        let verifier = self.client_cert_verifier()?;
        // Only the leaf goes on the wire, not the root: a client that
        // does not already hold this root as a trust anchor must not be
        // able to complete the handshake, and sending the root would
        // never help one that does.
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![leaf], key)?;

        Ok(Arc::new(config))
    }

    /// Builds the [`RevocationAwareClientCertVerifier`]
    /// [`HubCa::server_tls_config`] installs: a `WebPkiClientVerifier`
    /// trusting only this hub's own root (never the public web PKI, see
    /// the module doc), wrapped so every presented certificate also
    /// gets checked against the control plane's `revoked_at` state.
    fn client_cert_verifier(&self) -> Result<Arc<dyn ClientCertVerifier>, Error> {
        let root = CertificateDer::from_pem_slice(self.cert_pem.as_bytes()).map_err(|e| {
            Error::ClientVerifierSetup(format!(
                "re-parsing the hub's own root certificate PEM: {e}"
            ))
        })?;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root)?;
        let inner = WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .map_err(|e| Error::ClientVerifierSetup(e.to_string()))?;
        Ok(Arc::new(RevocationAwareClientCertVerifier { inner }))
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

    /// Signs `csr_pem`, a device's PKCS#10 certificate signing request,
    /// minting a client-authentication certificate for the ed25519 key
    /// it proves possession of (M5-9, ADR-0011).
    ///
    /// **Only the CSR's embedded public key and its own self-signature
    /// are trusted.** `rcgen::CertificateSigningRequestParams::from_pem`
    /// also parses a requested subject, SANs, key usage, and basic
    /// constraints out of the CSR -- all attacker-controlled, since the
    /// CSR comes from an unauthenticated device. Naively signing those
    /// parsed params (`CertificateSigningRequestParams::signed_by`)
    /// would let a device request, and receive, a certificate with
    /// `BasicConstraints: CA:true` of its own choosing. Every field of
    /// the issued certificate is instead built fresh here, the same way
    /// [`mint_server_leaf`] builds the front door's own leaf from
    /// scratch rather than from caller-supplied parameters.
    pub fn sign_device_csr(
        &self,
        csr_pem: &str,
        common_name: &str,
    ) -> Result<SignedDeviceCert, Error> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem)?;
        if csr.public_key.algorithm() != &rcgen::PKCS_ED25519 {
            return Err(Error::UnsupportedKeyAlgorithm);
        }
        let public_key_raw: [u8; 32] = csr
            .public_key
            .der_bytes()
            .try_into()
            .map_err(|_| Error::UnsupportedKeyAlgorithm)?;

        let issuer = self.issuer()?;
        let serial_bytes = random_serial_bytes()?;
        let not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        let not_after =
            time::OffsetDateTime::now_utc() + time::Duration::days(DEVICE_CERT_VALIDITY_DAYS);

        let mut params = CertificateParams::default();
        params.serial_number = Some(SerialNumber::from(serial_bytes));
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        params.distinguished_name = dn;
        params.not_before = not_before;
        params.not_after = not_after;

        let cert = params.signed_by(&csr.public_key, &issuer)?;

        // Re-derived from the certificate's own DER, not hex-encoded
        // directly from `serial_bytes` above: DER's INTEGER encoding
        // (`yasna::write_bigint_bytes`, which `rcgen` uses) prepends a
        // leading `0x00` byte whenever the serial's own first byte has
        // its high bit set, to keep the value unambiguously
        // non-negative -- true for roughly half of all random serials.
        // Hex-encoding the pre-padding bytes directly silently produced
        // a `cert_serial` that didn't match what
        // `RevocationAwareClientCertVerifier` (`x509_parser`, reading
        // the certificate as it actually goes out on the wire) would
        // compute for the same certificate on every handshake where
        // that padding applied -- caught by this module's own
        // `sign_device_csr_returns_a_certificate_for_the_csrs_own_public_key`
        // test intermittently failing (about half the time, matching
        // the padding's own ~50% probability) before this fix, not a
        // hypothetical. Re-parsing here guarantees the value this
        // struct hands back is byte-for-byte what any later DER parse
        // of the same certificate will also compute.
        let (_, reparsed) = X509Certificate::from_der(cert.der()).map_err(|_| {
            Error::CertificateReparse("reading back its own serial number".to_string())
        })?;
        let serial_hex = reparsed
            .raw_serial()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        Ok(SignedDeviceCert {
            certificate_pem: cert.pem(),
            serial_hex,
            public_key_raw,
            not_before,
            not_after,
        })
    }
}

/// What [`HubCa::sign_device_csr`] hands back: the signed certificate
/// itself, plus the pieces `crate::http::handle_verify_submit` needs to
/// both register the device (the raw public key, converted to OpenSSH
/// form by `wkp_crypto::signing_identity::openssh_public_key_from_raw_ed25519`)
/// and persist the certificate metadata (`crate::control_plane::issue_device_certificate`).
pub struct SignedDeviceCert {
    pub certificate_pem: String,
    /// Hex-encoded, matching `devices.cert_serial`'s own encoding.
    pub serial_hex: String,
    pub public_key_raw: [u8; 32],
    pub not_before: time::OffsetDateTime,
    pub not_after: time::OffsetDateTime,
}

/// 16 random bytes (a 128-bit serial, the same size convention most
/// real CAs use) read straight off `/dev/urandom` -- the same
/// direct-read pattern `control_plane::grants::random_hex` already
/// established (CLAUDE.md's slim-core rule: no `rand` dependency only
/// for this), kept as its own small copy here rather than reaching into
/// `control_plane` (a Postgres-bookkeeping module with no business
/// knowing about certificate signing) for it.
fn random_serial_bytes() -> Result<Vec<u8>, Error> {
    use std::io::Read;
    let mut bytes = vec![0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(Error::Io)?;
    Ok(bytes)
}

/// M5-10 (ADR-0011): wraps a `WebPkiClientVerifier` (chain validity,
/// expiry, signed-by-this-root -- ordinary TLS trust) with the one
/// check standard TLS has no notion of: whether the control plane's
/// `devices.revoked_at` is set for the presenting device.
///
/// **Client auth is requested but not required at the TLS layer**
/// (`offer_client_auth() = true`, `client_auth_mandatory() = false`).
/// The front door serves two different kinds of route behind one TLS
/// listener: the RFC 8628 enrollment endpoints (`/device/code`,
/// `/device/token`, `/verify`), which a device calls *before* it holds
/// any certificate at all, and the git-http routes
/// (`crate::http::handle_git_http`), which do require one. `rustls`
/// has no per-route client-auth policy -- one `ServerConfig`, one
/// listener -- so the TLS layer stays permissive (any client, cert or
/// no cert, completes the handshake) and `handle_git_http` itself is
/// what actually rejects a connection with no certificate presented.
/// A certificate that *is* presented, however, is always fully
/// verified here regardless of which route it's ultimately used
/// against -- an invalid or revoked certificate fails the handshake
/// outright, never reaching any handler.
///
/// **Fail-closed, per-handshake** (ADR-0011's addendum): if the
/// control-plane lookup itself fails (Postgres unreachable), the
/// certificate is rejected, not admitted-by-default. This makes a
/// revocation effective starting with the *next* connection -- the
/// same guarantee the old SSH transport gave via `sshd`'s
/// `AuthorizedKeysCommand` being consulted on every new connection.
/// An already-open connection surviving past a revocation event is a
/// separate, deliberately out-of-scope concern (#128, active-connection
/// reset via Postgres `LISTEN`/`NOTIFY`).
struct RevocationAwareClientCertVerifier {
    inner: Arc<dyn ClientCertVerifier>,
}

impl std::fmt::Debug for RevocationAwareClientCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevocationAwareClientCertVerifier")
            .finish_non_exhaustive()
    }
}

impl ClientCertVerifier for RevocationAwareClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // See the struct's own doc comment: enrollment routes on this
        // same listener have no certificate to present yet.
        false
    }

    fn root_hint_subjects(&self) -> &[RustlsDistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Ordinary TLS trust first: chains to the hub's own root, not
        // expired, not yet valid, well-formed. Only once this succeeds
        // is the certificate's *identity* worth looking up at all.
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;

        let (_, cert) = X509Certificate::from_der(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let serial_hex: String = cert
            .raw_serial()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        // Blocking Postgres call inside a `rustls` handshake callback,
        // itself invoked from an async task (the front door's own
        // `axum`/`tokio` listener) -- a real tradeoff, not an
        // oversight. This crate already accepts "one Postgres
        // connection per request, not pooled" as correct-and-simple at
        // this milestone's traffic level (`control_plane`'s own module
        // doc); a single indexed lookup, once per TLS handshake (not
        // per request), is the same tradeoff applied one layer down.
        // Revisit alongside that same connection-pooling note if this
        // ever needs to hold up under real concurrent load.
        //
        // **A plain `std::thread::spawn`, not just a blocking call.**
        // `postgres::Client::connect`'s synchronous API builds and
        // drives its own private, single-threaded Tokio runtime
        // internally (`runtime::Builder::new_current_thread()...
        // block_on(...)`, see its own source). Calling that directly
        // from this callback panics with "Cannot start a runtime from
        // within a runtime" -- confirmed by hitting it for real while
        // implementing this, not a hypothetical -- because this
        // callback already executes on a thread the front door's own
        // multi-thread `tokio` runtime is using to drive the TLS
        // accept future. `tokio::task::block_in_place` does not help:
        // it only excuses the thread from cooperative scheduling, it
        // does not clear the thread-local "already inside a runtime"
        // marker `postgres`'s own nested `block_on` trips over. A
        // plain OS thread has no such marker at all, so `postgres`'s
        // own internal runtime construction works exactly as it does
        // everywhere else in this crate.
        let lookup = std::thread::spawn(move || {
            let mut client = control_plane::connect()?;
            control_plane::find_device_by_cert_serial(&mut client, &serial_hex)
        })
        .join()
        .map_err(|_| {
            rustls::Error::General(
                "control plane lookup thread panicked during mTLS handshake".to_string(),
            )
        })?;
        let device = lookup
            .map_err(|e| {
                rustls::Error::General(format!(
                    "control plane lookup failed during mTLS handshake: {e}"
                ))
            })?
            .ok_or(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ))?;
        if device.revoked_at.is_some() {
            return Err(rustls::Error::InvalidCertificate(CertificateError::Revoked));
        }

        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
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

    /// M5-9's own core acceptance criterion: signing a real device CSR
    /// (from `wkp_crypto::signing_identity`, the exact same code path
    /// `wkp hub register` calls) produces a certificate, and the raw
    /// public key `sign_device_csr` hands back matches the identity
    /// that produced the CSR -- the correctness property everything
    /// downstream (`devices.public_key`, `wkp-shell`'s SSH-path lookup)
    /// depends on.
    #[test]
    fn sign_device_csr_returns_a_certificate_for_the_csrs_own_public_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ca = HubCa::ensure(dir.path()).expect("ensure");

        let identity =
            wkp_crypto::signing_identity::SigningIdentity::generate().expect("generate identity");
        let csr_pem = identity.to_csr_pem().expect("to_csr_pem");

        let signed = ca
            .sign_device_csr(&csr_pem, "wkp device test")
            .expect("sign_device_csr");

        assert!(signed.certificate_pem.contains("BEGIN CERTIFICATE"));
        // 16 random bytes (`random_serial_bytes`), hex-encoded -- 32
        // hex chars -- *unless* DER's own canonical INTEGER encoding
        // prepended a leading `0x00` (whenever the first random byte's
        // high bit was set, true for about half of all serials, to
        // keep the value unambiguously non-negative), making it 17
        // bytes / 34 hex chars. `serial_hex` is deliberately read back
        // from the signed certificate's own DER (see `sign_device_csr`'s
        // own comment on why), so either length is a correct result --
        // asserting exactly one would make this test itself flaky.
        assert!(
            signed.serial_hex.len() == 32 || signed.serial_hex.len() == 34,
            "expected a 16- or 17-byte serial as hex, got {} chars",
            signed.serial_hex.len()
        );
        assert!(signed.not_before < signed.not_after);

        let derived_public_key = wkp_crypto::signing_identity::openssh_public_key_from_raw_ed25519(
            &signed.public_key_raw,
        )
        .expect("openssh_public_key_from_raw_ed25519");
        assert_eq!(
            derived_public_key,
            identity.public_key_openssh().expect("public_key_openssh"),
            "the certificate's embedded public key must be the exact key the CSR was for"
        );
    }

    /// Two devices' CSRs must never be issued the same serial -- a
    /// collision here would break any future lookup-by-serial
    /// (#124/#128's own job).
    #[test]
    fn sign_device_csr_issues_distinct_serials_for_distinct_devices() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ca = HubCa::ensure(dir.path()).expect("ensure");

        let first_csr = wkp_crypto::signing_identity::SigningIdentity::generate()
            .expect("generate")
            .to_csr_pem()
            .expect("to_csr_pem");
        let second_csr = wkp_crypto::signing_identity::SigningIdentity::generate()
            .expect("generate")
            .to_csr_pem()
            .expect("to_csr_pem");

        let first = ca
            .sign_device_csr(&first_csr, "wkp device test")
            .expect("sign first");
        let second = ca
            .sign_device_csr(&second_csr, "wkp device test")
            .expect("sign second");
        assert_ne!(first.serial_hex, second.serial_hex);
    }

    /// A malformed CSR is rejected, not signed -- untrusted input from
    /// an unauthenticated device.
    #[test]
    fn sign_device_csr_rejects_a_malformed_csr() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ca = HubCa::ensure(dir.path()).expect("ensure");
        assert!(ca.sign_device_csr("not a csr", "wkp device test").is_err());
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
