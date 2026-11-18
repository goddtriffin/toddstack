//! First-party proxies for Plausible and the Sentry browser SDK.
//!
//! Both are **byte relays**. Nothing is parsed, rewritten, or re-reported: the
//! upstream receives exactly what the browser sent, and the browser receives
//! exactly what the upstream returned, headers included. That matters most for
//! Sentry — a server that *interpreted* a client error and re-raised it through
//! its own SDK would file a browser problem as a server one, with the wrong
//! stack and the wrong context. Relaying the envelope keeps it a genuine
//! browser event.
//!
//! The point of proxying at all is that a same-origin request is
//! indistinguishable from the site's own assets. Plausible puts the cost of not
//! doing it at "typically between 5% and 25%" of visitors, depending on
//! audience.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::time::Instant;
use tracing::{error, instrument, warn};

use super::ip::resolve_true_client_ip_address;
use super::retry::{self, RetryPolicy};
use crate::traces::UpstreamFailure;

/// `status` as a response, marked [`UpstreamFailure`] when it is a 5xx, with a
/// `warn` naming the `what` that failed.
fn relayed(status: StatusCode, what: &str) -> Response {
    let mut response: Response = status.into_response();
    if status.is_server_error() {
        warn!("upstream answered {status} to {what}");
        response.extensions_mut().insert(UpstreamFailure);
    }
    response
}

/// Fetches an upstream script and returns it with the upstream's own headers.
///
/// Content type, cache policy and `ETag` are echoed rather than invented, so
/// the proxied script behaves exactly as the vendor intends it to.
/// Intermittent upstream failures are retried for a few seconds. Uncached: a
/// site serves its scripts through [`ScriptRelay`], which keeps a copy.
#[instrument(skip_all)]
pub async fn relay_script(client: &reqwest::Client, upstream: &str) -> Response {
    match fetch_script(client, upstream, &RetryPolicy::READ).await {
        Ok(script) => script.respond(&HeaderMap::new()),
        Err(status) => {
            error!("failed to fetch `{upstream}`");
            status.into_response()
        }
    }
}

/// How long a cached script is served before it is refreshed. Refreshing
/// happens in the background, so no visitor waits on it.
const SCRIPT_TTL: Duration = Duration::from_secs(300);

/// A copy this old that still cannot be refreshed is reported: visitors are
/// fine, but the upstream URL has probably stopped working for good.
const STALE_ALARM: Duration = Duration::from_hours(24);

/// A proxied third-party script, held in memory.
///
/// Keeps the vendor's CDN off the page's critical path: after the first fetch
/// every request is answered from memory, a copy older than five minutes is
/// refreshed in the background, and a failed refresh keeps serving the last
/// good copy. Only a cold cache waits on the upstream, and then for a few
/// seconds of retries at most. `If-None-Match` is answered with `304` from the
/// cached `ETag`.
///
/// Cheap to clone: clones share one cache.
#[derive(Clone)]
pub struct ScriptRelay {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    upstream: String,
    policy: RetryPolicy,
    ttl: Duration,
    cached: Mutex<Option<Script>>,
    refreshing: AtomicBool,
}

#[derive(Clone)]
struct Script {
    headers: HeaderMap,
    body: Bytes,
    fetched_at: Instant,
}

impl ScriptRelay {
    /// A relay for `upstream`, empty until its first request.
    #[must_use]
    pub fn new(client: reqwest::Client, upstream: impl Into<String>) -> Self {
        Self::with(client, upstream.into(), RetryPolicy::READ, SCRIPT_TTL)
    }

    fn with(client: reqwest::Client, upstream: String, policy: RetryPolicy, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                upstream,
                policy,
                ttl,
                cached: Mutex::new(None),
                refreshing: AtomicBool::new(false),
            }),
        }
    }

    /// The script, answering `request_headers`' `If-None-Match` when it can.
    #[instrument(skip_all)]
    pub async fn respond(&self, request_headers: &HeaderMap) -> Response {
        if let Some(script) = self.inner.cached() {
            if script.fetched_at.elapsed() > self.inner.ttl {
                self.refresh_in_background();
            }
            return script.respond(request_headers);
        }
        match self.inner.fetch().await {
            Ok(script) => script.respond(request_headers),
            Err(status) => {
                error!(
                    "failed to fetch `{}` with no copy cached",
                    self.inner.upstream
                );
                status.into_response()
            }
        }
    }

    fn refresh_in_background(&self) {
        // One refresh at a time: a busy page would otherwise start one per
        // request until the first finished.
        if self.inner.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner: Arc<Inner> = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if inner.fetch().await.is_err() {
                let age: Duration = inner
                    .cached()
                    .map_or(Duration::ZERO, |script| script.fetched_at.elapsed());
                if age > STALE_ALARM {
                    error!(
                        "`{}` has failed to refresh for {}h; still serving the last good copy",
                        inner.upstream,
                        age.as_secs() / 3600
                    );
                } else {
                    warn!(
                        "failed to refresh `{}`; serving the last good copy",
                        inner.upstream
                    );
                }
            }
            inner.refreshing.store(false, Ordering::Release);
        });
    }
}

impl Inner {
    fn cached(&self) -> Option<Script> {
        self.cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Fetches and caches the script. Only a success is cached, so a failure
    /// never replaces a good copy.
    async fn fetch(&self) -> Result<Script, StatusCode> {
        let script: Script = fetch_script(&self.client, &self.upstream, &self.policy).await?;
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some(script.clone());
        Ok(script)
    }
}

impl Script {
    fn respond(&self, request_headers: &HeaderMap) -> Response {
        let not_modified: bool = match (
            self.headers.get(header::ETAG),
            request_headers.get(header::IF_NONE_MATCH),
        ) {
            (Some(etag), Some(wanted)) => wanted.to_str().is_ok_and(|wanted: &str| {
                wanted
                    .split(',')
                    .any(|tag: &str| tag.trim().as_bytes() == etag.as_bytes())
            }),
            _ => false,
        };
        if not_modified {
            return (StatusCode::NOT_MODIFIED, self.headers.clone()).into_response();
        }
        (StatusCode::OK, self.headers.clone(), self.body.clone()).into_response()
    }
}

/// A successful fetch, or the status to answer when there is nothing better:
/// the upstream's own, or `502` when it never answered.
async fn fetch_script(
    client: &reqwest::Client,
    upstream: &str,
    policy: &RetryPolicy,
) -> Result<Script, StatusCode> {
    let response: reqwest::Response = match retry::get(client, upstream, policy).await {
        Ok(response) => response,
        Err(error) => {
            warn!("failed to fetch `{upstream}`: {error}");
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status: StatusCode =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        warn!("`{upstream}` answered {status}");
        return Err(status);
    }
    let mut headers: HeaderMap = HeaderMap::new();
    // ETag too, so a revalidation still costs nothing when the script has not
    // changed.
    for name in [header::CONTENT_TYPE, header::CACHE_CONTROL, header::ETAG] {
        if let Some(value) = response.headers().get(&name)
            && let Ok(value) = HeaderValue::from_bytes(value.as_bytes())
        {
            headers.insert(name, value);
        }
    }

    match response.bytes().await {
        Ok(body) => Ok(Script {
            headers,
            body,
            fetched_at: Instant::now(),
        }),
        Err(error) => {
            warn!("failed to read `{upstream}`: {error}");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

/// Forwards a Plausible event, carrying the real visitor IP.
///
/// `X-Forwarded-For` is not optional: without it Plausible's bot filter rejects
/// proxied events outright, because every one of them would appear to originate
/// from this server.
///
/// Sent once, never retried: a retry of an event that had in fact landed is a
/// duplicate pageview.
#[instrument(skip_all)]
pub async fn relay_event(
    client: &reqwest::Client,
    upstream: &str,
    headers: &HeaderMap,
    peer: SocketAddr,
    body: Bytes,
) -> Response {
    let mut request: reqwest::RequestBuilder = client.post(upstream).body(body).header(
        "X-Forwarded-For",
        resolve_true_client_ip_address(peer, headers),
    );
    if let Some(user_agent) = headers.get(header::USER_AGENT) {
        request = request.header(header::USER_AGENT, user_agent.clone());
    }
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        request = request.header(header::CONTENT_TYPE, content_type.clone());
    }

    match request.send().await {
        Ok(response) => relayed(
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::ACCEPTED),
            "an analytics event",
        ),
        Err(error) => {
            error!("failed to forward an analytics event: {error}");
            // A dropped pageview must never surface to the visitor.
            StatusCode::ACCEPTED.into_response()
        }
    }
}

/// Relays a Sentry envelope verbatim.
///
/// The body is passed through untouched — it is a complete envelope the browser
/// SDK built, carrying its own stack trace, breadcrumbs and release. The
/// destination comes from the configured DSN rather than from the envelope
/// header, so this cannot be pointed at an arbitrary Sentry project.
///
/// Sent once, never retried: a retry of an envelope that had in fact landed is
/// a duplicate error report.
#[instrument(skip_all)]
pub async fn relay_envelope(client: &reqwest::Client, upstream: &str, body: Bytes) -> Response {
    match client
        .post(upstream)
        .header(header::CONTENT_TYPE, "application/x-sentry-envelope")
        .body(body)
        .send()
        .await
    {
        Ok(response) => relayed(
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK),
            "a Sentry envelope",
        ),
        Err(error) => {
            error!("failed to relay a Sentry envelope: {error}");
            StatusCode::OK.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::to_bytes;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::Response;

    use super::ScriptRelay;
    use crate::analytics::fake_upstream::{FakeUpstream, Reply};
    use crate::analytics::retry::RetryPolicy;

    const FAST: RetryPolicy = RetryPolicy {
        attempts: 3,
        attempt_timeout: Duration::from_millis(500),
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(20),
        max_retry_after: Duration::from_secs(2),
        budget: Duration::from_secs(2),
    };

    fn relay(upstream: &FakeUpstream, ttl: Duration) -> ScriptRelay {
        ScriptRelay::with(reqwest::Client::new(), upstream.url.clone(), FAST, ttl)
    }

    async fn body(response: Response) -> String {
        let bytes: axum::body::Bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    /// Lets a background refresh run to completion.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    #[tokio::test]
    async fn a_cached_script_is_served_without_asking_the_upstream_again() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![Reply::script("v1")]).await;
        let relay: ScriptRelay = relay(&upstream, Duration::from_secs(300));

        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        assert_eq!(1, upstream.hits());
    }

    #[tokio::test]
    async fn the_vendors_headers_are_echoed() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![
            Reply::script("v1")
                .header("content-type", "application/javascript")
                .header("cache-control", "public, max-age=60"),
        ])
        .await;
        let relay: ScriptRelay = relay(&upstream, Duration::from_secs(300));

        let response: Response = relay.respond(&HeaderMap::new()).await;
        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            Some("application/javascript"),
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value: &HeaderValue| value.to_str().ok())
        );
        assert_eq!(
            Some("public, max-age=60"),
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value: &HeaderValue| value.to_str().ok())
        );
    }

    #[tokio::test]
    async fn a_matching_if_none_match_gets_a_304_from_the_cache() {
        let upstream: FakeUpstream =
            FakeUpstream::start(vec![Reply::script("v1").header("etag", "\"abc\"")]).await;
        let relay: ScriptRelay = relay(&upstream, Duration::from_secs(300));
        let mut headers: HeaderMap = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"zzz\", \"abc\""),
        );

        assert_eq!(
            StatusCode::NOT_MODIFIED,
            relay.respond(&headers).await.status()
        );
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"zzz\""));
        assert_eq!(StatusCode::OK, relay.respond(&headers).await.status());
        assert_eq!(1, upstream.hits());
    }

    #[tokio::test]
    async fn an_expired_copy_is_served_at_once_and_refreshed_in_the_background() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![Reply::script("v1")]).await;
        let relay: ScriptRelay = relay(&upstream, Duration::ZERO);
        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        upstream.reply_from_now_on(Reply::script("v2").after(Duration::from_millis(100)));

        // The slow refresh does not hold up the visitor.
        let started: std::time::Instant = std::time::Instant::now();
        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        assert!(started.elapsed() < Duration::from_millis(100));
        settle().await;
        assert_eq!("v2", body(relay.respond(&HeaderMap::new()).await).await);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_serving_the_last_good_copy() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![Reply::script("v1")]).await;
        let relay: ScriptRelay = relay(&upstream, Duration::ZERO);
        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        upstream.reply_from_now_on(Reply::status(503));

        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        settle().await;
        let response: Response = relay.respond(&HeaderMap::new()).await;
        assert_eq!(StatusCode::OK, response.status());
        assert_eq!("v1", body(response).await);
    }

    #[tokio::test]
    async fn a_cold_cache_retries_an_intermittent_failure() {
        let upstream: FakeUpstream =
            FakeUpstream::start(vec![Reply::status(503), Reply::script("v1")]).await;
        let relay: ScriptRelay = relay(&upstream, Duration::from_secs(300));

        assert_eq!("v1", body(relay.respond(&HeaderMap::new()).await).await);
        assert_eq!(2, upstream.hits());
    }

    #[tokio::test]
    async fn a_failure_is_answered_with_the_upstreams_status_and_never_cached() {
        let upstream: FakeUpstream = FakeUpstream::start(vec![Reply::status(404)]).await;
        let relay: ScriptRelay = relay(&upstream, Duration::from_secs(300));

        assert_eq!(
            StatusCode::NOT_FOUND,
            relay.respond(&HeaderMap::new()).await.status()
        );
        assert_eq!(
            StatusCode::NOT_FOUND,
            relay.respond(&HeaderMap::new()).await.status()
        );
        assert_eq!(2, upstream.hits());
    }

    #[tokio::test]
    async fn an_upstream_that_never_answers_is_a_bad_gateway() {
        let relay: ScriptRelay = ScriptRelay::with(
            reqwest::Client::new(),
            "http://127.0.0.1:9/script.js".to_owned(),
            FAST,
            Duration::from_secs(300),
        );

        assert_eq!(
            StatusCode::BAD_GATEWAY,
            relay.respond(&HeaderMap::new()).await.status()
        );
    }
}
