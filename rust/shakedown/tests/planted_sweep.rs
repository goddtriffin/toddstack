//! `sweep` end to end against a tiny static site in the pinned Chrome: layout rules fail it, pixel changes only
//! get reported, `--pack` writes a manifest and contact sheets within 1568 px, and only local bases are accepted.

use std::path::PathBuf;
use std::process::{Command, Output};

const HEAD: &str = "<!doctype html><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>t</title><body style=\"margin:0;font:16px sans-serif\">";

struct Site {
    dir: PathBuf,
}

impl Site {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("shakedown-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("site")).unwrap();
        let site = Self { dir };
        site.page("ok.html", "<h1>Hello</h1><p>Some text.</p>");
        site.page(
            "wide.html",
            "<div style=\"width:3000px;height:20px;background:#ccc\">wide</div>",
        );
        std::fs::write(
            site.dir.join("shakedown.toml"),
            format!(
                "shakedown = \"{}\"\nproject = \"planted\"\n[server]\nstart = \"python3 -m http.server {{port}} --bind 127.0.0.1 --directory site\"\nready = \"/ok.html\"\n[devices]\nsweep = [\"phone\", \"desktop\", \"tablet-land\"]\nthemes = [\"light\", \"dark\"]\n[sweep]\nroutes = [\"/ok.html\", \"/wide.html\"]\n",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
        site
    }

    fn page(&self, name: &str, body: &str) {
        std::fs::write(self.dir.join("site").join(name), format!("{HEAD}{body}")).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_shakedown"))
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap()
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

fn png_size(path: &std::path::Path) -> (u32, u32) {
    let b = std::fs::read(path).unwrap();
    let be = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    (be(16), be(20))
}

#[test]
fn a_page_that_scrolls_sideways_fails_the_sweep_and_the_clean_one_does_not() {
    let site = Site::new("rules");
    let o = site.run(&["sweep"]);
    let t = text(&o);
    assert_eq!(o.status.code(), Some(1), "{t}");
    assert!(
        t.contains("wide-html@phone-light: ")
            && t.contains("sideways-scroll: the page scrolls sideways"),
        "{t}"
    );
    assert!(
        !t.contains("ok-html@"),
        "the clean page has no finding: {t}"
    );
    assert!(t.contains("FAIL sweep 6 of 12 shots"), "{t}");
    assert!(
        site.dir
            .join("target/shakedown/sweep/report.html")
            .is_file()
    );
    let clean = site.run(&["sweep", "--routes", "ok-html"]);
    assert_eq!(clean.status.code(), Some(0), "{}", text(&clean));
    assert!(
        text(&clean).contains("ok sweep 6 shots"),
        "{}",
        text(&clean)
    );
}

#[test]
fn pixel_changes_are_reported_and_never_fail_and_pack_writes_the_review_pack() {
    let site = Site::new("pack");
    let first = site.run(&["sweep", "--routes", "ok-html", "--pack"]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    let pack = site.dir.join("target/shakedown/review");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pack.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["shots"], 6);
    assert_eq!(
        manifest["devices"],
        serde_json::json!(["phone", "desktop", "tablet-land"])
    );
    assert_eq!(manifest["pixels_new"], 6);
    for theme in ["light", "dark"] {
        let sheet = pack.join(format!("ok-html-{theme}.png"));
        let (w, h) = png_size(&sheet);
        assert!(w.max(h) <= 1568 && w.max(h) > 700, "{theme}: {w}x{h}");
    }
    // The same page again: nothing changed.
    let again = site.run(&["sweep", "--routes", "ok-html"]);
    assert!(
        !text(&again).contains("changed against the baseline"),
        "{}",
        text(&again)
    );
    // A visible change is reported, the sweep still passes, and the pack lists it.
    site.page(
        "ok.html",
        "<h1 style=\"background:#e33\">Hello</h1><p>Some text.</p>",
    );
    let changed = site.run(&["sweep", "--routes", "ok-html", "--pack"]);
    let t = text(&changed);
    assert_eq!(changed.status.code(), Some(0), "{t}");
    assert!(t.contains("pixels: 6 changed against the baseline"), "{t}");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pack.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["pixel_changes"].as_array().unwrap().len(), 6);
    // Rebasing accepts it.
    assert_eq!(
        site.run(&["sweep", "--routes", "ok-html", "--rebase"])
            .status
            .code(),
        Some(0)
    );
    let after = site.run(&["sweep", "--routes", "ok-html"]);
    assert!(
        !text(&after).contains("changed against the baseline"),
        "{}",
        text(&after)
    );
}

#[test]
fn only_local_bases_and_known_routes_are_accepted() {
    let site = Site::new("usage");
    let o = site.run(&["sweep", "--base", "https://accounts.toddgriffin.me"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(
        text(&o).contains("not a local address") || text(&o).contains("only http"),
        "{}",
        text(&o)
    );
    let o = site.run(&["sweep", "--routes", "nope"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("unknown route"), "{}", text(&o));
}
