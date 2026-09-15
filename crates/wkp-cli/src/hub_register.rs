//! `wkp hub register`: the client (device) side of the RFC 8628 device
//! authorization grant (design 6.4, 8.1, M5-2). The server side
//! (`POST /device/code`, `POST /device/token`, the `/verify` page)
//! lives in `wkp-hub`, which this crate deliberately never links
//! (`crates/wkp-hub/src/main.rs`'s own doc comment: "the laptop wkp
//! binary never links the Postgres client or control-plane code").
//!
//! M5-13 (ADR-0011): the hub is HTTPS-only (M5-8's front door has no
//! plain-HTTP listener at all), and it is always its own private CA --
//! never a publicly trusted one, and this crate deliberately has no
//! `webpki-roots` dependency to fall back to even if it wanted to (see
//! `Cargo.toml`'s own comment on why). That leaves a real bootstrapping
//! question this module has to answer: *how does a device that has
//! never talked to this hub before know to trust it at all* --
//! `--ca-cert <path>` is the answer, an out-of-band root an operator
//! hands the device (the exact bytes `wkp-hub ca-cert` prints
//! operator-side). Required, not optional with a silent public-PKI
//! fallback: the whole point of a private CA is that nothing else is
//! trusted, and a quiet fallback to the public web PKI here would be
//! actively wrong, not just less strict.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct HubRegisterOptions {
    pub(crate) path: PathBuf,
    pub(crate) hub_url: String,
    pub(crate) tenant_slug: String,
    pub(crate) ca_cert_path: PathBuf,
}

/// Parses `wkp hub register --hub-url <url> --tenant <slug> --ca-cert <path> [--path <dir>]`.
pub(crate) fn parse_hub_register_args(
    mut args: impl Iterator<Item = String>,
) -> Result<HubRegisterOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut hub_url: Option<String> = None;
    let mut tenant_slug: Option<String> = None;
    let mut ca_cert_path: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hub-url" => hub_url = Some(args.next().ok_or("--hub-url requires a value")?),
            "--tenant" => tenant_slug = Some(args.next().ok_or("--tenant requires a value")?),
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            "--ca-cert" => {
                ca_cert_path = Some(PathBuf::from(
                    args.next().ok_or("--ca-cert requires a value")?,
                ))
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let hub_url = hub_url.ok_or_else(|| "hub register requires --hub-url <url>".to_string())?;
    let tenant_slug =
        tenant_slug.ok_or_else(|| "hub register requires --tenant <slug>".to_string())?;
    let ca_cert_path = ca_cert_path.ok_or_else(|| {
        "hub register requires --ca-cert <path> (the hub's own CA root, from `wkp-hub ca-cert`)"
            .to_string()
    })?;

    Ok(HubRegisterOptions {
        path,
        hub_url: hub_url.trim_end_matches('/').to_string(),
        tenant_slug,
        ca_cert_path,
    })
}

/// Where this store's hub signing identity lives when the OS keystore
/// isn't reachable -- parallel to `filter::DEVICE_IDENTITY_FALLBACK`'s
/// `.wkp/device-identity` for the (unrelated) age encryption identity.
const SIGNING_IDENTITY_FALLBACK: &str = ".wkp/hub-signing-identity";

/// Where this store keeps the certificate the hub's CA issues (M5-9,
/// ADR-0011) -- public information, ordinary permissions, unlike the
/// signing identity above. Overwritten by every successful
/// registration; there's exactly one certificate this store is
/// currently using, never a history of past ones.
const DEVICE_CERT_FILE: &str = ".wkp/hub-device-cert.pem";

/// Where this store keeps the hub's own CA root once a registration
/// hands it back (M5-9, ADR-0011) -- the same bytes `wkp-hub ca-cert`
/// prints operator-side, now delivered over the registration flow
/// itself. Public information. Actually *pinning* against this file
/// (using it to verify a later connection) is #124's job, not this
/// one's -- this only ever writes it.
const CA_CERT_FILE: &str = ".wkp/hub-ca-cert.pem";

/// Where this store keeps a PKCS#8-PEM copy of the hub signing
/// identity's own private key (M5-10, ADR-0011) -- what `git` itself
/// needs (`http.sslKey`) to actually present the certificate above
/// during an mTLS handshake, since `git`/`curl`'s OpenSSL-backed TLS
/// stack cannot load [`SIGNING_IDENTITY_FALLBACK`]'s OpenSSH-format
/// storage directly (confirmed empirically: `openssl pkey -in
/// <that file>` fails to parse it at all). Not a second key -- see
/// [`wkp_crypto::signing_identity::SigningIdentity::to_pkcs8_pem`]'s
/// own doc comment. Secret material, unlike the two files above:
/// written `0600` via [`write_private_file`], never [`write_public_file`].
const DEVICE_KEY_PEM_FILE: &str = ".wkp/hub-device-key.pem";

/// Printed to stdout so a caller can watch progress; kept separate
/// from the final [`HubRegisterSummary`] so `wkp hub register`'s
/// eventual "waiting for approval..." output happens as it happens,
/// not buffered until the whole polling loop finishes.
fn report(message: &str) {
    println!("wkp: {message}");
}

/// Writes `contents` to `path` with ordinary permissions, creating
/// parent directories as needed -- for the certificate and CA root
/// (M5-9, ADR-0011), both public information unlike
/// [`SIGNING_IDENTITY_FALLBACK`]'s private key, which is why this
/// doesn't need `file_fallback::ensure`'s `0600` construction.
/// Overwrites on every call: a fresh registration always replaces
/// whatever was there before.
fn write_public_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents).map_err(|e| e.to_string())
}

/// Like [`write_public_file`], but `0600` -- for
/// [`DEVICE_KEY_PEM_FILE`], secret material unlike the certificate/CA
/// root that function writes. Same `create_new`-free overwrite
/// semantics as `write_public_file` (a fresh registration always
/// replaces whatever key encoding was there before); the mode is set
/// via a separate `set_permissions` call, not `OpenOptions::mode`,
/// because a re-registration must overwrite an existing file, which
/// `create_new(true)` (the construction
/// `wkp_crypto`'s own `0600` file-fallback identities use) refuses to
/// do by design.
fn write_private_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents).map_err(|e| e.to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())
}

pub(crate) struct HubRegisterSummary {
    pub(crate) tenant_slug: String,
    pub(crate) device_id: i64,
}

impl std::fmt::Display for HubRegisterSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: registered with tenant {} (device id {}), certificate stored at {}, \
             git-usable key at {}",
            self.tenant_slug, self.device_id, DEVICE_CERT_FILE, DEVICE_KEY_PEM_FILE
        )
    }
}

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: i64,
    interval: u64,
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    status: Option<String>,
    tenant_slug: Option<String>,
    device_id: Option<i64>,
    /// The hub-signed device certificate (M5-9, ADR-0011), once
    /// `status == "approved"` -- absent on every earlier poll.
    certificate_pem: Option<String>,
    /// The hub's own CA root, alongside the certificate above.
    ca_cert_pem: Option<String>,
    error: Option<String>,
}

/// `wkp hub register`: generates (or reuses) this store's ed25519 hub
/// signing identity, builds a certificate signing request (CSR) for it
/// (M5-9, ADR-0011 -- the hub's CA signs this instead of accepting a
/// bare public key), requests a device code, prints the human-facing
/// verification instructions, and polls until approved (storing the
/// resulting certificate) or the grant expires.
pub(crate) fn run_hub_register(opts: &HubRegisterOptions) -> Result<HubRegisterSummary, String> {
    let device_id = wkp_git::sync::device_id(&opts.path)?;
    let identity = wkp_crypto::signing_identity::ensure(
        &device_id,
        &opts.path.join(SIGNING_IDENTITY_FALLBACK),
    )
    .map_err(|e| e.to_string())?;
    let csr_pem = identity.to_csr_pem().map_err(|e| e.to_string())?;

    let ca_cert_pem = std::fs::read(&opts.ca_cert_path)
        .map_err(|e| format!("reading --ca-cert {}: {e}", opts.ca_cert_path.display()))?;
    let ca_cert = ureq::tls::Certificate::from_pem(&ca_cert_pem).map_err(|e| {
        format!(
            "{} is not a valid PEM certificate: {e}",
            opts.ca_cert_path.display()
        )
    })?;
    let tls_config = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        // `ring`, matching the crypto provider `wkp-hub`'s own
        // `rustls`/`rcgen` usage pins throughout this workspace --
        // `rustls-no-provider` (this crate's `ureq` feature, see
        // `Cargo.toml`) leaves the provider unset until this call.
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        // Trust *only* the hub's own root -- never `RootCerts::WebPki`
        // (the default), which would be actively wrong for a private
        // CA (see the module doc comment).
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(vec![ca_cert])))
        .build();
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .tls_config(tls_config)
        .build();
    let agent: ureq::Agent = agent.into();

    let code_url = format!("{}/device/code", opts.hub_url);
    let mut code_http_response = agent
        .post(&code_url)
        .send_json(serde_json::json!({
            "tenant_slug": opts.tenant_slug,
            "csr_pem": csr_pem,
        }))
        .map_err(|e| format!("requesting a device code from {code_url}: {e}"))?;
    if code_http_response.status() != 200 {
        let body = code_http_response
            .body_mut()
            .read_to_string()
            .unwrap_or_default();
        return Err(format!(
            "hub refused the device-code request ({}): {body}",
            code_http_response.status()
        ));
    }
    let code: DeviceCodeResponse = code_http_response
        .body_mut()
        .read_json()
        .map_err(|e| format!("parsing device-code response: {e}"))?;

    let verification_url = if code.verification_uri.starts_with("http") {
        code.verification_uri.clone()
    } else {
        format!("{}{}", opts.hub_url, code.verification_uri)
    };
    report(&format!(
        "visit {verification_url} and enter code: {}",
        code.user_code
    ));
    report("waiting for approval...");

    let token_url = format!("{}/device/token", opts.hub_url);
    let deadline = std::time::Instant::now() + Duration::from_secs(code.expires_in.max(0) as u64);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("device code expired before it was approved".to_string());
        }
        std::thread::sleep(Duration::from_secs(code.interval));

        let mut token_http_response = agent
            .post(&token_url)
            .send_json(serde_json::json!({ "device_code": code.device_code }))
            .map_err(|e| format!("polling {token_url}: {e}"))?;
        let token: TokenResponse = token_http_response
            .body_mut()
            .read_json()
            .map_err(|e| format!("parsing token response: {e}"))?;

        match token.error.as_deref() {
            Some("authorization_pending") => continue,
            Some(other) => return Err(format!("hub reported an error: {other}")),
            None => {}
        }
        if token.status.as_deref() == Some("approved") {
            let certificate_pem = token
                .certificate_pem
                .ok_or_else(|| "hub approved the grant but sent no certificate_pem".to_string())?;
            write_public_file(&opts.path.join(DEVICE_CERT_FILE), &certificate_pem)?;
            if let Some(ca_cert_pem) = token.ca_cert_pem {
                write_public_file(&opts.path.join(CA_CERT_FILE), &ca_cert_pem)?;
            }
            let pkcs8_pem = identity.to_pkcs8_pem().map_err(|e| e.to_string())?;
            write_private_file(&opts.path.join(DEVICE_KEY_PEM_FILE), &pkcs8_pem)?;
            return Ok(HubRegisterSummary {
                tenant_slug: token
                    .tenant_slug
                    .ok_or_else(|| "hub approved the grant but sent no tenant_slug".to_string())?,
                device_id: token
                    .device_id
                    .ok_or_else(|| "hub approved the grant but sent no device_id".to_string())?,
            });
        }
        return Err("unexpected response from hub while polling".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::args;

    #[test]
    fn parse_hub_register_args_requires_hub_url_tenant_and_ca_cert() {
        assert!(parse_hub_register_args(args(&[])).is_err());
        assert!(parse_hub_register_args(args(&["--hub-url", "https://localhost:8080"])).is_err());
        assert!(parse_hub_register_args(args(&["--tenant", "myteam"])).is_err());
        assert!(parse_hub_register_args(args(&[
            "--hub-url",
            "https://localhost:8080",
            "--tenant",
            "myteam",
        ]))
        .is_err());
    }

    #[test]
    fn parse_hub_register_args_reads_all_flags_and_trims_trailing_slash() {
        let opts = parse_hub_register_args(args(&[
            "--hub-url",
            "https://localhost:8080/",
            "--tenant",
            "myteam",
            "--ca-cert",
            "/tmp/ca-cert.pem",
            "--path",
            "/tmp/store",
        ]))
        .expect("parse_hub_register_args");
        assert_eq!(opts.hub_url, "https://localhost:8080");
        assert_eq!(opts.tenant_slug, "myteam");
        assert_eq!(opts.ca_cert_path, PathBuf::from("/tmp/ca-cert.pem"));
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn parse_hub_register_args_rejects_unrecognized_flags() {
        assert!(parse_hub_register_args(args(&[
            "--hub-url",
            "https://localhost:8080",
            "--tenant",
            "myteam",
            "--ca-cert",
            "/tmp/ca-cert.pem",
            "--bogus"
        ]))
        .is_err());
    }
}
