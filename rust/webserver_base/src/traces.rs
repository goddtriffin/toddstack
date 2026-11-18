//! How much of the traffic Sentry traces.
//!
//! Browser and server are separate knobs: a page load is a few spans and a
//! visitor opens many, a server request is one span per handler. Both default
//! here, so every site traces alike unless it names its own rate.

use std::fmt::{self, Display, Formatter};

/// The browser default: the share of page loads that send a trace.
///
/// Placeholder until real traffic numbers set it. Changing it here changes
/// every site that has not chosen its own.
pub const DEFAULT_BROWSER_TRACES_SAMPLE_RATE: f32 = 0.2;

/// The server default: the share of dynamic requests that send a trace.
///
/// Placeholder until real traffic numbers set it. Low because the server sees
/// every request, including every browser-initiated one.
pub const DEFAULT_SERVER_TRACES_SAMPLE_RATE: f32 = 0.001;

/// The `op` of a request the server traces. Anything else is dropped by the
/// sampler.
#[cfg(any(feature = "webserver", feature = "observability"))]
pub(crate) const HTTP_SERVER_OP: &str = "http.server";

/// Response extension: this 5xx is an upstream's, relayed as-is, not a fault of
/// this server. The server's trace layer treats the response as a success, so
/// `tower_http` logs no `response failed` error and Sentry files no event for
/// it. The relay logs a `warn` itself.
#[cfg(any(feature = "webserver", feature = "analytics"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct UpstreamFailure;

/// A share of traces to send, from `0.0` (off) to `1.0` (all).
///
/// `0.0` means tracing is off: nothing is loaded, started or sent for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracesSampleRate(f32);

/// A rate that is not a number from `0.0` to `1.0`.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("traces sample rate {rate} is not between 0.0 and 1.0; use 0.0 to turn tracing off")]
pub struct TracesSampleRateError {
    rate: f32,
}

impl TracesSampleRate {
    /// Tracing off.
    pub const OFF: Self = Self(0.0);

    /// Every trace sent.
    pub const ALL: Self = Self(1.0);

    /// The browser default, [`DEFAULT_BROWSER_TRACES_SAMPLE_RATE`].
    pub const BROWSER_DEFAULT: Self = Self(DEFAULT_BROWSER_TRACES_SAMPLE_RATE);

    /// The server default, [`DEFAULT_SERVER_TRACES_SAMPLE_RATE`].
    pub const SERVER_DEFAULT: Self = Self(DEFAULT_SERVER_TRACES_SAMPLE_RATE);

    /// A rate of `rate`, from `0.0` to `1.0` inclusive.
    ///
    /// # Errors
    ///
    /// [`TracesSampleRateError`] if `rate` is `NaN`, infinite or outside that
    /// range.
    pub fn new(rate: f32) -> Result<Self, TracesSampleRateError> {
        if (0.0..=1.0).contains(&rate) {
            Ok(Self(rate))
        } else {
            Err(TracesSampleRateError { rate })
        }
    }

    /// The rate, from `0.0` to `1.0`.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    /// Whether any trace can be sent.
    #[must_use]
    pub const fn is_on(self) -> bool {
        self.0 > 0.0
    }
}

impl Display for TracesSampleRate {
    /// The shortest decimal that reads back as the same `f32` (`0.2`, never
    /// `0.20000000298`), which is what a script literal needs.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl TryFrom<f32> for TracesSampleRate {
    type Error = TracesSampleRateError;

    fn try_from(rate: f32) -> Result<Self, Self::Error> {
        Self::new(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BROWSER_TRACES_SAMPLE_RATE, DEFAULT_SERVER_TRACES_SAMPLE_RATE, TracesSampleRate,
    };

    #[test]
    fn both_ends_of_the_range_are_valid() {
        assert_eq!(Ok(TracesSampleRate::OFF), TracesSampleRate::new(0.0));
        assert_eq!(Ok(TracesSampleRate::ALL), TracesSampleRate::new(1.0));
        assert_eq!(0.25, TracesSampleRate::new(0.25).expect("valid").get());
    }

    #[test]
    fn a_rate_outside_the_range_or_not_a_number_is_rejected() {
        for rate in [
            -0.001,
            1.001,
            2.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert!(TracesSampleRate::new(rate).is_err(), "{rate}");
        }
    }

    #[test]
    fn the_error_says_what_to_do() {
        let message: String = TracesSampleRate::new(1.5)
            .expect_err("too high")
            .to_string();
        assert_eq!(
            "traces sample rate 1.5 is not between 0.0 and 1.0; use 0.0 to turn tracing off",
            message
        );
    }

    #[test]
    fn only_a_rate_above_zero_is_on() {
        assert!(!TracesSampleRate::OFF.is_on());
        assert!(TracesSampleRate::SERVER_DEFAULT.is_on());
        assert!(TracesSampleRate::ALL.is_on());
    }

    #[test]
    fn the_defaults_are_the_named_constants() {
        assert_eq!(
            DEFAULT_BROWSER_TRACES_SAMPLE_RATE,
            TracesSampleRate::BROWSER_DEFAULT.get()
        );
        assert_eq!(
            DEFAULT_SERVER_TRACES_SAMPLE_RATE,
            TracesSampleRate::SERVER_DEFAULT.get()
        );
    }

    #[test]
    fn a_rate_prints_as_the_shortest_decimal_for_a_script_literal() {
        assert_eq!("0.2", TracesSampleRate::BROWSER_DEFAULT.to_string());
        assert_eq!("0.001", TracesSampleRate::SERVER_DEFAULT.to_string());
        assert_eq!("1", TracesSampleRate::ALL.to_string());
        assert_eq!("0", TracesSampleRate::OFF.to_string());
    }
}
