//! The accept loop every server runs: timeouts, a connection cap, and an
//! accept error that backs off and reports once a minute.
//!
//! `axum::serve` gives hyper no timer, so hyper's header read timeout never
//! runs: a client that connects and sends nothing, or sends half a request,
//! or keeps an idle keep-alive connection open, holds its socket forever. It
//! has no connection cap either, and it logs every failed accept at `error`,
//! once a second, while the process is out of file descriptors. This loop is
//! `axum::serve` with those three fixed; the router, `ConnectInfo`, upgrades
//! and the graceful drain behave as before.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use axum::http::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;
use tower::ServiceExt;
use tracing::{debug, error, trace};

/// How long a connection may take to send a request's headers, counted from
/// when the server is ready to read them. That includes the idle time between
/// two requests on a keep-alive connection, so this is also the idle timeout.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Most connections served at once. Past it the server stops accepting until
/// one closes; new connections wait in the kernel's backlog.
pub const DEFAULT_MAX_CONNECTIONS: usize = 4096;

/// The window in which a failing accept is reported once.
const ACCEPT_REPORT_WINDOW: Duration = Duration::from_secs(60);

/// The first wait after a failed accept; it doubles up to [`ACCEPT_BACKOFF_MAX`].
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);

/// The longest wait after a failed accept: what `axum::serve` always waits.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Per-connection limits, set with [`WebServer::connection_limits`](super::WebServer::connection_limits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    header_read_timeout: Duration,
    max_connections: usize,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionLimits {
    /// [`DEFAULT_HEADER_READ_TIMEOUT`] and [`DEFAULT_MAX_CONNECTIONS`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }

    /// Closes a connection that has not sent a request's headers this long
    /// after the server is ready for them, idle keep-alive time included.
    #[must_use]
    pub const fn header_read_timeout(mut self, timeout: Duration) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    /// Serves at most this many connections at once (at least one). Long-lived
    /// streams (server-sent events, `WebSocket`s) count. Keep it under the
    /// process's open-file limit, with room for the files and outbound
    /// connections the app itself opens.
    #[must_use]
    pub const fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = if max == 0 { 1 } else { max };
        self
    }
}

/// What to do after a failed accept.
#[derive(Debug, PartialEq, Eq)]
struct AcceptVerdict {
    /// `Some(n)`: report this error, saying `n` more were not reported since
    /// the last report. `None`: log it at `debug` only.
    report: Option<u64>,
    /// How long to wait before accepting again.
    wait: Duration,
}

/// Failed accepts since the last success: the backoff and the report window.
#[derive(Debug)]
struct AcceptErrors {
    backoff: Duration,
    reported_at: Option<Instant>,
    suppressed: u64,
}

impl AcceptErrors {
    const fn new() -> Self {
        Self {
            backoff: ACCEPT_BACKOFF_MIN,
            reported_at: None,
            suppressed: 0,
        }
    }

    /// An accept failed at `now`.
    fn failed(&mut self, now: Instant) -> AcceptVerdict {
        let wait: Duration = self.backoff;
        self.backoff = (self.backoff * 2).min(ACCEPT_BACKOFF_MAX);
        let due: bool = self
            .reported_at
            .is_none_or(|at| now.duration_since(at) >= ACCEPT_REPORT_WINDOW);
        let report: Option<u64> = if due {
            self.reported_at = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed += 1;
            None
        };
        AcceptVerdict { report, wait }
    }

    /// An accept succeeded: the next failure waits the shortest time again.
    /// The report window stays, so a flapping accept still reports once a window.
    const fn succeeded(&mut self) {
        self.backoff = ACCEPT_BACKOFF_MIN;
    }
}

/// Failures of one connection, not of the listener: retried at once and never
/// reported (the same set `axum::serve` skips).
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// Serves `router` on `listener` until `signal`, then drains: no new
/// connections, every open one finishes its request and closes. Returns once
/// the last connection has closed.
pub(crate) async fn serve<F>(
    listener: TcpListener,
    router: Router,
    limits: ConnectionLimits,
    signal: F,
) where
    F: Future<Output = ()> + Send,
{
    let permits: Arc<Semaphore> = Arc::new(Semaphore::new(limits.max_connections));
    // Every connection holds a receiver of each: `drain` tells them to finish,
    // and `closed` resolves once the last one has dropped its receiver.
    let (drain_tx, drain_rx) = watch::channel(());
    let (closed_tx, closed_rx) = watch::channel(());
    let mut errors: AcceptErrors = AcceptErrors::new();
    tokio::pin!(signal);

    loop {
        let permit = tokio::select! {
            biased;
            () = &mut signal => break,
            permit = Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                // Never closed, so this does not happen; stop rather than spin.
                Err(_closed) => break,
            },
        };
        let accepted: io::Result<(TcpStream, SocketAddr)> = tokio::select! {
            biased;
            () = &mut signal => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, remote) = match accepted {
            Ok(pair) => {
                errors.succeeded();
                pair
            }
            Err(e) if is_connection_error(&e) => continue,
            Err(e) => {
                let verdict: AcceptVerdict = errors.failed(Instant::now());
                metrics::counter!("http_accept_errors_total").increment(1);
                match verdict.report {
                    Some(0) => error!("accept error: {e}; backing off"),
                    Some(n) => {
                        error!("accept error: {e}; backing off ({n} more since the last report)");
                    }
                    None => debug!("accept error: {e}"),
                }
                tokio::select! {
                    biased;
                    () = &mut signal => break,
                    () = tokio::time::sleep(verdict.wait) => continue,
                }
            }
        };

        let service = TowerToHyperService::new(router.clone().map_request(
            move |mut request: Request<Incoming>| {
                request.extensions_mut().insert(ConnectInfo(remote));
                request
            },
        ));
        let mut drain_rx: watch::Receiver<()> = drain_rx.clone();
        let closed_rx: watch::Receiver<()> = closed_rx.clone();
        tokio::spawn(async move {
            let open = metrics::gauge!("http_open_connections");
            open.increment(1.0);
            let mut builder: http1::Builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout);
            let connection = builder
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            tokio::pin!(connection);
            let result: Result<(), hyper::Error> = tokio::select! {
                result = connection.as_mut() => result,
                _ = drain_rx.changed() => {
                    connection.as_mut().graceful_shutdown();
                    connection.as_mut().await
                }
            };
            if let Err(e) = result {
                trace!("connection ended: {e}");
            }
            open.decrement(1.0);
            drop(permit);
            drop(closed_rx);
        });
    }

    drop(listener);
    drop(closed_rx);
    // Ignored: no connection open means no receiver, which is the goal.
    let _ignored: Result<(), watch::error::SendError<()>> = drain_tx.send(());
    closed_tx.closed().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    /// Bounds every wait, so a regression fails instead of hanging.
    const WAIT: Duration = Duration::from_secs(5);

    async fn start(limits: ConnectionLimits) -> (SocketAddr, crate::webserver::Shutdown) {
        let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address: SocketAddr = listener.local_addr().expect("address");
        let router: Router = Router::new().route("/", get(|| async { "ok" })).route(
            "/peer",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
        );
        let shutdown = crate::webserver::Shutdown::manual();
        let signal = shutdown.clone();
        tokio::spawn(serve(listener, router, limits, async move {
            signal.recv().await;
        }));
        (address, shutdown)
    }

    /// Sends one request on `stream` and reads until the server closes it.
    async fn request(stream: &mut TcpStream, path: &str) -> String {
        let head: String = format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).await.expect("write");
        let mut response: String = String::new();
        timeout(WAIT, stream.read_to_string(&mut response))
            .await
            .expect("answered in time")
            .expect("read");
        response
    }

    #[test]
    fn the_first_failure_reports_and_the_rest_of_the_window_does_not() {
        let mut errors: AcceptErrors = AcceptErrors::new();
        let start: Instant = Instant::now();
        assert_eq!(errors.failed(start).report, Some(0));
        assert_eq!(errors.failed(start + Duration::from_secs(1)).report, None);
        assert_eq!(errors.failed(start + Duration::from_secs(59)).report, None);
        assert_eq!(
            errors.failed(start + ACCEPT_REPORT_WINDOW).report,
            Some(2),
            "the next window's report counts what the last one held back"
        );
    }

    #[test]
    fn the_backoff_doubles_to_a_second_and_resets_on_success() {
        let mut errors: AcceptErrors = AcceptErrors::new();
        let now: Instant = Instant::now();
        let waits: Vec<Duration> = (0..9).map(|_| errors.failed(now).wait).collect();
        assert_eq!(waits[0], ACCEPT_BACKOFF_MIN);
        assert_eq!(waits[1], ACCEPT_BACKOFF_MIN * 2);
        assert_eq!(waits[8], ACCEPT_BACKOFF_MAX);
        errors.succeeded();
        assert_eq!(errors.failed(now).wait, ACCEPT_BACKOFF_MIN);
    }

    #[test]
    fn a_zero_cap_still_serves_one() {
        assert_eq!(
            ConnectionLimits::new().max_connections(0).max_connections,
            1
        );
    }

    #[tokio::test]
    async fn serves_requests_with_the_peer_address() {
        let (address, _shutdown) = start(ConnectionLimits::new()).await;
        let mut stream: TcpStream = TcpStream::connect(address).await.expect("connect");
        let response: String = request(&mut stream, "/peer").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("127.0.0.1"), "{response}");
    }

    #[tokio::test]
    async fn a_silent_connection_is_closed_after_the_header_read_timeout() {
        let limits: ConnectionLimits =
            ConnectionLimits::new().header_read_timeout(Duration::from_millis(100));
        let (address, _shutdown) = start(limits).await;
        let mut stream: TcpStream = TcpStream::connect(address).await.expect("connect");
        let mut buffer: Vec<u8> = Vec::new();
        // Before this change the read never ended: nothing closed the socket.
        let read: usize = timeout(WAIT, stream.read_to_end(&mut buffer))
            .await
            .expect("closed by the server")
            .unwrap_or(0);
        assert!(
            read == 0 || buffer.starts_with(b"HTTP/1.1 408"),
            "{buffer:?}"
        );
    }

    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_closed_too() {
        let limits: ConnectionLimits =
            ConnectionLimits::new().header_read_timeout(Duration::from_millis(100));
        let (address, _shutdown) = start(limits).await;
        let mut stream: TcpStream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response: Vec<u8> = Vec::new();
        timeout(WAIT, stream.read_to_end(&mut response))
            .await
            .expect("closed after going idle")
            .expect("read");
        assert!(response.starts_with(b"HTTP/1.1 200"), "{response:?}");
    }

    #[tokio::test]
    async fn past_the_cap_a_connection_waits_for_a_free_slot() {
        let limits: ConnectionLimits = ConnectionLimits::new().max_connections(1);
        let (address, _shutdown) = start(limits).await;
        let holder: TcpStream = TcpStream::connect(address).await.expect("connect");
        let mut waiting: TcpStream = TcpStream::connect(address).await.expect("connect");
        waiting
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .expect("write");
        let mut byte: [u8; 1] = [0];
        assert!(
            timeout(Duration::from_millis(200), waiting.read(&mut byte))
                .await
                .is_err(),
            "served while the only slot was taken"
        );
        drop(holder);
        let mut rest: String = String::new();
        timeout(WAIT, waiting.read_to_string(&mut rest))
            .await
            .expect("served once the slot freed")
            .expect("read");
        assert!(rest.starts_with("HTTP/1.1 200"), "{rest}");
    }

    #[tokio::test]
    async fn shutdown_returns_once_connections_drain() {
        let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address: SocketAddr = listener.local_addr().expect("address");
        let shutdown = crate::webserver::Shutdown::manual();
        let signal = shutdown.clone();
        let serving = tokio::spawn(serve(
            listener,
            Router::new().route("/", get(|| async { "ok" })),
            ConnectionLimits::new(),
            async move { signal.recv().await },
        ));
        let mut idle: TcpStream = TcpStream::connect(address).await.expect("connect");
        assert!(
            request(
                &mut TcpStream::connect(address).await.expect("connect"),
                "/"
            )
            .await
            .starts_with("HTTP/1.1 200")
        );
        idle.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .expect("write");
        shutdown.trigger();
        timeout(WAIT, serving)
            .await
            .expect("drained")
            .expect("no panic");
    }
}
