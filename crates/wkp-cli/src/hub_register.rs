//! `wkp hub register`: the client (device) side of the RFC 8628 device
//! authorization grant (design 6.4, 8.1, M5-2). The server side
//! (`POST /device/code`, `POST /device/token`, the `/verify` page)
//! lives in `wkp-hub`, which this crate deliberately never links
//! (`crates/wkp-hub/src/main.rs`'s own doc comment: "the laptop wkp
//! binary never links the Postgres client or control-plane code") --
//! this module only ever talks to a hub over plain HTTP, the same as
//! any other client would.

use std::path::PathBuf;
use std::time::Duration;

pub(crate) struct HubRegisterOptions {
    pub(crate) path: PathBuf,
    pub(crate) hub_url: String,
    pub(crate) tenant_slug: String,
}

/// Parses `wkp hub register --hub-url <url> --tenant <slug> [--path <dir>]`.
pub(crate) fn parse_hub_register_args(
    mut args: impl Iterator<Item = String>,
) -> Result<HubRegisterOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut hub_url: Option<String> = None;
    let mut tenant_slug: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hub-url" => hub_url = Some(args.next().ok_or("--hub-url requires a value")?),
            "--tenant" => tenant_slug = Some(args.next().ok_or("--tenant requires a value")?),
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let hub_url = hub_url.ok_or_else(|| "hub register requires --hub-url <url>".to_string())?;
    let tenant_slug =
        tenant_slug.ok_or_else(|| "hub register requires --tenant <slug>".to_string())?;

    Ok(HubRegisterOptions {
        path,
        hub_url: hub_url.trim_end_matches('/').to_string(),
        tenant_slug,
    })
}

/// Where this store's hub signing identity lives when the OS keystore
/// isn't reachable -- parallel to `filter::DEVICE_IDENTITY_FALLBACK`'s
/// `.wkp/device-identity` for the (unrelated) age encryption identity.
const SIGNING_IDENTITY_FALLBACK: &str = ".wkp/hub-signing-identity";

/// Printed to stdout so a caller can watch progress; kept separate
/// from the final [`HubRegisterSummary`] so `wkp hub register`'s
/// eventual "waiting for approval..." output happens as it happens,
/// not buffered until the whole polling loop finishes.
fn report(message: &str) {
    println!("wkp: {message}");
}

pub(crate) struct HubRegisterSummary {
    pub(crate) tenant_slug: String,
    pub(crate) device_id: i64,
}

impl std::fmt::Display for HubRegisterSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: registered with tenant {} (device id {})",
            self.tenant_slug, self.device_id
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
    error: Option<String>,
}

/// `wkp hub register`: generates (or reuses) this store's ed25519 hub
/// signing identity, requests a device code, prints the human-facing
/// verification instructions, and polls until approved or the grant
/// expires.
pub(crate) fn run_hub_register(opts: &HubRegisterOptions) -> Result<HubRegisterSummary, String> {
    let device_id = wkp_git::sync::device_id(&opts.path)?;
    let identity = wkp_crypto::signing_identity::ensure(
        &device_id,
        &opts.path.join(SIGNING_IDENTITY_FALLBACK),
    )
    .map_err(|e| e.to_string())?;
    let public_key = identity.public_key_openssh().map_err(|e| e.to_string())?;

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = agent.into();

    let code_url = format!("{}/device/code", opts.hub_url);
    let mut code_http_response = agent
        .post(&code_url)
        .send_json(serde_json::json!({
            "tenant_slug": opts.tenant_slug,
            "public_key": public_key,
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
    fn parse_hub_register_args_requires_hub_url_and_tenant() {
        assert!(parse_hub_register_args(args(&[])).is_err());
        assert!(parse_hub_register_args(args(&["--hub-url", "http://localhost:8080"])).is_err());
        assert!(parse_hub_register_args(args(&["--tenant", "myteam"])).is_err());
    }

    #[test]
    fn parse_hub_register_args_reads_all_flags_and_trims_trailing_slash() {
        let opts = parse_hub_register_args(args(&[
            "--hub-url",
            "http://localhost:8080/",
            "--tenant",
            "myteam",
            "--path",
            "/tmp/store",
        ]))
        .expect("parse_hub_register_args");
        assert_eq!(opts.hub_url, "http://localhost:8080");
        assert_eq!(opts.tenant_slug, "myteam");
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn parse_hub_register_args_rejects_unrecognized_flags() {
        assert!(parse_hub_register_args(args(&[
            "--hub-url",
            "http://localhost:8080",
            "--tenant",
            "myteam",
            "--bogus"
        ]))
        .is_err());
    }
}
