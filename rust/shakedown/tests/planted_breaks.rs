//! Planted-break tests: each plants a known break and requires `shakedown` to fail, and plants a harmless change
//! and requires it to pass. They run the real binary, so exit codes are what is tested.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shakedown-planted-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn shakedown(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shakedown"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

/// 200x200 image of vertical 3 px stripes: stands in for a screen full of text and edges.
fn stripes(shift: u32, tint: u8) -> Vec<u8> {
    sized(200, 200, shift, tint)
}

fn sized(w: u32, h: u32, shift: u32, tint: u8) -> Vec<u8> {
    let mut rgb = Vec::new();
    for _y in 0..h {
        for x in 0..w {
            let v: u8 = if ((x + shift) / 3).is_multiple_of(2) {
                40
            } else {
                220
            };
            rgb.extend_from_slice(&[v, v, v.saturating_add(tint)]);
        }
    }
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(&rgb).unwrap();
    out
}

fn compare(tag: &str, reference: &[u8], actual: &[u8]) -> i32 {
    let d = tmp(tag);
    std::fs::write(d.join("ref.png"), reference).unwrap();
    std::fs::write(d.join("actual.png"), actual).unwrap();
    let o = shakedown(&d, &["pixels", "compare", "ref.png", "actual.png"]);
    let c = code(&o);
    if c == 1 {
        assert!(
            d.join("target/shakedown/pixels/report.html").exists(),
            "a failure writes the report"
        );
        let resized = String::from_utf8_lossy(&o.stderr).contains("size changed");
        assert!(
            resized || d.join("target/shakedown/pixels/actual.diff.png").exists(),
            "a failure writes the diff (a size change has none)"
        );
    }
    let _ = std::fs::remove_dir_all(&d);
    c
}

#[test]
fn one_pixel_shift_fails() {
    assert_eq!(compare("shift", &stripes(0, 0), &stripes(1, 0)), 1);
}

#[test]
fn identical_passes() {
    assert_eq!(compare("same", &stripes(0, 0), &stripes(0, 0)), 0);
}

#[test]
fn sub_threshold_drift_passes() {
    // Every channel off by 16 everywhere: at the threshold, not over it.
    assert_eq!(compare("drift", &stripes(0, 0), &stripes(0, 16)), 0);
}

#[test]
fn over_threshold_color_change_fails() {
    assert_eq!(compare("color", &stripes(0, 0), &stripes(0, 17)), 1);
}

#[test]
fn a_size_change_fails() {
    // One extra row: same pixels everywhere else, but the page is a different size.
    assert_eq!(
        compare("resize", &sized(200, 200, 0, 0), &sized(200, 201, 0, 0)),
        1
    );
}

#[test]
fn size_change_and_missing_reference_fail() {
    let d = tmp("size");
    std::fs::write(d.join("actual.png"), stripes(0, 0)).unwrap();
    assert_eq!(
        code(&shakedown(
            &d,
            &["pixels", "compare", "nope.png", "actual.png"]
        )),
        1
    );
    let _ = std::fs::remove_dir_all(&d);
}

fn budget_project(tag: &str, max_raw: u64, file_bytes: usize, build: bool) -> PathBuf {
    let d = tmp(tag);
    if build {
        std::fs::write(d.join("app.js"), vec![b'x'; file_bytes]).unwrap();
    }
    std::fs::write(
        d.join("shakedown.toml"),
        format!(
            "shakedown = \"{}\"\nproject = \"t\"\n[check]\nbuiltin = [\"budgets.sizes\"]\n[budgets.sizes]\n\"app raw\" = {{ file = \"app.js\", raw = {max_raw} }}\n",
            env!("CARGO_PKG_VERSION")
        ),
    )
    .unwrap();
    d
}

#[test]
fn budget_at_the_limit_passes_and_one_byte_over_fails() {
    let at = budget_project("at", 1000, 1000, true);
    assert_eq!(code(&shakedown(&at, &["budgets", "sizes"])), 0);
    assert_eq!(code(&shakedown(&at, &["check"])), 0);
    let over = budget_project("over", 999, 1000, true);
    assert_eq!(code(&shakedown(&over, &["budgets", "sizes"])), 1);
    let o = shakedown(&over, &["check"]);
    assert_eq!(code(&o), 1);
    assert!(String::from_utf8_lossy(&o.stdout).contains("OVER app raw raw 1000 > 999 (+1)"));
    let _ = std::fs::remove_dir_all(&at);
    let _ = std::fs::remove_dir_all(&over);
}

#[test]
fn unbuilt_artifact_fails_budgets_and_check() {
    let d = budget_project("unbuilt", 1000, 0, false);
    let o = shakedown(&d, &["budgets", "sizes"]);
    assert_eq!(code(&o), 1, "a skipped artifact is never a pass");
    assert!(String::from_utf8_lossy(&o.stdout).contains("skipped"));
    assert_eq!(code(&shakedown(&d, &["check"])), 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn json_only_reports_steps_that_ran() {
    let d = budget_project("json", 999, 1000, true);
    let toml = std::fs::read_to_string(d.join("shakedown.toml")).unwrap();
    std::fs::write(
        d.join("shakedown.toml"),
        toml.replacen(
            "[check]",
            "[check]\nsteps = [{ name = \"other\", run = \"true\" }]",
            1,
        ),
    )
    .unwrap();
    let o = shakedown(&d, &["check", "--only", "other", "--json"]);
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    let json = std::fs::read_to_string(d.join("target/shakedown/summary.json")).unwrap();
    assert!(
        json.contains("\"passed\": true") && !json.contains("over"),
        "{json}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn failing_step_fails_check_and_exit_codes_are_distinct() {
    let d = tmp("steps");
    let write = |steps: &str| {
        std::fs::write(
            d.join("shakedown.toml"),
            format!(
                "shakedown = \"{}\"\nproject = \"t\"\n[check]\nsteps = [{steps}]\n",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
    };
    write(r#"{ name = "ok", run = "true" }"#);
    assert_eq!(code(&shakedown(&d, &["check"])), 0);
    write(r#"{ name = "ok", run = "true" }, { name = "bad", run = "echo planted; exit 7" }"#);
    let o = shakedown(&d, &["check"]);
    assert_eq!(code(&o), 1);
    assert!(String::from_utf8_lossy(&o.stdout).contains("planted"));
    assert!(d.join("target/shakedown/check/report.html").exists());
    write(r#"{ name = "x", run = "true", after = ["ghost"] }"#);
    assert_eq!(code(&shakedown(&d, &["check"])), 2);
    let _ = std::fs::remove_dir_all(&d);
}
