//! Retries for idempotent upstream reads (the proxied scripts).
//!
//! Only intermittent failures are retried: a connect error, a timeout, a 5xx
//! or a 429. Anything else is an answer, and asking again would get the same
//! one. Writes (Plausible events, Sentry envelopes, Telegram messages) are
//! never sent through here: a retried write that had in fact landed the first
//! time is a duplicate pageview or a duplicate error report.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;

use axum::http::{StatusCode, header};
use tokio::time::Instant;

/// How hard to try before giving up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryPolicy {
    /// Attempts in all, the first included.
    pub(crate) attempts: u32,
    /// Each attempt's own deadline, so one hung connection cannot use up the
    /// whole budget.
    pub(crate) attempt_timeout: Duration,
    /// The backoff ceiling for the first retry; it doubles per retry.
    pub(crate) base_delay: Duration,
    pub(crate) max_delay: Duration,
    /// A `Retry-After` longer than this is obeyed by not retrying at all:
    /// waiting it out would stall the caller, and retrying sooner ignores it.
    pub(crate) max_retry_after: Duration,
    /// No retry starts once this much time has passed, so the total is at
    /// most this plus one `attempt_timeout`.
    pub(crate) budget: Duration,
}

impl RetryPolicy {
    pub(crate) const READ: Self = Self {
        attempts: 3,
        attempt_timeout: Duration::from_secs(2),
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
        max_retry_after: Duration::from_secs(2),
        budget: Duration::from_secs(4),
    };

    /// Full jitter: uniformly random in `[0, min(max_delay, base_delay * 2^(retry - 1))]`,
    /// so a fleet of servers that failed together does not retry together.
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
    fn delay(&self, retry: u32, outcome: &reqwest::Result<reqwest::Response>) -> Option<Duration> {
        if retry >= self.attempts {
            return None;
        }
        match outcome {
            Ok(response) => {
                let status: u16 = response.status().as_u16();
                if !is_retryable_status(status) {
                    return None;
                }
                match retry_after(response.headers()) {
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

const fn is_retryable_status(status: u16) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS.as_u16() || (status >= 500 && status < 600)
}

/// `Retry-After` in delta-seconds. The HTTP-date form is not honoured (it
/// falls back to backoff): no upstream proxied here sends it.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER.as_str())?
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

/// `GET url` under `policy`. Returns the last outcome: a success, a final
/// answer (any non-retryable status), or the last failure once attempts or
/// the budget run out.
pub(crate) async fn get(
    client: &reqwest::Client,
    url: &str,
    policy: &RetryPolicy,
) -> reqwest::Result<reqwest::Response> {
    let started: Instant = Instant::now();
    let mut retry: u32 = 1;
    loop {
        let outcome: reqwest::Result<reqwest::Response> =
            client.get(url).timeout(policy.attempt_timeout).send().await;
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
mod tests {
    use std::time::{Duration, Instant};

    use super::RetryPolicy;
    use crate::analytics::fake_upstream::{FakeUpstream, Reply};

    const POLICY: RetryPolicy = RetryPolicy {
        attempts: 5,
        attempt_timeout: Duration::from_secs(1),
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_millis(250),
        max_retry_after: Duration::from_secs(1),
        budget: Duration::from_secs(10),
    };

    #[test]
    fn backoff_stays_within_the_doubling_ceiling_and_the_cap() {
        for _ in 0..200 {
            assert!(POLICY.backoff(1) <= Duration::from_millis(100));
            assert!(POLICY.backoff(2) <= Duration::from_millis(200));
            assert!(POLICY.backoff(3) <= Duration::from_millis(250));
            assert!(POLICY.backoff(30) <= Duration::from_millis(250));
        }
    }

    #[test]
    fn backoff_is_jittered_rather_than_fixed() {
        let delays: std::collections::HashSet<Duration> =
            (0..50).map(|_| POLICY.backoff(3)).collect();
        assert!(delays.len() > 1, "{delays:?}");
    }

    #[test]
    fn only_rate_limits_and_server_errors_are_retryable() {
        let expected: Vec<bool> = vec![false, false, false, true, true, true, true];
        let actual: Vec<bool> = [200, 400, 404, 429, 500, 503, 504]
            .into_iter()
            .map(super::is_retryable_status)
            .collect();
        assert_eq!(expected, actual);
    }

    const FAST: RetryPolicy = RetryPolicy {
        attempts: 3,
        attempt_timeout: Duration::from_millis(500),
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(20),
        max_retry_after: Duration::from_secs(2),
        budget: Duration::from_secs(4),
    };

    async fn status_of_get(upstream: &FakeUpstream) -> u16 {
        super::get(&reqwest::Client::new(), &upstream.url, &FAST)
            .await
            .expect("an answer")
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn a_503_is_retried_until_it_succeeds() {
        let upstream: FakeUpstream =
            FakeUpstream::start(vec![Reply::status(503), Reply::status(200)]).await;

        assert_eq!(200, status_of_get(&upstream).await);
        assert_eq!(2, upstream.hits());
    }

    #[tokio::test]
    async fn retries_stop_after_the_last_attempt() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![Reply::status(500)]).await;

        assert_eq!(500, status_of_get(&upstream).await);
        assert_eq!(3, upstream.hits());
    }

    #[tokio::test]
    async fn a_400_is_an_answer_and_is_not_retried() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![Reply::status(400)]).await;

        assert_eq!(400, status_of_get(&upstream).await);
        assert_eq!(1, upstream.hits());
    }

    #[tokio::test]
    async fn a_short_retry_after_is_waited_out() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![
            Reply::status(429).header("retry-after", "1"),
            Reply::status(200),
        ])
        .await;

        let started: Instant = Instant::now();
        assert_eq!(200, status_of_get(&upstream).await);
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(2, upstream.hits());
    }

    #[tokio::test]
    async fn a_retry_after_beyond_the_cap_is_not_waited_for() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![
            Reply::status(503).header("retry-after", "30"),
            Reply::status(200),
        ])
        .await;

        let started: Instant = Instant::now();
        assert_eq!(503, status_of_get(&upstream).await);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(1, upstream.hits());
    }

    #[tokio::test]
    async fn a_hung_attempt_times_out_and_is_retried() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![
            Reply::status(200).after(Duration::from_secs(5)),
            Reply::status(200),
        ])
        .await;

        assert_eq!(200, status_of_get(&upstream).await);
        assert_eq!(2, upstream.hits());
    }
}
