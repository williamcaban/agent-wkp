//! The RFC 8628 device-authorization HTTP endpoints (design 6.4, 8.1,
//! M5-2): `POST /device/code`, `POST /device/token`, and the human
//! verification page (`GET`/`POST /verify`).
//!
//! Every handler here is blocking, and stays blocking: it matches
//! `control_plane`'s own design decision not to pull an async runtime
//! into this crate's call sites -- a handful of low-traffic,
//! infrequent-by-nature endpoints (device registration is a
//! once-per-device event, not a hot path) has no real concurrency need
//! an async framework would justify. One Postgres connection per
//! request (via [`control_plane::connect`]), not a pool -- correct and
//! simple at this traffic level; a real connection pool is exactly the
//! kind of production hardening this milestone's own scope notes
//! (`docs/plan/milestones.md`) defer past a CI-testable slice, the
//! same posture already taken for TLS in `control_plane::connect`.
//!
//! M5-8 (ADR-0011): the *front door* no longer listens here. Its
//! listener moved to [`crate::front_door`] (`axum` + `axum-server` on
//! `rustls`, so client-facing TLS is terminated by a mature library
//! rather than something this project hand-rolled); every handler
//! below is unchanged and is called from there on a blocking-pool
//! thread. What stayed: [`serve_single_tenant`], the mode a tenant's
//! own pod runs -- plain HTTP inside the shared podman network, never
//! client-facing (ADR-0009/0010 make the front door the sole
//! TLS-terminating process), so it has nothing to gain from an async
//! TLS stack and keeps `tiny_http`.
//!
//! Both listeners hand a handler the same [`Incoming`]: the request's
//! headers and body, and nothing else, since that is all any handler
//! here ever looked at. That is what lets one set of handlers serve
//! two transports without either one's request type leaking into them.
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

/// One incoming request, reduced to the only two things any handler in
/// this module ever reads: its headers and its body.
///
/// Deliberately transport-independent -- [`serve_single_tenant`]'s
/// `tiny_http` listener and [`crate::front_door`]'s `axum` one both
/// build one of these, so neither framework's request type appears in
/// a handler signature and the handlers themselves are identical
/// across both.
///
/// The body is read up front rather than lazily. That is forced by the
/// async front door (extracting a body is an `await`, and the handlers
/// it calls run on a blocking thread where there is nothing to await
/// on), and it is what any HTTP framework does anyway; the visible
/// consequence is that a request rejected by
/// [`handle_git_http`]'s auth checks has had its body read before the
/// rejection, rather than after.
pub(crate) struct Incoming {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Incoming {
    pub(crate) fn new(headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Incoming { headers, body }
    }

    fn from_tiny_http(request: &mut tiny_http::Request) -> Self {
        let headers = request
            .headers()
            .iter()
            .map(|h| {
                (
                    h.field.as_str().as_str().to_string(),
                    h.value.as_str().to_string(),
                )
            })
            .collect();
        let mut body = Vec::new();
        let _ = request.as_reader().read_to_end(&mut body);
        Incoming { headers, body }
    }

    /// The body as text. Lossy on invalid UTF-8 rather than an error:
    /// the only callers are the JSON and form-encoded endpoints, where
    /// a mangled byte means the parse fails or the database lookup
    /// misses -- both already-handled outcomes, never a panic on
    /// attacker-controlled input.
    fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The exact body bytes -- M5-6's git-over-HTTP routes carry binary
    /// pack data, which a lossy conversion would corrupt the same way
    /// `wkp_git::read_blob`'s own doc comment explains for a private
    /// item's ciphertext.
    fn body_bytes(&self) -> &[u8] {
        &self.body
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }
}

/// A rendered response: status, content type, extra headers, body.
/// Every handler returns one of these rather than touching a
/// framework's own response type directly, so each listener is the
/// only place that needs to know how to actually send one.
pub(crate) struct Rendered {
    pub(crate) status: u16,
    pub(crate) content_type: String,
    /// Anything beyond `Content-Type` -- empty for every JSON/HTML
    /// response this module renders itself; populated when relaying a
    /// [`wkp_git::http_backend::CgiResponse`] (M5-6), whose headers
    /// (`Cache-Control`, `Expires`, ...) come from `git http-backend`
    /// itself, not something this crate decides.
    pub(crate) extra_headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Rendered {
    fn json<T: Serialize>(status: u16, value: &T) -> Self {
        Rendered {
            status,
            content_type: "application/json".to_string(),
            extra_headers: Vec::new(),
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn html(status: u16, body: String) -> Self {
        Rendered {
            status,
            content_type: "text/html; charset=utf-8".to_string(),
            extra_headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    /// M5-6: relays `git http-backend`'s own CGI response verbatim,
    /// splitting out `Content-Type` (this struct's own dedicated
    /// field, matching every other constructor) from the rest.
    fn cgi(response: wkp_git::http_backend::CgiResponse) -> Self {
        let mut content_type = "application/octet-stream".to_string();
        let mut extra_headers = Vec::new();
        for (key, value) in response.headers {
            if key.eq_ignore_ascii_case("Content-Type") {
                content_type = value;
            } else {
                extra_headers.push((key, value));
            }
        }
        Rendered {
            status: response.status,
            content_type,
            extra_headers,
            body: response.body,
        }
    }

    /// The `404` body an unrecognized path gets -- shared by
    /// [`serve_single_tenant`]'s dispatcher and the front door's
    /// (`crate::front_door`), so both listeners answer an unknown route
    /// identically.
    pub(crate) fn not_found() -> Self {
        Rendered::json(
            404,
            &ErrorBody {
                error: "not_found".to_string(),
            },
        )
    }

    /// A bare status code with no body -- `401`/`403` for the bearer-
    /// token checks M5-6's own routes make before ever calling `git
    /// http-backend` at all.
    pub(crate) fn status_only(status: u16) -> Self {
        Rendered {
            status,
            content_type: "text/plain".to_string(),
            extra_headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// Sends a [`Rendered`] response back over `request` -- the one place
/// that needs to know how to actually talk to `tiny_http::Response`,
/// used by [`serve_single_tenant`] (M5-7's own no-auth, one-repo mode a
/// tenant's pod runs). The front door has its own equivalent for
/// `axum` (`crate::front_door::into_response`).
fn respond(request: tiny_http::Request, rendered: Rendered) {
    let status = StatusCode(rendered.status);
    let content_type_header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], rendered.content_type.as_bytes())
            .expect("content-type header value is always valid ASCII/UTF-8");
    let mut response = Response::from_data(rendered.body)
        .with_status_code(status)
        .with_header(content_type_header);
    for (key, value) in &rendered.extra_headers {
        if let Ok(header) = tiny_http::Header::from_bytes(key.as_bytes(), value.as_bytes()) {
            response = response.with_header(header);
        }
    }
    if let Err(e) = request.respond(response) {
        eprintln!("wkp-hub: failed to send response: {e}");
    }
}

/// M5-7 (ADR-0010): the mode a tenant's own pod runs -- serves exactly
/// one fixed tenant's repo via `git http-backend`, with no bearer-token
/// check at all (the front door already made that decision before ever
/// proxying a request here) and no RFC 8628/`/verify` routes (a pod
/// has no reason to run them; those stay the front door's own job).
/// Refuses (404) any request naming a *different* tenant than the one
/// this process was started for -- defense in depth even though the
/// front door should never send one here, the same posture
/// `handle_git_http`'s own tenant-match check takes for the same
/// reason.
pub fn serve_single_tenant(
    port: u16,
    tenant_slug: String,
    repos_root: std::path::PathBuf,
) -> Result<(), String> {
    let server = tiny_http::Server::http(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    eprintln!("wkp-hub: serving tenant {tenant_slug} on http://0.0.0.0:{port}");
    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        let query = url
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();

        let method_str = match method {
            Method::Get => "GET",
            Method::Post => "POST",
            _ => {
                respond(request, Rendered::status_only(405));
                continue;
            }
        };
        let incoming = Incoming::from_tiny_http(&mut request);
        let rendered = match git_http_path(&path) {
            Some((slug, suffix)) if slug == tenant_slug => serve_git_http(
                &incoming,
                method_str,
                &slug,
                &suffix,
                &query,
                &repos_root,
                "pod",
            ),
            _ => Rendered::status_only(404),
        };
        respond(request, rendered);
    }
    Ok(())
}

/// Recognizes `/<tenant-slug>.git/<suffix>` (`info/refs`,
/// `git-upload-pack`, `git-receive-pack`) -- the URL shape a real git
/// client constructs against a `https://.../<tenant-slug>.git` remote,
/// matching `wkp-shell`'s own `<slug>.git` bare-repo naming convention
/// (M5-3/M5-4) so both transports name the same repo the same way.
pub(crate) fn git_http_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix('/')?;
    let (repo, suffix) = rest.split_once('/')?;
    let tenant_slug = repo.strip_suffix(".git")?;
    if tenant_slug.is_empty() || suffix.is_empty() {
        return None;
    }
    Some((tenant_slug.to_string(), suffix.to_string()))
}

/// M5-6 (design 8.1): validates a device-scoped bearer token against
/// the control plane, checks it names *this* URL's own tenant (a
/// token valid for one tenant must never reach another tenant's repo,
/// even though both live under the same `repos_root` -- the same
/// never-trust-the-client's-own-path posture `wkp-shell`'s SSH path
/// already takes, M5-3), and only then hands the request off to `git
/// http-backend` (`wkp_git::http_backend`, this crate's own plumbing
/// for it).
pub(crate) fn handle_git_http(
    request: &Incoming,
    method: &str,
    tenant_slug: &str,
    suffix: &str,
    query: &str,
    repos_root: &std::path::Path,
    image: &str,
) -> Rendered {
    let method_str = match method {
        "GET" => "GET",
        "POST" => "POST",
        _ => return Rendered::status_only(405),
    };

    let Some(token) = request
        .header("Authorization")
        .and_then(|v| v.strip_prefix("Bearer ").map(|t| t.trim().to_string()))
    else {
        return Rendered::status_only(401);
    };

    let mut client = match control_plane::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wkp-hub: git-http: control plane connect failed: {e}");
            return Rendered::status_only(500);
        }
    };
    let device = match control_plane::find_device_by_bearer_token(&mut client, &token) {
        Ok(Some(d)) => d,
        Ok(None) => return Rendered::status_only(401),
        Err(e) => {
            eprintln!("wkp-hub: git-http: bearer token lookup failed: {e}");
            return Rendered::status_only(500);
        }
    };
    if device.revoked_at.is_some() {
        return Rendered::status_only(401);
    }
    let tenant = match control_plane::find_tenant_by_slug(&mut client, tenant_slug) {
        Ok(Some(t)) => t,
        Ok(None) => return Rendered::status_only(404),
        Err(e) => {
            eprintln!("wkp-hub: git-http: tenant lookup failed: {e}");
            return Rendered::status_only(500);
        }
    };
    if device.tenant_id != tenant.id {
        // Deliberately the same 404 an unknown repo gets, not 403 --
        // confirming "this repo exists, you're just not allowed to
        // see it" to an unauthenticated-for-it caller is its own small
        // information leak, the same reasoning a lot of real git hosts
        // apply to private-repo 404s.
        return Rendered::status_only(404);
    }
    // M5-7 (ADR-0010): the reaper's own idle-timeout decision
    // (`find_idle_running_tenants`) reads this -- best-effort, a
    // failure here must never block serving the actual request.
    if let Err(e) = control_plane::touch_last_active(&mut client, tenant.id) {
        eprintln!("wkp-hub: git-http: touch_last_active failed (non-fatal): {e}");
    }

    // M5-7 (ADR-0009/0010): the front door never runs `git http-backend`
    // itself -- it proxies to the resolved tenant's own pod over the
    // shared network, starting it first if this control plane's own
    // bookkeeping ([`control_plane::Tenant::pod_running`]) says it
    // isn't already up. Simplest cold-start policy that's actually
    // correct (ADR-0010 explicitly left this undecided): retry the
    // proxy attempt a few times with a short wait rather than queueing
    // or failing the first request outright -- a fresh container needs
    // a moment to actually start listening.
    if !tenant.pod_running {
        if let Err(e) = crate::tenant_pod::start_pod(image, repos_root, tenant_slug) {
            eprintln!("wkp-hub: git-http: failed to start tenant pod: {e}");
            return Rendered::status_only(503);
        }
        if let Err(e) = control_plane::record_pod_started(&mut client, tenant.id) {
            eprintln!("wkp-hub: git-http: pod started but failed to record it: {e}");
        }
    }

    let content_type = request.header("Content-Type");
    proxy_to_tenant_pod(
        tenant_slug,
        suffix,
        query,
        method_str,
        content_type.as_deref(),
        request.body_bytes(),
    )
}

/// Proxies one git-http request to `tenant_slug`'s own pod, over the
/// shared user-defined network ([`crate::tenant_pod::NETWORK_NAME`]),
/// reached by its container-runtime DNS alias -- never
/// `podman exec`/local execution (ADR-0009's own decision, and the
/// whole reason a pod's isolation actually holds). A handful of short
/// retries: a pod [`handle_git_http`] just cold-started needs a moment
/// to actually be listening; see that function's own comment on why
/// this simple retry, not a request queue, is this PR's answer to
/// ADR-0010's explicitly undecided cold-start question.
fn proxy_to_tenant_pod(
    tenant_slug: &str,
    suffix: &str,
    query: &str,
    method_str: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Rendered {
    let url = format!(
        "http://{tenant_slug}:{}/{tenant_slug}.git/{suffix}?{query}",
        crate::tenant_pod::SERVE_PORT
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();

    let mut last_err = String::new();
    for attempt in 0..10 {
        let result = match method_str {
            "GET" => {
                let mut builder = agent.get(&url);
                if let Some(ct) = content_type {
                    builder = builder.header("Content-Type", ct);
                }
                builder.call()
            }
            "POST" => {
                let mut builder = agent.post(&url);
                if let Some(ct) = content_type {
                    builder = builder.header("Content-Type", ct);
                }
                builder.send(body)
            }
            _ => return Rendered::status_only(405),
        };
        match result {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let mut content_type = "application/octet-stream".to_string();
                let mut extra_headers = Vec::new();
                for (name, value) in response.headers().iter() {
                    let Ok(v) = value.to_str() else { continue };
                    if name.as_str().eq_ignore_ascii_case("content-type") {
                        content_type = v.to_string();
                    } else {
                        extra_headers.push((name.to_string(), v.to_string()));
                    }
                }
                let body = response.body_mut().read_to_vec().unwrap_or_default();
                return Rendered {
                    status,
                    content_type,
                    extra_headers,
                    body,
                };
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    eprintln!("wkp-hub: git-http: proxy to tenant {tenant_slug}'s pod failed: {last_err}");
    Rendered::status_only(502)
}

/// The actual `git http-backend` call, shared by [`handle_git_http`]
/// (the front door, after its own auth checks -- passes the resolved
/// device's own identity as `remote_user`) and [`serve_single_tenant`]
/// (a tenant's own pod, which has no device to attribute a request to
/// at all -- the front door already made that decision before ever
/// proxying here).
fn serve_git_http(
    request: &Incoming,
    method_str: &str,
    tenant_slug: &str,
    suffix: &str,
    query: &str,
    repos_root: &std::path::Path,
    remote_user: &str,
) -> Rendered {
    let content_type = request.header("Content-Type");
    let path_info = format!("/{tenant_slug}.git/{suffix}");
    let cgi_request = wkp_git::http_backend::CgiRequest {
        method: method_str,
        path_info: &path_info,
        query_string: query,
        content_type: content_type.as_deref(),
        remote_user,
        body: request.body_bytes(),
    };

    match wkp_git::http_backend::run_http_backend(repos_root, &cgi_request) {
        Ok(response) => Rendered::cgi(response),
        Err(e) => {
            eprintln!("wkp-hub: git-http: http-backend failed: {e}");
            Rendered::status_only(500)
        }
    }
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
pub(crate) fn handle_device_code(request: &Incoming) -> Rendered {
    let body = request.body_string();
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
pub(crate) fn handle_device_token(request: &Incoming) -> Rendered {
    let body = request.body_string();
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
pub(crate) fn handle_verify_page(query: &str) -> Rendered {
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
pub(crate) fn handle_verify_submit(request: &Incoming) -> Rendered {
    let body = request.body_string();
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
    use std::net::TcpListener;

    fn unique_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{nanos}")
    }

    /// A `ureq` agent configured to hand back every response as `Ok`,
    /// regardless of HTTP status -- this module's own endpoints use
    /// ordinary 4xx/5xx status codes for real, expected outcomes, not
    /// just unexpected failures, so tests need to read those bodies
    /// rather than treat every non-2xx as a Rust `Err` to unwrap
    /// around.
    fn test_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into()
    }

    /// Test-only exception to CLAUDE.md's "no `Command::new("git")`
    /// outside `wkp-git`", the same documented pattern
    /// `wkp-cli/tests/encryption_filter.rs` and `wkp-hub`'s own
    /// `tenant_repo.rs`/`post_receive_hook.rs` tests already use for
    /// real end-to-end git wiring.
    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git") // nosemgrep: rust.lang.security.command-injection.command-injection
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
        assert!(status.success(), "git {args:?} failed: {status:?}");
    }

    /// The URL shape both listeners route on, exercised directly --
    /// M5-8 moved the front door's dispatcher to `crate::front_door`,
    /// so this parser is now shared rather than owned by one of them.
    #[test]
    fn git_http_path_recognizes_a_real_git_remote_url() {
        assert_eq!(
            git_http_path("/acme.git/info/refs"),
            Some(("acme".to_string(), "info/refs".to_string()))
        );
        assert_eq!(
            git_http_path("/acme.git/git-upload-pack"),
            Some(("acme".to_string(), "git-upload-pack".to_string()))
        );
        assert_eq!(git_http_path("/verify"), None);
        assert_eq!(git_http_path("/device/code"), None);
        assert_eq!(git_http_path("/acme.git/"), None);
    }

    /// M5-7 (ADR-0010): the mode a tenant's own pod runs -- serves its
    /// one fixed repo with no bearer-token check at all (the front door
    /// already made that decision before ever proxying here).
    #[test]
    fn serve_single_tenant_serves_its_one_configured_tenant_with_no_auth_needed() {
        let repos_root = tempfile::Builder::new()
            .prefix("wkp-hub-http-single-tenant-test-repos-")
            .tempdir()
            .expect("temp dir")
            .keep();
        let tenant_slug = unique_slug("single-tenant");
        wkp_git::init_bare_repo(&repos_root.join(format!("{tenant_slug}.git")))
            .expect("init_bare_repo");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        let slug_for_thread = tenant_slug.clone();
        std::thread::spawn(move || {
            let _ = serve_single_tenant(port, slug_for_thread, repos_root);
        });

        let remote = format!("http://127.0.0.1:{port}/{tenant_slug}.git");
        let client_dir = tempfile::Builder::new()
            .prefix("wkp-hub-http-single-tenant-test-client-")
            .tempdir()
            .expect("temp dir");
        run_git(client_dir.path(), &["init", "--quiet", "-b", "main"]);
        std::fs::write(
            client_dir.path().join("shared.md"),
            "---\nvisibility: shared\n---\n\nno auth needed here\n",
        )
        .expect("write shared.md");
        run_git(client_dir.path(), &["add", "-A"]);
        run_git(
            client_dir.path(),
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "single-tenant test",
            ],
        );

        // No `-c http.extraHeader=Authorization: ...` at all -- this is
        // the whole point of this mode; retried a few times since the
        // server thread above needs a moment to actually bind.
        let mut last_err = None;
        for attempt in 0..20 {
            match std::process::Command::new("git")
                .current_dir(client_dir.path())
                .args(["push", "--quiet", &remote, "main"])
                .output()
            {
                Ok(output) if output.status.success() => {
                    last_err = None;
                    break;
                }
                Ok(output) => last_err = Some(String::from_utf8_lossy(&output.stderr).into_owned()),
                Err(e) => last_err = Some(e.to_string()),
            }
            if attempt < 19 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        assert!(last_err.is_none(), "push failed: {last_err:?}");
    }

    #[test]
    fn serve_single_tenant_refuses_a_request_for_a_different_tenant() {
        let repos_root = tempfile::Builder::new()
            .prefix("wkp-hub-http-single-tenant-wrong-tenant-repos-")
            .tempdir()
            .expect("temp dir")
            .keep();
        let configured_slug = unique_slug("single-tenant-configured");
        let other_slug = unique_slug("single-tenant-other");
        wkp_git::init_bare_repo(&repos_root.join(format!("{other_slug}.git")))
            .expect("init_bare_repo");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        std::thread::spawn(move || {
            let _ = serve_single_tenant(port, configured_slug, repos_root);
        });

        let agent = test_agent();
        let url =
            format!("http://127.0.0.1:{port}/{other_slug}.git/info/refs?service=git-upload-pack");
        let mut response = None;
        for attempt in 0..20 {
            match agent.get(&url).call() {
                Ok(r) => {
                    response = Some(r);
                    break;
                }
                Err(_) if attempt < 19 => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => panic!("request never succeeded: {e}"),
            }
        }
        assert_eq!(
            response.expect("got a response").status(),
            404,
            "a pod configured for one tenant must refuse requests naming a different one"
        );
    }
}
