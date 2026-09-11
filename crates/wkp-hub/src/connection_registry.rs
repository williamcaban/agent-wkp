//! Active-connection revocation reset (M5-12, ADR-0011 addendum,
//! design 8.1/8.3).
//!
//! M5-10's `RevocationAwareClientCertVerifier` (`crate::hub_ca`) makes
//! revocation effective on the *next* TLS handshake -- the same
//! guarantee the old SSH path had ("`sshd` re-evaluates
//! `AuthorizedKeysCommand` on the next connection"). This module adds
//! the stronger option on top: force-closing a connection that is
//! *already open*, for the incident-response case where "next
//! connection" isn't good enough (a leaked credential, a suspected CA
//! compromise).
//!
//! Two pieces:
//!
//! - [`ConnectionRegistry`]: an in-memory map from a device's
//!   certificate serial to every currently-open connection's
//!   [`tokio_util::sync::CancellationToken`], populated by
//!   [`front_door::PeerCertAcceptor`](crate::front_door) once per
//!   connection (not per request) as soon as the handshake resolves a
//!   peer certificate, and deregistered automatically when the
//!   connection ends ([`ConnectionGuard`]'s own `Drop`).
//! - [`CancellableStream`]: wraps the accepted TLS stream so that,
//!   once its token is cancelled, any read or write in progress (or
//!   the next one) fails instead of continuing to serve the
//!   connection -- the actual mechanism that turns "this device is
//!   revoked" into "this socket stops working."
//!
//! Reached two ways, both converging on the same registry:
//! [`crate::control_plane::revoke_device`] issues a Postgres `NOTIFY`
//! in the same transaction that sets `revoked_at`; the front door's own
//! long-lived `LISTEN` subscriber (`listen_for_revocations`, spawned
//! once from `front_door::serve_on`) reacts to it. A periodic fallback
//! sweep, piggybacking on that same subscriber's own reconnect loop,
//! covers the case where the `LISTEN` connection was down when the
//! `NOTIFY` fired -- there is no separate "idle-pod reaper's own
//! periodic sweep" to piggyback on the way the ADR addendum's own
//! wording suggested; that reaper has never run as an in-process timer
//! loop itself (only ever invoked externally, `wkp-hub
//! reap-idle-pods`), so this module's own sweep is independent, not
//! attached to it.

use postgres::fallible_iterator::FallibleIterator;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;

/// How often [`listen_for_revocations`] wakes up even with no
/// notification pending -- both to retry a dropped `LISTEN` connection
/// and to run the periodic fallback sweep (see the module doc).
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait before retrying a failed `LISTEN` connection --
/// separate from [`SWEEP_INTERVAL`] so a genuinely down Postgres
/// doesn't get hammered with reconnect attempts at the same cadence
/// the happy path polls at.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// The channel every revocation-reset `NOTIFY` is sent on. One
/// channel, two payload shapes, rather than two channels: a specific
/// certificate serial (targeted, single-device close) or the literal
/// [`RESET_ALL_PAYLOAD`] (reset-all, every open connection).
pub const NOTIFY_CHANNEL: &str = "wkp_hub_connection_reset";

/// The reset-all sentinel payload -- deliberately not a value any real
/// certificate serial (always a hex string, [`hub_ca`](crate::hub_ca))
/// could collide with.
pub const RESET_ALL_PAYLOAD: &str = "*";

/// In-memory registry of every currently-open, certificate-authenticated
/// connection, keyed by the presented certificate's own serial (the
/// same value [`crate::control_plane::find_device_by_cert_serial`]
/// indexes on) -- not a device id, so registering a connection never
/// needs its own control-plane round trip; the certificate itself
/// already names the device unambiguously.
#[derive(Default)]
pub struct ConnectionRegistry {
    // cert_serial -> connection_id -> its cancellation token.
    connections: Mutex<HashMap<String, HashMap<u64, CancellationToken>>>,
    next_id: AtomicU64,
}

impl ConnectionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers one new connection for `cert_serial`. Returns the
    /// [`CancellationToken`] [`CancellableStream`] watches, and a
    /// [`ConnectionGuard`] the caller must keep alive for exactly as
    /// long as the connection itself -- dropping it deregisters this
    /// entry, which [`CancellableStream`] does automatically by owning
    /// the guard.
    pub fn register(self: &Arc<Self>, cert_serial: String) -> (CancellationToken, ConnectionGuard) {
        let token = CancellationToken::new();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.connections
            .lock()
            .unwrap()
            .entry(cert_serial.clone())
            .or_default()
            .insert(id, token.clone());
        let guard = ConnectionGuard {
            registry: Arc::clone(self),
            cert_serial,
            id,
        };
        (token, guard)
    }

    fn deregister(&self, cert_serial: &str, id: u64) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(by_id) = connections.get_mut(cert_serial) {
            by_id.remove(&id);
            if by_id.is_empty() {
                connections.remove(cert_serial);
            }
        }
    }

    /// Force-closes every currently-open connection for one device.
    /// Returns how many were actually closed (0 is not an error -- the
    /// device may simply have no open connection right now, revocation
    /// still took effect for its *next* one via M5-10's own
    /// per-handshake check).
    pub fn close_device(&self, cert_serial: &str) -> usize {
        let Some(by_id) = self.connections.lock().unwrap().remove(cert_serial) else {
            return 0;
        };
        let n = by_id.len();
        for (_, token) in by_id {
            token.cancel();
        }
        n
    }

    /// Force-closes every open connection, for every device -- the
    /// "assume broader compromise" tier. Never called as a side effect
    /// of an ordinary single-device revoke; only
    /// [`crate::main`]'s own dedicated, separately-audited admin
    /// subcommand reaches this.
    pub fn close_all(&self) -> usize {
        let mut connections = self.connections.lock().unwrap();
        let n = connections.values().map(|by_id| by_id.len()).sum();
        for (_, by_id) in connections.drain() {
            for (_, token) in by_id {
                token.cancel();
            }
        }
        n
    }
}

/// Deregisters its own entry from the [`ConnectionRegistry`] it came
/// from when dropped -- held inside [`CancellableStream`] so this
/// happens automatically at connection end, the same lifetime the
/// connection's own I/O object has, with no explicit cleanup call
/// needed at every one of `axum-server`'s own connection-teardown
/// paths (a normal close, a client disconnect, a revocation-triggered
/// force-close all drop this the same way).
pub struct ConnectionGuard {
    registry: Arc<ConnectionRegistry>,
    cert_serial: String,
    id: u64,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.registry.deregister(&self.cert_serial, self.id);
    }
}

/// Wraps an accepted connection's own I/O stream so that, once
/// `token` is cancelled, any read or write already in progress -- or
/// the next one -- fails immediately instead of continuing to serve
/// the connection. This, not the registry alone, is what actually
/// turns a revocation into a closed socket: `axum-server`'s own
/// `Accept` trait has no hook into the request-serving loop itself
/// (only into what happens once, before a connection is handed off to
/// it), so the only lever available is making the stream the loop
/// reads and writes through start failing.
///
/// `guard` (an already-registered [`ConnectionGuard`], or `None` for a
/// connection with no client certificate at all -- the RFC 8628
/// enrollment routes, never revocable by definition) is carried purely
/// for its `Drop` side effect; nothing here ever reads it directly.
///
/// Deliberately holds the "cancelled" watcher as a boxed, already-pinned
/// future (`Pin<Box<dyn Future<...>>>`) rather than a bare
/// `tokio_util` future type in a struct field: every field this way is
/// itself `Unpin` (a `Pin<Box<T>>` is `Unpin` regardless of `T`, and
/// `S: Unpin` is already required below), which makes the whole
/// wrapper `Unpin` too and lets every `poll_*` method below use a
/// plain `self.get_mut()` -- no manual pin-projection, no `unsafe`
/// (`#![forbid(unsafe_code)]`), no extra crate.
pub struct CancellableStream<S> {
    inner: S,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    is_cancelled: bool,
    _guard: Option<ConnectionGuard>,
}

impl<S> CancellableStream<S> {
    pub fn new(inner: S, token: CancellationToken, guard: Option<ConnectionGuard>) -> Self {
        Self {
            inner,
            cancelled: Box::pin(async move { token.cancelled_owned().await }),
            is_cancelled: false,
            _guard: guard,
        }
    }

    /// Polls the cancellation watcher at most once (it never needs
    /// re-polling after firing), returning `Err` on either the freshly
    /// observed or a previously observed cancellation. Shared by every
    /// `poll_*` method below.
    fn poll_cancelled(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.is_cancelled && self.cancelled.as_mut().poll(cx).is_ready() {
            self.is_cancelled = true;
        }
        if self.is_cancelled {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "device revoked; connection force-closed",
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CancellableStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Poll::Ready(Err(e)) = this.poll_cancelled(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CancellableStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Poll::Ready(Err(e)) = this.poll_cancelled(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Poll::Ready(Err(e)) = this.poll_cancelled(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Deliberately does *not* check cancellation first: a
        // shutdown is itself a close, and refusing to let one proceed
        // just because the connection was independently revoked would
        // only make an already-desired outcome (this socket ending)
        // harder to reach, not easier.
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The long-lived `LISTEN` subscriber `front_door::serve_on` spawns
/// once, on its own OS thread (not a `tokio` task): the sync
/// `postgres::Client` this crate uses everywhere else builds its own
/// internal Tokio runtime per call, which panics
/// ("Cannot start a runtime from within a runtime") if driven from a
/// thread `tokio`'s own multi-thread runtime is already using --
/// confirmed the hard way building M5-10's `RevocationAwareClientCertVerifier`,
/// same reasoning applies here. Blocks this dedicated thread
/// indefinitely; never returns except on an unrecoverable setup error
/// (a malformed `DATABASE_URL`, say) -- a connection that merely drops
/// mid-run is reconnected, not treated as fatal.
///
/// `Notifications::timeout_iter` (not `blocking_iter`) does double
/// duty deliberately: it delivers a `NOTIFY` the moment one arrives,
/// *and* wakes this loop on a plain timeout with nothing pending,
/// which is what drives both the reconnect-on-failure retry and the
/// periodic fallback sweep (see the module doc) off one connection,
/// one loop, rather than two independent mechanisms with their own
/// separate failure modes to reason about.
pub fn listen_for_revocations(database_url: String, registry: Arc<ConnectionRegistry>) {
    loop {
        match run_listen_loop(&database_url, &registry) {
            Ok(()) => {
                // The server closed the connection cleanly (`None`
                // from the iterator) -- reconnect, same as any other
                // disconnection.
            }
            Err(e) => {
                eprintln!("wkp-hub: revocation LISTEN connection failed: {e}");
            }
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

fn run_listen_loop(
    database_url: &str,
    registry: &Arc<ConnectionRegistry>,
) -> Result<(), postgres::Error> {
    let mut client = postgres::Client::connect(database_url, postgres::NoTls)?;
    client.batch_execute(&format!("LISTEN {NOTIFY_CHANNEL}"))?;

    loop {
        // Recreated every iteration, not held across the loop: it
        // borrows `client` mutably, which `sweep_revoked` below also
        // needs -- cheap to rebuild (just wraps the same underlying
        // connection), unlike reconnecting outright.
        let mut notifications = client.notifications();
        let received = match notifications.timeout_iter(SWEEP_INTERVAL).next()? {
            Some(notification) if notification.channel() == NOTIFY_CHANNEL => {
                Some(notification.payload().to_string())
            }
            Some(_) => None, // some other channel; nothing subscribes to those here
            None => return Ok(()), // server disconnected; caller reconnects
        };
        drop(notifications);

        if let Some(payload) = received {
            if payload == RESET_ALL_PAYLOAD {
                let n = registry.close_all();
                eprintln!("wkp-hub: revocation reset-all: closed {n} connection(s)");
            } else {
                let n = registry.close_device(&payload);
                if n > 0 {
                    eprintln!(
                        "wkp-hub: revocation: closed {n} open connection(s) for a revoked device"
                    );
                }
            }
        }
        sweep_revoked(&mut client, registry);
    }
}

/// The periodic fallback: for every certificate serial this process
/// currently has an open connection for, check the control plane's
/// own `revoked_at` state directly and close it if set -- covers a
/// `NOTIFY` that fired while this subscriber's own `LISTEN` connection
/// was down (the gap between "connection lost" and "reconnected"
/// above), which by construction never receives a notification sent
/// during that gap.
fn sweep_revoked(client: &mut postgres::Client, registry: &Arc<ConnectionRegistry>) {
    let open_serials: Vec<String> = {
        let connections = registry.connections.lock().unwrap();
        connections.keys().cloned().collect()
    };
    for cert_serial in open_serials {
        let revoked = client
            .query_opt(
                "SELECT revoked_at FROM devices WHERE cert_serial = $1 AND revoked_at IS NOT NULL",
                &[&cert_serial],
            )
            .ok()
            .flatten()
            .is_some();
        if revoked {
            let n = registry.close_device(&cert_serial);
            if n > 0 {
                eprintln!(
                    "wkp-hub: revocation sweep: closed {n} connection(s) for a device revoked \
                     while the LISTEN connection was down"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The periodic fallback sweep itself (acceptance criterion #128:
    /// "a periodic fallback sweep catches revocations if the LISTEN
    /// connection was down when the NOTIFY fired"), exercised directly
    /// against a real Postgres rather than via the full LISTEN/NOTIFY
    /// plumbing (`front_door::tests` already covers that end to end,
    /// through the fast path this sweep is deliberately the *fallback*
    /// for) -- proves `sweep_revoked` finds and closes a revoked
    /// device's registered connection purely from `devices.revoked_at`,
    /// with no notification involved at all.
    #[test]
    fn sweep_revoked_closes_a_registered_connection_whose_device_was_revoked() {
        let mut client = crate::control_plane::connect().expect("connect");
        let tenant = crate::control_plane::create_tenant(
            &mut client,
            &format!(
                "sweep-test-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ),
        )
        .expect("create_tenant");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let device = crate::control_plane::register_device(
            &mut client,
            tenant.id,
            &format!("ssh-ed25519 fake-key-{nonce}"),
        )
        .expect("register_device");
        let cert_serial = format!("sweep-test-serial-{}", device.id);
        let now = time::OffsetDateTime::now_utc();
        crate::control_plane::issue_device_certificate(
            &mut client,
            device.id,
            "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----\n",
            &cert_serial,
            now,
            now + time::Duration::days(1),
        )
        .expect("issue_device_certificate");

        let registry = ConnectionRegistry::new();
        let (token, _guard) = registry.register(cert_serial.clone());
        assert!(!token.is_cancelled(), "sanity: not cancelled before revoke");

        crate::control_plane::revoke_device(&mut client, device.id).expect("revoke_device");

        // The sweep itself, not the NOTIFY this same revoke_device call
        // also fired -- nothing here ever LISTENs for it.
        sweep_revoked(&mut client, &registry);

        assert!(
            token.is_cancelled(),
            "sweep_revoked must close a connection whose device is now revoked"
        );
    }

    #[tokio::test]
    async fn close_device_cancels_only_that_devices_tokens() {
        let registry = ConnectionRegistry::new();
        let (token_a1, _guard_a1) = registry.register("serial-a".to_string());
        let (token_a2, _guard_a2) = registry.register("serial-a".to_string());
        let (token_b, _guard_b) = registry.register("serial-b".to_string());

        let closed = registry.close_device("serial-a");
        assert_eq!(closed, 2);
        assert!(token_a1.is_cancelled());
        assert!(token_a2.is_cancelled());
        assert!(!token_b.is_cancelled());
    }

    #[tokio::test]
    async fn close_device_on_an_unknown_serial_closes_nothing() {
        let registry = ConnectionRegistry::new();
        assert_eq!(registry.close_device("never-registered"), 0);
    }

    #[tokio::test]
    async fn close_all_cancels_every_open_connection() {
        let registry = ConnectionRegistry::new();
        let (token_a, _guard_a) = registry.register("serial-a".to_string());
        let (token_b, _guard_b) = registry.register("serial-b".to_string());

        let closed = registry.close_all();
        assert_eq!(closed, 2);
        assert!(token_a.is_cancelled());
        assert!(token_b.is_cancelled());
    }

    #[tokio::test]
    async fn dropping_the_guard_deregisters_without_cancelling() {
        let registry = ConnectionRegistry::new();
        let (token, guard) = registry.register("serial-a".to_string());
        drop(guard);
        // An ordinary connection close (guard dropped) must not read
        // as a revocation -- the token stays uncancelled, and the
        // entry is gone so a later close_device for the same serial
        // (e.g. a real revocation arriving just after) finds nothing
        // left to close.
        assert!(!token.is_cancelled());
        assert_eq!(registry.close_device("serial-a"), 0);
    }

    #[tokio::test]
    async fn cancellable_stream_read_fails_once_its_token_is_cancelled() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client, mut server) = tokio::io::duplex(64);
        let token = CancellationToken::new();
        let mut stream = CancellableStream::new(client, token.clone(), None);

        // Send something first, confirm the wrapper is transparent
        // before cancellation.
        server.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        token.cancel();
        let mut buf = [0u8; 1];
        let err = stream.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[tokio::test]
    async fn cancellable_stream_read_already_in_progress_is_woken_by_a_later_cancel() {
        use tokio::io::AsyncReadExt;

        let (client, _server) = tokio::io::duplex(64);
        let token = CancellationToken::new();
        let mut stream = CancellableStream::new(client, token.clone(), None);

        let mut buf = [0u8; 1];
        let read_fut = stream.read(&mut buf);
        tokio::pin!(read_fut);

        // Nothing written yet -- this poll is genuinely pending, not
        // already resolved, so the test actually exercises the
        // "wake a blocked read" path, not just "check cancelled up
        // front."
        assert!(
            futures_util_poll_once(&mut read_fut).is_none(),
            "read must be pending with nothing written and no cancellation yet"
        );

        token.cancel();
        let err = read_fut.await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    }

    /// Polls a future exactly once against a no-op waker, without
    /// pulling in `futures`/`futures-util`'s own test helpers as a new
    /// dev-dependency -- this crate already has `std::task::Waker`
    /// available, which is all a single manual poll needs.
    fn futures_util_poll_once<F: Future + Unpin>(fut: &mut F) -> Option<F::Output> {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match Pin::new(fut).poll(&mut cx) {
            Poll::Ready(v) => Some(v),
            Poll::Pending => None,
        }
    }
}
