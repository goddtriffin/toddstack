//! `shakedown devices serve`: every device side by side in iframes, light and dark, with the layout rules run on
//! each frame. It replaces infra's `scripts/devices/devices.py`.
//!
//! A proxy in front of a local app, bound to 127.0.0.1 only. It serves the viewer at `/__devices`, removes the
//! headers that forbid framing, and injects the collector into every HTML page. Frame `i` loads from
//! `p<i+1>.localhost:<port>`: Chrome resolves `*.localhost` to this machine and keeps cookies per host, so every
//! frame is its own signed-in player. Frames are cross-origin to the viewer, so each frame posts its layout tree
//! up and the viewer sends it to `/__devices/check`, where the same Rust rules as the sweep judge it.
//! The browser window is never resized.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use crate::config::{Config, SweepRoute};
use crate::devices;
use crate::error::Error;
use crate::layout::{self, Tree};

const VIEWER: &str = include_str!("viewer.html");
const COLLECTOR: &str = include_str!("collector.js");
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 8 * 1024 * 1024;
const HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    "content-length",
];

/// What the viewer needs to know.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The port on 127.0.0.1.
    pub port: u16,
    /// `host:port` of the app.
    pub target: String,
    /// A page list, re-read on every load (the infra format).
    pub routes_file: Option<PathBuf>,
    /// Pages from `[sweep]`.
    pub config_routes: Vec<Route>,
    /// Accounts and the sign-in path, from `[players]`.
    pub players: Option<(Vec<String>, String)>,
    /// `[layout].ignore`, as JSON for the collector.
    pub ignores: String,
}

/// One entry of the page picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The path.
    pub path: String,
    /// A label (may be empty).
    pub label: String,
    /// The group (may be empty).
    pub group: String,
}

impl Settings {
    /// Settings from a `shakedown.toml`, when there is one.
    pub fn with_config(mut self, config: &Config) -> Self {
        self.ignores = serde_json::to_string(&config.layout.ignore).unwrap_or_else(|_| "[]".into());
        self.players = config
            .players
            .as_ref()
            .map(|p| (p.accounts.clone(), p.signin.clone()));
        if let Some(s) = &config.sweep {
            for r in &s.routes {
                let path = match r {
                    SweepRoute::Path(p) => p.clone(),
                    SweepRoute::Page(p) => p.path.clone(),
                };
                self.config_routes.push(Route {
                    label: crate::sweep::page_name(None, &path),
                    path,
                    group: "Sweep".into(),
                });
            }
            for g in &s.groups {
                self.config_routes.push(Route {
                    path: g.path.clone(),
                    label: g.name.clone(),
                    group: "Sweep groups".into(),
                });
            }
        }
        self
    }
}

/// A target must be a local app: the viewer never proxies anywhere else. Returns `host:port`.
///
/// # Errors
///
/// Not an http URL, or not a loopback host.
pub fn target_addr(target: &str) -> Result<String, Error> {
    let base = crate::sweep::check_base(target)?;
    let url = url::Url::parse(&base).map_err(|e| Error::Config(e.to_string()))?;
    let host = url.host_str().unwrap_or("127.0.0.1");
    let host = if host == "localhost" || host.ends_with(".localhost") {
        "127.0.0.1"
    } else {
        host
    };
    let port = url.port_or_known_default().unwrap_or(80);
    Ok(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}

/// Parses the page list file: one page per line (a path, then an optional label after two or more spaces),
/// `[Name]` starts a group, blank lines and `#` comments are ignored.
pub fn parse_routes(text: &str) -> Vec<Route> {
    let mut out = Vec::new();
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            line[1..line.len() - 1].trim().clone_into(&mut group);
            continue;
        }
        let (path, label) = match line.find("  ") {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        out.push(Route {
            path: path.into(),
            label: label.into(),
            group: group.clone(),
        });
    }
    out
}

/// Only this machine may talk to the viewer (a page on the internet cannot reach it through DNS rebinding).
pub fn host_allowed(host: &str) -> bool {
    let name = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    name == "localhost" || name == "127.0.0.1" || name.ends_with(".localhost")
}

/// Appends the script tag at the very end of the page. Nothing in the document is parsed: whatever follows `</html>`
/// is parsed into the body, so the document mode (standards or quirks) can never change, and the tag cannot land in a
/// comment or a string unless the page itself ends inside an unclosed comment or script (a broken page anyway).
/// The collector does not depend on running early: it reports on load, or at once when the page already loaded.
pub fn inject(html: &str, tag: &str) -> String {
    format!("{html}{tag}")
}

/// `frame-ancestors` may name the viewer's own origins and nobody else.
pub fn rewrite_csp(value: &str, port: u16) -> String {
    let mine = format!("frame-ancestors http://localhost:{port} http://127.0.0.1:{port}");
    let mut parts: Vec<String> = Vec::new();
    let mut replaced = false;
    for p in value.split(';') {
        if p.trim_start().starts_with("frame-ancestors") {
            parts.push(mine.clone());
            replaced = true;
        } else {
            parts.push(p.trim().to_string());
        }
    }
    let _ = replaced;
    parts.retain(|p| !p.is_empty());
    parts.join("; ")
}

/// Decodes a chunked body.
pub fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(nl) = body.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = std::str::from_utf8(&body[..nl])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok());
        let Some(size) = size else { return out };
        body = &body[nl + 2..];
        if size == 0 || body.len() < size {
            if size > 0 {
                out.extend_from_slice(body);
            }
            return out;
        }
        out.extend_from_slice(&body[..size]);
        body = body.get(size + 2..).unwrap_or(&[]);
    }
}

struct Head {
    first: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(raw: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(raw);
        let mut lines = text.split("\r\n");
        let first = lines.next()?.to_string();
        let headers = lines
            .filter(|l| !l.is_empty())
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        Some(Self { first, headers })
    }
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads up to the blank line; returns the head and whatever came after it.
fn read_head(stream: &mut impl Read) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            buf.truncate(i);
            return Some((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn respond(out: &mut impl Write, status: &str, kind: &str, body: &[u8]) {
    let _ = write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = out.write_all(body);
}

/// The collector for frames: no frozen clock, and it reports up to the viewer.
pub(crate) fn frame_js(ignores: &str) -> String {
    let collector = COLLECTOR
        .replace("__FIXED__", "false")
        .replace("__AT__", "0")
        .replace("__SEED__", "0");
    format!(
        "(() => {{ if (window.parent === window) return;\n{collector}\n\
const IGN = {ignores};\n\
const up = (m) => {{ try {{ parent.postMessage({{ shakedown: true, ...m }}, '*'); }} catch {{ /* no parent */ }} }};\n\
const links = () => [...document.querySelectorAll('a[href]')].map((a) => {{ try {{ const u = new URL(a.href); return u.origin === location.origin && !u.pathname.startsWith('/static/') ? u.pathname + u.search : null; }} catch {{ return null; }} }}).filter(Boolean);\n\
const lead = () => up({{ shakedown: 'lead' }});\n\
addEventListener('click', (e) => {{ if (e.target.closest && e.target.closest('a[href],button,input[type=submit]')) lead(); }}, true);\n\
addEventListener('submit', lead, true);\n\
let timer = 0, tries = 0;\n\
const report = () => {{ clearTimeout(timer); timer = setTimeout(() => {{ let tree = null; try {{ tree = window.__shakedown.tree(IGN); }} catch {{ if (++tries < 6) report(); return; }} up({{ shakedown: 'loaded', path: location.pathname + location.search, links: links(), tree }}); }}, 500); }};\n\
addEventListener('load', report); addEventListener('resize', report);\n\
if (document.readyState === 'complete') report();\n\
}})();"
    )
}

fn handle(mut client: TcpStream, s: &Settings, script: &str) {
    let _ = client.set_read_timeout(Some(Duration::from_secs(30)));
    let Some((raw, mut body)) = read_head(&mut client) else {
        return;
    };
    let Some(head) = Head::parse(&raw) else {
        return;
    };
    let mut parts = head.first.split(' ');
    let (method, target) = (
        parts.next().unwrap_or("GET").to_string(),
        parts.next().unwrap_or("/").to_string(),
    );
    if !host_allowed(head.get("host").unwrap_or("")) {
        respond(&mut client, "403 Forbidden", "text/plain", b"local only");
        return;
    }
    let wanted = head
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_BODY);
    while body.len() < wanted {
        let mut chunk = [0_u8; 8192];
        match client.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let path = target.split('?').next().unwrap_or("/");
    if !internal(&mut client, &method, path, &body, s, script) {
        proxy(client, &method, &target, &head, &body, s);
    }
}

/// The viewer's own pages and endpoints; `false` when the path is the app's.
fn internal(
    client: &mut TcpStream,
    method: &str,
    path: &str,
    body: &[u8],
    s: &Settings,
    script: &str,
) -> bool {
    match path {
        "/__devices" | "/__devices/" => {
            respond(
                client,
                "200 OK",
                "text/html; charset=utf-8",
                VIEWER.as_bytes(),
            );
        }
        "/__devices/devices.json" => {
            let list: Vec<_> = devices::all()
                .iter()
                .map(|d| json!({"name": d.name, "label": d.label, "width": d.width, "height": d.height, "mobile": d.mobile}))
                .collect();
            respond(
                client,
                "200 OK",
                "application/json",
                json!(list).to_string().as_bytes(),
            );
        }
        "/__devices/routes" => {
            let mut routes = s
                .routes_file
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map_or_else(Vec::new, |t| parse_routes(&t));
            routes.extend(s.config_routes.iter().cloned());
            let list: Vec<_> = routes
                .iter()
                .map(|r| json!({"path": r.path, "label": r.label, "group": r.group}))
                .collect();
            respond(
                client,
                "200 OK",
                "application/json",
                json!(list).to_string().as_bytes(),
            );
        }
        "/__devices/config" => {
            let v = s.players.as_ref().map_or_else(
                || json!({"players": [], "signin": ""}),
                |(a, p)| json!({"players": a, "signin": p}),
            );
            respond(
                client,
                "200 OK",
                "application/json",
                v.to_string().as_bytes(),
            );
        }
        "/__devices/frame.js" => {
            respond(
                client,
                "200 OK",
                "text/javascript; charset=utf-8",
                script.as_bytes(),
            );
        }
        "/__devices/check" if method == "POST" => {
            let findings: Vec<String> = serde_json::from_slice::<Tree>(body).map_or_else(
                |e| vec![format!("could not read the layout tree: {e}")],
                |t| {
                    layout::check(&t)
                        .iter()
                        .map(layout::Finding::line)
                        .collect()
                },
            );
            respond(
                client,
                "200 OK",
                "application/json",
                json!(findings).to_string().as_bytes(),
            );
        }
        _ => return false,
    }
    true
}

const ATTEMPTS: u32 = 4;

/// Connects to the app and reads the answer head, retrying with a short backoff (a busy app drops connections when
/// many frames load at once). A request with a side effect is retried only while nothing was sent.
fn upstream(
    method: &str,
    target: &str,
    head: &Head,
    body: &[u8],
    s: &Settings,
) -> Result<(TcpStream, Head, Vec<u8>), String> {
    let mut req = format!("{method} {target} HTTP/1.0\r\n");
    for (k, v) in &head.headers {
        let lk = k.to_ascii_lowercase();
        if HOP.contains(&lk.as_str()) || lk == "accept-encoding" {
            continue;
        }
        let _ = write!(req, "{k}: {v}\r\n");
    }
    if !body.is_empty() {
        let _ = write!(req, "Content-Length: {}\r\n", body.len());
    }
    req.push_str("Connection: close\r\n\r\n");
    let safe = matches!(method, "GET" | "HEAD" | "OPTIONS");
    let mut why = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100 << (attempt - 1)));
        }
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&s.target)
            .ok()
            .and_then(|mut a| a.next());
        let Some(Ok(mut up)) = addr.map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(5)))
        else {
            why = format!("the app at {} is not answering", s.target);
            continue;
        };
        let _ = up.set_read_timeout(Some(Duration::from_secs(60)));
        if up.write_all(req.as_bytes()).is_err() || up.write_all(body).is_err() {
            why = format!("the app at {} dropped the request", s.target);
            if safe {
                continue;
            }
            break;
        }
        let answered = read_head(&mut up).and_then(|(raw, rest)| Some((Head::parse(&raw)?, rest)));
        if let Some((answer, rest)) = answered {
            return Ok((up, answer, rest));
        }
        why = format!(
            "the app at {} closed the connection without an answer",
            s.target
        );
        if !safe {
            break;
        }
    }
    Err(why)
}

/// A failed upstream: a 502 whose page tells the viewer (the frame's parent) that this frame will not report.
fn bad_gateway(client: &mut TcpStream, why: &str) {
    let safe: String = why
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " :.-_/".contains(*c))
        .collect();
    let page = format!(
        "<!doctype html><meta charset=utf-8><body>502: {safe}<script>try{{parent.postMessage({{shakedown:'failed',reason:'{safe}'}},'*')}}catch(e){{}}</script>"
    );
    respond(
        client,
        "502 Bad Gateway",
        "text/html; charset=utf-8",
        page.as_bytes(),
    );
}

fn proxy(
    mut client: TcpStream,
    method: &str,
    target: &str,
    head: &Head,
    body: &[u8],
    s: &Settings,
) {
    let (mut up, answer, mut rest) = match upstream(method, target, head, body, s) {
        Ok(v) => v,
        Err(why) => {
            bad_gateway(&mut client, &why);
            return;
        }
    };
    let is_html = answer
        .get("content-type")
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("text/html"));
    let chunked = answer
        .get("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    let mut out = format!("{}\r\n", answer.first.replacen("HTTP/1.0", "HTTP/1.1", 1));
    for (k, v) in &answer.headers {
        let lk = k.to_ascii_lowercase();
        if lk == "x-frame-options"
            || lk == "connection"
            || lk == "content-length" && is_html
            || lk == "transfer-encoding" && is_html
        {
            continue;
        }
        if lk == "content-security-policy" {
            let _ = write!(out, "{k}: {}\r\n", rewrite_csp(v, s_port(s)));
        } else {
            let _ = write!(out, "{k}: {v}\r\n");
        }
    }
    if is_html {
        let _ = up.read_to_end(&mut rest);
        let bytes = if chunked { dechunk(&rest) } else { rest };
        let html = String::from_utf8_lossy(&bytes);
        let page = inject(&html, "<script src=\"/__devices/frame.js\"></script>");
        let _ = write!(
            out,
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            page.len()
        );
        let _ = client.write_all(out.as_bytes());
        let _ = client.write_all(page.as_bytes());
        return;
    }
    out.push_str("Connection: close\r\n\r\n");
    if client.write_all(out.as_bytes()).is_err() || client.write_all(&rest).is_err() {
        return;
    }
    // Everything else passes through as it comes (event streams stay open).
    let mut chunk = [0_u8; 16 * 1024];
    while let Ok(n) = up.read(&mut chunk) {
        if n == 0 || client.write_all(&chunk[..n]).is_err() {
            break;
        }
        let _ = client.flush();
    }
}

fn s_port(s: &Settings) -> u16 {
    s.port
}

/// Runs the viewer until the process is stopped.
///
/// # Errors
///
/// The port is taken.
pub fn serve(s: &Settings) -> Result<(), Error> {
    let listener = TcpListener::bind(("127.0.0.1", s.port))
        .map_err(|e| Error::Environment(format!("cannot listen on 127.0.0.1:{}: {e}", s.port)))?;
    println!(
        "devices: http://localhost:{}/__devices?path=/  (app: {}; bound to 127.0.0.1 only; Ctrl-C stops)",
        s.port, s.target
    );
    serve_on(&listener, s);
    Ok(())
}

fn serve_on(listener: &TcpListener, s: &Settings) {
    let script: std::sync::Arc<str> = frame_js(&s.ignores).into();
    let settings = std::sync::Arc::new(s.clone());
    for stream in listener.incoming().flatten() {
        let (settings, script) = (settings.clone(), script.clone());
        std::thread::spawn(move || handle(stream, &settings, &script));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_machine_is_a_valid_host() {
        assert!(host_allowed("localhost:8799"));
        assert!(host_allowed("127.0.0.1:8799"));
        assert!(host_allowed("p3.localhost:8799"));
        assert!(!host_allowed("evil.example.com"));
        assert!(!host_allowed("localhost.evil.com:80"));
        assert!(!host_allowed(""));
    }

    #[test]
    fn the_target_must_be_local() {
        assert_eq!(
            target_addr("http://127.0.0.1:8790").unwrap(),
            "127.0.0.1:8790"
        );
        assert_eq!(
            target_addr("http://localhost:3000/").unwrap(),
            "127.0.0.1:3000"
        );
        assert!(target_addr("https://accounts.toddgriffin.me").is_err());
        assert!(target_addr("http://67.205.146.205").is_err());
    }

    #[test]
    fn the_page_list_reads_the_infra_format() {
        let r = parse_routes(
            "# pages\n[Auth]\n/sign-in  Sign in\n/sign-up\n\n[Account]\n/account    Account home\n",
        );
        assert_eq!(r.len(), 3);
        assert_eq!(
            r[0],
            Route {
                path: "/sign-in".into(),
                label: "Sign in".into(),
                group: "Auth".into()
            }
        );
        assert_eq!(r[1].label, "");
        assert_eq!(r[2].group, "Account");
        assert_eq!(r[2].label, "Account home");
    }

    #[test]
    fn the_script_is_appended_and_nothing_is_parsed() {
        let t = "<script src=x></script>";
        for page in [
            "\u{feff}<!doctype html><body>x",
            "<?xml version=\"1.0\"?><!doctype html><p>x",
            "<!--><!doctype html><p>x",
            "--!><!DOCTYPE html><p>x",
            "<!-- a > b --><!-- <!doctype html> -->\n<!doctype html><p>x",
            "<!doctype html><html lang=\"x\" data-a=\"a>b\"><head></head>",
            "<!doctype html><body><script>var a = \"<html>\";</script>",
            "<p>hi</p>",
            "",
        ] {
            assert_eq!(inject(page, t), format!("{page}{t}"), "{page}");
        }
    }

    fn app_that(
        handler: impl Fn(usize, TcpStream) + Send + Sync + 'static,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let i = n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                handler(i, c);
            }
        });
        (addr, n)
    }

    fn through_proxy(target: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let settings = Settings {
            port,
            target: target.into(),
            routes_file: None,
            config_routes: Vec::new(),
            players: None,
            ignores: "[]".into(),
        };
        std::thread::spawn(move || serve_on(&listener, &settings));
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = write!(c, "GET / HTTP/1.1\r\nHost: p1.localhost\r\n\r\n");
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    }

    #[test]
    fn a_connection_the_app_drops_is_retried_and_then_answered() {
        let (addr, hits) = app_that(|i, mut c| {
            if i < 2 {
                return; // dropped without an answer
            }
            let _ = read_head(&mut c);
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\nhi"
            );
        });
        let out = through_proxy(&addr);
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(
            out.contains("hi<script src=\"/__devices/frame.js\">"),
            "{out}"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn an_app_that_never_answers_gets_a_502_that_tells_the_viewer() {
        let (addr, hits) = app_that(|_, _| {});
        let out = through_proxy(&addr);
        assert!(out.starts_with("HTTP/1.1 502"), "{out}");
        assert!(out.contains("shakedown:'failed'"), "{out}");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            ATTEMPTS as usize
        );
        // Nothing is listening at all.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = l.local_addr().unwrap().to_string();
        drop(l);
        assert!(through_proxy(&dead).starts_with("HTTP/1.1 502"));
    }

    #[test]
    fn frame_ancestors_name_the_viewer_only() {
        let v = rewrite_csp(
            "default-src 'self'; frame-ancestors 'none'; img-src *",
            8799,
        );
        assert_eq!(
            v,
            "default-src 'self'; frame-ancestors http://localhost:8799 http://127.0.0.1:8799; img-src *"
        );
        assert_eq!(rewrite_csp("default-src 'self'", 1), "default-src 'self'");
    }

    #[test]
    fn chunked_bodies_decode() {
        assert_eq!(
            dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            b"hello world"
        );
        assert_eq!(dechunk(b""), b"");
    }

    #[test]
    fn the_frame_script_is_live_and_reports_up() {
        let js = frame_js("[]");
        assert!(js.contains("if (false)"), "the clock must not be frozen");
        assert!(!js.contains("__AT__") && !js.contains("__FIXED__") && !js.contains("__SEED__"));
        assert!(js.contains("postMessage") && js.contains("window.__shakedown.tree(IGN)"));
    }

    #[test]
    fn a_real_proxy_round_trip_strips_framing_headers_and_injects_the_collector() {
        // A tiny app: an HTML page that forbids framing.
        let app = TcpListener::bind("127.0.0.1:0").unwrap();
        let app_port = app.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut c in app.incoming().flatten() {
                let _ = read_head(&mut c);
                let body = "<html><head></head><body>hi</body></html>";
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let settings = Settings {
            port,
            target: format!("127.0.0.1:{app_port}"),
            routes_file: None,
            config_routes: Vec::new(),
            players: None,
            ignores: "[]".into(),
        };
        let s2 = settings.clone();
        std::thread::spawn(move || serve_on(&listener, &s2));
        let get = |host: &str, path: &str| {
            for _ in 0..50 {
                if let Ok(mut c) = TcpStream::connect(("127.0.0.1", port)) {
                    let _ = write!(c, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n");
                    let mut out = String::new();
                    let _ = c.read_to_string(&mut out);
                    return out;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            String::new()
        };
        let page = get("p2.localhost", "/");
        assert!(
            page.contains("<script src=\"/__devices/frame.js\"></script>"),
            "{page}"
        );
        assert!(!page.to_lowercase().contains("x-frame-options"), "{page}");
        assert!(page.contains("frame-ancestors http://localhost:"), "{page}");
        assert!(get("localhost", "/__devices").contains("Device preview"));
        assert!(get("localhost", "/__devices/devices.json").contains("\"phone\""));
        assert!(get("evil.example.com", "/").starts_with("HTTP/1.1 403"));
        let tree = "{}";
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = write!(
            c,
            "POST /__devices/check HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{tree}",
            tree.len()
        );
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    }
}
