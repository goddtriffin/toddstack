//! Retries for reads from the accounts service.
//!
//! This is the canonical retry rule (infra `docs/canonical-pattern.md` 6d): at
//! most 3 retries (4 attempts in all), waiting 0.5 s, 1 s and 2 s with +-25%
//! jitter. The whole call, attempts and waits together, never takes longer than
//! 15 s (every policy's `budget`), because it runs inside a caller's HTTP
//! request handler: a retry starts only if it can finish inside that budget.
//! Only a `GET` is ever retried, and only on an intermittent failure:
//! a connect error, a timeout, a 5xx, a 408 or a 429 (see
//! [`RetryPolicy::retry_429_without_hint`] for the hourly-limited calls). Anything else is an
//! answer, and asking again would get the same one. After the last retry the
//! caller gets the plain last outcome (this crate has no Sentry).
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
    /// Retries after the first attempt. The rule's ceiling is 3.
    pub(crate) max_retries: u32,
    /// Each attempt's own deadline, so one hung connection cannot use up the
    /// whole budget. Set per policy: 3 s for reads, 10 s for keyed sends.
    pub(crate) attempt_timeout: Duration,
    /// The nominal wait before the first retry; it doubles per retry.
    pub(crate) base_delay: Duration,
    /// A `Retry-After` longer than this is obeyed by not retrying at all:
    /// waiting it out would stall the caller, and retrying sooner ignores it.
    /// 5 s: twice the longest backoff, so a short hint is honoured.
    pub(crate) max_retry_after: Duration,
    /// The cap on the whole call, 15 s for every policy: a retry starts only
    /// if its wait plus its own `attempt_timeout` still fits, so the total
    /// never exceeds this.
    pub(crate) budget: Duration,
    /// Whether a 429 with no `Retry-After` is retried (with backoff). The
    /// server's write and notification limits are hourly, so backing off a
    /// few seconds cannot clear one and retrying only burns the caller's time:
    /// for those policies a 429 is retried only when it carries a
    /// `Retry-After` within `max_retry_after`, and is otherwise final. Reads
    /// have short limits and keep retrying a bare 429.
    pub(crate) retry_429_without_hint: bool,
}

/// The rule's ceiling on retries.
pub(crate) const MAX_RETRIES: u32 = 3;

/// The cap on a whole call, every policy.
const BUDGET: Duration = Duration::from_secs(15);

/// The longest a `Retry-After` is waited out.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);

impl RetryPolicy {
    pub(crate) const READ: Self = Self {
        max_retries: MAX_RETRIES,
        attempt_timeout: Duration::from_secs(3),
        base_delay: Duration::from_millis(500),
        max_retry_after: MAX_RETRY_AFTER,
        budget: BUDGET,
        retry_429_without_hint: true,
    };

    /// A notification status read: the same as [`Self::READ`], except that
    /// the notification limit is hourly, so a bare 429 is final.
    pub(crate) const NOTIFICATION_STATUS: Self = Self {
        retry_429_without_hint: false,
        ..Self::READ
    };

    /// A notification send that carries an idempotency key. Its deadline is
    /// the client's own, since a send to one player is delivered inline.
    pub(crate) const KEYED_SEND: Self = Self {
        max_retries: MAX_RETRIES,
        attempt_timeout: Duration::from_secs(10),
        base_delay: Duration::from_millis(500),
        max_retry_after: MAX_RETRY_AFTER,
        budget: BUDGET,
        retry_429_without_hint: false,
    };

    /// The wait before retry number `retry` (1 is the first): `base_delay`
    /// doubled per retry (0.5 s, 1 s, 2 s), scaled by a jitter factor between
    /// 0.75 and 1.25. `draw` is a random number in `0..=1000` (per mille).
    fn backoff_for(&self, retry: u32, draw: u64) -> Duration {
        let nominal_ms: u64 = u64::try_from(
            self.base_delay
                .saturating_mul(1_u32 << retry.saturating_sub(1).min(16))
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        // 750..=1250 per mille.
        let factor: u64 = 750 + draw.min(1000) / 2;
        Duration::from_millis(nominal_ms.saturating_mul(factor) / 1000)
    }

    fn backoff(&self, retry: u32) -> Duration {
        self.backoff_for(retry, random() % 1001)
    }

    /// How long to wait before retrying after `retry` failed attempts, or
    /// `None` when the outcome is final.
    fn delay(&self, retry: u32, outcome: &reqwest::Result<Response>) -> Option<Duration> {
        if retry > self.max_retries.min(MAX_RETRIES) {
            return None;
        }
        match outcome {
            Ok(response) => {
                if !is_retryable_status(response.status().as_u16()) {
                    return None;
                }
                match retry_after(response) {
                    Some(wait) if wait > self.max_retry_after => None,
                    Some(wait) => Some(wait),
                    None if response.status().as_u16() == 429 && !self.retry_429_without_hint => {
                        None
                    }
                    None => Some(self.backoff(retry)),
                }
            }
            Err(error) if error.is_connect() || error.is_timeout() => Some(self.backoff(retry)),
            Err(_) => None,
        }
    }
}

const fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
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
    run_counting(client, request, policy, retry_writes, &mut 0).await
}

/// [`run`], also reporting how many attempts were made.
async fn run_counting(
    client: &Client,
    request: Request,
    policy: &RetryPolicy,
    retry_writes: bool,
    attempts: &mut u32,
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
        *attempts += 1;
        let Some(wait) = policy.delay(retry, &outcome) else {
            return outcome;
        };
        if started.elapsed() + wait + policy.attempt_timeout > policy.budget {
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
        max_retries: 3,
        attempt_timeout: Duration::from_millis(500),
        base_delay: Duration::from_millis(10),
        max_retry_after: Duration::from_secs(2),
        budget: Duration::from_secs(4),
        retry_429_without_hint: true,
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
    async fn a_keyed_post_is_retried_on_a_429_with_a_short_retry_after() {
        let server: Scripted =
            Scripted::start(vec![(429, "retry-after: 1\r\n", NOW), (200, "", NOW)]).await;

        assert_eq!(200, keyed_post_status(&server).await);
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_hourly_limited_429_without_a_retry_after_is_final() {
        let server: Scripted = Scripted::start(vec![(429, "", NOW), (200, "", NOW)]).await;
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client
            .post(&server.url)
            .json(&serde_json::json!({ "idempotency_key": "k1" }))
            .build()
            .unwrap();
        let hourly: RetryPolicy = RetryPolicy {
            retry_429_without_hint: false,
            ..FAST
        };

        let status: u16 = super::execute_keyed(&client, request, &hourly)
            .await
            .unwrap()
            .status()
            .as_u16();

        assert_eq!(429, status);
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn a_read_429_without_a_retry_after_is_retried() {
        let server: Scripted = Scripted::start(vec![(429, "", NOW), (200, "", NOW)]).await;

        assert_eq!(200, status_of(&server, reqwest::Method::GET).await);
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_retry_that_cannot_finish_inside_the_budget_is_not_started() {
        let server: Scripted = Scripted::start(vec![(503, "", NOW)]).await;
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client.get(&server.url).build().unwrap();
        let tight: RetryPolicy = RetryPolicy {
            budget: Duration::from_millis(400),
            ..FAST
        };

        super::execute(&client, request, &tight).await.unwrap();

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
        assert_eq!(4, server.hits());
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
    fn backoff_is_half_a_second_then_one_then_two_with_quarter_jitter() {
        let policy: RetryPolicy = RetryPolicy::READ;
        for (retry, nominal_ms) in [(1_u32, 500_u64), (2, 1000), (3, 2000)] {
            assert_eq!(
                Duration::from_millis(nominal_ms * 3 / 4),
                policy.backoff_for(retry, 0)
            );
            assert_eq!(
                Duration::from_millis(nominal_ms),
                policy.backoff_for(retry, 500)
            );
            assert_eq!(
                Duration::from_millis(nominal_ms * 5 / 4),
                policy.backoff_for(retry, 1000)
            );
            for _ in 0..200 {
                let wait: Duration = policy.backoff(retry);
                assert!(wait >= Duration::from_millis(nominal_ms * 3 / 4));
                assert!(wait <= Duration::from_millis(nominal_ms * 5 / 4));
            }
        }
    }

    #[test]
    fn every_policy_makes_at_most_three_retries() {
        for policy in [
            RetryPolicy::READ,
            RetryPolicy::NOTIFICATION_STATUS,
            RetryPolicy::KEYED_SEND,
        ] {
            assert_eq!(3, policy.max_retries);
            assert_eq!(Duration::from_millis(500), policy.base_delay);
            assert!(policy.max_retry_after <= Duration::from_secs(5));
            assert_eq!(Duration::from_secs(15), policy.budget);
        }
    }

    #[test]
    fn only_network_timeout_5xx_408_and_429_statuses_are_retried() {
        let retried: Vec<u16> = (100_u16..600)
            .filter(|status| super::is_retryable_status(*status))
            .collect();
        let mut expected: Vec<u16> = vec![408, 429];
        expected.extend(500..600);
        expected.sort_unstable();
        assert_eq!(expected, retried);
    }

    #[tokio::test]
    async fn a_408_is_retried() {
        let server: Scripted = Scripted::start(vec![(408, "", NOW), (200, "", NOW)]).await;

        assert_eq!(200, status_of(&server, reqwest::Method::GET).await);
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_connection_error_is_retried_three_times_then_given_up() {
        let listener: std::net::TcpListener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url: String = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client.get(&url).build().unwrap();
        let mut attempts: u32 = 0;

        let outcome = super::run_counting(&client, request, &FAST, false, &mut attempts).await;

        assert!(outcome.unwrap_err().is_connect());
        assert_eq!(4, attempts);
    }

    #[tokio::test]
    async fn a_policy_asking_for_more_than_three_retries_is_held_to_three() {
        let server: Scripted = Scripted::start(vec![(503, "", NOW)]).await;
        let client: reqwest::Client = reqwest::Client::new();
        let request: reqwest::Request = client.get(&server.url).build().unwrap();
        let greedy: RetryPolicy = RetryPolicy {
            max_retries: 50,
            ..FAST
        };

        super::execute(&client, request, &greedy).await.unwrap();

        assert_eq!(4, server.hits());
    }
}
