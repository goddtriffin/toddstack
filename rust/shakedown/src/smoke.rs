//! `shakedown smoke`: the production Docker image, started by the repo's own Makefile targets and checked over
//! HTTP (design section 10). The tool never runs `docker`: `smoke.up` and `smoke.down` are `make` targets.
//!
//! Order: up (records the image digest), ready, built-in HTTP checks, the project's checks, the sweep, the
//! container logs, down. Down always runs: it is a drop guard, so success, failure, a panic and a caught signal
//! (the run ends through `interrupted()`) all reach it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::Config;
use crate::error::Error;
use crate::steps::{Status, StepResult};
use crate::world::{self, Signals};

/// `[smoke]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smoke {
    /// Starts the production image (the repo's Makefile target) and returns. `{port}` is a free local port; the
    /// image must be reachable on `127.0.0.1:{port}`. It prints the image digest (`sha256:` and 64 hex digits).
    pub up: String,
    /// Stops and removes everything `up` made. Always runs, so it must be safe when `up` failed half way.
    /// `{port}` is the same port.
    pub down: String,
    /// Prints the container's logs (the end of a crashed container's too, so the target keeps them). Optional.
    pub logs: Option<String>,
    /// A path that answers (below 500) once the container is up.
    #[serde(default = "default_ready")]
    pub ready: String,
    /// Seconds to wait for ready.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Pages the built-in checks cover, besides the paths of `checks`.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Response headers every route must have (default: `content-security-policy`, `x-content-type-options`,
    /// `referrer-policy`; HSTS is left to the edge). `[]` turns the check off.
    #[serde(default = "default_headers")]
    pub headers: Vec<String>,
    /// The least `max-age` (seconds) an asset's `Cache-Control` must grant. 0 turns the check off.
    #[serde(default = "default_cache")]
    pub min_cache: u64,
    /// The project's own checks.
    #[serde(default)]
    pub checks: Vec<Check>,
    /// Lines of the container logs to leave out of the error scan (substrings).
    #[serde(default)]
    pub log_ignore: Vec<String>,
    /// Paths (globs; `*` matches any run of characters, `/` included) whose local-host URLs are left out of the
    /// `smoke.http` scan, e.g. `["/script/accounts-*.js"]`. A bare `"localhost"` string is never flagged anyway.
    #[serde(default)]
    pub http_allow: Vec<String>,
    /// Run the layout sweep against the container (the `[sweep]` pages drawn against the container).
    #[serde(default)]
    pub sweep: bool,
}

/// One project check: a path and what its response must be.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// The path (and query) to request.
    pub path: String,
    /// The status it must answer (default 200). Redirects are not followed.
    #[serde(default = "default_status")]
    pub status: u16,
    /// A text the body must contain.
    pub contains: Option<String>,
    /// The `Location` header, for a redirect: the path or URL it must send.
    pub location: Option<String>,
    /// A text the `Content-Type` must contain.
    pub content_type: Option<String>,
    /// The body must be no bigger than this.
    pub max_bytes: Option<u64>,
}

fn default_ready() -> String {
    "/".into()
}
const fn default_timeout() -> u64 {
    30
}
fn default_headers() -> Vec<String> {
    [
        "content-security-policy",
        "x-content-type-options",
        "referrer-policy",
    ]
    .map(String::from)
    .to_vec()
}
const fn default_cache() -> u64 {
    86_400
}
const fn default_status() -> u16 {
    200
}

impl Smoke {
    /// Checks the values the type cannot.
    ///
    /// # Errors
    ///
    /// A path without a leading `/` (or with a space or control character), an empty command, or `sweep = true`
    /// with no `[sweep]` section (`has_sweep` says whether there is one).
    pub fn validate(&self, has_sweep: bool) -> Result<(), Error> {
        let bad = |m: String| Err(Error::Config(format!("[smoke] {m}")));
        if self.up.trim().is_empty() || self.down.trim().is_empty() {
            return bad("needs up and down (the repo's Makefile targets)".into());
        }
        for p in self
            .routes
            .iter()
            .chain(self.checks.iter().map(|c| &c.path))
            .chain([&self.ready])
        {
            if !p.starts_with('/') {
                return bad(format!("path {p:?} must start with /"));
            }
            if p.chars().any(|c| c == ' ' || c.is_control()) {
                return bad(format!(
                    "path {p:?} must not hold spaces or control characters"
                ));
            }
        }
        if self.timeout == 0 {
            return bad("timeout must be at least 1".into());
        }
        if self.sweep && !has_sweep {
            return bad("sweep = true needs a [sweep] section in shakedown.toml".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// A small HTTP/1.0 client: loopback only, no redirects, no TLS, no compression. The container is the only server.

/// What a request got back.
#[derive(Debug, Clone)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Header names (lower case) and values.
    pub headers: Vec<(String, String)>,
    /// The body (chunked encoding removed).
    pub body: Vec<u8>,
}

impl Response {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn is_html(&self) -> bool {
        self.header("content-type")
            .is_some_and(|c| c.to_ascii_lowercase().contains("text/html"))
    }
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = body.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = String::from_utf8_lossy(&body[..eol]);
        let size = size.split(';').next().unwrap_or_default().trim();
        let Ok(n) = usize::from_str_radix(size, 16) else {
            return out;
        };
        body = &body[eol + 2..];
        if n == 0 || body.len() < n {
            out.extend_from_slice(&body[..body.len().min(n)]);
            return out;
        }
        out.extend_from_slice(&body[..n]);
        body = body.get(n + 2..).unwrap_or_default();
    }
}

/// How long one request may take in all, and how much of the answer is kept.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Total time from connecting to the last byte.
    pub total: Duration,
    /// The most bytes read (headers and body).
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(30),
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

/// `GET path` on `127.0.0.1:port`, within the default [`Limits`].
///
/// # Errors
///
/// Nothing answers, or the answer is not HTTP, or too slow, or too big.
pub fn get(port: u16, path: &str) -> Result<Response, String> {
    get_with(port, path, Limits::default(), &|| false)
}

/// `GET path` with one total deadline and a byte cap, giving up the moment `stop` turns true. Reads wake every
/// 100 ms, so neither a dribbling route nor a signal can hold it.
///
/// # Errors
///
/// Nothing answers, the answer is not HTTP, it is too slow or too big, or `stop` turned true.
pub fn get_with(
    port: u16,
    path: &str,
    limits: Limits,
    stop: &dyn Fn() -> bool,
) -> Result<Response, String> {
    let started = Instant::now();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2).min(limits.total))
        .map_err(|e| format!("cannot connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nAccept-Encoding: identity\r\n\
         User-Agent: shakedown-smoke\r\nConnection: close\r\n\r\n"
    );
    s.write_all(request.as_bytes())
        .map_err(|e| format!("cannot send: {e}"))?;
    let mut raw = Vec::new();
    let mut buf = [0_u8; 16 * 1024];
    loop {
        if stop() {
            return Err("interrupted".into());
        }
        if started.elapsed() > limits.total {
            return Err(format!("took over {}s", limits.total.as_secs_f64()));
        }
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > limits.max_bytes {
                    return Err(format!("answer is over {} bytes", limits.max_bytes));
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(format!("cannot read: {e}")),
        }
    }
    parse(&raw)
}

fn parse(raw: &[u8]) -> Result<Response, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("the answer is not HTTP (no header end)")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("the answer is not HTTP (bad status line)")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let body = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    Ok(Response {
        status,
        headers,
        body: if chunked {
            dechunk(body)
        } else {
            body.to_vec()
        },
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The built-in checks.

/// The attributes of a tag as `(name, value)`, names lower case.
fn attrs(tag: &str) -> Vec<(String, String)> {
    let b = tag.as_bytes();
    let mut i = tag
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let name = tag[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let from = i + 1;
                i = from;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = tag[from..i].to_string();
                i += 1;
            } else {
                let from = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = tag[from..i].to_string();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// The URLs a page loads as parts of itself: scripts, stylesheets, icons, images, media (not links to other
/// pages), in document order without repeats.
pub fn asset_refs(html: &str) -> Vec<String> {
    const REL: [&str; 8] = [
        "stylesheet",
        "icon",
        "preload",
        "modulepreload",
        "manifest",
        "apple-touch-icon",
        "mask-icon",
        "shortcut",
    ];
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut at = 0;
    while let Some(off) = html[at..].find('<') {
        let from = at + off + 1;
        let Some(end) = html[from..].find('>') else {
            break;
        };
        let tag = &html[from..from + end];
        at = from + end + 1;
        let name = tag
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let a = attrs(tag);
        let get = |k: &str| a.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let url = match name.as_str() {
            "script" => {
                // Inline script text is not markup: skip to its end.
                if let Some(close) = lower[at..].find("</script")
                    && get("src").is_none()
                {
                    at += close;
                }
                get("src")
            }
            "img" | "source" | "video" | "audio" | "track" | "embed" => get("src"),
            "link" => get("rel")
                .is_some_and(|r| {
                    r.to_ascii_lowercase()
                        .split_whitespace()
                        .any(|w| REL.contains(&w))
                })
                .then(|| get("href"))
                .flatten(),
            _ => None,
        };
        if let Some(u) = url.map(str::trim).filter(|u| !u.is_empty())
            && !out.iter().any(|x| x == u)
        {
            out.push(u.to_string());
        }
    }
    out
}

/// A page's asset URL as a path to request on the container, or `None` for anything off the container (other
/// hosts, `data:` URIs, anchors).
fn local_path(url: &str, page: &str) -> Option<String> {
    let url = url.split('#').next().unwrap_or_default();
    if url.is_empty() || url.starts_with("//") {
        return None;
    }
    if url.starts_with('/') {
        return Some(url.to_string());
    }
    // `http:`, `https:`, `data:`, `mailto:`: a scheme before any `/` or `?`.
    if url
        .split(['/', '?'])
        .next()
        .is_some_and(|first| first.contains(':'))
    {
        return None;
    }
    let dir = page.split(['?', '#']).next().unwrap_or("/");
    let dir = &dir[..=dir.rfind('/').unwrap_or(0)];
    Some(format!("{dir}{url}"))
}

/// The `max-age` a `Cache-Control` grants, in seconds (`immutable` counts as unlimited; `no-store` and
/// `no-cache` as none).
fn cache_age(value: Option<&str>) -> u64 {
    let Some(v) = value else {
        return 0;
    };
    let v = v.to_ascii_lowercase();
    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
    if parts.iter().any(|p| *p == "no-store" || *p == "no-cache") {
        return 0;
    }
    let age = parts
        .iter()
        .filter_map(|p| {
            p.strip_prefix("max-age=")
                .or_else(|| p.strip_prefix("s-maxage="))
        })
        .filter_map(|n| n.trim_matches('"').parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    if parts.contains(&"immutable") {
        age.max(31_536_000)
    } else {
        age
    }
}

/// Does `path` match `pattern`, where `*` stands for any run of characters (`/` included)?
fn glob_match(pattern: &str, path: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == path;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !path.starts_with(first) || path.len() < first.len() + last.len() || !path.ends_with(last) {
        return false;
    }
    let mut rest = &path[first.len()..path.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Is the text just before `at` a URL authority marker (`//`, also as `\/\/` in JSON or `%2F%2F` in a query)?
fn in_url_context(before: &str) -> bool {
    let b = before.to_ascii_lowercase();
    b.ends_with("//") || b.ends_with("\\/\\/") || b.ends_with("%2f%2f")
}

/// Where a served page names this machine or a dev server in a URL (`http://localhost:3000`, `//127.0.0.1/`,
/// `ws://[::1]`): the matching text with a little around it. A bare string such as `"localhost"` is a hostname
/// comparison (the vendored Sentry SDK has them), not a leaked URL, and is not reported.
fn dev_urls(text: &str) -> Vec<String> {
    const NEEDLES: [&str; 4] = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"];
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    for n in NEEDLES {
        let Some(i) = lower
            .match_indices(n)
            .map(|(i, _)| i)
            .find(|i| in_url_context(&lower[..*i]))
        else {
            continue;
        };
        let from = lower[..i]
            .char_indices()
            .rev()
            .nth(20)
            .map_or(0, |(p, _)| p);
        let to = (i + n.len() + 20).min(lower.len());
        let to = (to..=lower.len())
            .find(|t| lower.is_char_boundary(*t))
            .unwrap_or(lower.len());
        let snippet: String = text[from..to]
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        out.push(format!("names {n}: ...{snippet}..."));
    }
    out
}

/// Dev URLs in a response's headers and, for text (HTML, JSON, XML, JavaScript, CSS), its body.
fn scan_dev_urls(path: &str, allow: &[String], r: &Response, problems: &mut Vec<String>) {
    if allow.iter().any(|g| glob_match(g, path)) {
        return;
    }
    let mut head = String::new();
    for (k, v) in &r.headers {
        let _ = writeln!(head, "{k}: {v}");
    }
    for found in dev_urls(&head) {
        problems.push(format!("{path}: header {found}"));
    }
    if is_text(r) {
        for found in dev_urls(&r.text()) {
            problems.push(format!("{path}: {found}"));
        }
    }
}

fn is_text(r: &Response) -> bool {
    r.header("content-type").is_some_and(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("text/html")
            || c.contains("json")
            || c.contains("xml")
            || c.contains("javascript")
            || c.contains("ecmascript")
            || c.contains("css")
    })
}

/// What the HTTP checks found.
#[derive(Debug, Default)]
pub struct Http {
    /// Routes requested.
    pub routes: usize,
    /// Assets requested.
    pub assets: usize,
    /// `path: what is wrong`, one per problem.
    pub problems: Vec<String>,
}

/// Every route: no 5xx, the security headers, no dev URLs in what is served, and every asset the page loads
/// answers 200 with a long cache lifetime.
pub fn builtin(
    smoke: &Smoke,
    fetch: &mut impl FnMut(&str) -> Result<Response, String>,
    stop: &dyn Fn() -> bool,
) -> Http {
    let mut http = Http::default();
    let mut routes: Vec<&str> = Vec::new();
    for p in smoke
        .routes
        .iter()
        .chain(smoke.checks.iter().map(|c| &c.path))
    {
        if !routes.contains(&p.as_str()) {
            routes.push(p);
        }
    }
    let mut assets: Vec<(String, String)> = Vec::new();
    for route in routes {
        if stop() {
            break;
        }
        http.routes += 1;
        let r = match fetch(route) {
            Ok(r) => r,
            Err(e) => {
                http.problems.push(format!("{route}: {e}"));
                continue;
            }
        };
        let expects_5xx = smoke
            .checks
            .iter()
            .any(|c| c.path == route && c.status >= 500);
        if r.status >= 500 && !expects_5xx {
            http.problems
                .push(format!("{route}: server error {}", r.status));
        }
        let missing: Vec<&str> = smoke
            .headers
            .iter()
            .map(String::as_str)
            .filter(|h| r.header(&h.to_ascii_lowercase()).is_none())
            .collect();
        if !missing.is_empty() {
            http.problems
                .push(format!("{route}: missing headers {}", missing.join(", ")));
        }
        scan_dev_urls(route, &smoke.http_allow, &r, &mut http.problems);
        if r.status == 200 && r.is_html() {
            for url in asset_refs(&r.text()) {
                if let Some(p) = local_path(&url, route)
                    && !assets.iter().any(|(a, _)| *a == p)
                {
                    assets.push((p, route.to_string()));
                }
            }
        }
    }
    for (asset, page) in assets {
        if stop() {
            break;
        }
        http.assets += 1;
        match fetch(&asset) {
            Err(e) => http.problems.push(format!("{asset} (from {page}): {e}")),
            Ok(r) if r.status != 200 => http.problems.push(format!(
                "{asset} (from {page}): answered {}, not 200",
                r.status
            )),
            Ok(r) => {
                scan_dev_urls(&asset, &smoke.http_allow, &r, &mut http.problems);
                let age = cache_age(r.header("cache-control"));
                if age < smoke.min_cache {
                    http.problems.push(format!(
                        "{asset} (from {page}): cache lifetime {age}s, needs {}s (Cache-Control: {})",
                        smoke.min_cache,
                        r.header("cache-control").unwrap_or("none")
                    ));
                }
            }
        }
    }
    http
}

/// The project's checks.
pub fn project_checks(
    checks: &[Check],
    host: &str,
    fetch: &mut impl FnMut(&str) -> Result<Response, String>,
    stop: &dyn Fn() -> bool,
) -> Vec<String> {
    let mut problems = Vec::new();
    for c in checks {
        if stop() {
            break;
        }
        let p = &c.path;
        let r = match fetch(p) {
            Ok(r) => r,
            Err(e) => {
                problems.push(format!("{p}: {e}"));
                continue;
            }
        };
        if r.status != c.status {
            problems.push(format!("{p}: status {}, expected {}", r.status, c.status));
        }
        if let Some(want) = &c.contains
            && !r.text().contains(want.as_str())
        {
            problems.push(format!("{p}: body does not contain {want:?}"));
        }
        if let Some(want) = &c.location {
            let got = r.header("location").unwrap_or("(none)");
            // A full URL to this server (`host`, nothing else) counts as its path; another host never does.
            let path = got
                .strip_prefix("http://")
                .and_then(|u| u.strip_prefix(host))
                .filter(|rest| rest.starts_with('/'))
                .unwrap_or(got);
            if got != want && path != want {
                problems.push(format!("{p}: redirects to {got}, expected {want}"));
            }
        }
        if let Some(want) = &c.content_type {
            let got = r.header("content-type").unwrap_or("(none)");
            if !got
                .to_ascii_lowercase()
                .contains(&want.to_ascii_lowercase())
            {
                problems.push(format!("{p}: content type {got}, expected {want}"));
            }
        }
        if let Some(max) = c.max_bytes
            && r.body.len() as u64 > max
        {
            problems.push(format!("{p}: {} bytes, over {max}", r.body.len()));
        }
    }
    problems
}

/// Panics and error lines in the container's logs. A line counts when it has a level-style token: the word
/// `ERROR`, `FATAL` or `PANIC` in capitals, `panicked at`, a line starting `Error:`, or a level field set to error, fatal or panic
/// (`level=error`, `"level":"error"`). Words in ordinary text (`error=none`, `/error-page`, `no errors`) do not
/// count. A line holding one of `ignore` is skipped.
pub fn scan_logs(text: &str, ignore: &[String]) -> Vec<String> {
    text.lines()
        .filter(|l| !ignore.iter().any(|i| l.contains(i.as_str())))
        .filter(|l| is_error_line(l))
        .map(|l| l.trim().chars().take(300).collect())
        .collect()
}

fn is_error_line(line: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if line
        .split(|c: char| !word(c))
        .any(|w| matches!(w, "ERROR" | "FATAL" | "PANIC"))
    {
        return true;
    }
    // What a Rust `main` prints when it returns an error.
    if line.trim_start().starts_with("Error:") {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    if lower.contains("panicked at") {
        return true;
    }
    let squeezed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    ["error", "fatal", "panic"].iter().any(|lv| {
        [
            format!("level={lv}"),
            format!("level=\"{lv}\""),
            format!("\"level\":\"{lv}\""),
        ]
        .iter()
        .any(|pat| squeezed.contains(pat.as_str()))
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The run.

/// The first `sha256:` plus 64 hex digits in `text`.
pub fn digest_in(text: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = text[from..].find("sha256:") {
        let start = from + i;
        let hex: String = text[start + 7..]
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        if hex.len() == 64 {
            return Some(format!("sha256:{}", hex.to_ascii_lowercase()));
        }
        from = start + 7;
    }
    None
}

fn fill(template: &str, port: u16) -> String {
    template.replace("{port}", &port.to_string())
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Runs `make <smoke.down>` once, however the run ends: dropping it runs the command if nobody did.
struct Down {
    command: String,
    root: PathBuf,
    log: PathBuf,
    done: bool,
    /// How long `down` may take before its whole process group is killed.
    limit: Duration,
}

impl Down {
    fn run(&mut self) -> Result<(), String> {
        if std::mem::replace(&mut self.done, true) {
            return Ok(());
        }
        let log = std::fs::File::create(&self.log).map_err(|e| e.to_string())?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(&self.root)
            .env("SHAKEDOWN", "1")
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            // Its own group: a Ctrl-C that is still arriving must not cut the cleanup short.
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot run smoke.down: {e}"))?;
        let deadline = Instant::now() + self.limit;
        loop {
            match child.try_wait() {
                Ok(Some(s)) if s.success() => return Ok(()),
                Ok(Some(s)) => {
                    let text = std::fs::read_to_string(&self.log).unwrap_or_default();
                    return Err(format!("smoke.down exited {s}: {}", tail(&text, 10)));
                }
                Ok(None) if Instant::now() > deadline => {
                    // The whole group (the shell and what it started), not just `sh`.
                    let _ = Command::new("kill")
                        .args(["-KILL", "--", &format!("-{}", child.id())])
                        .stderr(Stdio::null())
                        .status();
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("smoke.down took over {}s", self.limit.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

impl Drop for Down {
    fn drop(&mut self) {
        let _ = self.run();
    }
}

/// A finished smoke run.
#[derive(Debug)]
pub struct Outcome {
    /// One result per step.
    pub results: Vec<StepResult>,
    /// The image digest `smoke.up` printed.
    pub digest: Option<String>,
}

impl Outcome {
    /// Did every step pass?
    pub fn passed(&self) -> bool {
        self.results
            .iter()
            .all(|r| !matches!(r.status, Status::Fail(_)))
    }
}

fn result(
    name: &str,
    status: Status,
    detail: &str,
    started: Instant,
    output: String,
) -> StepResult {
    StepResult {
        name: name.into(),
        status,
        detail: detail.into(),
        secs: started.elapsed().as_secs_f64(),
        output,
    }
}

fn problems_result(name: &str, detail: &str, started: Instant, problems: &[String]) -> StepResult {
    let status = if problems.is_empty() {
        Status::Ok
    } else {
        Status::Fail(format!(
            "{} problem{}",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" }
        ))
    };
    result(name, status, detail, started, problems.join("\n"))
}

fn skipped(name: &str, why: &str) -> StepResult {
    result(
        name,
        Status::Skipped(why.into()),
        "",
        Instant::now(),
        String::new(),
    )
}

/// The sweep against the container (design section 10, step 5): layout rules and layout shift on live content.
///
/// `Err` means the step is skipped (named in the line, never a pass); `Ok` holds the findings, none meaning a pass.
pub type Sweeper<'a> = &'a dyn Fn(u16) -> Result<Vec<String>, String>;

/// The real sweeper: the project's `[sweep]` pages drawn against `http://127.0.0.1:{port}` with no world of its own.
/// It works in its own directory (`target/shakedown/smoke/sweep`) and writes no baseline: the dev sweep's
/// baselines, pictures and reports are never touched, and pixel changes are not judged here.
fn sweep_container(config: &Config, root: &Path, port: u16) -> Result<Vec<String>, String> {
    if config.sweep.is_none() {
        return Err("no [sweep] section in shakedown.toml".into());
    }
    let opts = crate::sweep::Options {
        base: Some(format!("http://127.0.0.1:{port}")),
        routes: Vec::new(),
        pack: false,
        rebase: false,
        out: Some(root.join("target/shakedown/smoke/sweep")),
    };
    Ok(match crate::sweep::run(config, root, &opts) {
        Ok(o) => o
            .failed
            .iter()
            .map(|r| {
                format!(
                    "{}@{}-{}: {}",
                    r.route,
                    r.device,
                    r.theme,
                    r.findings
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })
            .collect(),
        Err(e) => vec![format!("the sweep could not run: {e}")],
    })
}

/// Starts the image on `port`, checks it, and always stops it.
///
/// # Errors
///
/// The output directory cannot be made.
pub fn run_on(
    smoke: &Smoke,
    root: &Path,
    out: &Path,
    port: u16,
    signals: &Signals,
    sweeper: Sweeper<'_>,
) -> Result<Outcome, Error> {
    std::fs::create_dir_all(out)
        .map_err(|e| Error::Environment(format!("{}: {e}", out.display())))?;
    let mut results: Vec<StepResult> = Vec::new();
    // Made before `up` runs: a target that fails half way may have left things behind.
    let mut down = Down {
        command: fill(&smoke.down, port),
        root: root.to_path_buf(),
        log: out.join("down.log"),
        done: false,
        limit: Duration::from_secs(180),
    };
    let mut digest = None;
    body(
        smoke,
        root,
        out,
        port,
        (signals, sweeper),
        &mut results,
        &mut digest,
    );
    let started = Instant::now();
    let r = down.run();
    results.push(match r {
        Ok(()) => result("smoke.down", Status::Ok, "", started, String::new()),
        Err(e) => result(
            "smoke.down",
            Status::Fail("did not stop cleanly".into()),
            "",
            started,
            e,
        ),
    });
    Ok(Outcome { results, digest })
}

const AFTER_UP: [&str; 4] = ["smoke.ready", "smoke.http", "smoke.checks", "smoke.sweep"];

fn interrupted(results: &mut Vec<StepResult>, from: &[&str]) {
    for n in from {
        results.push(skipped(n, "interrupted"));
    }
    results.push(result(
        "smoke.interrupted",
        Status::Fail("interrupted".into()),
        "",
        Instant::now(),
        String::new(),
    ));
}

/// Steps 1 to 6. Returns early when a step leaves nothing to check, and straight away (no checks, no logs) once
/// a signal has arrived; the caller still runs down.
fn body(
    smoke: &Smoke,
    root: &Path,
    out: &Path,
    port: u16,
    (signals, sweeper): (&Signals, Sweeper<'_>),
    results: &mut Vec<StepResult>,
    digest: &mut Option<String>,
) {
    if !step_up(smoke, root, out, port, signals, results, digest) {
        if !signals.interrupted() {
            logs_step(smoke, root, out, port, results, true);
        }
        return;
    }
    if signals.interrupted() {
        interrupted(results, &AFTER_UP);
        return;
    }
    if !step_ready(smoke, port, signals, results) {
        if !signals.interrupted() {
            logs_step(smoke, root, out, port, results, true);
        }
        return;
    }
    let stopped = || signals.interrupted();
    // Each path is requested once, however many checks look at it.
    let mut cache: BTreeMap<String, Result<Response, String>> = BTreeMap::new();
    let mut fetch = |p: &str| {
        if let Some(hit) = cache.get(p) {
            return hit.clone();
        }
        let got = get_with(port, p, Limits::default(), &stopped);
        // An interrupted request says nothing about the route: do not keep it.
        if !stopped() {
            cache.insert(p.to_string(), got.clone());
        }
        got
    };
    let started = Instant::now();
    let http = builtin(smoke, &mut fetch, &stopped);
    if stopped() {
        interrupted(results, &AFTER_UP[1..]);
        return;
    }
    results.push(problems_result(
        "smoke.http",
        &format!("{} routes {} assets", http.routes, http.assets),
        started,
        &http.problems,
    ));
    let started = Instant::now();
    let host = format!("127.0.0.1:{port}");
    let problems = project_checks(&smoke.checks, &host, &mut fetch, &stopped);
    if stopped() {
        interrupted(results, &AFTER_UP[2..]);
        return;
    }
    results.push(problems_result(
        "smoke.checks",
        &format!("{} checks", smoke.checks.len()),
        started,
        &problems,
    ));
    if smoke.sweep {
        let started = Instant::now();
        results.push(match sweeper(port) {
            Ok(findings) => problems_result("smoke.sweep", "", started, &findings),
            Err(why) => skipped("smoke.sweep", &why),
        });
        if stopped() {
            interrupted(results, &[]);
            return;
        }
    }
    logs_step(smoke, root, out, port, results, false);
}

/// Step 1. False when nothing is running to check.
fn step_up(
    smoke: &Smoke,
    root: &Path,
    out: &Path,
    port: u16,
    signals: &Signals,
    results: &mut Vec<StepResult>,
    digest: &mut Option<String>,
) -> bool {
    let started = Instant::now();
    let up_log = out.join("up.log");
    let up = run_up(smoke, root, &up_log, port, signals);
    let up_text = std::fs::read_to_string(&up_log).unwrap_or_default();
    *digest = digest_in(&up_text);
    if let Err(e) = up {
        results.push(result(
            "smoke.up",
            Status::Fail(e),
            "",
            started,
            tail(&up_text, 20),
        ));
        for n in AFTER_UP {
            results.push(skipped(n, "smoke.up did not pass"));
        }
        return false;
    }
    match digest.as_deref() {
        None => results.push(result(
            "smoke.up",
            Status::Fail("printed no image digest".into()),
            "",
            started,
            "smoke.up must print the image it runs, as `sha256:` and 64 hex digits, so the push step can confirm the same image shipped".into(),
        )),
        Some(d) => results.push(result(
            "smoke.up",
            Status::Ok,
            &d.chars().take(19).collect::<String>(),
            started,
            String::new(),
        )),
    }
    true
}

/// Step 2. False when the container never answered (or the run was interrupted).
fn step_ready(smoke: &Smoke, port: u16, signals: &Signals, results: &mut Vec<StepResult>) -> bool {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(smoke.timeout);
    loop {
        if signals.interrupted() {
            interrupted(results, &AFTER_UP);
            return false;
        }
        if get(port, &smoke.ready).is_ok_and(|r| r.status < 500) {
            results.push(result(
                "smoke.ready",
                Status::Ok,
                "",
                started,
                String::new(),
            ));
            return true;
        }
        if Instant::now() > deadline {
            results.push(result(
                "smoke.ready",
                Status::Fail(format!(
                    "{} did not answer within {}s",
                    smoke.ready, smoke.timeout
                )),
                "",
                started,
                "the container's logs follow (smoke.logs)".into(),
            ));
            for n in &AFTER_UP[1..] {
                results.push(skipped(n, "the container never became ready"));
            }
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn run_up(
    smoke: &Smoke,
    root: &Path,
    log: &Path,
    port: u16,
    signals: &Signals,
) -> Result<(), String> {
    let file = std::fs::File::create(log).map_err(|e| e.to_string())?;
    let err = file.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(fill(&smoke.up, port))
        .current_dir(root)
        .env("SHAKEDOWN", "1")
        .env("SHAKEDOWN_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("cannot run smoke.up: {e}"))?;
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => return Err(format!("smoke.up exited {s}")),
            Ok(None) if signals.interrupted() => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("interrupted".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Step 6: the logs. `diagnose` is for a run that already failed: the end of the logs is shown, nothing is
/// judged.
fn logs_step(
    smoke: &Smoke,
    root: &Path,
    out: &Path,
    port: u16,
    results: &mut Vec<StepResult>,
    diagnose: bool,
) {
    let Some(cmd) = &smoke.logs else {
        if !diagnose {
            results.push(skipped("smoke.logs", "no [smoke].logs command"));
        }
        return;
    };
    let started = Instant::now();
    let o = Command::new("sh")
        .arg("-c")
        .arg(fill(cmd, port))
        .current_dir(root)
        .env("SHAKEDOWN", "1")
        .stdin(Stdio::null())
        .output();
    let mut exit_failure = None;
    let text = match o {
        Ok(o) => {
            if !o.status.success() {
                exit_failure = Some(o.status);
            }
            String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned()
        }
        Err(e) => {
            results.push(result(
                "smoke.logs",
                Status::Fail(format!("cannot run the logs command: {e}")),
                "",
                started,
                String::new(),
            ));
            return;
        }
    };
    let _ = std::fs::write(out.join("container.log"), &text);
    if diagnose {
        results.push(result(
            "smoke.logs",
            Status::Skipped("shown because the run failed".into()),
            "",
            started,
            format!(
                "end of target/shakedown/smoke/container.log:\n{}",
                tail(&text, 20)
            ),
        ));
        return;
    }
    if let Some(status) = exit_failure {
        results.push(result(
            "smoke.logs",
            Status::Fail(format!("the logs command exited {status}")),
            "",
            started,
            tail(&text, 20),
        ));
        return;
    }
    let hits = scan_logs(&text, &smoke.log_ignore);
    results.push(problems_result(
        "smoke.logs",
        &format!("{} lines", text.lines().count()),
        started,
        &hits,
    ));
}

/// The lines for stdout: one per step, then each failure's detail.
pub fn lines(outcome: &Outcome) -> Vec<String> {
    let mut out = Vec::new();
    for r in &outcome.results {
        out.push(match &r.status {
            Status::Ok if r.detail.is_empty() => format!("ok {} {:.1}s", r.name, r.secs),
            Status::Ok => format!("ok {} {} {:.1}s", r.name, r.detail, r.secs),
            Status::Fail(why) => format!("FAIL {} {why}", r.name),
            Status::Skipped(why) => format!("skip {} ({why})", r.name),
        });
    }
    for r in &outcome.results {
        if r.output.is_empty() || matches!(r.status, Status::Ok) {
            continue;
        }
        for l in r.output.lines().take(30) {
            out.push(format!("  {}: {l}", r.name));
        }
    }
    out
}

fn summary_json(project: &str, outcome: &Outcome) -> String {
    let steps: Vec<_> = outcome
        .results
        .iter()
        .map(|r| {
            let (status, reason) = match &r.status {
                Status::Ok => ("ok", None),
                Status::Fail(w) => ("fail", Some(w.as_str())),
                Status::Skipped(w) => ("skip", Some(w.as_str())),
            };
            serde_json::json!({"name": r.name, "status": status, "reason": reason, "detail": r.detail, "problems": r.output.lines().collect::<Vec<_>>()})
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "project": project,
        "command": "smoke",
        "digest": outcome.digest,
        "passed": outcome.passed(),
        "steps": steps,
    }))
    .unwrap_or_default()
        + "\n"
}

/// `shakedown smoke`.
///
/// # Errors
///
/// No `[smoke]` section (config), or a step failed.
pub fn command(config: &Config, root: &Path, json: bool) -> Result<(), Error> {
    let smoke = config
        .smoke
        .as_ref()
        .ok_or_else(|| Error::Config("no [smoke] section in shakedown.toml".into()))?;
    let out = root.join("target/shakedown/smoke");
    let port = world::free_port()?;
    // Shielded: Ctrl-C any number of times still lets `down` run.
    let signals = world::watch_signals_shielded();
    let outcome = run_on(smoke, root, &out, port, &signals, &|p| {
        sweep_container(config, root, p)
    })?;
    drop(signals);
    for l in lines(&outcome) {
        println!("{l}");
    }
    if let Some(d) = &outcome.digest {
        println!("digest: {d}");
    }
    if json {
        std::fs::write(
            out.join("summary.json"),
            summary_json(&config.project, &outcome),
        )
        .map_err(|e| Error::Environment(format!("cannot write summary.json: {e}")))?;
    }
    if outcome.passed() {
        return Ok(());
    }
    let html =
        crate::report::steps_html(&outcome.results).replace("shakedown check", "shakedown smoke");
    std::fs::write(out.join("report.html"), html)
        .map_err(|e| Error::Environment(format!("cannot write report: {e}")))?;
    println!("report: target/shakedown/smoke/report.html");
    // A missing Makefile target or tool is the environment's, not the image's.
    if let Some(r) = outcome.results.first()
        && r.name == "smoke.up"
        && let Ok(text) = std::fs::read_to_string(out.join("up.log"))
    {
        if text.contains("No rule to make target") {
            return Err(Error::Config(
                "smoke.up names a make target the Makefile does not have".into(),
            ));
        }
        if text.contains("command not found") {
            return Err(Error::Environment(format!(
                "smoke.up needs a tool that is not installed: {}",
                tail(&text, 3)
            )));
        }
    }
    let failed = outcome
        .results
        .iter()
        .filter(|r| matches!(r.status, Status::Fail(_)))
        .count();
    Err(Error::Failed(format!(
        "{failed} of {} smoke steps failed",
        outcome.results.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::net::TcpListener;
    use std::process::Child;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// `(path, status, headers, body)`.
    type Page = (
        &'static str,
        u16,
        Vec<(&'static str, &'static str)>,
        &'static str,
    );

    /// A local server with fixed answers: the stand-in for the container.
    fn serve(pages: Vec<Page>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else {
                    continue;
                };
                let mut req = String::new();
                let mut buf = [0_u8; 512];
                while !req.contains("\r\n\r\n") {
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req += &String::from_utf8_lossy(&buf[..n]);
                }
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let answer = match pages.iter().find(|p| p.0 == path) {
                    Some((_, status, headers, body)) => {
                        let mut out = format!("HTTP/1.0 {status} X\r\n");
                        for (k, v) in headers {
                            let _ = write!(out, "{k}: {v}\r\n");
                        }
                        let _ = write!(out, "content-length: {}\r\n\r\n{body}", body.len());
                        out
                    }
                    None => "HTTP/1.0 404 Not Found\r\ncontent-length: 0\r\n\r\n".into(),
                };
                let _ = s.write_all(answer.as_bytes());
            }
        });
        port
    }

    const HTML: (&str, &str) = ("content-type", "text/html; charset=utf-8");
    const SEC: [(&str, &str); 3] = [
        ("content-security-policy", "default-src 'none'"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "same-origin"),
    ];
    const LONG: (&str, &str) = ("cache-control", "public, max-age=31536000, immutable");

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<_> = SEC.to_vec();
        v.extend_from_slice(extra);
        v
    }

    fn smoke(extra: &str) -> Smoke {
        toml::from_str(&format!(
            "up = \"true\"\ndown = \"true\"\nroutes = [\"/\"]\n{extra}"
        ))
        .unwrap()
    }

    fn none() -> Vec<String> {
        Vec::new()
    }

    fn problems(s: &Smoke, port: u16) -> Vec<String> {
        let mut fetch = |p: &str| get(port, p);
        builtin(s, &mut fetch, &|| false).problems
    }

    fn page_with(body: &'static str) -> Vec<Page> {
        vec![
            ("/", 200, with(&[HTML]), body),
            ("/a.css", 200, with(&[LONG]), "body{}"),
        ]
    }

    #[test]
    fn a_good_site_has_no_problems() {
        let port = serve(page_with(
            "<link rel=\"stylesheet\" href=\"/a.css\"><a href=\"/nope\">x</a>",
        ));
        assert_eq!(problems(&smoke(""), port), Vec::<String>::new());
    }

    #[test]
    fn a_server_error_fails() {
        let port = serve(vec![("/", 503, with(&[HTML]), "down")]);
        let p = problems(&smoke(""), port);
        assert!(p.iter().any(|l| l.contains("server error 503")), "{p:?}");
    }

    #[test]
    fn missing_security_headers_fail() {
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let p = problems(&smoke(""), port);
        assert!(
            p.iter().any(|l| l.contains(
                "missing headers content-security-policy, x-content-type-options, referrer-policy"
            )),
            "{p:?}"
        );
        assert_eq!(problems(&smoke("headers = []"), port), none());
    }

    #[test]
    fn a_missing_asset_fails() {
        let port = serve(page_with(
            "<link rel=\"stylesheet\" href=\"/a.css\"><script src=\"/gone.js\"></script><img src=\"//cdn.example/x.png\">",
        ));
        let p = problems(&smoke(""), port);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("/gone.js") && p[0].contains("404"), "{p:?}");
    }

    #[test]
    fn a_short_cache_lifetime_fails_and_can_be_turned_off() {
        let port = serve(vec![
            ("/", 200, with(&[HTML]), "<link rel=stylesheet href=/a.css>"),
            (
                "/a.css",
                200,
                with(&[("cache-control", "max-age=60")]),
                "b{}",
            ),
        ]);
        let p = problems(&smoke(""), port);
        assert!(p.iter().any(|l| l.contains("cache lifetime 60s")), "{p:?}");
        assert_eq!(problems(&smoke("min_cache = 0"), port), none());
        let none = serve(vec![
            ("/", 200, with(&[HTML]), "<script src=\"/a.js\"></script>"),
            ("/a.js", 200, with(&[]), "x"),
        ]);
        assert!(
            problems(&smoke(""), none)
                .iter()
                .any(|l| l.contains("Cache-Control: none"))
        );
    }

    #[test]
    fn dev_urls_in_pages_fail() {
        for bad in [
            "http://localhost:3000/x",
            "http://127.0.0.1:8080/a",
            "ws://0.0.0.0/",
        ] {
            let port = serve(vec![(
                "/",
                200,
                with(&[HTML]),
                Box::leak(format!("<a href=\"{bad}\">x</a>").into_boxed_str()),
            )]);
            let p = problems(&smoke(""), port);
            assert!(p.iter().any(|l| l.contains("names ")), "{bad}: {p:?}");
        }
    }

    #[test]
    fn sentry_like_hostname_comparisons_pass() {
        let js = r#"var i=location.hostname;e["sentry.is_localhost"]="localhost"===i||"127.0.0.1"===i||"[::1]"===i;"#;
        let port = serve(vec![
            ("/", 200, with(&[HTML]), "<script src=\"/s.js\"></script>"),
            (
                "/s.js",
                200,
                with(&[LONG, ("content-type", "text/javascript")]),
                js,
            ),
        ]);
        assert_eq!(problems(&smoke(""), port), none());
    }

    #[test]
    fn leaked_url_in_js_still_fails_unless_allowed() {
        let js = "fetch(\"http://localhost:3000/api\")";
        let page = || {
            vec![
                ("/", 200, with(&[HTML]), "<script src=\"/s.js\"></script>"),
                (
                    "/s.js",
                    200,
                    with(&[LONG, ("content-type", "text/javascript")]),
                    js,
                ),
            ]
        };
        let p = problems(&smoke(""), serve(page()));
        assert!(p.iter().any(|l| l.contains("names localhost")), "{p:?}");
        let allowed = smoke("http_allow = [\"/s.*\"]");
        assert_eq!(problems(&allowed, serve(page())), none());
    }

    #[test]
    fn glob_and_url_context() {
        assert!(glob_match(
            "/script/accounts-*.js",
            "/script/accounts-8b221d79.js"
        ));
        assert!(!glob_match("/script/accounts-*.js", "/script/other.js"));
        assert!(glob_match("/a", "/a") && !glob_match("/a", "/ab"));
        assert_eq!(dev_urls("\"localhost\"===i"), none());
        assert_eq!(dev_urls("ws://[::1]:5555/x").len(), 1);
        assert_eq!(dev_urls("{\"u\":\"http:\\/\\/localhost\"}").len(), 1);
        assert_eq!(dev_urls("//127.0.0.1:80/").len(), 1);
    }

    #[test]
    fn project_checks_judge_status_body_redirect_type_and_size() {
        let port = serve(vec![
            (
                "/ok",
                200,
                vec![("content-type", "application/json")],
                "{\"a\":1}",
            ),
            ("/old", 301, vec![("location", "/new")], ""),
            (
                "/abs",
                301,
                vec![("location", "http://example.test/new")],
                "",
            ),
        ]);
        let checks = |t: &str| -> Vec<Check> {
            toml::from_str::<toml::Table>(&format!("c = [{t}]")).unwrap()["c"]
                .clone()
                .try_into()
                .unwrap()
        };
        let run = |t: &str| {
            let mut fetch = |p: &str| get(port, p);
            project_checks(
                &checks(t),
                &format!("127.0.0.1:{port}"),
                &mut fetch,
                &|| false,
            )
        };
        assert_eq!(
            run(
                r#"{ path = "/ok", contains = "\"a\":1", content_type = "json", max_bytes = 7 },
                   { path = "/old", status = 301, location = "/new" }"#
            ),
            none()
        );
        let bad = run(
            r#"{ path = "/ok", status = 201, contains = "zzz", content_type = "html", max_bytes = 3 },
               { path = "/old", status = 301, location = "/other" },
               { path = "/missing" }"#,
        );
        for want in [
            "status 200, expected 201",
            "does not contain \"zzz\"",
            "content type application/json, expected html",
            "7 bytes, over 3",
            "redirects to /new, expected /other",
            "/missing: status 404, expected 200",
        ] {
            assert!(bad.iter().any(|l| l.contains(want)), "{want}: {bad:?}");
        }
        assert_eq!(bad.len(), 6, "{bad:?}");
        assert_eq!(run(r#"{ path = "/abs", status = 301 }"#), none());
    }

    #[test]
    fn requests_are_parsed() {
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-A: b\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n")
            .unwrap();
        assert_eq!(
            (r.status, r.header("x-a"), r.body.as_slice()),
            (200, Some("b"), b"abcde".as_slice())
        );
        assert!(parse(b"nonsense").is_err());
        assert!(get(serve(vec![]), "/").is_ok_and(|r| r.status == 404));
    }

    #[test]
    fn asset_refs_find_parts_of_the_page_and_skip_links_and_inline_code() {
        let html = r#"<head><link rel="stylesheet" href="/a.css"><link rel="canonical" href="/c">
            <link rel="icon" href='/i.png'><script>if (a<b && c>d) { x = "<img src=/fake.png>" }</script>
            <script defer src=/b.js></script></head><body><a href="/page">p</a><IMG SRC="c.png"></body>"#;
        assert_eq!(asset_refs(html), ["/a.css", "/i.png", "/b.js", "c.png"]);
        assert_eq!(local_path("c.png", "/x/y"), Some("/x/c.png".into()));
        assert_eq!(local_path("/a.css?v=2#f", "/"), Some("/a.css?v=2".into()));
        for off in [
            "https://cdn.test/a.js",
            "//cdn.test/a.js",
            "data:image/png;base64,AA",
            "#top",
            "mailto:a@b",
        ] {
            assert_eq!(local_path(off, "/"), None, "{off}");
        }
    }

    #[test]
    fn cache_lifetimes() {
        assert_eq!(cache_age(Some("public, max-age=31536000")), 31_536_000);
        assert_eq!(cache_age(Some("max-age=60, no-cache")), 0);
        assert_eq!(cache_age(Some("no-store")), 0);
        assert!(cache_age(Some("immutable")) >= 31_536_000);
        assert_eq!(cache_age(None), 0);
    }

    #[test]
    fn logs_with_panics_and_errors_fail_but_info_and_ignored_lines_do_not() {
        let log = "INFO started\nthread 'main' panicked at src/main.rs:3\n{\"level\":\"ERROR\",\"message\":\"x\"}\n\
                   WARN 0 errors seen\nINFO no_errors=1\nError: WebServer(missing)\nERROR sentry: dns lookup";
        let hits = scan_logs(log, &[]);
        assert_eq!(hits.len(), 4, "{hits:?}");
        let hits = scan_logs(log, &["sentry".to_string()]);
        assert_eq!(hits.len(), 3, "{hits:?}");
    }

    #[test]
    fn info_lines_that_mention_errors_are_not_flagged() {
        let log =
            "INFO error=none\nINFO GET /error-page 200\nlevel=info msg=\"no error\"\nerror_count=0";
        assert_eq!(scan_logs(log, &[]), none());
        assert_eq!(
            scan_logs("ts level=error msg=x\nts level=info", &[]).len(),
            1
        );
        assert_eq!(scan_logs("{\"level\": \"fatal\"}", &[]).len(), 1);
    }

    #[test]
    fn a_location_on_another_host_is_not_the_local_path() {
        let port = serve(vec![
            ("/abs", 301, vec![("location", "http://other.test/x")], ""),
            ("/self", 301, vec![("location", "http://HOST/x")], ""),
        ]);
        let host = format!("127.0.0.1:{port}");
        let mut fetch = |p: &str| {
            get(port, p).map(|mut r| {
                for (_, v) in &mut r.headers {
                    *v = v.replace("HOST", &host);
                }
                r
            })
        };
        let checks: Vec<Check> = toml::from_str::<toml::Table>(
            r#"c = [{ path = "/abs", status = 301, location = "/x" }, { path = "/self", status = 301, location = "/x" }]"#,
        )
        .unwrap()["c"]
            .clone()
            .try_into()
            .unwrap();
        let p = project_checks(&checks, &host, &mut fetch, &|| false);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].starts_with("/abs: redirects to http://other.test/x"),
            "{p:?}"
        );
    }

    #[test]
    fn the_dev_url_scan_covers_scripts_styles_and_headers() {
        let port = serve(vec![
            (
                "/",
                200,
                vec![HTML],
                "<script src=/a.js></script><link rel=stylesheet href=/a.css>",
            ),
            (
                "/a.js",
                200,
                vec![
                    ("content-type", "text/javascript"),
                    ("cache-control", "max-age=31536000"),
                ],
                "fetch('http://localhost:3000/api')",
            ),
            (
                "/a.css",
                200,
                vec![
                    ("content-type", "text/css"),
                    ("cache-control", "max-age=31536000"),
                ],
                "a{background:url(http://127.0.0.1/x.png)}",
            ),
        ]);
        let p = problems(&smoke("headers = []"), port);
        assert!(
            p.iter()
                .any(|l| l.starts_with("/a.js:") && l.contains("localhost")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|l| l.starts_with("/a.css:") && l.contains("127.0.0.1")),
            "{p:?}"
        );
        let port = serve(vec![(
            "/",
            200,
            vec![HTML, ("link", "<http://localhost:5173/x>; rel=preload")],
            "hi",
        )]);
        let p = problems(&smoke("headers = []"), port);
        assert!(
            p.iter()
                .any(|l| l.contains("header") && l.contains("localhost")),
            "{p:?}"
        );
    }

    #[test]
    fn paths_with_spaces_or_control_characters_are_rejected() {
        for bad in ["/a b", "/a\tb", "/a\nb"] {
            let s: Smoke =
                toml::from_str(&format!("up = \"u\"\ndown = \"d\"\nroutes = [{bad:?}]\n")).unwrap();
            assert!(s.validate(true).is_err(), "{bad:?}");
        }
        let ok: Smoke =
            toml::from_str("up = \"u\"\ndown = \"d\"\nroutes = [\"/a?b=c\"]\n").unwrap();
        assert!(ok.validate(true).is_ok());
    }

    #[test]
    fn sweep_true_without_a_sweep_section_is_rejected() {
        let s: Smoke = toml::from_str("up = \"u\"\ndown = \"d\"\nsweep = true\n").unwrap();
        assert!(s.validate(false).is_err());
        assert!(s.validate(true).is_ok());
    }

    #[test]
    fn a_dribbling_route_times_out_and_a_stop_ends_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = stream;
                    let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
                    for _ in 0..600 {
                        if s.write_all(b"x").is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                });
            }
        });
        let started = Instant::now();
        let limits = Limits {
            total: Duration::from_millis(600),
            max_bytes: 1 << 20,
        };
        let e = get_with(port, "/", limits, &|| false).unwrap_err();
        assert!(e.contains("took over"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let e = get_with(port, "/", Limits::default(), &|| true).unwrap_err();
        assert_eq!(e, "interrupted");
    }

    #[test]
    fn a_huge_answer_is_cut_off_at_the_byte_cap() {
        let body: &'static str = Box::leak("y".repeat(200_000).into_boxed_str());
        let port = serve(vec![("/big", 200, vec![HTML], body)]);
        let limits = Limits {
            total: Duration::from_secs(10),
            max_bytes: 50_000,
        };
        let e = get_with(port, "/big", limits, &|| false).unwrap_err();
        assert!(e.contains("over 50000 bytes"), "{e}");
    }

    #[test]
    fn a_failing_logs_command_fails_the_run() {
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let (o, marker) = run_marked(
            "logsfail",
            &up_ok(),
            "routes = [\"/\"]\nlogs = \"echo all fine; exit 5\"",
            port,
        );
        assert!(!o.passed(), "{:?}", lines(&o));
        assert!(
            names(&o).contains(&"smoke.logs FAIL".to_string()),
            "{:?}",
            names(&o)
        );
        assert!(marker.exists());
        let (o, _) = run_marked(
            "logsok",
            &up_ok(),
            "routes = [\"/\"]\nlogs = \"echo all fine\"",
            port,
        );
        assert!(o.passed(), "{:?}", lines(&o));
    }

    /// Down that hangs: the whole group dies at the limit, including what the shell started.
    #[test]
    fn a_hung_down_is_killed_with_its_whole_group() {
        let dir = scratch("hungdown");
        let pidfile = dir.join("pid");
        let mut down = Down {
            command: format!("sleep 300 & echo $! > {}; wait", pidfile.display()),
            root: dir.clone(),
            log: dir.join("down.log"),
            done: false,
            limit: Duration::from_millis(600),
        };
        let e = down.run().unwrap_err();
        assert!(e.contains("took over"), "{e}");
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        std::thread::sleep(Duration::from_millis(300));
        let alive = Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the grandchild sleep {pid} survived");
    }

    /// A signal while the checks run: no later step runs (no logs), and down still does.
    #[test]
    fn a_smoke_child_for_the_mid_checks_test() {
        let Ok(marker) = std::env::var("SHAKEDOWN_SMOKE_MID") else {
            return;
        };
        let dir = PathBuf::from(&marker).parent().unwrap().to_path_buf();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut head = Vec::new();
                    let mut buf = [0_u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    if String::from_utf8_lossy(&head).contains("GET /slow") {
                        // Never answers.
                        std::thread::sleep(Duration::from_secs(60));
                    } else {
                        let _ =
                            s.write_all(b"HTTP/1.0 200 OK\r\ncontent-type: text/html\r\n\r\nhi");
                    }
                });
            }
        });
        let s: Smoke = toml::from_str(&format!(
            "up = {:?}\ndown = \"touch {marker}\"\nheaders = []\nroutes = [\"/\", \"/slow\"]\nlogs = \"touch {}\"\n",
            up_ok(),
            dir.join("logs-ran").display()
        ))
        .unwrap();
        let signals = world::watch_signals_shielded();
        println!("waiting");
        let o = run_on(&s, &dir, &dir.join("out"), port, &signals, &no_sweep).unwrap();
        println!("finished {}", names(&o).join(","));
    }

    #[test]
    fn two_signals_during_the_checks_skip_to_down_and_do_not_kill_it() {
        let dir = scratch("mid");
        let marker = dir.join("down-ran");
        let mut c: Child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "smoke::tests::a_smoke_child_for_the_mid_checks_test",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SHAKEDOWN_SMOKE_MID", &marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(c.stdout.take().unwrap()).lines();
        loop {
            if lines
                .next()
                .expect("the child ended early")
                .unwrap()
                .ends_with("waiting")
            {
                break;
            }
        }
        // Let it get stuck on /slow, then Ctrl-C twice.
        std::thread::sleep(Duration::from_millis(1500));
        for _ in 0..2 {
            let _ = Command::new("kill")
                .args(["-INT", &c.id().to_string()])
                .status();
            std::thread::sleep(Duration::from_millis(100));
        }
        let rest: Vec<String> = lines.map_while(Result::ok).collect();
        assert!(
            c.wait().unwrap().success(),
            "the child died on the second Ctrl-C: {rest:?}"
        );
        assert!(marker.exists(), "down ran: {rest:?}");
        assert!(
            !dir.join("logs-ran").exists(),
            "the logs step ran after the signal: {rest:?}"
        );
        let done = rest
            .iter()
            .find(|l| l.starts_with("finished"))
            .expect("finished");
        assert!(done.contains("smoke.interrupted FAIL"), "{done}");
        assert!(!done.contains("smoke.logs"), "{done}");
    }

    #[test]
    fn the_digest_is_found_in_the_up_output() {
        assert_eq!(
            digest_in(&format!("image: {DIGEST}\n")),
            Some(DIGEST.into())
        );
        assert_eq!(digest_in("sha256:abc"), None);
        assert_eq!(digest_in("nothing"), None);
    }

    fn no_sweep(_port: u16) -> Result<Vec<String>, String> {
        Err("no [sweep] section in shakedown.toml".into())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shakedown-smoke-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs the whole sequence against the local server with up and down that only leave marks.
    fn run_marked(name: &str, up: &str, extra: &str, port: u16) -> (Outcome, PathBuf) {
        let dir = scratch(name);
        let marker = dir.join("down-ran");
        let s: Smoke = toml::from_str(&format!(
            "up = {up:?}\ndown = \"touch {}\"\nheaders = []\n{extra}",
            marker.display()
        ))
        .unwrap();
        let _serial = world::tests::serial();
        let signals = world::watch_signals();
        let outcome = run_on(&s, &dir, &dir.join("out"), port, &signals, &no_sweep).unwrap();
        (outcome, marker)
    }

    /// `sweep = true` runs the sweeper against the container's port: findings fail the step, none pass it.
    #[test]
    fn the_sweep_step_fails_on_findings_and_passes_without() {
        let dir = scratch("sweep-step");
        let s: Smoke = toml::from_str(&format!(
            "up = {:?}\ndown = \"true\"\nheaders = []\nsweep = true\n",
            up_ok()
        ))
        .unwrap();
        let _serial = world::tests::serial();
        let signals = world::watch_signals();
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let seen = std::sync::atomic::AtomicU16::new(0);
        let bad = |p: u16| {
            seen.store(p, std::sync::atomic::Ordering::SeqCst);
            Ok(vec!["home@phone-light: scrolls sideways".to_string()])
        };
        let o = run_on(&s, &dir, &dir.join("out"), port, &signals, &bad).unwrap();
        let step = o.results.iter().find(|r| r.name == "smoke.sweep").unwrap();
        assert!(matches!(step.status, Status::Fail(_)), "{:?}", step.status);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), port);
        let good = |_p: u16| Ok(Vec::new());
        let o = run_on(&s, &dir, &dir.join("out"), port, &signals, &good).unwrap();
        let step = o.results.iter().find(|r| r.name == "smoke.sweep").unwrap();
        assert!(matches!(step.status, Status::Ok), "{:?}", step.status);
    }

    fn up_ok() -> String {
        format!("echo image: {DIGEST}")
    }

    fn names(o: &Outcome) -> Vec<String> {
        o.results
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    r.name,
                    match &r.status {
                        Status::Ok => "ok",
                        Status::Fail(_) => "FAIL",
                        Status::Skipped(_) => "skip",
                    }
                )
            })
            .collect()
    }

    #[test]
    fn down_runs_after_a_pass() {
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let (o, marker) = run_marked("pass", &up_ok(), "routes = [\"/\"]", port);
        assert!(o.passed(), "{:?}", lines(&o));
        assert_eq!(o.digest.as_deref(), Some(DIGEST));
        assert!(marker.exists(), "down ran");
        assert_eq!(
            names(&o),
            [
                "smoke.up ok",
                "smoke.ready ok",
                "smoke.http ok",
                "smoke.checks ok",
                "smoke.logs skip",
                "smoke.down ok"
            ]
        );
    }

    #[test]
    fn down_runs_after_a_failed_check() {
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let (o, marker) = run_marked(
            "failed",
            &up_ok(),
            "checks = [{ path = \"/\", contains = \"nope\" }]",
            port,
        );
        assert!(!o.passed());
        assert!(
            names(&o).contains(&"smoke.checks FAIL".to_string()),
            "{:?}",
            names(&o)
        );
        assert!(marker.exists(), "down ran");
    }

    #[test]
    fn down_runs_when_up_fails_and_the_rest_is_skipped() {
        let (o, marker) = run_marked("upfail", "echo partial; exit 3", "", 1);
        assert!(!o.passed());
        assert!(marker.exists(), "down ran even though up failed half way");
        assert_eq!(names(&o)[0], "smoke.up FAIL");
        assert!(
            names(&o)
                .iter()
                .skip(1)
                .take(4)
                .all(|n| n.ends_with("skip")),
            "{:?}",
            names(&o)
        );
    }

    #[test]
    fn an_image_that_prints_no_digest_fails() {
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let (o, marker) = run_marked("nodigest", "echo started", "", port);
        assert!(names(&o).contains(&"smoke.up FAIL".to_string()));
        assert!(marker.exists());
    }

    #[test]
    fn down_runs_when_the_container_never_answers() {
        let port = world::free_port().unwrap();
        let (o, marker) = run_marked("never", &up_ok(), "timeout = 1", port);
        assert!(
            names(&o).contains(&"smoke.ready FAIL".to_string()),
            "{:?}",
            names(&o)
        );
        assert!(marker.exists(), "down ran");
    }

    #[test]
    fn a_failing_down_fails_the_run() {
        let dir = scratch("downfail");
        let s: Smoke = toml::from_str(&format!("up = {:?}\ndown = \"exit 4\"\n", up_ok())).unwrap();
        let port = serve(vec![("/", 200, vec![HTML], "hi")]);
        let _serial = world::tests::serial();
        let signals = world::watch_signals();
        let o = run_on(&s, &dir, &dir.join("out"), port, &signals, &no_sweep).unwrap();
        assert!(!o.passed());
        assert_eq!(names(&o).last().unwrap(), "smoke.down FAIL");
    }

    #[test]
    fn down_runs_when_a_panic_unwinds() {
        let dir = scratch("panic");
        let marker = dir.join("down-ran");
        let caught = std::panic::catch_unwind(|| {
            let _down = Down {
                command: format!("touch {}", marker.display()),
                root: dir.clone(),
                log: dir.join("down.log"),
                done: false,
                limit: Duration::from_secs(5),
            };
            panic!("a bug in a check");
        });
        assert!(caught.is_err());
        assert!(marker.exists(), "the drop guard ran down");
    }

    #[test]
    fn down_runs_once() {
        let dir = scratch("once");
        let counter = dir.join("count");
        let mut down = Down {
            command: format!("echo x >> {}", counter.display()),
            root: dir.clone(),
            log: dir.join("down.log"),
            done: false,
            limit: Duration::from_secs(5),
        };
        down.run().unwrap();
        drop(down);
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1
        );
    }

    /// The child below runs a smoke whose container never answers, so it waits; the parent interrupts it.
    #[test]
    fn a_smoke_child_for_the_signal_test() {
        let Ok(marker) = std::env::var("SHAKEDOWN_SMOKE_CHILD") else {
            return;
        };
        let dir = PathBuf::from(&marker).parent().unwrap().to_path_buf();
        let s: Smoke = toml::from_str(&format!(
            "up = {:?}\ndown = \"touch {marker}\"\ntimeout = 30\n",
            up_ok()
        ))
        .unwrap();
        let signals = world::watch_signals();
        println!("waiting");
        let o = run_on(
            &s,
            &dir,
            &dir.join("out"),
            world::free_port().unwrap(),
            &signals,
            &no_sweep,
        )
        .unwrap();
        println!("finished {}", names(&o).join(","));
        println!("down-marker {}", PathBuf::from(&marker).exists());
    }

    #[test]
    fn ctrl_c_during_the_wait_still_runs_down() {
        let dir = scratch("ctrlc");
        let marker = dir.join("down-ran");
        let mut c: Child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "smoke::tests::a_smoke_child_for_the_signal_test",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SHAKEDOWN_SMOKE_CHILD", &marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(c.stdout.take().unwrap()).lines();
        loop {
            if lines
                .next()
                .expect("the child ended early")
                .unwrap()
                .ends_with("waiting")
            {
                break;
            }
        }
        let _ = Command::new("kill")
            .args(["-INT", &c.id().to_string()])
            .status();
        let rest: Vec<String> = lines.map_while(Result::ok).collect();
        assert!(
            c.wait().unwrap().success(),
            "the child survived the first Ctrl-C and ended on its own"
        );
        assert!(marker.exists(), "down ran after Ctrl-C: {rest:?}");
        let done = rest
            .iter()
            .find(|l| l.starts_with("finished"))
            .expect("finished");
        assert!(
            done.contains("smoke.interrupted FAIL") && done.contains("smoke.down ok"),
            "{done}"
        );
    }
}
