//! A deliberate "too busy" reply, which the trace layer must not report as a fault.
//!
//! The trace layer files every 5xx as a failed response at `error`, and
//! `error` reaches Sentry. An app that sheds load on purpose (a full queue, a
//! busy dependency) would turn each shed request into a Sentry event, a flood
//! exactly when the app is busiest. Marking the response keeps it out.

use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Response extension: this 5xx is a deliberate overload or unavailable reply,
/// not a fault. The trace layer treats the response as a success: no
/// `response failed` error and no Sentry event. The request is still logged at
/// `info` with its status and counted in `http_requests_total` under that
/// status, so shedding stays visible in logs and metrics.
///
/// Insert it with `response.extensions_mut().insert(DeliberateOverload)`, or
/// build the whole reply with [`overloaded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliberateOverload;

/// A `503 Service Unavailable` with a `Retry-After` of `retry_after` (rounded
/// up to whole seconds, at least one), marked [`DeliberateOverload`].
#[must_use]
pub fn overloaded(retry_after: Duration) -> Response {
    let seconds: u64 = retry_after
        .as_secs()
        .saturating_add(u64::from(retry_after.subsec_nanos() > 0))
        .max(1);
    let mut response: Response = StatusCode::SERVICE_UNAVAILABLE.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    response.extensions_mut().insert(DeliberateOverload);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_a_503_with_a_retry_after_and_the_marker() {
        let response: Response = overloaded(Duration::from_millis(1500));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "2");
        assert!(response.extensions().get::<DeliberateOverload>().is_some());
    }

    #[test]
    fn the_retry_after_is_at_least_one_second() {
        let response: Response = overloaded(Duration::ZERO);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    }
}
