# Changelog

Earlier releases are described in the git history and the README.

## 0.4.2

- `FrontendParams::with_browser_release(release)`: an optional Sentry `release` in the browser init, JSON-escaped (`<`
  becomes `\u003c`, so `</script>` cannot end the inline script; U+2028 and U+2029 become `\u2028` and `\u2029`). Without it the page is byte-identical to 0.4.1.
- `SentryBrowser` gains `release_js: Option<String>`; code that builds one by struct literal must set it.
