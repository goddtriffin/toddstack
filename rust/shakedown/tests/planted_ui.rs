//! Planted-break tests for `ui web`: every layout rule and pixel failure is planted in a real page, drawn in the
//! pinned Chrome, and shakedown must fail naming the rule. Harmless changes must pass. They run the real binary
//! against a tiny static site, so they need the pinned Chrome (installed on first use, shared by every project).

use std::path::PathBuf;
use std::process::{Command, Output};

const HEAD: &str = "<!doctype html><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>t</title><body style=\"margin:0;font:16px sans-serif\">";
const OK_BODY: &str =
    "<h1>Hello</h1><p>Some text here.</p><button style=\"height:44px;padding:0 20px\">Go</button>";

struct Site {
    dir: PathBuf,
}

impl Site {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("shakedown-ui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("site")).unwrap();
        let site = Self { dir };
        site.config("");
        site
    }

    fn config(&self, extra: &str) {
        let text = format!(
            "shakedown = \"{}\"\nproject = \"planted\"\n[server]\nstart = \"python3 -m http.server {{port}} --bind 127.0.0.1 --directory site\"\nready = \"/page.html\"\n[devices]\nui = [\"phone\"]\nthemes = [\"light\"]\n{extra}",
            env!("CARGO_PKG_VERSION")
        );
        std::fs::write(self.dir.join("shakedown.toml"), text).unwrap();
    }

    fn raw(&self, text: &str) {
        std::fs::write(
            self.dir.join("shakedown.toml"),
            format!(
                "shakedown = \"{}\"\nproject = \"planted\"\n{text}",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
    }

    fn fixture(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).unwrap();
    }

    fn page(&self, body: &str) {
        self.file("page.html", &format!("{HEAD}{body}"));
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.dir.join("site").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn screen(&self, extra_toml: &str) {
        self.config(&format!(
            "[[screen]]\nname = \"page\"\npath = \"/page.html\"\n{extra_toml}"
        ));
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_shakedown"))
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap()
    }

    /// Approves the page as it is now.
    fn approve(&self) {
        let o = self.run(&["update", "--screens", "page"]);
        assert!(o.status.success(), "update: {}", text(&o));
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

/// Plants `body`, approves a clean page first when `approve_ok` (so the only reason to fail is the rule), then
/// requires the check to fail and to name `rule`.
fn planted(tag: &str, body: &str, rule: &str) {
    let site = Site::new(tag);
    site.screen("");
    site.page(OK_BODY);
    site.approve();
    let ok = site.run(&["ui", "web"]);
    assert_eq!(code(&ok), 0, "the clean page passes: {}", text(&ok));
    site.page(body);
    let o = site.run(&["ui", "web"]);
    let t = text(&o);
    assert_eq!(code(&o), 1, "{rule} must fail the run: {t}");
    assert!(
        t.contains(&format!("{rule}: ")),
        "{rule} must be named: {t}"
    );
    assert!(
        site.dir.join("target/shakedown/web/report.html").is_file(),
        "a failure writes the report"
    );
}

#[test]
fn sideways_scroll_fails() {
    planted(
        "scroll",
        &format!("{OK_BODY}<div style=\"width:900px;height:20px;background:#ccc\"></div>"),
        "sideways-scroll",
    );
}

#[test]
fn off_screen_fails() {
    planted(
        "offscreen",
        &format!(
            "{OK_BODY}<div style=\"position:absolute;left:-60px;top:300px;width:100px;height:30px;background:#ccc\">out</div>"
        ),
        "off-screen",
    );
}

#[test]
fn clipped_text_fails() {
    planted(
        "clipped",
        &format!(
            "{OK_BODY}<div style=\"width:40px;overflow:hidden;white-space:nowrap\">A long sentence that does not fit</div>"
        ),
        "clipped-text",
    );
}

#[test]
fn text_overlap_fails() {
    planted(
        "overlap",
        &format!(
            "{OK_BODY}<div style=\"position:absolute;top:200px;left:10px\">First line of text</div><div style=\"position:absolute;top:205px;left:20px\">Second line of text</div>"
        ),
        "text-overlap",
    );
}

#[test]
fn small_text_fails() {
    planted(
        "tiny",
        &format!("{OK_BODY}<p style=\"font-size:9px\">tiny print</p>"),
        "text-min-size",
    );
}

#[test]
fn small_tap_target_fails() {
    planted(
        "tap",
        &format!("{OK_BODY}<br><button style=\"width:16px;height:16px;padding:0\">x</button>"),
        "tap-min-size",
    );
}

#[test]
fn layout_shift_fails() {
    // Late data (the fake world answers after 150 ms) pushes the page down: the classic shift.
    let site = Site::new("shift");
    std::fs::create_dir_all(site.dir.join("fixtures")).unwrap();
    site.fixture("fixtures/banner.json", "{\"text\":\"late banner\"}");
    site.screen("routes = { \"GET /api/v1/banner\" = \"fixtures/banner.json\" }\n");
    site.page(&format!("{OK_BODY}<script>fetch('/api/v1/banner').then((r) => r.json()).then((j) => {{ const b = document.createElement('div'); b.style.cssText = 'height:80px;background:#fc0'; b.textContent = j.text; document.body.prepend(b); }});</script>"));
    // Approve the finished page, so pixels match and the shift is the only reason to fail.
    let u = site.run(&["update", "--screens", "page"]);
    assert!(text(&u).contains("layout-shift"), "{}", text(&u));
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 1, "{}", text(&o));
    assert!(text(&o).contains("layout-shift: "), "{}", text(&o));
    assert!(!text(&o).contains("pixels: "), "{}", text(&o));
    // The same page with the content in the first response does not shift.
    site.page(&format!("<div style=\"height:80px;background:#fc0\">late banner</div>{OK_BODY}<script>fetch('/api/v1/banner')</script>"));
    site.run(&["update", "--screens", "page"]);
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
}

#[test]
fn console_error_fails() {
    planted(
        "console",
        &format!("{OK_BODY}<script>console.error('boom')</script>"),
        "console-error",
    );
}

#[test]
fn page_error_fails() {
    planted(
        "pageerror",
        &format!("{OK_BODY}<script>throw new Error('kaput')</script>"),
        "page-error",
    );
}

#[test]
fn a_request_that_leaves_the_machine_fails() {
    planted(
        "network",
        &format!("{OK_BODY}<script src=\"https://example.com/a.js\"></script>"),
        "network",
    );
}

#[test]
fn an_ignore_needs_a_reason() {
    planted(
        "why",
        &format!(
            "{OK_BODY}<br><button data-shakedown-ignore=\"tap-min-size\" style=\"width:16px;height:16px;padding:0\">x</button>"
        ),
        "ignore-without-why",
    );
}

#[test]
fn an_ignore_with_a_reason_passes() {
    let site = Site::new("whyok");
    site.screen("");
    site.page(&format!("{OK_BODY}<br><button data-shakedown-ignore=\"tap-min-size\" data-shakedown-why=\"decorative\" style=\"width:16px;height:16px;padding:0\">x</button>"));
    site.approve();
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
}

#[test]
fn a_config_ignore_passes_and_needs_a_reason() {
    let site = Site::new("cfgignore");
    site.page(&format!(
        "{OK_BODY}<br><button class=tiny style=\"width:16px;height:16px;padding:0\">x</button>"
    ));
    let screen = "[[screen]]\nname = \"page\"\npath = \"/page.html\"\n";
    site.config(&format!("[layout]\nignore = [{{ rule = \"tap-min-size\", match = \".tiny\", why = \"decorative\" }}]\n{screen}"));
    site.approve();
    assert_eq!(code(&site.run(&["ui", "web"])), 0);
    site.config(&format!("[layout]\nignore = [{{ rule = \"tap-min-size\", match = \".tiny\", why = \"\" }}]\n{screen}"));
    assert_eq!(
        code(&site.run(&["ui", "web"])),
        2,
        "an ignore with no why is a config error"
    );
}

#[test]
fn a_clean_page_passes_and_a_repeat_is_identical() {
    let site = Site::new("clean");
    site.screen("");
    site.page(OK_BODY);
    site.approve();
    let o = site.run(&["ui", "web", "--repeat", "3"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert!(text(&o).contains("ok ui.web 1 shots"));
}

#[test]
fn a_run_that_differs_from_the_last_fails_the_repeat() {
    let site = Site::new("repeat");
    site.screen("");
    // Not frozen: the time since this page loaded differs every run.
    site.page("<h1 id=t>x</h1><script>document.getElementById('t').textContent = String(performance.now() * 1000);</script>");
    let o = site.run(&["ui", "web", "--repeat", "3"]);
    assert_eq!(code(&o), 1, "{}", text(&o));
    assert!(text(&o).contains("repeat: run 2"), "{}", text(&o));
}

/// A solid 200x200 box, `shift` px from the left, `tone` gray.
fn box_page(shift: u32, tone: &str) -> String {
    format!(
        "<div style=\"margin-left:{shift}px;width:200px;height:200px;background:{tone}\"></div>"
    )
}

#[test]
fn a_one_pixel_shift_fails_and_sub_threshold_drift_passes() {
    let site = Site::new("pixels");
    site.screen("");
    site.page(&box_page(10, "#808080"));
    site.approve();
    assert_eq!(code(&site.run(&["ui", "web"])), 0);
    // Harmless: 4 of 255 on every channel, well under the threshold of 16.
    site.page(&box_page(10, "#848484"));
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 0, "sub-threshold drift passes: {}", text(&o));
    // Break: the box moves 1 px.
    site.page(&box_page(11, "#808080"));
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 1, "{}", text(&o));
    assert!(text(&o).contains("pixels: "), "{}", text(&o));
    let report =
        std::fs::read_to_string(site.dir.join("target/shakedown/web/report.html")).unwrap();
    assert!(
        report.contains("page@phone-light.diff.png")
            && report.contains("page@phone-light.reference.png")
    );
    // A colour change just over the threshold.
    site.page(&box_page(10, "#a0a0a0"));
    assert_eq!(code(&site.run(&["ui", "web"])), 1);
}

#[test]
fn a_missing_reference_fails_and_names_the_command() {
    let site = Site::new("noref");
    site.screen("");
    site.page(OK_BODY);
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 1);
    assert!(
        text(&o).contains("shakedown update --screens page"),
        "{}",
        text(&o)
    );
}

#[test]
fn update_writes_a_reference_only_when_its_pixels_changed() {
    let site = Site::new("update");
    site.screen("");
    site.page(&box_page(10, "#808080"));
    let first = site.run(&["update", "--screens", "page"]);
    assert!(
        text(&first).contains("1 written, 0 unchanged"),
        "{}",
        text(&first)
    );
    let again = site.run(&["update", "--screens", "page"]);
    assert!(
        text(&again).contains("0 written, 1 unchanged"),
        "{}",
        text(&again)
    );
    let reference = site.dir.join("ui/refs/web/page@phone-light.png");
    let before = std::fs::read(&reference).unwrap();
    site.page(&box_page(30, "#808080"));
    let changed = site.run(&["update", "--screens", "page"]);
    assert!(text(&changed).contains("1 written"), "{}", text(&changed));
    assert_ne!(before, std::fs::read(&reference).unwrap());
    // `all` is for a first generation only; update needs named screens.
    assert_eq!(code(&site.run(&["update", "--screens", "all"])), 2);
    assert_eq!(code(&site.run(&["update", "--screens", "nope"])), 2);
}

#[test]
fn a_fake_world_answers_from_fixtures_and_refuses_unfaked_calls() {
    let site = Site::new("fake");
    std::fs::create_dir_all(site.dir.join("fixtures")).unwrap();
    site.fixture("fixtures/me.json", "{\"name\":\"Ann\"}");
    let page = |call: &str| {
        site.page(&format!("<h1 id=n>loading</h1><script>fetch('{call}').then((r) => r.json()).then((j) => {{ document.getElementById('n').textContent = j.name || 'none'; }});</script>"));
    };
    let config = |routes: &str| {
        site.screen(&format!("routes = {routes}\n"));
    };
    page("/api/v1/me");
    config("{ \"GET /api/v1/me\" = \"fixtures/me.json\" }");
    // The fake world is the default world.
    site.approve();
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    // Planted: the page makes a call nobody faked.
    page("/api/v1/other");
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 1, "{}", text(&o));
    assert!(
        text(&o).contains("unfaked-call: GET /api/v1/other"),
        "{}",
        text(&o)
    );
    // Planted: a faked call the page never makes.
    page("/api/v1/me");
    config(
        "{ \"GET /api/v1/me\" = \"fixtures/me.json\", \"GET /api/v1/never\" = \"fixtures/me.json\" }",
    );
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 1, "{}", text(&o));
    assert!(
        text(&o).contains("never-called: GET /api/v1/never"),
        "{}",
        text(&o)
    );
}

#[test]
fn actions_drive_the_page_and_a_failed_action_fails_the_screen() {
    let site = Site::new("act");
    site.page("<button id=b onclick=\"document.getElementById('o').textContent='opened'\" style=\"height:44px\">Open</button><p id=o></p><input id=i style=\"height:30px\"><script>document.getElementById('i').addEventListener('keydown', (e) => { if (e.key === 'Enter') document.getElementById('o').textContent = 'got ' + e.target.value; });</script>");
    site.screen("act = [{ click = \"#b\" }, { wait_text = \"opened\" }, { type = { selector = \"#i\", text = \"hi\" } }, { press = \"Enter\" }, { wait_text = \"got hi\" }, { scroll = \"#o\" }]\n");
    site.approve();
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    site.screen("act = [{ click = \"#b\" }, { wait_text = \"never appears\" }]\n");
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 1, "{}", text(&o));
    assert!(text(&o).contains("action-failed"), "{}", text(&o));
}

#[test]
fn the_server_stops_when_the_run_ends() {
    let site = Site::new("stops");
    site.screen("");
    site.page(OK_BODY);
    site.approve();
    let _ = site.run(&["ui", "web"]);
    let marker = site.dir.join("site").display().to_string();
    let out = Command::new("pgrep")
        .args(["-f", &format!("http.server .*{marker}")])
        .output()
        .unwrap();
    assert!(
        out.stdout.is_empty(),
        "no server left running: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn server_that_never_comes_up_is_an_environment_error() {
    let site = Site::new("down");
    site.raw(
        "[server]\nstart = \"echo nope; exit 3\"\n[[screen]]\nname = \"page\"\npath = \"/\"\n",
    );
    let o = site.run(&["ui", "web"]);
    assert_eq!(code(&o), 3, "{}", text(&o));
    assert!(
        text(&o).contains("nope"),
        "the end of the server log is shown: {}",
        text(&o)
    );
}

#[test]
fn ui_without_screens_or_server_is_a_config_error() {
    let site = Site::new("empty");
    assert_eq!(code(&site.run(&["ui", "web"])), 2);
}

#[test]
fn update_leaves_a_reference_alone_when_the_change_is_within_tolerance() {
    let site = Site::new("update-tolerance");
    site.screen("");
    site.page(&box_page(10, "#808080"));
    let _ = site.run(&["update", "--screens", "page"]);
    let reference = site.dir.join("ui/refs/web/page@phone-light.png");
    let before = std::fs::read(&reference).unwrap();
    // 8 of 255 on every channel: under the compare threshold, but a different quantized image.
    site.page(&box_page(10, "#888888"));
    let o = site.run(&["update", "--screens", "page"]);
    assert!(text(&o).contains("0 written, 1 unchanged"), "{}", text(&o));
    assert_eq!(before, std::fs::read(&reference).unwrap(), "byte-identical");
}

#[test]
fn a_websocket_that_leaves_the_machine_fails() {
    planted(
        "websocket",
        &format!(
            "{OK_BODY}<script>try{{new WebSocket('wss://example.com/live')}}catch(e){{}}</script>"
        ),
        "network",
    );
}

/// This machine's address on the network (not loopback), if it has one.
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // A UDP "connect" only picks the route; nothing is sent.
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[test]
fn a_request_or_websocket_to_another_host_never_connects() {
    let Some(ip) = lan_ip() else {
        eprintln!("no network address: skipped");
        return;
    };
    // A listener posing as the outside world, reachable only through a non-loopback address. It runs in python3
    // (the sites here already need it): it prints its port, then "connected" or "none" once stdin closes.
    const LISTENER: &str = "import select, socket, sys\n\
s = socket.socket(); s.bind((sys.argv[1], 0)); s.listen(5)\n\
print(s.getsockname()[1], flush=True)\n\
ready = select.select([s, sys.stdin], [], [], 120)[0]\n\
print('connected' if s in ready else 'none')\n";
    let mut listener = Command::new("python3")
        .args(["-c", LISTENER, &ip.to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(listener.stdout.take().unwrap());
    let mut port = String::new();
    std::io::BufRead::read_line(&mut out, &mut port).unwrap();
    let port = port.trim().to_string();
    let site = Site::new("no-connect");
    site.screen("");
    site.page(&format!(
        "{OK_BODY}<script>try{{new WebSocket('ws://{ip}:{port}/live')}}catch(e){{}}</script>"
    ));
    let o = site.run(&["update", "--screens", "page"]);
    drop(listener.stdin.take());
    let mut verdict = String::new();
    std::io::BufRead::read_line(&mut out, &mut verdict).unwrap();
    let _ = listener.wait();
    assert!(text(&o).contains("network: "), "{}", text(&o));
    assert_eq!(
        verdict.trim(),
        "none",
        "Chrome connected to the outside listener"
    );
}

#[test]
fn a_websocket_to_the_local_server_is_allowed() {
    let site = Site::new("websocket-local");
    site.screen("");
    site.page(&format!(
        "{OK_BODY}<script>try{{new WebSocket('ws://'+location.host+'/live')}}catch(e){{}}</script>"
    ));
    // The static server refuses the handshake (a console error); the wall must not be what complains.
    let o = site.run(&["update", "--screens", "page"]);
    assert!(!text(&o).contains("leaves the machine"), "{}", text(&o));
}

#[test]
fn a_websocket_to_localhost_on_the_servers_port_is_allowed() {
    let site = Site::new("websocket-localhost");
    site.screen("");
    site.page(&format!(
        "{OK_BODY}<script>try{{new WebSocket('ws://localhost:'+location.port+'/live')}}catch(e){{}}</script>"
    ));
    let o = site.run(&["update", "--screens", "page"]);
    assert!(!text(&o).contains("leaves the machine"), "{}", text(&o));
}

/// A server that drops the first two requests for `/page.html` (Chrome itself retries a dropped GET once), then serves the page: the browser
/// fails the first try and the retry passes.
const FLAKY_SERVER: &str = r"
import functools, http.server, sys
seen = []
class H(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/page.html' and len(seen) < 2:
            seen.append(1)
            self.connection.close()
            return
        super().do_GET()
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), functools.partial(H, directory='site')).serve_forever()
";

#[test]
fn a_shot_that_fails_once_and_passes_on_the_retry_is_reported_as_flaky() {
    let site = Site::new("flaky");
    site.fixture("flaky.py", FLAKY_SERVER);
    site.page(OK_BODY);
    site.file("ok.html", "ok");
    site.raw(
        "[server]\nstart = \"python3 flaky.py {port}\"\nready = \"/ok.html\"\n[devices]\nui = [\"phone\"]\nthemes = [\"light\"]\n[[screen]]\nname = \"page\"\npath = \"/page.html\"\n",
    );
    let o = site.run(&["update", "--screens", "page"]);
    assert!(o.status.success(), "the retry passes: {}", text(&o));
    assert!(text(&o).contains("warning: flaky"), "{}", text(&o));
    let o = site.run(&["ui", "web", "--json"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    let all = text(&o);
    assert!(all.contains("warning: flaky"), "{all}");
    let json = std::fs::read_to_string(site.dir.join("target/shakedown/summary.json")).unwrap();
    assert!(
        json.contains("\"flaky\"") && json.contains("page@phone-light"),
        "{json}"
    );
}

#[test]
fn a_request_with_another_scheme_fails() {
    planted(
        "ftp",
        &format!("{OK_BODY}<img src=\"ftp://example.com/a.png\" width=1 height=1>"),
        "network",
    );
}

#[test]
fn a_signal_stops_the_server_even_before_it_is_ready() {
    for sig in ["-TERM", "-INT"] {
        let site = Site::new(&format!("signal{sig}"));
        let marker = format!("shakedown-signal-marker-{}{sig}", std::process::id());
        site.raw(&format!(
            "[server]\nstart = \"sleep 300 && echo {marker}\"\nready = \"/\"\ntimeout = 120\n[[screen]]\nname = \"page\"\npath = \"/\"\n"
        ));
        let mut child = Command::new(env!("CARGO_BIN_EXE_shakedown"))
            .args(["ui", "web"])
            .current_dir(&site.dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let running = || {
            let out = Command::new("pgrep")
                .args(["-f", &marker])
                .output()
                .unwrap();
            !out.stdout.is_empty()
        };
        let start = std::time::Instant::now();
        while !running() && start.elapsed().as_secs() < 90 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(running(), "the server started");
        let _ = Command::new("kill")
            .args([sig, &child.id().to_string()])
            .status();
        let status = child.wait().unwrap();
        assert!(!status.success(), "an interrupted run fails");
        assert!(!running(), "{sig}: the server is gone");
    }
}
