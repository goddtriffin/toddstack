use std::cell::RefCell;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};

use super::matching::{exact_diff, partial_diff};
use super::session::{Answerer, Factory, HttpResponse, Session};
use super::world::Script;

pub struct Options {
    /// Rewrite snapshots instead of comparing (the caller decides how to ask for it, for
    /// example an environment variable of the app's own).
    pub golden: bool,
    /// Directory holding `<scenario path without extension>/<n>.json`.
    pub snapshot_root: PathBuf,
    /// Stands for `$APP_VERSION` in a `boot` step (a saved snapshot names the app's version).
    pub app_version: String,
    /// Answers the app's own effects, which the world has no built-in answer for.
    pub answerer: Option<Answerer>,
}

#[derive(Debug)]
pub struct Failure {
    pub file: PathBuf,
    pub line: usize,
    pub step: String,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}: {}\n{}",
            self.file.display(),
            self.line,
            self.step,
            self.message
        )
    }
}

struct Runner<'a> {
    path: &'a Path,
    opts: &'a Options,
    factory: Factory<'a>,
    session: Option<Session>,
    script: Rc<RefCell<Script>>,
    snapshots: usize,
}

/// Run one `.jsonl` scenario: one step per line; blank lines and `#` lines are skipped.
///
/// Returns the session's response log, so callers can compare repeated runs.
///
/// # Errors
/// The first failing step.
pub fn run_file(path: &Path, factory: Factory<'_>, opts: &Options) -> Result<Vec<Value>, Failure> {
    let text = fs::read_to_string(path).map_err(|e| Failure {
        file: path.to_owned(),
        line: 0,
        step: "read".to_owned(),
        message: e.to_string(),
    })?;
    let mut runner = Runner {
        path,
        opts,
        factory,
        session: None,
        script: Rc::default(),
        snapshots: 0,
    };
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |step: &str, message: String| Failure {
            file: path.to_owned(),
            line: i + 1,
            step: step.to_owned(),
            message,
        };
        let step: Value = serde_json::from_str(line).map_err(|e| fail("parse", e.to_string()))?;
        let Some(verb) = step
            .as_object()
            .and_then(|o| o.keys().find(|k| *k != "seed"))
        else {
            return Err(fail("parse", "step has no verb".to_owned()));
        };
        runner.step(verb, &step).map_err(|m| fail(verb, m))?;
    }
    Ok(runner.session.map_or_else(Vec::new, |s| s.log))
}

impl Runner<'_> {
    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn session(&mut self) -> Result<&mut Session, String> {
        self.session
            .as_mut()
            .ok_or_else(|| "no `boot` step yet".to_owned())
    }

    fn step(&mut self, verb: &str, step: &Value) -> Result<(), String> {
        let arg = &step[verb];
        match verb {
            "boot" => self.boot(arg, step.get("seed").and_then(Value::as_u64).unwrap_or(1)),
            "http" => self.http(arg),
            "send" => {
                let s = self.session()?;
                s.clear();
                s.send(arg.clone()).map(drop)
            }
            "pointer" => {
                let points = arg.as_array().ok_or("pointer takes [[phase,x,y],...]")?;
                let s = self.session()?;
                s.clear();
                for p in points {
                    let (phase, x, y) = (&p[0], &p[1], &p[2]);
                    s.send(json!({"pointer": {"phase": phase, "x": x, "y": y}}))?;
                }
                Ok(())
            }
            "blob" => {
                let name: &str = arg["name"].as_str().ok_or("blob needs name")?;
                let file: &str = arg["file"].as_str().ok_or("blob needs file")?;
                let file_path = self.dir().join(file);
                let bytes =
                    fs::read(&file_path).map_err(|e| format!("{}: {e}", file_path.display()))?;
                let s = self.session()?;
                s.clear();
                s.provide_blob(name, &bytes).map(drop)
            }
            "advance_ms" => {
                let ms = arg.as_u64().ok_or("advance_ms takes a number")?;
                let s = self.session()?;
                s.clear();
                s.advance_ms(ms).map(drop)
            }
            "expect" => self.expect(arg),
            "snapshot" => self.snapshot(arg),
            other => Err(format!("unknown step `{other}`")),
        }
    }

    fn boot(&mut self, boot: &Value, seed: u64) -> Result<(), String> {
        let text = boot
            .to_string()
            .replace("$APP_VERSION", &self.opts.app_version);
        let boot = &serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?;
        let mut session =
            Session::new(self.factory, boot, seed, Box::new(Rc::clone(&self.script)))?;
        session.strict = true;
        session.answerer = self.opts.answerer;
        self.session = Some(session);
        Ok(())
    }

    /// Scripted HTTP may be declared before or after `boot`.
    fn http(&mut self, arg: &Value) -> Result<(), String> {
        let routes = arg
            .as_object()
            .ok_or("http takes {\"METHOD path\": response}")?;
        for (key, def) in routes {
            let defs = def
                .as_array()
                .map_or_else(|| vec![def.clone()], Clone::clone);
            let mut list = Vec::new();
            for d in &defs {
                let body = if let Some(f) = d.get("body_file").and_then(Value::as_str) {
                    let p = self.dir().join(f);
                    let text =
                        fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?
                } else {
                    d.get("body").cloned().unwrap_or(Value::Null)
                };
                list.push(HttpResponse {
                    status: u16::try_from(d["status"].as_u64().unwrap_or(200)).unwrap_or(200),
                    body,
                    latency_ms: d.get("latency_ms").and_then(Value::as_u64).unwrap_or(0),
                    delay_ms: d.get("delay_ms").and_then(Value::as_u64).unwrap_or(0),
                });
            }
            self.script.borrow_mut().add(key, list);
        }
        Ok(())
    }

    fn expect(&mut self, exp: &Value) -> Result<(), String> {
        let s = self.session()?;
        let mut problems = Vec::new();
        if let Some(want) = exp.get("effects").and_then(Value::as_array) {
            if want.is_empty() && !s.effects.is_empty() {
                problems.push(format!(
                    "effects: expected none, got {}",
                    Value::Array(s.effects.clone())
                ));
            }
            for w in want {
                if !s.effects.iter().any(|e| partial_diff(w, e).is_empty()) {
                    let near = s.effects.iter().map(|e| partial_diff(w, e).join("; "));
                    problems.push(format!(
                        "effects: no emitted effect matches {w}\n  emitted: {}\n  closest diffs: {}",
                        Value::Array(s.effects.clone()),
                        near.collect::<Vec<_>>().join(" | ")
                    ));
                }
            }
        }
        if let Some(want) = exp.get("dirty").and_then(Value::as_array) {
            for k in want.iter().filter_map(Value::as_str) {
                if !s.dirty.iter().any(|d| d == k) {
                    problems.push(format!("dirty: `{k}` not marked (dirty: {:?})", s.dirty));
                }
            }
        }
        if let Some(views) = exp.get("view").and_then(Value::as_object) {
            for (key, want) in views {
                let actual = view_json(s, key)?;
                for d in partial_diff(want, &actual) {
                    problems.push(format!("view `{key}` {d}"));
                }
            }
        }
        if exp.get("screen").is_some() || exp.get("actions").is_some() {
            let intro = introspect(s)?;
            if let Some(want) = exp.get("screen") {
                for d in partial_diff(want, &intro["screen"]) {
                    problems.push(format!("screen {d}"));
                }
            }
            if let Some(want) = exp.get("actions").and_then(Value::as_array) {
                let names: Vec<&str> = intro["actions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["name"].as_str())
                    .collect();
                for n in want.iter().filter_map(Value::as_str) {
                    if !names.contains(&n) {
                        problems.push(format!("actions: `{n}` not legal (legal: {names:?})"));
                    }
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        }
    }

    /// One view key, or a list of keys written together as `{key: view}` (a shell's whole screen).
    fn snapshot(&mut self, arg: &Value) -> Result<(), String> {
        self.snapshots += 1;
        let n = self.snapshots;
        let s = self.session.as_ref().ok_or("no `boot` step yet")?;
        let (key, actual) = if let Some(keys) = arg.as_array() {
            let mut all = serde_json::Map::new();
            // The core's clock at this point: a shell draws countdowns against it.
            all.insert("_now".to_owned(), Value::from(s.world.now));
            for k in keys {
                let k = k.as_str().ok_or("snapshot keys are strings")?;
                all.insert(k.to_owned(), view_json(s, k)?);
            }
            (
                keys.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
                Value::Object(all),
            )
        } else {
            let key = arg
                .as_str()
                .ok_or("snapshot takes a view key or a list of keys")?;
            (key.to_owned(), view_json(s, key)?)
        };
        let dir = self
            .opts
            .snapshot_root
            .join(self.path.with_extension("").file_name().unwrap_or_default());
        let file = dir.join(format!("{n}.json"));
        let text = serde_json::to_string_pretty(&actual).map_err(|e| e.to_string())? + "\n";
        if self.opts.golden {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            return fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()));
        }
        let want_text = fs::read_to_string(&file).map_err(|_| {
            format!(
                "missing snapshot {} (rerun with golden enabled to write it)",
                file.display()
            )
        })?;
        let want: Value =
            serde_json::from_str(&want_text).map_err(|e| format!("{}: {e}", file.display()))?;
        let diffs = exact_diff(&want, &actual);
        if diffs.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "snapshot {} (view `{key}`) differs:\n  {}",
                file.display(),
                diffs.join("\n  ")
            ))
        }
    }
}

fn view_json(s: &Session, key: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_slice(&s.bridge().view(key)).map_err(|e| e.to_string())?;
    if v.is_null() {
        Err(format!("view `{key}` does not exist"))
    } else {
        Ok(v)
    }
}

fn introspect(s: &mut Session) -> Result<Value, String> {
    // Not a recorded step: read-only, so the effect log stays untouched.
    let out = s.dispatch_raw(&json!({"at": s.world.now, "introspect": {}}))?;
    Ok(out)
}
