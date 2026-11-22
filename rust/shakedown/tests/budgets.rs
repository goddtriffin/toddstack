//! `budgets sizes` on a repo where some artifacts were not built, and with ceilings from a shared JSON file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo(name: &str, toml: &str, json: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shakedown-bt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("web")).unwrap();
    std::fs::write(dir.join("web/app.js"), "x".repeat(1000)).unwrap();
    std::fs::write(dir.join("shakedown.toml"), toml).unwrap();
    if let Some(j) = json {
        std::fs::write(dir.join("budgets.json"), j).unwrap();
    }
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shakedown"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const HEAD: &str = "shakedown = \"0.4.3\"\nproject = \"p\"\n";
const SIZES: &str = "[budgets.sizes]\n\"web raw\" = { file = \"web/app.js\", raw = 2000 }\n\"ios raw\" = { file = \"ios/App\", raw = 2000 }\n";

#[test]
fn an_unbuilt_artifact_fails_by_default_and_is_skipped_on_request() {
    let dir = repo("flag", &format!("{HEAD}{SIZES}"), None);
    let o = run(&dir, &["budgets", "sizes"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(
        text(&o).contains("skipped (not built) ios raw: ios/App"),
        "{}",
        text(&o)
    );
    let o = run(&dir, &["budgets", "sizes", "--skip-unbuilt"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(
        text(&o).contains("ok web raw raw 1000 <= 2000"),
        "{}",
        text(&o)
    );
    assert!(
        text(&o).contains("skipped (not built) ios raw"),
        "{}",
        text(&o)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_config_key_skips_unbuilt_and_over_budget_still_fails() {
    let dir = repo(
        "key",
        &format!("{HEAD}[budgets]\nskip_unbuilt = true\n{SIZES}")
            .replace("raw = 2000 }\n\"ios", "raw = 10 }\n\"ios"),
        None,
    );
    let o = run(&dir, &["budgets", "sizes"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("OVER web raw raw"), "{}", text(&o));
    let _ = std::fs::remove_dir_all(&dir);
    let dir = repo(
        "key2",
        &format!("{HEAD}[budgets]\nskip_unbuilt = true\n{SIZES}"),
        None,
    );
    assert_eq!(run(&dir, &["budgets", "sizes"]).status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ceilings_come_from_a_shared_json_file() {
    let toml = format!(
        "{HEAD}[budgets]\nfile = \"budgets.json\"\n[budgets.files]\n\"web app.js raw\" = \"web/app.js\"\n\"ios stripped\" = {{ file = \"ios/App\", strip = true }}\n"
    );
    let json =
        r#"{"sizes":{"web app.js raw":1000,"ios stripped":5,"android .so":7},"timing":{"x":1}}"#;
    let dir = repo("json", &toml, Some(json));
    let o = run(&dir, &["budgets", "sizes", "--skip-unbuilt"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(
        text(&o).contains("ok web app.js raw raw 1000 <= 1000"),
        "{}",
        text(&o)
    );
    assert!(
        text(&o).contains("skipped (not built) ios stripped"),
        "{}",
        text(&o)
    );
    assert!(
        !text(&o).contains("android"),
        "unmapped numbers are another tool's: {}",
        text(&o)
    );
    std::fs::write(dir.join("budgets.json"), json.replace(":1000,", ":999,")).unwrap();
    let o = run(&dir, &["budgets", "sizes", "--skip-unbuilt"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(
        text(&o).contains("OVER web app.js raw raw 1000 > 999"),
        "{}",
        text(&o)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
