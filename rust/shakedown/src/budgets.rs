//! `budgets sizes`: exact bytes, no device. Raw, gzip level 9, brotli 11, and stripped static libraries.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::config::SizeBudget;

/// One measured ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Budget label plus metric, e.g. `web main.css gzip`.
    pub label: String,
    /// What was found.
    pub verdict: Verdict,
}

/// Result of one ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Within the ceiling.
    Ok {
        /// Measured bytes.
        bytes: u64,
        /// Ceiling.
        max: u64,
    },
    /// Over the ceiling.
    Over {
        /// Measured bytes.
        bytes: u64,
        /// Ceiling.
        max: u64,
    },
    /// The artifact could not be measured (two files match, a read error, `strip` missing). Never a pass.
    Skipped(String),
    /// The artifact file does not exist. A failure unless the run skips unbuilt artifacts (`skip_unbuilt`).
    NotBuilt(String),
}

impl Row {
    /// The printed line.
    pub fn line(&self) -> String {
        match &self.verdict {
            Verdict::Ok { bytes, max } => format!("ok {} {bytes} <= {max}", self.label),
            Verdict::Over { bytes, max } => {
                format!("OVER {} {bytes} > {max} (+{})", self.label, bytes - max)
            }
            Verdict::Skipped(why) => format!("skipped {} ({why})", self.label),
            Verdict::NotBuilt(file) => format!("skipped (not built) {}: {file}", self.label),
        }
    }
}

/// Why a run of rows fails, or `None` when it passes. A not-built artifact fails unless `skip_unbuilt`; anything
/// else skipped never passes.
pub fn failure(rows: &[Row], skip_unbuilt: bool) -> Option<String> {
    let count = |f: fn(&Verdict) -> bool| rows.iter().filter(|r| f(&r.verdict)).count();
    let over = count(|v| matches!(v, Verdict::Over { .. }));
    let skipped = count(|v| matches!(v, Verdict::Skipped(_)));
    let unbuilt = if skip_unbuilt {
        0
    } else {
        count(|v| matches!(v, Verdict::NotBuilt(_)))
    };
    if over > 0 {
        Some(format!("{over} over budget"))
    } else if skipped + unbuilt > 0 {
        Some(format!(
            "{} not built (a skipped artifact is never a pass; --skip-unbuilt or [budgets].skip_unbuilt skips the missing ones)",
            skipped + unbuilt
        ))
    } else {
        None
    }
}

/// Measures every budget, in label order.
pub fn check<'a>(
    budgets: impl IntoIterator<Item = (&'a String, &'a SizeBudget)>,
    root: &Path,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (label, b) in budgets {
        match resolve(root, &b.file) {
            Err(Missing::NotBuilt) => rows.push(Row {
                label: label.clone(),
                verdict: Verdict::NotBuilt(b.file.clone()),
            }),
            Err(Missing::Other(why)) => rows.push(Row {
                label: label.clone(),
                verdict: Verdict::Skipped(why),
            }),
            Ok(path) => match std::fs::read(&path) {
                Err(e) => rows.push(Row {
                    label: label.clone(),
                    verdict: Verdict::Skipped(format!("{}: {e}", b.file)),
                }),
                Ok(data) => measure(label, b, &path, &data, &mut rows),
            },
        }
    }
    rows
}

fn measure(label: &str, b: &SizeBudget, path: &Path, data: &[u8], rows: &mut Vec<Row>) {
    let mut push = |metric: &str, bytes: Result<u64, String>, max: u64| {
        let verdict = match bytes {
            Ok(bytes) if bytes <= max => Verdict::Ok { bytes, max },
            Ok(bytes) => Verdict::Over { bytes, max },
            Err(why) => Verdict::Skipped(why),
        };
        rows.push(Row {
            label: format!("{label}{metric}"),
            verdict,
        });
    };
    let raw_bytes = if b.strip {
        stripped(path)
    } else {
        Ok(data.len() as u64)
    };
    let suffix = if b.strip { " stripped" } else { "" };
    if let Some(max) = b.max {
        push(suffix, raw_bytes.clone(), max);
    }
    if let Some(max) = b.raw {
        push(" raw", Ok(data.len() as u64), max);
    }
    if let Some(max) = b.gzip {
        push(" gzip", Ok(gzip_len(data)), max);
    }
    if let Some(max) = b.brotli {
        push(" brotli", Ok(brotli_len(data)), max);
    }
}

/// Exact size of `data` at gzip level 9.
pub fn gzip_len(data: &[u8]) -> u64 {
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    let _ = enc.write_all(data);
    enc.finish().map_or(0, |v| v.len() as u64)
}

/// Exact size of `data` at brotli quality 11 (window 22, the encoder default for large inputs).
pub fn brotli_len(data: &[u8]) -> u64 {
    let mut out = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 11, 22);
        let _ = w.write_all(data);
    }
    out.len() as u64
}

fn stripped(path: &Path) -> Result<u64, String> {
    let dir = std::env::temp_dir().join(format!("shakedown-strip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let copy = dir.join("artifact");
    std::fs::copy(path, &copy).map_err(|e| e.to_string())?;
    let out = Command::new("strip").args(["-S", "-x"]).arg(&copy).output();
    let size = match out {
        Ok(o) if o.status.success() => std::fs::metadata(&copy)
            .map(|m| m.len())
            .map_err(|e| e.to_string()),
        Ok(o) => Err(format!(
            "strip failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("strip not available: {e}")),
    };
    let _ = std::fs::remove_dir_all(&dir);
    size
}

/// Why a file was not found.
#[derive(Debug, PartialEq, Eq)]
enum Missing {
    /// Nothing is there: the artifact was not built.
    NotBuilt,
    /// Something else is wrong.
    Other(String),
}

/// Finds the file. The last path component may hold one `*`; it must then match exactly one file.
fn resolve(root: &Path, file: &str) -> Result<PathBuf, Missing> {
    let full = root.join(file);
    let Some(name) = full.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
        return Err(Missing::Other(format!("{file}: not a file path")));
    };
    let Some((pre, post)) = name.split_once('*') else {
        return if full.is_file() {
            Ok(full)
        } else {
            Err(Missing::NotBuilt)
        };
    };
    let dir = full.parent().unwrap_or(root);
    let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flat_map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.len() >= pre.len() + post.len() && n.starts_with(pre) && n.ends_with(post)
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();
    hits.sort();
    match hits.len() {
        0 => Err(Missing::NotBuilt),
        1 => Ok(hits.remove(0)),
        n => Err(Missing::Other(format!(
            "{file} matches {n} files; clean the stale ones"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn budget(file: &str, gzip: Option<u64>, raw: Option<u64>) -> SizeBudget {
        SizeBudget {
            file: file.into(),
            raw,
            gzip,
            brotli: None,
            strip: false,
            max: None,
        }
    }

    #[test]
    fn ok_over_and_skipped() {
        let d = std::env::temp_dir().join(format!("shakedown-budgets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin/app.abc123.js"), "x".repeat(10_000)).unwrap();
        let mut m = BTreeMap::new();
        m.insert(
            "a raw".to_string(),
            budget("bin/app.*.js", None, Some(10_000)),
        );
        m.insert(
            "b raw".to_string(),
            budget("bin/app.*.js", None, Some(9_999)),
        );
        m.insert("c".to_string(), budget("bin/missing.js", Some(1), None));
        m.insert("d".to_string(), budget("bin/app.*.js", Some(100), None));
        let rows = check(&m, &d);
        assert_eq!(
            rows[0].verdict,
            Verdict::Ok {
                bytes: 10_000,
                max: 10_000
            }
        );
        assert_eq!(
            rows[1].verdict,
            Verdict::Over {
                bytes: 10_000,
                max: 9_999
            }
        );
        assert_eq!(rows[2].verdict, Verdict::NotBuilt("bin/missing.js".into()));
        assert_eq!(rows[2].line(), "skipped (not built) c: bin/missing.js");
        assert!(failure(&rows, false).unwrap().contains("over budget"));
        assert!(
            failure(&[rows[0].clone(), rows[2].clone()], false)
                .unwrap()
                .contains("not built")
        );
        assert!(failure(&rows[..2], true).unwrap().contains("over budget"));
        assert_eq!(failure(&[rows[0].clone(), rows[2].clone()], true), None);
        assert!(failure(&[rows[0].clone(), rows[2].clone()], false).is_some());
        assert!(
            matches!(rows[3].verdict, Verdict::Ok { .. }),
            "{:?}",
            rows[3]
        );
        assert!(rows[1].line().starts_with("OVER b raw raw 10000"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_glob_hits_is_skipped_not_ok() {
        let d = std::env::temp_dir().join(format!("shakedown-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.1.js"), "x").unwrap();
        std::fs::write(d.join("a.2.js"), "x").unwrap();
        assert!(matches!(resolve(&d, "a.*.js"), Err(Missing::Other(m)) if m.contains("matches 2")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn compression_is_exact_and_smaller_for_repeats() {
        let data = vec![b'a'; 5000];
        assert!(gzip_len(&data) < 100);
        assert!(brotli_len(&data) < 100);
        assert_eq!(gzip_len(&data), gzip_len(&data));
    }
}
