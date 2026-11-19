//! Planted-break tests for `shakedown smoke`: the real binary runs a throwaway `python3 -m http.server` as the
//! "container" (started and stopped by shell commands standing in for the Makefile targets) and must fail on each
//! planted break, pass on a good site, and run `smoke.down` every time.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn site(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shakedown-smoke-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("site")).unwrap();
    for (name, body) in files {
        std::fs::write(d.join("site").join(name), body).unwrap();
    }
    d
}

/// A config whose up starts a static server on `{port}` and whose down stops it and leaves a mark.
fn config(dir: &Path, extra: &str) -> String {
    let up = format!(
        "python3 -m http.server {{port}} --bind 127.0.0.1 --directory {d}/site >/dev/null 2>&1 & echo $! > {d}/pid; echo image: {DIGEST}",
        d = dir.display()
    );
    let down = format!(
        "kill $(cat {d}/pid) 2>/dev/null; touch {d}/down-ran",
        d = dir.display()
    );
    format!(
        "shakedown = \"{}\"\nproject = \"t\"\n[smoke]\nup = {up:?}\ndown = {down:?}\nheaders = []\nmin_cache = 0\n{extra}\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn smoke(dir: &Path, toml: &str) -> Output {
    std::fs::write(dir.join("shakedown.toml"), toml).unwrap();
    Command::new(env!("CARGO_BIN_EXE_shakedown"))
        .arg("smoke")
        .current_dir(dir)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

#[test]
fn a_good_site_passes_and_records_the_digest() {
    let d = site(
        "good",
        &[
            (
                "index.html",
                "<title>hi</title><link rel=stylesheet href=a.css>",
            ),
            ("a.css", "b{}"),
        ],
    );
    let o = smoke(
        &d,
        &config(
            &d,
            "routes = [\"/\"]\nchecks = [{ path = \"/\", contains = \"<title>\" }]",
        ),
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(
        text(&o).contains(&format!("digest: {DIGEST}")),
        "{}",
        text(&o)
    );
    assert!(d.join("down-ran").exists(), "down ran");
}

#[test]
fn a_missing_asset_fails_with_exit_1_and_down_still_runs() {
    let d = site(
        "asset",
        &[("index.html", "<link rel=stylesheet href=/gone.css>")],
    );
    let o = smoke(&d, &config(&d, "routes = [\"/\"]"));
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(
        text(&o).contains("/gone.css") && text(&o).contains("404"),
        "{}",
        text(&o)
    );
    assert!(d.join("target/shakedown/smoke/report.html").exists());
    assert!(d.join("down-ran").exists(), "down ran");
}

#[test]
fn a_wrong_status_fails_with_exit_1() {
    let d = site("status", &[("index.html", "hi")]);
    let o = smoke(
        &d,
        &config(&d, "checks = [{ path = \"/nope\", status = 200 }]"),
    );
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(d.join("down-ran").exists());
}

#[test]
fn a_server_that_never_comes_up_fails_after_the_timeout_and_down_runs() {
    let d = site("never", &[]);
    let t = config(&d, "timeout = 2").replace(
        "python3 -m http.server",
        "sleep 0; : python3 -m http.server",
    );
    let o = smoke(&d, &t);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("smoke.ready"), "{}", text(&o));
    assert!(d.join("down-ran").exists(), "down ran");
}

#[test]
fn up_that_fails_exits_1_and_down_still_runs() {
    let d = site("upfail", &[]);
    let t = config(&d, "").replace("python3 -m http.server", "exit 7; python3 -m http.server");
    let o = smoke(&d, &t);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(d.join("down-ran").exists(), "down ran");
}

#[test]
fn a_missing_make_target_is_a_config_error() {
    let d = site("notarget", &[]);
    let t = format!(
        "shakedown = \"{}\"\nproject = \"t\"\n[smoke]\nup = \"make no_such_target PORT={{port}}\"\ndown = \"true\"\n",
        env!("CARGO_PKG_VERSION")
    );
    let o = smoke(&d, &t);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[test]
fn no_smoke_section_is_a_config_error() {
    let d = site("nosection", &[]);
    let t = format!(
        "shakedown = \"{}\"\nproject = \"t\"\n",
        env!("CARGO_PKG_VERSION")
    );
    let o = smoke(&d, &t);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}
