//! Reports: `summary.json` (machine form, deterministic: no times, no absolute paths) and `report.html`,
//! written only on failure.

use std::fmt::Write as _;

use serde_json::json;

use crate::budgets::{Row, Verdict};
use crate::steps::{Status, StepResult};

/// Escapes text for HTML.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const STYLE: &str = "body{font:14px system-ui,sans-serif;margin:24px;background:#fff;color:#111}\
@media(prefers-color-scheme:dark){body{background:#111;color:#eee}}\
pre{background:#8881;padding:12px;overflow:auto;border-radius:6px}.fail{color:#c0152f}.ok{color:#1a7f37}\
.row{display:flex;gap:12px;flex-wrap:wrap}.row figure{margin:0}.row img{max-width:360px;border:1px solid #8886}";

fn status_word(s: &Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Fail(_) => "fail",
        Status::Skipped(_) => "skipped",
    }
}

/// `summary.json` text for a `check` run.
pub fn summary_json(project: &str, results: &[StepResult], budget_rows: &[Row]) -> String {
    let steps: Vec<_> = results
        .iter()
        .map(|r| {
            let reason = match &r.status {
                Status::Ok => None,
                Status::Fail(w) | Status::Skipped(w) => Some(w.as_str()),
            };
            json!({ "name": r.name, "status": status_word(&r.status), "reason": reason, "detail": r.detail })
        })
        .collect();
    let rows: Vec<_> = budget_rows
        .iter()
        .map(|r| match &r.verdict {
            Verdict::Ok { bytes, max } => {
                json!({ "label": r.label, "status": "ok", "bytes": bytes, "max": max })
            }
            Verdict::Over { bytes, max } => {
                json!({ "label": r.label, "status": "over", "bytes": bytes, "max": max })
            }
            Verdict::Skipped(why) => {
                json!({ "label": r.label, "status": "skipped", "reason": why })
            }
        })
        .collect();
    let passed = results.iter().all(|r| r.status == Status::Ok);
    let mut out = serde_json::to_string_pretty(
        &json!({ "project": project, "passed": passed, "steps": steps, "budgets": rows }),
    )
    .unwrap_or_default();
    out.push('\n');
    out
}

/// The failure report for a `check` run: every non-passing step with its full output.
pub fn steps_html(results: &[StepResult]) -> String {
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><title>shakedown check</title><style>{STYLE}</style><h1>shakedown check</h1>"
    );
    for r in results {
        let _ = write!(
            h,
            "<h2 class={}>{} {}</h2>",
            status_word(&r.status),
            status_word(&r.status),
            esc(&r.name)
        );
        if let Status::Fail(w) | Status::Skipped(w) = &r.status {
            let _ = write!(h, "<p>{}</p>", esc(w));
        }
        if r.status != Status::Ok && !r.output.is_empty() {
            let _ = write!(h, "<pre>{}</pre>", esc(&r.output));
        }
    }
    h
}

/// One failed screen, files named relative to the report.
#[derive(Debug, Clone)]
pub struct PixelFailure {
    /// Screen id, e.g. `home@phone-light`.
    pub id: String,
    /// Why it failed.
    pub reason: String,
    /// Reference image file, if there is one.
    pub reference: Option<String>,
    /// Actual image file.
    pub actual: Option<String>,
    /// Diff image file, if sizes matched.
    pub diff: Option<String>,
}

/// The pixel failure report: reference, actual and diff side by side, and a click-to-flip between reference and
/// actual.
pub fn pixels_html(failures: &[PixelFailure]) -> String {
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><title>shakedown pixels</title><style>{STYLE}</style>\
<h1>shakedown pixels: {} failed</h1><p>Click the actual image to flip it with the reference.</p>",
        failures.len()
    );
    for f in failures {
        let _ = write!(
            h,
            "<h2 class=fail>{}</h2><p>{}</p><div class=row>",
            esc(&f.id),
            esc(&f.reason)
        );
        let fig = |label: &str, file: &Option<String>, flip: Option<&str>| match file {
            Some(file) => {
                let alt = flip.map_or(String::new(), |o| format!(" data-alt=\"{}\" onclick=\"let t=this.src;this.src=this.dataset.alt;this.dataset.alt=t\"", esc(o)));
                format!(
                    "<figure><figcaption>{label}</figcaption><img src=\"{}\"{alt}></figure>",
                    esc(file)
                )
            }
            None => String::new(),
        };
        h.push_str(&fig("reference", &f.reference, None));
        h.push_str(&fig("actual", &f.actual, f.reference.as_deref()));
        h.push_str(&fig("diff", &f.diff, None));
        h.push_str("</div>");
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, status: Status) -> StepResult {
        StepResult {
            name: name.into(),
            status,
            detail: String::new(),
            secs: 1.234,
            output: "<b>boom</b>".into(),
        }
    }

    #[test]
    fn summary_has_no_times_and_is_stable() {
        let rs = [r("a", Status::Ok), r("b", Status::Fail("exit 1".into()))];
        let s = summary_json("p", &rs, &[]);
        assert_eq!(s, summary_json("p", &rs, &[]));
        assert!(s.contains("\"passed\": false") && s.contains("exit 1") && !s.contains("1.234"));
    }

    #[test]
    fn html_escapes_output() {
        let h = steps_html(&[r("b", Status::Fail("exit 1".into()))]);
        assert!(h.contains("&lt;b&gt;boom") && !h.contains("<b>boom"));
    }

    #[test]
    fn pixel_report_lists_the_three_images() {
        let h = pixels_html(&[PixelFailure {
            id: "home@phone-light".into(),
            reason: "0.5% differ".into(),
            reference: Some("ref.png".into()),
            actual: Some("a.png".into()),
            diff: Some("d.png".into()),
        }]);
        assert!(
            h.contains("ref.png")
                && h.contains("a.png")
                && h.contains("d.png")
                && h.contains("data-alt")
        );
    }
}
