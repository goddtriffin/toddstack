//! Deterministic retry and clock helpers: randomness and time come from the caller.

/// Retry with exponential backoff and jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_ms: u64,
    pub cap_ms: u64,
    /// Largest fraction (per mille) a delay may shrink by.
    pub jitter_permille: u64,
}

impl RetryPolicy {
    /// Reads retry 3 times, 0.25 s up to 4 s.
    #[must_use]
    pub const fn read() -> Self {
        Self {
            max_retries: 3,
            base_ms: 250,
            cap_ms: 4000,
            jitter_permille: 250,
        }
    }

    /// Writes retry 5 times, 0.5 s up to 8 s.
    #[must_use]
    pub const fn write() -> Self {
        Self {
            max_retries: 5,
            base_ms: 500,
            cap_ms: 8000,
            jitter_permille: 250,
        }
    }

    /// Delay before retry number `failures` (0 after the first failure); `None` when retries are
    /// spent. `rand_permille` is the caller's random draw in 0..=1000.
    #[must_use]
    pub fn delay_ms(&self, failures: u32, rand_permille: u32) -> Option<u64> {
        if failures >= self.max_retries {
            return None;
        }
        let exp: u64 = self
            .base_ms
            .saturating_mul(1u64 << failures.min(32))
            .min(self.cap_ms);
        let cut: u64 = exp * self.jitter_permille * u64::from(rand_permille.min(1000)) / 1_000_000;
        Some(exp - cut)
    }

    /// Network failure (status 0), timeout, rate limit and server errors are worth retrying.
    #[must_use]
    pub const fn is_retryable(status: u16) -> bool {
        matches!(status, 0 | 408 | 429 | 500..=599)
    }
}

/// Server clock offset from the best (lowest round trip) sample: the server's time is assumed to
/// be read at the call midpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockOffset {
    offset_ms: i64,
    best_rtt_ms: Option<u64>,
}

impl ClockOffset {
    /// `sent` / `recv`: local ms around the call; `server_ms`: the server's clock in the reply.
    pub fn observe(&mut self, sent: u64, recv: u64, server_ms: u64) {
        let rtt: u64 = recv.saturating_sub(sent);
        if self.best_rtt_ms.is_some_and(|best| rtt > best) {
            return;
        }
        let mid: i128 = i128::from(sent) + i128::from(rtt / 2);
        let offset: i128 = i128::from(server_ms) - mid;
        self.offset_ms = i64::try_from(offset).unwrap_or(0);
        self.best_rtt_ms = Some(rtt);
    }

    #[must_use]
    pub const fn offset_ms(&self) -> i64 {
        self.offset_ms
    }

    /// Server time for a local wall-clock reading.
    #[must_use]
    pub fn server_now(&self, local_ms: u64) -> u64 {
        let v: i128 = i128::from(local_ms) + i128::from(self.offset_ms);
        u64::try_from(v.max(0)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap_then_stops() {
        let policy: RetryPolicy = RetryPolicy::write();
        let actual: Vec<Option<u64>> = (0..6).map(|n| policy.delay_ms(n, 0)).collect();
        let expected: Vec<Option<u64>> = vec![
            Some(500),
            Some(1000),
            Some(2000),
            Some(4000),
            Some(8000),
            None,
        ];
        assert_eq!(expected, actual);
    }

    #[test]
    fn jitter_only_ever_shortens_a_delay() {
        assert_eq!(Some(375), RetryPolicy::write().delay_ms(0, 1000));
        assert_eq!(Some(375), RetryPolicy::write().delay_ms(0, 5000));
    }

    #[test]
    fn only_transient_statuses_are_retryable() {
        let retryable: Vec<u16> = [0, 200, 400, 404, 408, 429, 500, 503]
            .into_iter()
            .filter(|s| RetryPolicy::is_retryable(*s))
            .collect();
        assert_eq!(vec![0, 408, 429, 500, 503], retryable);
    }

    #[test]
    fn the_offset_follows_the_lowest_round_trip() {
        let mut clock: ClockOffset = ClockOffset::default();
        clock.observe(1000, 1100, 5050);
        assert_eq!(4000, clock.offset_ms());
        clock.observe(2000, 2400, 9999);
        assert_eq!(4000, clock.offset_ms());
        assert_eq!(7000, clock.server_now(3000));
    }
}
