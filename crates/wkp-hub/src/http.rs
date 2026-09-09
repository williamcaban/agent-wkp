//! The RFC 8628 device-authorization HTTP endpoints (design 6.4, 8.1,
//! M5-2): `POST /device/code`, `POST /device/token`, and the human
//! verification page (`GET`/`POST /verify`).
//!
//! `tiny_http`, not an async web framework: matches `control_plane`'s
//! own design decision to stay blocking rather than pull an async
//! runtime into this crate's call sites -- a handful of low-traffic,
//! infrequent-by-nature endpoints (device registration is a
//! once-per-device event, not a hot path) has no real concurrency need
//! an async framework would justify. One Postgres connection per
//! request (via [`control_plane::connect`]), not a pool -- correct and
//! simple at this traffic level; a real connection pool is exactly the
//! kind of production hardening this milestone's own scope notes
//! (`docs/plan/milestones.md`) defer past a CI-testable slice, the
//! same posture already taken for TLS in `control_plane::connect`.
//!
//! `/device/code` and `/device/token` speak JSON (RFC 8628's own wire
//! format, and this is a CLI-to-server exchange, not a browser).
//! `/verify` is the one human-facing page in this module: a bare HTML
//! form (design 3.3 explicitly keeps the real account/billing web app
//! out of this repo; this is the bare minimum needed for this
//! milestone's own CI-testable flow, not that eventual product) POSTing
//! standard `application/x-www-form-urlencoded`, the same as any plain
//! HTML form without JavaScript would.

use crate::control_plane::{self, grants};
use serde::{Deserialize, Serialize};
use tiny_http::{Method, Response, StatusCode};

pub fn serve(port: u16) -> Result<(), String> {
    let server = tiny_http::Server::http(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    eprintln!("wkp-hub: listening on http://0.0.0.0:{port}");
    for request in server.incoming_requests() {
        handle(request);
    }
    Ok(())
}

/// A rendered response: status, content type, body. Every handler
/// returns one of these rather than touching `tiny_http::Response`
/// directly, so the dispatcher is the only place that needs to know
/// how to actually send one.
struct Rendered {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Rendered {
    fn json<T: Serialize>(status: u16, value: &T) -> Self {
        Rendered {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn html(status: u16, body: String) -> Self {
        Rendered {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
        }
    }
}

fn handle(mut request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();

    let rendered = match (&method, path.as_str()) {
        (Method::Post, "/device/code") => handle_device_code(&mut request),
        (Method::Post, "/device/token") => handle_device_token(&mut request),
        (Method::Get, "/verify") => handle_verify_page(&query),
        (Method::Post, "/verify") => handle_verify_submit(&mut request),
        _ => Rendered::json(
            404,
            &ErrorBody {
                error: "not_found".to_string(),
            },
        ),
    };

    let status = StatusCode(rendered.status);
    let header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], rendered.content_type.as_bytes())
            .expect("static content-type header is always valid");
    let response = Response::from_data(rendered.body)
        .with_status_code(status)
        .with_header(header);
    if let Err(e) = request.respond(response) {
        eprintln!("wkp-hub: failed to send response: {e}");
    }
}

fn read_body(request: &mut tiny_http::Request) -> String {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    body
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Deserialize)]
struct DeviceCodeRequest {
    tenant_slug: String,
    public_key: String,
}

#[derive(Serialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: i64,
    interval: i64,
}

/// `POST /device/code` (RFC 8628 §3.1/§3.2): the polling client's own
/// public key and its chosen tenant are both already decided by the
/// client (see the module doc comment and `grants`'s own doc comment
/// for why) -- this just persists the grant and hands back the codes.
fn handle_device_code(request: &mut tiny_http::Request) -> Rendered {
    let body = read_body(request);
    let parsed: DeviceCodeRequest = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            return Rendered::json(
                400,
                &ErrorBody {
                    error: format!("invalid request body: {e}"),
                },
            )
        }
    };

    let mut client = match control_plane::connect() {
        Ok(c) => c,
        Err(e) => {
            return Rendered::json(
                500,
                &ErrorBody {
                    error: e.to_string(),
                },
            )
        }
    };
    let tenant = match control_plane::find_tenant_by_slug(&mut client, &parsed.tenant_slug) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Rendered::json(
                404,
                &ErrorBody {
                    error: "invalid_tenant".to_string(),
                },
            )
        }
        Err(e) => {
            return Rendered::json(
                500,
                &ErrorBody {
                    error: e.to_string(),
                },
            )
        }
    };

    match grants::create_device_grant(&mut client, tenant.id, &parsed.public_key) {
        Ok(grant) => Rendered::json(
            200,
            &DeviceCodeResponse {
                device_code: grant.device_code,
                user_code: grant.user_code,
                verification_uri: "/verify".to_string(),
                expires_in: grants::GRANT_TTL.whole_seconds(),
                interval: grants::POLL_INTERVAL_SECONDS,
            },
        ),
        Err(e) => Rendered::json(
            500,
            &ErrorBody {
                error: e.to_string(),
            },
        ),
    }
}

#[derive(Deserialize)]
struct TokenRequest {
    device_code: String,
}

#[derive(Serialize)]
struct TokenSuccessResponse {
    status: String,
    tenant_slug: String,
    device_id: i64,
}

/// `POST /device/token` (RFC 8628 §3.4/§3.5): `authorization_pending`
/// (RFC 8628's own error code) until a human has approved via
/// `/verify`; `expired_token` (also RFC 8628's own) once the grant's
/// TTL has passed with no approval; a real result once approved.
fn handle_device_token(request: &mut tiny_http::Request) -> Rendered {
    let body = read_body(request);
    let parsed: TokenRequest = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            return Rendered::json(
                400,
                &ErrorBody {
                    error: format!("invalid request body: {e}"),
                },
            )
        }
    };

    let mut client = match control_plane::connect() {
        Ok(c) => c,
        Err(e) => {
            return Rendered::json(
                500,
                &ErrorBody {
                    error: e.to_string(),
                },
            )
        }
    };
    let grant = match grants::find_grant_by_device_code(&mut client, &parsed.device_code) {
        Ok(Some(g)) => g,
        Ok(None) => {
            return Rendered::json(
                400,
                &ErrorBody {
                    error: "invalid_grant".to_string(),
                },
            )
        }
        Err(e) => {
            return Rendered::json(
                500,
                &ErrorBody {
                    error: e.to_string(),
                },
            )
        }
    };

    if grant.is_expired() {
        return Rendered::json(
            400,
            &ErrorBody {
                error: "expired_token".to_string(),
            },
        );
    }
    let Some(device_id) = grant.approved_device_id else {
        return Rendered::json(
            400,
            &ErrorBody {
                error: "authorization_pending".to_string(),
            },
        );
    };
    let tenant_slug = match control_plane::find_tenant_by_id(&mut client, grant.tenant_id) {
        Ok(Some(tenant)) => tenant.slug,
        Ok(None) => {
            return Rendered::json(
                500,
                &ErrorBody {
                    error: "grant references a tenant that no longer exists".to_string(),
                },
            )
        }
        Err(e) => {
            return Rendered::json(
                500,
                &ErrorBody {
                    error: e.to_string(),
                },
            )
        }
    };

    Rendered::json(
        200,
        &TokenSuccessResponse {
            status: "approved".to_string(),
            tenant_slug,
            device_id,
        },
    )
}

/// `GET /verify?user_code=...`: a bare HTML form, prefilled with
/// `user_code` from the query string if the device flow's own
/// `verification_uri_complete` convention (RFC 8628 §3.3.1) supplied
/// one -- entirely optional, a human can also type the code by hand.
fn handle_verify_page(query: &str) -> Rendered {
    let prefilled = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("user_code="))
        .map(percent_decode)
        .unwrap_or_default();

    Rendered::html(
        200,
        format!(
            "<!doctype html><html><head><title>wkp hub: approve device</title></head><body>\
             <h1>Approve this device</h1>\
             <p>Enter the code shown on the device you're registering.</p>\
             <form method=\"post\" action=\"/verify\">\
             <input type=\"text\" name=\"user_code\" value=\"{}\" autofocus>\
             <button type=\"submit\">Approve</button>\
             </form></body></html>",
            html_escape(&prefilled)
        ),
    )
}

/// `POST /verify`: a plain HTML form submission
/// (`application/x-www-form-urlencoded`), the same shape a human's
/// browser sends with no JavaScript involved -- the actual approval
/// step ([`grants::approve_grant`]) is exactly the same call whether
/// this body came from a real browser or (this task's own acceptance
/// criterion) a scripted stand-in for one.
fn handle_verify_submit(request: &mut tiny_http::Request) -> Rendered {
    let body = read_body(request);
    let Some(user_code) = body
        .split('&')
        .find_map(|pair| pair.strip_prefix("user_code="))
        .map(percent_decode)
    else {
        return Rendered::html(400, "<p>missing user_code</p>".to_string());
    };

    let mut client = match control_plane::connect() {
        Ok(c) => c,
        Err(e) => return Rendered::html(500, format!("<p>{}</p>", html_escape(&e.to_string()))),
    };
    let grant = match grants::find_grant_by_user_code(&mut client, &user_code) {
        Ok(Some(g)) => g,
        Ok(None) => return Rendered::html(404, "<p>unknown or expired code</p>".to_string()),
        Err(e) => return Rendered::html(500, format!("<p>{}</p>", html_escape(&e.to_string()))),
    };
    if grant.is_expired() {
        return Rendered::html(400, "<p>this code has expired</p>".to_string());
    }

    match grants::approve_grant(&mut client, &grant) {
        Ok(_device) => Rendered::html(
            200,
            "<!doctype html><html><body><h1>Device approved</h1>\
             <p>You can close this page and return to your device.</p></body></html>"
                .to_string(),
        ),
        Err(e) => Rendered::html(500, format!("<p>{}</p>", html_escape(&e.to_string()))),
    }
}

/// A minimal `application/x-www-form-urlencoded` value decoder: `+` is
/// a space, `%XX` is a byte -- the two escapes this module's own form
/// ever produces, not a general-purpose decoder. Any malformed `%XX`
/// sequence is left as-is rather than erroring: this only ever feeds a
/// database lookup by exact string match, so a decode glitch just
/// means "no such code" (an ordinary, already-handled outcome), never
/// a parser panic on attacker-controlled input.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::create_tenant;
    use std::net::TcpListener;

    fn unique_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{nanos}")
    }

    /// Starts a real server on an OS-assigned free port, in a
    /// background thread, and returns its base URL -- every test in
    /// this module drives real HTTP requests against it, not handler
    /// functions called directly, so a test failure here is evidence
    /// the actual wire protocol works, not just the Rust functions
    /// behind it.
    fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener); // release it so tiny_http can bind the same port itself

        std::thread::spawn(move || {
            let _ = serve(port);
        });
        // tiny_http's Server::http binds synchronously before returning,
        // but `serve` doesn't hand that moment back to this thread -- a
        // short, bounded retry loop on the first real request (below)
        // covers the brief window before the listener is actually up,
        // rather than a blind sleep.
        format!("http://127.0.0.1:{port}")
    }

    /// A `ureq` agent configured to hand back every response as `Ok`,
    /// regardless of HTTP status -- this module's own endpoints use
    /// ordinary 4xx/5xx status codes for real, expected outcomes
    /// (`authorization_pending`, an unknown tenant, ...), not just
    /// unexpected failures, so tests need to read those bodies the
    /// same way a real `wkp hub register` polling loop would, not
    /// treat every non-2xx as a Rust `Err` to unwrap around.
    fn test_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn get_with_retry(agent: &ureq::Agent, url: &str) {
        for attempt in 0..20 {
            match agent.get(url).call() {
                Ok(_) => return,
                Err(_) if attempt < 19 => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => panic!("request to {url} never succeeded: {e}"),
            }
        }
    }

    /// M5-2's own acceptance criterion: a scripted stand-in for the
    /// human approval step (a direct HTTP POST to `/verify`) drives a
    /// full register round trip -- device-code request, a still-pending
    /// poll, the scripted "approval", then a successful poll -- against
    /// a real running server and a real Postgres database.
    #[test]
    fn full_device_registration_round_trip_over_real_http() {
        let base = start_test_server();
        let agent = test_agent();
        // Block until the server is actually accepting connections (see
        // start_test_server's own comment).
        get_with_retry(&agent, &format!("{base}/verify"));

        let mut client = control_plane::connect().expect("connect (seeding the tenant directly)");
        let tenant = create_tenant(&mut client, &unique_slug("http-round-trip")).expect("tenant");
        let public_key = unique_slug("ssh-ed25519 AAAA...http-round-trip");

        let mut code_http_response = agent
            .post(format!("{base}/device/code"))
            .send_json(serde_json::json!({
                "tenant_slug": tenant.slug,
                "public_key": public_key,
            }))
            .expect("POST /device/code");
        assert_eq!(code_http_response.status(), 200);
        let code_response: serde_json::Value = code_http_response
            .body_mut()
            .read_json()
            .expect("parse device/code response");
        let device_code = code_response["device_code"]
            .as_str()
            .expect("device_code")
            .to_string();
        let user_code = code_response["user_code"]
            .as_str()
            .expect("user_code")
            .to_string();
        assert!(code_response["verification_uri"].as_str().is_some());
        assert!(code_response["interval"].as_i64().unwrap() > 0);

        // Not yet approved: polling now must report authorization_pending.
        let mut pending_http_response = agent
            .post(format!("{base}/device/token"))
            .send_json(serde_json::json!({ "device_code": device_code }))
            .expect("POST /device/token before approval");
        assert_eq!(pending_http_response.status(), 400);
        let pending_body: serde_json::Value = pending_http_response
            .body_mut()
            .read_json()
            .expect("parse pending response");
        assert_eq!(pending_body["error"], "authorization_pending");

        // The scripted stand-in for the human clicking "approve": a
        // direct, real HTTP POST to /verify, exactly like a plain HTML
        // form (no JavaScript) would send.
        let verify_response = agent
            .post(format!("{base}/verify"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(format!("user_code={user_code}"))
            .expect("POST /verify");
        assert_eq!(verify_response.status(), 200);

        // Now polling must report success.
        let mut approved_http_response = agent
            .post(format!("{base}/device/token"))
            .send_json(serde_json::json!({ "device_code": device_code }))
            .expect("POST /device/token after approval");
        assert_eq!(approved_http_response.status(), 200);
        let approved_body: serde_json::Value = approved_http_response
            .body_mut()
            .read_json()
            .expect("parse token response");
        assert_eq!(approved_body["status"], "approved");
        assert_eq!(approved_body["tenant_slug"], tenant.slug);

        // The real, load-bearing assertion: the device actually landed
        // in the control plane, under the right tenant, from the
        // registered public key -- not just that the HTTP responses
        // looked right.
        let registered = control_plane::find_device_by_public_key(&mut client, &public_key)
            .expect("find_device_by_public_key")
            .expect("device must actually be registered");
        assert_eq!(registered.tenant_id, tenant.id);
        assert!(registered.revoked_at.is_none());
    }

    #[test]
    fn device_code_request_for_an_unknown_tenant_is_refused() {
        let base = start_test_server();
        let agent = test_agent();
        get_with_retry(&agent, &format!("{base}/verify"));

        let response = agent
            .post(format!("{base}/device/code"))
            .send_json(serde_json::json!({
                "tenant_slug": unique_slug("never-created-tenant"),
                "public_key": unique_slug("ssh-ed25519 AAAA...unknown-tenant"),
            }))
            .expect("POST /device/code");
        assert_eq!(response.status(), 404, "an unknown tenant must not succeed");
    }
}
