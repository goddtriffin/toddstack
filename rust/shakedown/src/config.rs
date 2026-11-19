//! `shakedown.toml`: parse and validate. Sections later phases use are accepted and reported as unused.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::error::Error;

/// The sections this version reads. Everything else in the design is accepted but ignored.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// The tool version the repo expects.
    pub shakedown: String,
    /// Project name.
    pub project: String,
    /// The local gate.
    #[serde(default)]
    pub check: Check,
    /// Budgets.
    #[serde(default)]
    pub budgets: Budgets,
    /// Sections owned by later phases (`server`, `devices`, `screen`, ...).
    #[serde(flatten)]
    pub unused: BTreeMap<String, toml::Value>,
}

/// `[check]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Shell steps.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Tool-run steps that join the same graph (`budgets.sizes`).
    #[serde(default)]
    pub builtin: Vec<String>,
}

/// One shell step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Unique name.
    pub name: String,
    /// The command, run with `sh -c` in the directory holding `shakedown.toml`.
    pub run: String,
    /// Steps that must pass first.
    #[serde(default)]
    pub after: Vec<String>,
    /// Runs only when named by `--only`.
    #[serde(default)]
    pub manual: bool,
}

/// `[budgets]`.
#[derive(Debug, Default, Deserialize)]
pub struct Budgets {
    /// Exact byte ceilings per artifact, keyed by label.
    #[serde(default)]
    pub sizes: BTreeMap<String, SizeBudget>,
}

/// One artifact's ceilings. `file` may hold one `*` in its last component (hashed names).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeBudget {
    /// Path from the repo root.
    pub file: String,
    /// Raw bytes ceiling.
    pub raw: Option<u64>,
    /// Gzip level 9 ceiling.
    pub gzip: Option<u64>,
    /// Brotli 11 ceiling.
    pub brotli: Option<u64>,
    /// Measure the file after `strip -S -x` (static libraries).
    #[serde(default)]
    pub strip: bool,
    /// Ceiling for the raw (or stripped) size.
    pub max: Option<u64>,
}

impl Config {
    /// Reads and validates the file.
    ///
    /// # Errors
    ///
    /// Unreadable file, bad TOML, a version mismatch, or a step graph that is not valid.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Self::parse(&text, env!("CARGO_PKG_VERSION"))
    }

    /// Parses text, with the running tool version passed in so tests can vary it.
    ///
    /// # Errors
    ///
    /// See [`Config::load`].
    pub fn parse(text: &str, tool_version: &str) -> Result<Self, Error> {
        let config: Self = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        if config.shakedown != tool_version {
            return Err(Error::Config(format!(
                "shakedown.toml expects shakedown {}, this is {tool_version}; run: cargo install shakedown@{} --locked",
                config.shakedown, config.shakedown
            )));
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Error> {
        let mut names = BTreeSet::new();
        for step in &self.check.steps {
            if step.name.is_empty() || step.run.trim().is_empty() {
                return Err(Error::Config("every step needs a name and a run".into()));
            }
            if !names.insert(step.name.as_str()) {
                return Err(Error::Config(format!(
                    "duplicate step name {:?}",
                    step.name
                )));
            }
        }
        for step in &self.check.steps {
            for dep in &step.after {
                if !names.contains(dep.as_str()) {
                    return Err(Error::Config(format!(
                        "step {:?} runs after unknown step {dep:?}",
                        step.name
                    )));
                }
            }
        }
        for b in &self.check.builtin {
            if b != "budgets.sizes" {
                return Err(Error::Config(format!(
                    "unknown builtin step {b:?} (this version has: budgets.sizes)"
                )));
            }
        }
        for (label, size) in &self.budgets.sizes {
            if size.strip && size.max.is_none() && size.raw.is_none() {
                return Err(Error::Config(format!("budget {label:?}: strip needs max")));
            }
            if size.raw.is_none()
                && size.gzip.is_none()
                && size.brotli.is_none()
                && size.max.is_none()
            {
                return Err(Error::Config(format!("budget {label:?} sets no ceiling")));
            }
        }
        crate::steps::order(&self.check.steps)?;
        Ok(())
    }

    /// Names of top-level sections this version does not read, sorted.
    pub fn unused_sections(&self) -> Vec<String> {
        self.unused.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
shakedown = "0.1.0"
project = "p"
[check]
steps = [
  { name = "a", run = "true" },
  { name = "b", run = "true", after = ["a"] },
]
builtin = ["budgets.sizes"]
[budgets.sizes]
"x gzip" = { file = "a.js", gzip = 10 }
[server]
start = "x"
"#;

    #[test]
    fn parses_and_reports_unused() {
        let c = Config::parse(OK, "0.1.0").unwrap();
        assert_eq!(c.check.steps.len(), 2);
        assert_eq!(c.unused_sections(), vec!["server"]);
    }

    #[test]
    fn version_mismatch_names_install_command() {
        let e = Config::parse(OK, "0.2.0").unwrap_err().to_string();
        assert!(e.contains("cargo install shakedown@0.1.0 --locked"), "{e}");
    }

    #[test]
    fn rejects_bad_graphs() {
        let dup = OK.replace(r#"name = "b""#, r#"name = "a""#);
        assert!(Config::parse(&dup, "0.1.0").is_err());
        let unknown = OK.replace(r#"["a"]"#, r#"["zzz"]"#);
        assert!(Config::parse(&unknown, "0.1.0").is_err());
        let cycle = OK.replace(
            r#"{ name = "a", run = "true" }"#,
            r#"{ name = "a", run = "true", after = ["b"] }"#,
        );
        assert!(
            Config::parse(&cycle, "0.1.0")
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
    }

    #[test]
    fn rejects_typos_in_known_sections() {
        let bad = OK.replace("gzip = 10", "gzp = 10");
        assert!(Config::parse(&bad, "0.1.0").is_err());
    }
}
