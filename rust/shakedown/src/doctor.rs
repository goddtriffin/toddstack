//! `shakedown doctor`: what is installed, what is missing, and the command that fixes each miss.

use std::path::PathBuf;
use std::process::Command;

/// The Chrome for Testing build later phases pin.
pub use crate::browser::CHROME_VERSION;

/// One line of the doctor's output.
#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    /// What was checked.
    pub name: &'static str,
    /// `None` when fine, else the fix.
    pub missing: Option<String>,
    /// Version or path found, or why it matters.
    pub detail: String,
    /// The check is needed only by later phases or other platforms: a miss warns but is not an error.
    pub optional: bool,
}

impl Finding {
    /// The printed line.
    pub fn line(&self) -> String {
        match (&self.missing, self.optional) {
            (None, _) => format!("ok {} {}", self.name, self.detail),
            (Some(fix), false) => format!("MISSING {}: {fix}", self.name),
            (Some(fix), true) => format!("missing {} ({}): {fix}", self.name, self.detail),
        }
    }
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    })
    .into_owned();
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

fn tool(
    name: &'static str,
    cmd: &str,
    args: &[&str],
    fix: &str,
    optional: bool,
    why: &str,
) -> Finding {
    match run(cmd, args) {
        Some(v) => Finding {
            name,
            missing: None,
            detail: v,
            optional,
        },
        None => Finding {
            name,
            missing: Some(fix.to_string()),
            detail: why.to_string(),
            optional,
        },
    }
}

/// Used and total share of the disk holding `path`, from `df`.
pub fn disk_percent(df_output: &str) -> Option<u32> {
    let line = df_output.lines().nth(1)?;
    line.split_whitespace()
        .find_map(|f| f.strip_suffix('%'))
        .and_then(|p| p.parse().ok())
}

/// Runs every check.
pub fn run_all() -> Vec<Finding> {
    let mut f = tools();
    f.extend(sdk_and_chrome());
    f.extend(disk());
    f
}

fn tools() -> Vec<Finding> {
    vec![
        tool(
            "rustc",
            "rustc",
            &["--version"],
            "install Rust: https://rustup.rs",
            false,
            "needed to build and install",
        ),
        tool(
            "cargo",
            "cargo",
            &["--version"],
            "install Rust: https://rustup.rs",
            false,
            "needed to build and install",
        ),
        tool(
            "sh",
            "sh",
            &["-c", "echo sh"],
            "a POSIX shell must be on PATH",
            false,
            "runs check steps",
        ),
        tool(
            "strip",
            "sh",
            &["-c", "command -v strip"],
            "xcode-select --install",
            true,
            "stripped-library size budgets",
        ),
        tool(
            "xcodebuild",
            "xcodebuild",
            &["-version"],
            "install Xcode from the App Store, then: sudo xcode-select -s /Applications/Xcode.app",
            true,
            "iOS checks (phase 5)",
        ),
        tool(
            "simctl",
            "xcrun",
            &["simctl", "list", "runtimes"],
            "open Xcode > Settings > Components and install an iOS runtime",
            true,
            "iOS checks (phase 5)",
        ),
        tool(
            "java",
            "java",
            &["-version"],
            "brew install --cask temurin",
            true,
            "Android checks (phase 5)",
        ),
        tool(
            if cfg!(target_os = "macos") {
                "ditto"
            } else {
                "unzip"
            },
            "sh",
            &[
                "-c",
                if cfg!(target_os = "macos") {
                    "command -v ditto"
                } else {
                    "command -v unzip"
                },
            ],
            if cfg!(target_os = "macos") {
                "ditto ships with macOS; this is not macOS"
            } else {
                "sudo apt install unzip"
            },
            true,
            "unpacks the pinned Chrome",
        ),
    ]
}

fn sdk_and_chrome() -> Vec<Finding> {
    let mut f = Vec::new();
    let sdk = std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from);
    f.push(match sdk.filter(|p| p.is_dir()) {
        Some(p) => Finding {
            name: "android-sdk",
            missing: None,
            detail: p.display().to_string(),
            optional: true,
        },
        None => Finding {
            name: "android-sdk",
            missing: Some(
                "brew install --cask android-commandlinetools, then set ANDROID_HOME".into(),
            ),
            detail: "Android checks (phase 5)".into(),
            optional: true,
        },
    });
    f.push(if crate::browser::installed() {
        Finding {
            name: "chrome",
            missing: None,
            detail: format!(
                "{CHROME_VERSION} at {}",
                crate::browser::chrome_dir().display()
            ),
            optional: true,
        }
    } else {
        Finding {
            name: "chrome",
            missing: Some("shakedown browser install".into()),
            detail: format!(
                "pinned Chrome {CHROME_VERSION} for ui checks (ui installs it on first use)"
            ),
            optional: true,
        }
    });
    f
}

fn disk() -> Vec<Finding> {
    let mut f = Vec::new();
    if let Some(o) = Command::new("df")
        .args(["-Pk", "."])
        .output()
        .ok()
        .filter(|o| o.status.success())
    {
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        match disk_percent(&text) {
            Some(p) if p > 90 => f.push(Finding {
                name: "disk",
                missing: Some(format!(
                    "{p}% used: cargo clean in repos you work in, and remove unused simulators"
                )),
                detail: String::new(),
                optional: false,
            }),
            Some(p) => f.push(Finding {
                name: "disk",
                missing: None,
                detail: format!("{p}% used"),
                optional: false,
            }),
            None => {}
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_df() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s5 971350180 509000000 420000000 55% /\n";
        assert_eq!(disk_percent(out), Some(55));
        assert_eq!(disk_percent(""), None);
    }

    #[test]
    fn missing_lines_carry_the_fix() {
        let f = Finding {
            name: "java",
            missing: Some("brew install --cask temurin".into()),
            detail: "Android".into(),
            optional: true,
        };
        assert!(f.line().contains("brew install --cask temurin"));
        let ok = Finding {
            name: "cargo",
            missing: None,
            detail: "cargo 1.100".into(),
            optional: false,
        };
        assert_eq!(ok.line(), "ok cargo cargo 1.100");
    }

    #[test]
    fn finds_the_shell() {
        assert!(run("sh", &["-c", "echo hi"]).is_some());
        assert!(run("definitely-not-a-command-xyz", &[]).is_none());
    }
}
