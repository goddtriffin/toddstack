# Changelog

Earlier releases are described in the git history and the README.

## 0.4.4

- `http_accept_errors_total{listener="app"}` and `{listener="metrics"}` are registered at 0 when the metrics
  recorder starts, so the series exist before the first accept error.
- The startup line `listening on http://{address}` now ends with `(webserver-base {version})`.

## 0.4.3

- The server's own accept loop replaces `axum::serve`, which ran hyper with no timer and so with no timeout. A
  connection that sends no request headers for 30 s (`DEFAULT_HEADER_READ_TIMEOUT`), idle keep-alive time included,
  is closed. At most 4096 connections (`DEFAULT_MAX_CONNECTIONS`) are served at once; past that the server stops
  accepting until one closes. Both are set with `WebServer::connection_limits(ConnectionLimits)`.
- A failed accept (out of file descriptors) backs off from 10 ms to 1 s and logs `accept error: {e}; backing off`
  once per minute, instead of `accept error: {e}` at `error` every second. New metrics: `http_accept_errors_total`
  and `http_open_connections`, both with a `listener` label (`app` or `metrics`). An upgraded connection (a
  `WebSocket`) keeps its cap slot and gauge count until its socket closes.
- `DeliberateOverload` and `overloaded(retry_after)`: mark a deliberate `503` so the trace layer does not log it
  as an error (and so Sentry gets no event for it).
- `WebServerError::Serve` is no longer returned (serving cannot fail once bound); the variant stays.

## 0.4.2

- `FrontendParams::with_browser_release(release)`: an optional Sentry `release` in the browser init, JSON-escaped (`<`
  becomes `\u003c`, so `</script>` cannot end the inline script; U+2028 and U+2029 become `\u2028` and `\u2029`). Without it the page is byte-identical to 0.4.1.
- `SentryBrowser` gains `release_js: Option<String>`; code that builds one by struct literal must set it.
