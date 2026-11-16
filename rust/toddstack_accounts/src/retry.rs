//! Retries for reads from the accounts service.
//!
//! Only a `GET` is ever retried, and only on an intermittent failure: a
//! connect error, a timeout, a 5xx or a 429 (not for notification calls: their
//! limits are hourly). Anything else is an answer, and
//! asking again would get the same one.
//!
//! Everything else is sent exactly once, and must stay that way: the token
//! exchange (an authorization code is single-use), refresh (rotation: a retried
//! refresh that had in fact landed presents a spent token and signs the person
//! out), revocation, and every `POST`, `PUT` and `DELETE` (a retried event or
//! friend request that had landed the first time is a duplicate). [`execute`]
//! enforces this itself, so a caller cannot opt a write in by mistake. The one
//! exception is [`execute_keyed`], for a send that carries an idempotency key:
//! the service replays the recorded answer instead of sending twice, so
//! retrying it cannot duplicate.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::{Duration, Instant};

use reqwest::header::RETRY_AFTER;
use reqwest::{Client, Method, Request, Response};

/// How hard to try before giving up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryPolicy {
    /// Attempts in all, the first included.
    pub(crate) attempts: u32,
    /// Each attempt's own deadline, so one hung connection cannot use up the
    /// whole budget. Fixed per policy: 3 s for reads, 10 s for keyed sends.
    pub(crate) attempt_timeout: Duration,
    /// The backoff ceiling for the first retry; it doubles per retry.
    pub(crate) base_delay: Duration,
    pub(crate) max_delay: Duration,
    /// A `Retry-After` longer than this is obeyed by not retrying at all:
    /// waiting it out would stall the caller, and retrying sooner ignores it.
    pub(crate) max_retry_after: Duration,
    /// Whether a 429 is retried. Off where the server's limits are hourly and
    /// send no `Retry-After`: a retry within seconds cannot succeed.
    pub(crate) retry_429: bool,
    /// No retry starts once this much time has passed, so the total is at
    /// most this plus one `attempt_timeout`.
    pub(crate) budget: Duration,
}

impl RetryPolicy {
    pub(crate) const READ: Self = Self {
        attempts: 3,
        attempt_timeout: Duration::from_secs(3),
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
        max_retry_after: Duration::from_secs(2),
        retry_429: true,
        budget: Duration::from_secs(6),
    };

    /// A notification status read: like [`Self::READ`], but a 429 is final.
    pub(crate) const NOTIFICATION_STATUS: Self = Self {
        retry_429: false,
        ..Self::READ
    };

    /// A notification send that carries an idempotency key. Its deadline is
    /// the client's own, since a send to one player is delivered inline.
    pub(crate) const KEYED_SEND: Self = Self {
        attempts: 3,
        attempt_timeout: Duration::from_secs(10),
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
        max_retry_after: Duration::from_secs(2),
        retry_429: false,
        budget: Duration::from_secs(15),
    };

    /// Full jitter: uniformly random in `[0, min(max_delay, base_delay * 2^(retry - 1))]`,
    /// so many servers that failed together do not retry together.
    fn backoff(&self, retry: u32) -> Duration {
        let ceiling: Duration = self
            .base_delay
            .saturating_mul(1_u32 << retry.saturating_sub(1).min(16))
            .min(self.max_delay);
        let ceiling_ms: u64 = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(random() % ceiling_ms.saturating_add(1))
    }

    /// How long to wait before retrying after `retry` failed attempts, or
    /// `None` when the outcome is final.
    fn delay(&self, retry: u32, outcome: &reqwest::Result<Response>) -> Option<Duration> {
        if retry >= self.attempts {
            return None;
        }
        match outcome {
            Ok(response) => {
                if !is_retryable_status(response.status().as_u16(), self.retry_429) {
                    return None;
                }
                match retry_after(response) {
                    Some(wait) if wait > self.max_retry_after => None,
                    Some(wait) => Some(wait),
                    None => Some(self.backoff(retry)),
                }
            }
            Err(error) if error.is_connect() || error.is_timeout() => Some(self.backoff(retry)),
            Err(_) => None,
        }
    }
}

const fn is_retryable_status(status: u16, retry_429: bool) -> bool {
    (status == 429 && retry_429) || (status >= 500 && status < 600)
}

/// `Retry-After` in delta-seconds. The HTTP-date form falls back to backoff:
/// the accounts service does not send it.
fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// A random `u64` from the standard library's per-instance hash keys, which
/// is plenty for jitter and needs no dependency.
fn random() -> u64 {
    RandomState::new().hash_one(Instant::now())
}

/// Sends `request`: once for anything but a `GET`, and under `policy` for a
/// `GET`. Returns the last outcome: a success, a final answer (any
/// non-retryable status), or the last failure once attempts or the budget run
/// out.
pub(crate) async fn execute(
    client: &Client,
    request: Request,
    policy: &RetryPolicy,
) -> reqwest::Result<Response> {
    run(client, request, policy, false).await
}

/// Like [`execute`], but retries a write too. Only for a request the service
/// deduplicates by an idempotency key.
pub(crate) async fn execute_keyed(
    client: &Client,
    request: Request,
    policy: &RetryPolicy,
) -> reqwest::Result<Response> {
    run(client, request, policy, true).await
}

async fn run(
    client: &Client,
    request: Request,
    policy: &RetryPolicy,
    retry_writes: bool,
) -> reqwest::Result<Response> {
    if request.method() != Method::GET && !retry_writes {
        return client.execute(request).await;
    }
    let started: Instant = Instant::now();
    let mut retry: u32 = 1;
    loop {
        // Neither a GET nor a JSON body is a stream, so it always clones.
        let Some(mut attempt) = request.try_clone() else {
            return client.execute(request).await;
        };
        *attempt.timeout_mut() = Some(policy.attempt_timeout);
        let outcome: reqwest::Result<Response> = client.execute(attempt).await;
        let Some(wait) = policy.delay(retry, &outcome) else {
            return outcome;
        };
        if started.elapsed() + wait > policy.budget {
            return outcome;
        }
        tokio::time::sleep(wait).await;
        retry += 1;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::RetryPolicy;

    pub(crate) const FAST: RetryPolicy = RetryPolicy {
        attempts: 3,
        attempt_timeout: Duration::from_millis(500),
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(20),
        max_retry_after: Duration::from_secs(2),
        retry_429: true,
        budget: Duration::from_secs(4),
    };

    /// A scripted server: request `n` gets reply `n` (the last one repeats),
    /// as `(status, extra header lines, delay)`, and every request is counted.
    pub(crate) struct Scripted {
        pub(crate) url: String,
        hits: Arc<AtomicUsize>,
    }

    impl Scripted {
        pub(crate) async fn start(replies: Vec<(u16, &'static str, Duration)>) -> Self {
            let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url: String = format!("http://{}/api/v1/friends", listener.local_addr().unwrap());
            let hits: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
            let counted: Arc<AtomicUsize> = Arc::clone(&hits);
            tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let n: usize = counted.fetch_add(1, Ordering::SeqCst);
                    let (status, headers, delay) = replies[n.min(replies.len() - 1)];
                    tokio::spawn(async move {
                        let mut request: [u8; 4096] = [0; 4096];
                        let _ = socket.read(&mut request).await;
                        tokio::time::sleep(delay).await;
                        let body: &str = "{}";
                        let response: String = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self { url, hits }
        }

        pub(crate) fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    async fn status_of(server: &Scripted, method: reqwest::Method) -> u16 {
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client.request(method, &server.url).build().unwrap();
        super::execute(&client, request, &FAST)
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    const NOW: Duration = Duration::ZERO;

    async fn keyed_post_status(server: &Scripted) -> u16 {
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client
            .post(&server.url)
            .json(&serde_json::json!({ "idempotency_key": "k1" }))
            .build()
            .unwrap();
        super::execute_keyed(&client, request, &FAST)
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn a_keyed_post_is_retried_on_a_503() {
        let server: Scripted = Scripted::start(vec![(503, "", NOW), (200, "", NOW)]).await;

        assert_eq!(200, keyed_post_status(&server).await);
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_keyed_post_is_not_retried_on_a_429() {
        let server: Scripted = Scripted::start(vec![(429, "", NOW), (200, "", NOW)]).await;
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client
            .post(&server.url)
            .json(&serde_json::json!({ "idempotency_key": "k1" }))
            .build()
            .unwrap();
        let policy: RetryPolicy = RetryPolicy {
            retry_429: false,
            ..FAST
        };
        let status: u16 = super::execute_keyed(&client, request, &policy)
            .await
            .unwrap()
            .status()
            .as_u16();

        assert_eq!(429, status);
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn a_keyed_post_that_is_refused_is_not_retried() {
        let server: Scripted = Scripted::start(vec![(422, "", NOW), (200, "", NOW)]).await;

        assert_eq!(422, keyed_post_status(&server).await);
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn a_get_that_gets_a_503_is_retried_until_it_succeeds() {
        let server: Scripted = Scripted::start(vec![(503, "", NOW), (200, "", NOW)]).await;

        assert_eq!(200, status_of(&server, reqwest::Method::GET).await);
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn retries_stop_after_the_last_attempt() {
        let server: Scripted = Scripted::start(vec![(500, "", NOW)]).await;

        assert_eq!(500, status_of(&server, reqwest::Method::GET).await);
        assert_eq!(3, server.hits());
    }

    #[tokio::test]
    async fn a_400_is_an_answer_and_is_not_retried() {
        let server: Scripted = Scripted::start(vec![(400, "", NOW), (200, "", NOW)]).await;

        assert_eq!(400, status_of(&server, reqwest::Method::GET).await);
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn a_post_is_never_retried_even_on_a_503() {
        let server: Scripted = Scripted::start(vec![(503, "", NOW), (200, "", NOW)]).await;

        assert_eq!(503, status_of(&server, reqwest::Method::POST).await);
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn a_short_retry_after_is_waited_out() {
        let server: Scripted =
            Scripted::start(vec![(429, "retry-after: 1\r\n", NOW), (200, "", NOW)]).await;

        let started: Instant = Instant::now();
        assert_eq!(200, status_of(&server, reqwest::Method::GET).await);
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_retry_after_beyond_the_cap_is_not_waited_for() {
        let server: Scripted =
            Scripted::start(vec![(503, "retry-after: 30\r\n", NOW), (200, "", NOW)]).await;

        let started: Instant = Instant::now();
        assert_eq!(503, status_of(&server, reqwest::Method::GET).await);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn a_hung_attempt_times_out_and_is_retried() {
        let server: Scripted =
            Scripted::start(vec![(200, "", Duration::from_secs(5)), (200, "", NOW)]).await;

        assert_eq!(200, status_of(&server, reqwest::Method::GET).await);
        assert_eq!(2, server.hits());
    }

    #[test]
    fn backoff_stays_within_the_doubling_ceiling_and_the_cap() {
        let policy: RetryPolicy = RetryPolicy {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(250),
            ..RetryPolicy::READ
        };
        for _ in 0..200 {
            assert!(policy.backoff(1) <= Duration::from_millis(100));
            assert!(policy.backoff(2) <= Duration::from_millis(200));
            assert!(policy.backoff(3) <= Duration::from_millis(250));
            assert!(policy.backoff(30) <= Duration::from_millis(250));
        }
    }
}
