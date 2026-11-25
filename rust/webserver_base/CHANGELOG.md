# Changelog

Earlier releases are described in the git history and the README.

## 0.4.3

- The server's own accept loop replaces `axum::serve`, which ran hyper with no timer and so with no timeout. A
  connection that sends no request headers for 30 s (`DEFAULT_HEADER_READ_TIMEOUT`), idle keep-alive time included,
  is closed. At most 4096 connections (`DEFAULT_MAX_CONNECTIONS`) are served at once; past that the server stops
  accepting until one closes. Both are set with `WebServer::connection_limits(ConnectionLimits)`.
- A failed accept (out of file descriptors) backs off from 10 ms to 1 s and logs `accept error: {e}; backing off`
  once per minute, instead of `accept error: {e}` at `error` every second. New metrics: `http_accept_errors_total`
  and `http_open_connections`.
- `WebServerError::Serve` is no longer returned (serving cannot fail once bound); the variant stays.

## 0.4.2

- `FrontendParams::with_browser_release(release)`: an optional Sentry `release` in the browser init, JSON-escaped (`<`
  becomes `\u003c`, so `</script>` cannot end the inline script; U+2028 and U+2029 become `\u2028` and `\u2029`). Without it the page is byte-identical to 0.4.1.
- `SentryBrowser` gains `release_js: Option<String>`; code that builds one by struct literal must set it.
