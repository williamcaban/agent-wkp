//! M5-8 (ADR-0011): the hub's front door -- the one process that
//! terminates client-facing TLS (ADR-0009/0010) and routes to the
//! handlers in [`crate::http`].
//!
//! **Why `axum`/`axum-server`/`tokio` here when the rest of this crate
//! is deliberately synchronous.** Everything else in `wkp-hub` blocks
//! on purpose: `postgres`, `wkp-git`'s subprocesses, `ureq` to a tenant
//! pod, and `tiny_http` for M5-2's endpoints. None of that changes --
//! `crate::http`'s handlers are still blocking functions, called here
//! on `tokio`'s blocking pool, and `serve-tenant` (a tenant's own pod,
//! plain HTTP inside the shared podman network) still runs on
//! `tiny_http` untouched.
//!
//! What changed is only this listener, and only because of what has to
//! hang off it. #124 needs a custom `rustls` `ClientCertVerifier` on
//! the front door's own acceptor. `tiny_http`'s bundled TLS feature is
//! pinned to rustls 0.20, which has no usable client-verifier API, so
//! keeping it would have meant writing a TLS-terminating proxy in
//! front of it by hand -- novel, security-critical code, in a project
//! whose whole job is other people's private knowledge documents. A
//! mature, heavily-audited library doing exactly this job is the
//! better trade: CLAUDE.md's priority 2 (security) and long-term
//! maintainability over priority 3 (slim core), decided explicitly
//! rather than by drift, and scoped to one listener.
//!
//! Routing is deliberately the same shape [`crate::http`]'s old
//! dispatcher had: the four RFC 8628/`/verify` routes by exact path,
//! and everything else through [`crate::http::git_http_path`], so a
//! request that reached `handle_git_http` before still does.

use crate::http::{self, Incoming, Rendered};
use crate::hub_ca::HubCa;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use rustls_pki_types::CertificateDer;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tower_layer::Layer;

/// The most of a request body the front door will buffer. `tiny_http`
/// read bodies unbounded before this; a cap is strictly better against
/// an unauthenticated client, and 1 GiB is far past any realistic
/// `git push` this store produces (design 4.3's own size targets are
/// orders of magnitude below it).
const MAX_BODY_BYTES: usize = 1024 * 1024 * 1024;

struct FrontDoorState {
    repos_root: PathBuf,
    image: String,
    /// M5-9 (ADR-0011): `device_token`/`verify_submit` need the CA to
    /// sign a device's CSR (`verify_submit`) and to hand back the root
    /// for pinning (`device_token`). An owned copy, not a reference --
    /// this state outlives `serve_on`'s own stack frame for as long as
    /// the server runs. `HubCa` derives `Clone` for exactly this.
    ca: HubCa,
    /// ADR-0012: resolved once when the server starts, not per request
    /// -- `handle_git_http`'s on-demand cold-start path (`git_http`
    /// below) is the only thing that ever calls it.
    orchestrator: Arc<dyn crate::tenant_pod::PodOrchestrator>,
}

/// Run the front door on `port`, terminating TLS with a certificate
/// minted by the hub's own CA. Blocks until the server stops.
pub fn serve(
    port: u16,
    ca: &HubCa,
    repos_root: PathBuf,
    image: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
    eprintln!("wkp-hub: listening on https://0.0.0.0:{port}");
    serve_on(listener, ca, repos_root, image)
}

/// Like [`serve`], but on a listener the caller already bound -- which
/// is how a test gets an OS-assigned free port *and* knows its number
/// before the server starts accepting on it.
pub fn serve_on(
    listener: std::net::TcpListener,
    ca: &HubCa,
    repos_root: PathBuf,
    image: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tls = (*ca.server_tls_config()?).clone();
    // `RustlsConfig::from_config` does not set ALPN for us (its own
    // doc comment says so). One protocol, advertised honestly: this
    // listener serves HTTP/1.1.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls = RustlsConfig::from_config(Arc::new(tls));

    let orchestrator: Arc<dyn crate::tenant_pod::PodOrchestrator> =
        Arc::from(crate::tenant_pod::orchestrator()?);
    let app = router(repos_root, image, ca.clone(), orchestrator);
    // M5-10 (ADR-0011): wraps the plain `RustlsAcceptor` to pull the
    // peer certificate (if any) out of each connection's completed TLS
    // handshake and inject it as a request extension -- see
    // `PeerCertAcceptor`'s own doc comment for why this, rather than
    // `handle_git_http` reaching for it some other way, is where that
    // has to happen.
    let acceptor = PeerCertAcceptor::new(RustlsAcceptor::new(tls));

    // `enable_all` rather than a hand-picked reactor set: `axum-server`
    // needs both the I/O driver (for the listener) and the timer.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // Tokio requires a non-blocking listener when adopting one from
        // `std`; `axum_server::from_tcp` hands it straight to
        // `tokio::net::TcpListener::from_std`.
        listener.set_nonblocking(true)?;
        axum_server::from_tcp(listener)?
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
    })?;
    Ok(())
}

/// The end-entity (leaf) certificate a client presented during the TLS
/// handshake, if any -- `None` for a connection that completed the
/// handshake with no certificate at all, which
/// [`RevocationAwareClientCertVerifier`](crate::hub_ca) permits (client
/// auth is requested but not mandatory at the TLS layer; see that
/// type's own doc comment). Injected once per connection by
/// [`PeerCertAcceptor`], read by [`git_http`] via axum's `Extension`
/// extractor.
///
/// A certificate that *is* present here was already fully verified --
/// chain, expiry, and the control plane's `revoked_at` state -- by the
/// time the handshake completed; nothing downstream needs to re-check
/// any of that, only which device it names.
#[derive(Clone)]
struct PeerCertificate(Option<CertificateDer<'static>>);

/// Wraps [`RustlsAcceptor`] to extract the peer certificate from each
/// connection's completed handshake and inject it as a request
/// extension every request on that connection can read.
///
/// **Why here, and not inside a handler or the `ClientCertVerifier`
/// itself.** `rustls`'s `ClientCertVerifier` (`crate::hub_ca`) runs
/// during the handshake and has no way to hand data forward to the
/// HTTP layer -- it can only accept or reject the connection.
/// `axum`/`axum-server` expose the verified peer certificate on the
/// underlying `tokio_rustls::server::TlsStream` (`stream.get_ref().1`),
/// which is only reachable at the point a connection is accepted, not
/// from inside a `Router` handler. This acceptor is that point: it
/// reads the certificate once per connection (not once per request --
/// an HTTP/1.1 keep-alive connection can carry several requests) and
/// wraps the per-connection `Service` with an [`Extension`] layer so
/// every request downstream sees the same value, the pattern
/// `axum-server`'s own `rustls_session` example documents for exactly
/// this class of "need data from the TLS layer in a handler" need.
#[derive(Clone)]
struct PeerCertAcceptor {
    inner: RustlsAcceptor,
}

impl PeerCertAcceptor {
    fn new(inner: RustlsAcceptor) -> Self {
        PeerCertAcceptor { inner }
    }
}

impl<I, S> Accept<I, S> for PeerCertAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = tokio_rustls::server::TlsStream<I>;
    type Service = axum::middleware::AddExtension<S, PeerCertificate>;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::io::Result<(Self::Stream, Self::Service)>> + Send,
        >,
    >;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (stream, service) = inner.accept(stream, service).await?;
            let peer_cert = PeerCertificate(
                stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|certs| certs.first())
                    .cloned(),
            );
            let service = Extension(peer_cert).layer(service);
            Ok((stream, service))
        })
    }
}

/// The front door's routes. Public to this crate so a test can drive
/// the same router the real `serve` does, rather than a stand-in.
fn router(
    repos_root: PathBuf,
    image: String,
    ca: HubCa,
    orchestrator: Arc<dyn crate::tenant_pod::PodOrchestrator>,
) -> Router {
    let state = Arc::new(FrontDoorState {
        repos_root,
        image,
        ca,
        orchestrator,
    });
    Router::new()
        .route("/device/code", post(device_code))
        .route("/device/token", post(device_token))
        .route("/verify", get(verify_page).post(verify_submit))
        // Everything else goes to the git-smart-HTTP path, which
        // answers 404 itself for a URL that isn't `<tenant>.git/<...>`
        // -- the same precedence the old `tiny_http` dispatcher had,
        // where the git match was tried first and the four static
        // routes above never collided with it.
        .fallback(git_http)
        .with_state(state)
}

/// Splits an `axum` request into the headers-and-body view
/// [`crate::http`]'s handlers take, plus the path and query string the
/// dispatcher needs.
async fn split(request: Request) -> Result<(String, String, String, Incoming), Box<Response>> {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("wkp-hub: front door: reading a request body failed: {e}");
            return Err(Box::new(into_response(Rendered::status_only(413))));
        }
    };
    Ok((
        method,
        path,
        query,
        Incoming::new(headers, Bytes::into(body)),
    ))
}

/// Runs one of [`crate::http`]'s blocking handlers on `tokio`'s
/// blocking pool. Every handler there talks to Postgres, spawns
/// `git http-backend`, or makes a `ureq` call to a tenant pod -- none
/// of which may run on an async worker thread, and none of which is
/// being converted to async (see the module doc).
async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Rendered + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(rendered) => into_response(rendered),
        Err(e) => {
            eprintln!("wkp-hub: front door: handler panicked or was cancelled: {e}");
            into_response(Rendered::status_only(500))
        }
    }
}

fn into_response(rendered: Rendered) -> Response {
    let mut builder = Response::builder()
        .status(rendered.status)
        .header("Content-Type", rendered.content_type);
    for (key, value) in &rendered.extra_headers {
        builder = builder.header(key, value);
    }
    builder
        .body(axum::body::Body::from(rendered.body))
        .unwrap_or_else(|e| {
            // Only reachable if `git http-backend` handed back a header
            // name or value HTTP itself rejects; answering 500 beats
            // relaying something malformed.
            eprintln!("wkp-hub: front door: could not build a response: {e}");
            Response::builder()
                .status(500)
                .body(axum::body::Body::empty())
                .expect("an empty 500 response is always constructible")
        })
}

async fn device_code(request: Request) -> Response {
    match split(request).await {
        Ok((_, _, _, incoming)) => blocking(move || http::handle_device_code(&incoming)).await,
        Err(response) => *response,
    }
}

async fn device_token(State(state): State<Arc<FrontDoorState>>, request: Request) -> Response {
    match split(request).await {
        Ok((_, _, _, incoming)) => {
            blocking(move || http::handle_device_token(&incoming, &state.ca)).await
        }
        Err(response) => *response,
    }
}

/// The one handler with no blocking work at all -- it renders a static
/// form, touching neither Postgres nor a subprocess.
async fn verify_page(request: Request) -> Response {
    match split(request).await {
        Ok((_, _, query, _)) => into_response(http::handle_verify_page(&query)),
        Err(response) => *response,
    }
}

async fn verify_submit(State(state): State<Arc<FrontDoorState>>, request: Request) -> Response {
    match split(request).await {
        Ok((_, _, _, incoming)) => {
            blocking(move || http::handle_verify_submit(&incoming, &state.ca)).await
        }
        Err(response) => *response,
    }
}

async fn git_http(
    State(state): State<Arc<FrontDoorState>>,
    Extension(PeerCertificate(peer_cert)): Extension<PeerCertificate>,
    request: Request,
) -> Response {
    let (method, path, query, incoming) = match split(request).await {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let Some((tenant_slug, suffix)) = http::git_http_path(&path) else {
        return into_response(Rendered::not_found());
    };
    blocking(move || {
        let route = http::GitHttpRoute {
            tenant_slug: &tenant_slug,
            suffix: &suffix,
            query: &query,
        };
        http::handle_git_http(
            &incoming,
            &method,
            &route,
            &state.repos_root,
            &state.image,
            state.orchestrator.as_ref(),
            peer_cert.as_ref(),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::{self, create_tenant};
    use rustls_pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn unique_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{nanos}")
    }

    /// A real CSR from a freshly generated device identity -- the exact
    /// same `SigningIdentity::to_csr_pem` a real `wkp hub register`
    /// client calls (M5-9, ADR-0011), not a placeholder string. Tests
    /// that need the OpenSSH-form public key this same device would
    /// register under (to check `find_device_by_public_key`
    /// afterwards) get it back alongside the CSR.
    fn generate_csr_and_public_key() -> (String, String) {
        let identity = wkp_crypto::signing_identity::SigningIdentity::generate()
            .expect("generate a signing identity");
        let csr_pem = identity.to_csr_pem().expect("to_csr_pem");
        let public_key = identity.public_key_openssh().expect("public_key_openssh");
        (csr_pem, public_key)
    }

    /// A running front door: the real `axum-server`-on-`rustls`
    /// listener, on an OS-assigned free port, with a real CA in a temp
    /// directory. Not a stand-in router and not plain HTTP -- a test
    /// that passes here is evidence the actual TLS listener works.
    struct TestFrontDoor {
        port: u16,
        root_pem: String,
        /// The same CA the running server uses (`HubCa` derives
        /// `Clone`) -- so a test can mint and sign a device certificate
        /// directly (`ca.sign_device_csr`) without going through the
        /// RFC 8628 HTTP flow, the same way this module already seeds a
        /// tenant directly via `control_plane::create_tenant` rather
        /// than an admin HTTP endpoint.
        ca: HubCa,
        _ca_dir: tempfile::TempDir,
    }

    fn start_front_door() -> TestFrontDoor {
        let ca_dir = tempfile::Builder::new()
            .prefix("wkp-hub-front-door-test-ca-")
            .tempdir()
            .expect("temp dir");
        let ca = HubCa::ensure(ca_dir.path()).expect("ensure the hub CA");
        let root_pem = ca.root_cert_pem().to_string();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();

        let repos_root = tempfile::Builder::new()
            .prefix("wkp-hub-front-door-test-repos-")
            .tempdir()
            .expect("temp dir")
            .keep();
        let server_ca = ca.clone();
        std::thread::spawn(move || {
            // The image name is irrelevant here: no test in this module
            // reaches the tenant-pod cold-start path.
            let _ = serve_on(listener, &server_ca, repos_root, "unused".to_string());
        });

        TestFrontDoor {
            port,
            root_pem,
            ca,
            _ca_dir: ca_dir,
        }
    }

    /// A real TLS client that trusts exactly the hub's own root and
    /// nothing else -- `rustls` directly rather than a new
    /// dev-dependency, and deliberately no public trust bundle (issue
    /// #122's own acceptance criterion, and the reason there is no
    /// `webpki-roots` anywhere in this crate). Panics on any failure,
    /// including a rejected TLS handshake -- the right behavior for
    /// every caller except the M5-10 tests that deliberately expect a
    /// handshake to fail, which call [`tls_request_with_identity`]
    /// directly instead.
    fn tls_request(front_door: &TestFrontDoor, request: &str) -> (u16, String) {
        tls_request_with_identity(front_door, request, None).expect("the TLS request must succeed")
    }

    /// Like [`tls_request`], but optionally presents a client
    /// certificate (M5-10, ADR-0011) and returns `Err` instead of
    /// panicking if the handshake itself is rejected -- the shape
    /// `RevocationAwareClientCertVerifier` rejecting a certificate
    /// takes (the connection never completes, so there is no HTTP
    /// response to parse at all), distinct from a connection that
    /// completed and got an ordinary 401/404 from `handle_git_http`.
    fn tls_request_with_identity(
        front_door: &TestFrontDoor,
        request: &str,
        client_identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    ) -> std::io::Result<(u16, String)> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in pem_certs(&front_door.root_pem) {
            roots.add(cert).expect("trust the hub's own root");
        }
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots);
        let config = match client_identity {
            Some((chain, key)) => builder
                .with_client_auth_cert(chain, key)
                .expect("valid client-auth certificate/key material"),
            None => builder.with_no_client_auth(),
        };

        let server_name = rustls_pki_types::ServerName::try_from("localhost").expect("server name");
        let mut connection = rustls::ClientConnection::new(Arc::new(config), server_name)
            .expect("client connection");

        // The listener thread needs a moment to actually bind.
        let mut socket = None;
        for attempt in 0..40 {
            match std::net::TcpStream::connect(("127.0.0.1", front_door.port)) {
                Ok(s) => {
                    socket = Some(s);
                    break;
                }
                Err(e) if attempt < 39 => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    let _ = e;
                }
                Err(e) => panic!("could not connect to the front door: {e}"),
            }
        }
        let mut socket = socket.expect("connected");
        let mut stream = rustls::Stream::new(&mut connection, &mut socket);

        stream
            .write_all(request.as_bytes())
            .and_then(|_| stream.flush())?;
        let mut raw = Vec::new();
        // `UnexpectedEof` is how a server that closed the connection
        // without a TLS close_notify shows up; the response itself is
        // already complete by then.
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e),
        }

        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("no header/body split in response: {text:?}"));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no status line in response: {head:?}"));
        Ok((status, body.to_string()))
    }

    fn pem_certs(pem: &str) -> Vec<rustls_pki_types::CertificateDer<'static>> {
        let mut out = Vec::new();
        let mut current: Option<String> = None;
        for line in pem.lines() {
            if line.starts_with("-----BEGIN CERTIFICATE-----") {
                current = Some(String::new());
            } else if line.starts_with("-----END CERTIFICATE-----") {
                if let Some(b64) = current.take() {
                    out.push(rustls_pki_types::CertificateDer::from(base64_decode(&b64)));
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

    fn post_json(front_door: &TestFrontDoor, path: &str, body: &str) -> (u16, String) {
        tls_request(
            front_door,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn post_form(front_door: &TestFrontDoor, path: &str, body: &str) -> (u16, String) {
        tls_request(
            front_door,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    /// A GET, optionally presenting a client certificate (M5-10,
    /// ADR-0011) -- the certificate-based replacement for M5-6's own
    /// `Authorization: Bearer` header. Panics if the handshake itself
    /// is rejected; callers expecting that (an untrusted-CA, expired,
    /// or revoked certificate) call [`tls_request_with_identity`]
    /// directly instead, to assert on the `Err` rather than panic.
    fn get(
        front_door: &TestFrontDoor,
        path: &str,
        client_identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    ) -> (u16, String) {
        tls_request_with_identity(
            front_door,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            client_identity,
        )
        .expect("the TLS request must succeed")
    }

    /// #122's own acceptance criterion for TLS termination: the front
    /// door speaks real TLS, verified by a client that trusts only the
    /// hub's own root -- and a route behind it round-trips end to end
    /// through the real `axum-server` listener, not a unit test of the
    /// router.
    #[test]
    fn a_route_round_trips_over_real_tls_against_the_hubs_own_ca() {
        let front_door = start_front_door();
        let mut client = control_plane::connect().expect("connect (seeding the tenant directly)");
        let tenant = create_tenant(&mut client, &unique_slug("front-door-tls")).expect("tenant");
        let (csr_pem, _public_key) = generate_csr_and_public_key();

        let (status, body) = post_json(
            &front_door,
            "/device/code",
            &serde_json::json!({ "tenant_slug": tenant.slug, "csr_pem": csr_pem }).to_string(),
        );
        assert_eq!(status, 200, "body: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("parse the JSON body");
        assert!(parsed["device_code"].as_str().is_some());
        assert!(parsed["user_code"].as_str().is_some());
    }

    /// A client that does *not* trust the hub's root must fail the
    /// handshake -- without this, the test above would pass against a
    /// server presenting anything at all.
    #[test]
    fn a_client_that_does_not_trust_the_hubs_root_fails_the_handshake() {
        let front_door = start_front_door();
        // Block until the listener is actually up, via a client that
        // does trust the root.
        let _ = get(&front_door, "/verify", None);

        let other_dir = tempfile::tempdir().expect("temp dir");
        let unrelated = HubCa::ensure(other_dir.path()).expect("an unrelated CA");
        let mut roots = rustls::RootCertStore::empty();
        for cert in pem_certs(unrelated.root_cert_pem()) {
            roots.add(cert).expect("trust the unrelated root");
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        let server_name = rustls_pki_types::ServerName::try_from("localhost").expect("server name");
        let mut connection = rustls::ClientConnection::new(Arc::new(config), server_name)
            .expect("client connection");
        let mut socket =
            std::net::TcpStream::connect(("127.0.0.1", front_door.port)).expect("connect");
        let mut stream = rustls::Stream::new(&mut connection, &mut socket);
        let result = stream
            .write_all(b"GET /verify HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .and_then(|_| stream.flush());
        assert!(
            result.is_err(),
            "a client trusting only an unrelated CA must not complete the handshake"
        );
    }

    /// M5-2's own acceptance criterion, now over the TLS front door and
    /// extended for M5-9 (ADR-0011): a scripted stand-in for the human
    /// approval step drives a full register round trip -- device-code
    /// request (now a CSR, not a bare public key), a still-pending
    /// poll, the scripted "approval" (which now also signs the CSR with
    /// the hub's CA), then a successful poll carrying the issued
    /// certificate -- against a real running server and a real
    /// Postgres database, ending with the device actually holding a
    /// hub-signed certificate (#123's own acceptance criterion).
    #[test]
    fn full_device_registration_round_trip_over_real_https() {
        let front_door = start_front_door();
        let mut client = control_plane::connect().expect("connect (seeding the tenant directly)");
        let tenant = create_tenant(&mut client, &unique_slug("https-round-trip")).expect("tenant");
        let (csr_pem, public_key) = generate_csr_and_public_key();

        let (status, body) = post_json(
            &front_door,
            "/device/code",
            &serde_json::json!({ "tenant_slug": tenant.slug, "csr_pem": csr_pem }).to_string(),
        );
        assert_eq!(status, 200);
        let code_response: serde_json::Value =
            serde_json::from_str(&body).expect("parse device/code response");
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
        let (status, body) = post_json(
            &front_door,
            "/device/token",
            &serde_json::json!({ "device_code": device_code }).to_string(),
        );
        assert_eq!(status, 400);
        let pending: serde_json::Value =
            serde_json::from_str(&body).expect("parse pending response");
        assert_eq!(pending["error"], "authorization_pending");

        // The scripted stand-in for the human clicking "approve": a
        // real form POST, exactly like a plain HTML form (no
        // JavaScript) would send.
        let (status, _) = post_form(&front_door, "/verify", &format!("user_code={user_code}"));
        assert_eq!(status, 200);

        let (status, body) = post_json(
            &front_door,
            "/device/token",
            &serde_json::json!({ "device_code": device_code }).to_string(),
        );
        assert_eq!(status, 200);
        let approved: serde_json::Value =
            serde_json::from_str(&body).expect("parse token response");
        assert_eq!(approved["status"], "approved");
        assert_eq!(approved["tenant_slug"], tenant.slug);
        let certificate_pem = approved["certificate_pem"]
            .as_str()
            .expect("certificate_pem must be present");
        assert!(certificate_pem.contains("BEGIN CERTIFICATE"));
        assert_eq!(
            approved["ca_cert_pem"].as_str().expect("ca_cert_pem"),
            front_door.root_pem,
            "the device must be handed the same root the front door itself presents"
        );

        // The real, load-bearing assertion: the device actually landed
        // in the control plane, under the right tenant, from the
        // registered public key, holding the exact certificate the
        // `/device/token` response just handed back -- not just that
        // the HTTP responses looked right.
        let registered = control_plane::find_device_by_public_key(&mut client, &public_key)
            .expect("find_device_by_public_key")
            .expect("device must actually be registered");
        assert_eq!(registered.tenant_id, tenant.id);
        assert!(registered.revoked_at.is_none());
        assert_eq!(registered.certificate_pem.as_deref(), Some(certificate_pem));
        assert!(registered.cert_serial.is_some());
        assert!(registered.cert_issued_at.is_some());
        assert!(registered.cert_expires_at.is_some());
    }

    #[test]
    fn device_code_request_for_an_unknown_tenant_is_refused() {
        let front_door = start_front_door();
        let (csr_pem, _public_key) = generate_csr_and_public_key();
        let (status, _) = post_json(
            &front_door,
            "/device/code",
            &serde_json::json!({
                "tenant_slug": unique_slug("never-created-tenant"),
                "csr_pem": csr_pem,
            })
            .to_string(),
        );
        assert_eq!(status, 404, "an unknown tenant must not succeed");
    }

    /// M5-9's own boundary-validation case: a garbage `csr_pem` is
    /// rejected at `/device/code` time, before a human ever gets a
    /// user-code to approve.
    #[test]
    fn device_code_request_with_a_malformed_csr_is_refused() {
        let front_door = start_front_door();
        let (status, body) = post_json(
            &front_door,
            "/device/code",
            &serde_json::json!({
                "tenant_slug": unique_slug("malformed-csr"),
                "csr_pem": "not a csr",
            })
            .to_string(),
        );
        assert_eq!(status, 400, "body: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("parse the JSON body");
        assert_eq!(parsed["error"], "invalid_csr");
    }

    /// A real device certificate, signed by `front_door`'s own CA and
    /// registered through the same control-plane calls
    /// `handle_verify_submit` makes for a real enrollment, along with
    /// the rustls-ready key material needed to present it in a TLS
    /// client-auth handshake. Built directly via `rcgen`, not through
    /// `wkp_crypto::signing_identity` -- that module has no reason to
    /// expose its key in a TLS-library-ready form; only these git-http
    /// auth tests need that.
    fn issue_device_cert(
        front_door: &TestFrontDoor,
        tenant_id: i64,
    ) -> (
        control_plane::Device,
        Vec<CertificateDer<'static>>,
        PrivateKeyDer<'static>,
    ) {
        let key_pair =
            rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("generate key pair");
        let csr_pem = rcgen::CertificateParams::default()
            .serialize_request(&key_pair)
            .expect("serialize_request")
            .pem()
            .expect("pem");
        let signed = front_door
            .ca
            .sign_device_csr(&csr_pem, "test device")
            .expect("sign_device_csr");
        let public_key = wkp_crypto::signing_identity::openssh_public_key_from_raw_ed25519(
            &signed.public_key_raw,
        )
        .expect("openssh_public_key_from_raw_ed25519");

        let mut client = control_plane::connect().expect("connect");
        let device = control_plane::register_device(&mut client, tenant_id, &public_key)
            .expect("register_device");
        control_plane::issue_device_certificate(
            &mut client,
            device.id,
            &signed.certificate_pem,
            &signed.serial_hex,
            signed.not_before,
            signed.not_after,
        )
        .expect("issue_device_certificate");

        let cert_chain = pem_certs(&signed.certificate_pem);
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        (device, cert_chain, key)
    }

    /// An already-expired device certificate, signed directly against
    /// `front_door`'s own CA. `HubCa::sign_device_csr` has no
    /// configurable validity window -- production code never has a
    /// reason to mint anything but a currently-valid certificate -- so
    /// this duplicates its signing shape (via the same public
    /// `HubCa::issuer` seam) with an explicit past `not_before`/
    /// `not_after` instead, test-only. Not registered in the control
    /// plane at all: an expired certificate must be rejected by
    /// ordinary TLS chain validation before the verifier's own
    /// revocation lookup ever runs, so there is nothing to seed.
    fn issue_expired_device_cert(
        front_door: &TestFrontDoor,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let key_pair =
            rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("generate key pair");
        let issuer = front_door.ca.issuer().expect("issuer");
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(30);
        params.not_after = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        let cert = params.signed_by(&key_pair, &issuer).expect("signed_by");

        let cert_chain = pem_certs(&cert.pem());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        (cert_chain, key)
    }

    /// M5-6's own fixture shape, unchanged apart from the credential
    /// (M5-10, ADR-0011: a hub-signed client certificate, not a bearer
    /// token): a real bare repo under a fresh `repos_root`, a tenant
    /// row, and a device holding a freshly issued certificate.
    struct GitHttpFixture {
        front_door: TestFrontDoor,
        tenant_slug: String,
        device_id: i64,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    }

    fn set_up_git_http_fixture() -> GitHttpFixture {
        let front_door = start_front_door();
        let tenant_slug = unique_slug("git-https");
        let mut client = control_plane::connect().expect("connect");
        let tenant = create_tenant(&mut client, &tenant_slug).expect("create_tenant");
        let (device, cert_chain, key) = issue_device_cert(&front_door, tenant.id);

        GitHttpFixture {
            front_door,
            tenant_slug,
            device_id: device.id,
            cert_chain,
            key,
        }
    }

    /// M5-10's own explicit acceptance criterion: a valid device
    /// certificate's push/fetch succeeds. What this proves directly: a
    /// valid, active, correctly-tenant-matched certificate is never
    /// rejected by the TLS handshake or by `handle_git_http`'s own
    /// auth checks (401/404) -- whatever happens after that (a
    /// successful proxy, or a 502/503 because no pod is actually
    /// running here) is a separate concern, covered by
    /// `deploy/hub/test-pod-lifecycle.sh` against a real pod.
    #[test]
    fn a_valid_active_devices_certificate_passes_auth_and_reaches_the_proxy_step() {
        let fixture = set_up_git_http_fixture();
        let (status, _) = get(
            &fixture.front_door,
            &format!(
                "/{}.git/info/refs?service=git-upload-pack",
                fixture.tenant_slug
            ),
            Some((fixture.cert_chain, fixture.key)),
        );
        assert_ne!(
            status, 401,
            "a valid, active certificate must not be rejected"
        );
        assert_ne!(
            status, 404,
            "a certificate correctly matched to its own tenant must not 404"
        );
        // The cold-start attempt above creates a real (empty, since
        // this module's own test setup passes a deliberately invalid
        // image name) podman pod even though the actual `podman run`
        // inside it fails -- cleaned up here rather than leaking one
        // `wkp-tenant-<slug>` pod per test run indefinitely.
        if let Ok(orchestrator) = crate::tenant_pod::orchestrator() {
            let _ = orchestrator.stop_pod(&fixture.tenant_slug);
        }
    }

    #[test]
    fn git_http_rejects_a_request_with_no_client_certificate() {
        let fixture = set_up_git_http_fixture();
        let (status, _) = get(
            &fixture.front_door,
            &format!(
                "/{}.git/info/refs?service=git-upload-pack",
                fixture.tenant_slug
            ),
            None,
        );
        assert_eq!(status, 401);
    }

    /// M5-10's own explicit acceptance criterion: a certificate not
    /// signed by the hub's own CA is rejected -- at the TLS handshake
    /// itself, never reaching `handle_git_http` at all (there is no
    /// HTTP response to check a status code on).
    #[test]
    fn git_http_rejects_a_certificate_from_an_untrusted_ca() {
        let fixture = set_up_git_http_fixture();
        let other_dir = tempfile::tempdir().expect("temp dir");
        let unrelated_ca = HubCa::ensure(other_dir.path()).expect("an unrelated CA");
        let key_pair =
            rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("generate key pair");
        let csr_pem = rcgen::CertificateParams::default()
            .serialize_request(&key_pair)
            .expect("serialize_request")
            .pem()
            .expect("pem");
        let signed = unrelated_ca
            .sign_device_csr(&csr_pem, "test device")
            .expect("sign_device_csr");
        let cert_chain = pem_certs(&signed.certificate_pem);
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

        let result = tls_request_with_identity(
            &fixture.front_door,
            &format!(
                "GET /{}.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
                 Host: localhost\r\nConnection: close\r\n\r\n",
                fixture.tenant_slug
            ),
            Some((cert_chain, key)),
        );
        assert!(
            result.is_err(),
            "a certificate from an unrelated CA must fail the handshake, not just 401"
        );
    }

    /// M5-10's own explicit acceptance criterion: an expired
    /// certificate is rejected, at the handshake -- ordinary TLS chain
    /// validation, before the verifier's own revocation lookup ever
    /// runs (there is nothing to look up: this certificate was never
    /// registered in the control plane at all).
    #[test]
    fn git_http_rejects_an_expired_certificate() {
        let fixture = set_up_git_http_fixture();
        let (cert_chain, key) = issue_expired_device_cert(&fixture.front_door);

        let result = tls_request_with_identity(
            &fixture.front_door,
            &format!(
                "GET /{}.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
                 Host: localhost\r\nConnection: close\r\n\r\n",
                fixture.tenant_slug
            ),
            Some((cert_chain, key)),
        );
        assert!(
            result.is_err(),
            "an expired certificate must fail the handshake"
        );
    }

    /// M5-10's own explicit acceptance criterion: a revoked device's
    /// certificate is rejected -- checked at the verifier itself
    /// (ADR-0011's addendum: fail-closed, per-handshake), so this
    /// fails the handshake outright rather than getting an ordinary
    /// 401 from `handle_git_http`.
    #[test]
    fn git_http_rejects_a_revoked_devices_certificate() {
        let fixture = set_up_git_http_fixture();
        let mut client = control_plane::connect().expect("connect");
        control_plane::revoke_device(&mut client, fixture.device_id).expect("revoke_device");

        let result = tls_request_with_identity(
            &fixture.front_door,
            &format!(
                "GET /{}.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
                 Host: localhost\r\nConnection: close\r\n\r\n",
                fixture.tenant_slug
            ),
            Some((fixture.cert_chain, fixture.key)),
        );
        assert!(
            result.is_err(),
            "a revoked device's certificate must fail the handshake"
        );
    }

    /// A certificate valid for one tenant must never reach another
    /// tenant's repo -- `handle_git_http`'s own explicit check,
    /// unchanged by the move from bearer tokens to certificates.
    #[test]
    fn git_http_rejects_a_certificate_used_against_a_different_tenant() {
        let fixture = set_up_git_http_fixture();
        let other_tenant_slug = unique_slug("git-https-other-tenant");
        let (status, _) = get(
            &fixture.front_door,
            &format!("/{other_tenant_slug}.git/info/refs?service=git-upload-pack"),
            Some((fixture.cert_chain, fixture.key)),
        );
        assert_eq!(status, 404);
    }

    /// An unrecognized path still answers the same 404 the old
    /// `tiny_http` dispatcher did, rather than `axum`'s own empty
    /// fallback.
    #[test]
    fn an_unknown_path_is_a_json_404() {
        let front_door = start_front_door();
        let (status, body) = get(&front_door, "/no/such/route", None);
        assert_eq!(status, 404);
        assert!(body.contains("not_found"), "body: {body}");
    }
}
