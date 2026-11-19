//! Deterministic retry and clock helpers: randomness and time come from the caller.

/// The canonical retry rule (infra `docs/canonical-pattern.md` 6d): at most 3 retries, waiting
/// 0.5 s, 1 s and 2 s with +-25% jitter. Retry only network errors, timeouts, 5xx, 408 and 429
/// (see [`RetryPolicy::is_retryable`]). Retry a write only when it is idempotent (keyed). Per-attempt
/// timeouts belong to the caller, per project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_ms: u64,
    pub cap_ms: u64,
    /// Largest fraction (per mille) a delay may move by, either way.
    pub jitter_permille: u64,
}

/// The rule's ceiling on retries.
pub const MAX_RETRIES: u32 = 3;

impl RetryPolicy {
    /// The rule: 3 retries, 0.5 s, 1 s, 2 s, +-25% jitter.
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            base_ms: 500,
            cap_ms: 2000,
            jitter_permille: 250,
        }
    }

    /// Reads follow the rule.
    #[must_use]
    pub const fn read() -> Self {
        Self::standard()
    }

    /// Writes follow the same rule, but the caller must retry only an idempotent (keyed) write.
    #[must_use]
    pub const fn write() -> Self {
        Self::standard()
    }

    /// Delay before retry number `failures` (0 after the first failure); `None` when retries are
    /// spent (never more than 3, whatever `max_retries` says). `rand_permille` is the caller's
    /// random draw in 0..=1000: 0 gives the shortest delay (-25%), 1000 the longest (+25%).
    #[must_use]
    pub fn delay_ms(&self, failures: u32, rand_permille: u32) -> Option<u64> {
        if failures >= self.max_retries.min(MAX_RETRIES) {
            return None;
        }
        let exp: u64 = self
            .base_ms
            .saturating_mul(1u64 << failures.min(32))
            .min(self.cap_ms);
        // Scale by 1 - j .. 1 + j, where j is the jitter fraction.
        let jitter: u64 = self.jitter_permille.min(1000);
        let span: u64 = 2 * jitter * u64::from(rand_permille.min(1000)) / 1000;
        Some(exp * (1000 - jitter + span) / 1000)
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
    fn backoff_is_half_a_second_then_one_then_two_then_stops() {
        for policy in [RetryPolicy::read(), RetryPolicy::write()] {
            let actual: Vec<Option<u64>> = (0..5).map(|n| policy.delay_ms(n, 500)).collect();
            let expected: Vec<Option<u64>> = vec![Some(500), Some(1000), Some(2000), None, None];
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn jitter_is_a_quarter_either_way() {
        let policy: RetryPolicy = RetryPolicy::read();
        assert_eq!(Some(375), policy.delay_ms(0, 0));
        assert_eq!(Some(625), policy.delay_ms(0, 1000));
        assert_eq!(Some(625), policy.delay_ms(0, 5000));
        assert_eq!(Some(750), policy.delay_ms(1, 0));
        assert_eq!(Some(2500), policy.delay_ms(2, 1000));
        for draw in 0..=1000 {
            for failures in 0..3 {
                let nominal: u64 = 500 << failures;
                let got: u64 = policy.delay_ms(failures, draw).unwrap();
                assert!(got >= nominal * 3 / 4 && got <= nominal * 5 / 4);
            }
        }
    }

    #[test]
    fn jitter_above_one_thousand_permille_cannot_underflow() {
        let wild: RetryPolicy = RetryPolicy {
            jitter_permille: 5000,
            ..RetryPolicy::read()
        };
        assert_eq!(Some(0), wild.delay_ms(0, 0));
        assert_eq!(Some(500), wild.delay_ms(0, 500));
        assert_eq!(Some(1000), wild.delay_ms(0, 1000));
    }

    #[test]
    fn never_more_than_three_retries_even_if_asked() {
        let greedy: RetryPolicy = RetryPolicy {
            max_retries: 50,
            ..RetryPolicy::standard()
        };
        let retries: usize = (0..60).filter_map(|n| greedy.delay_ms(n, 500)).count();
        assert_eq!(3, retries);
        assert_eq!(3, RetryPolicy::read().max_retries);
        assert_eq!(500, RetryPolicy::read().base_ms);
        assert_eq!(250, RetryPolicy::read().jitter_permille);
    }

    #[test]
    fn only_transient_statuses_are_retryable() {
        let retryable: Vec<u16> = (0..=599)
            .filter(|s| RetryPolicy::is_retryable(*s))
            .collect();
        let mut expected: Vec<u16> = vec![0, 408, 429];
        expected.extend(500..=599);
        assert_eq!(expected, retryable);
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
