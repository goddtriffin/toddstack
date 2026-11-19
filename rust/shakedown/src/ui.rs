//! `shakedown ui web` and `shakedown update`: every screen on every `ui` device and theme, layout rules and
//! pixel compare against the approved references.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::capture::{self, Env, Fixture, Job, Plan, Shot};
use crate::config::{Config, Route, Screen, World};
use crate::devices::{self, Device};
use crate::error::Error;
use crate::layout::Finding;
use crate::pixels::{self, Image};
use crate::report::{self, PixelFailure};
use crate::world;

/// What to do with the shots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Compare with the references.
    Check,
    /// Rewrite the named references.
    Update,
}

/// What the caller asked for.
#[derive(Debug, Clone)]
pub struct Options {
    /// Screens by name; `all` for every screen. Empty means every screen for a check.
    pub screens: Vec<String>,
    /// Only these devices (a quick look while working; references are never judged on a subset by accident:
    /// `update` refuses it).
    pub devices: Vec<String>,
    /// Run everything this many times and fail if any run differs.
    pub repeat: u32,
    /// Check or update.
    pub mode: Mode,
}

/// A shot that did not pass.
#[derive(Debug, Clone)]
pub struct Failure {
    /// `<screen>@<device>-<theme>`.
    pub id: String,
    /// One line each: layout findings, then the pixel verdict.
    pub reasons: Vec<String>,
    /// The findings behind the reasons.
    pub findings: Vec<Finding>,
}

/// The result of a run.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Shots drawn (per run).
    pub shots: usize,
    /// What did not pass.
    pub failures: Vec<Failure>,
    /// Wall time.
    pub secs: f64,
    /// Update: references written.
    pub written: usize,
    /// Update: references already identical.
    pub unchanged: usize,
    /// Warnings to print before the verdict.
    pub notes: Vec<String>,
    /// Shots that failed once and passed on the retry: `(shot id, first failure)`. Always reported.
    pub flaky: Vec<(String, String)>,
    /// Where the failure report is, relative to the repo root.
    pub report: Option<String>,
}

fn out_dir(root: &Path) -> PathBuf {
    root.join("target/shakedown/web")
}

fn refs_dir(config: &Config, root: &Path) -> PathBuf {
    root.join(&config.ui.refs).join("web")
}

fn read_fixture(root: &Path, file: &str, now_ms: i64) -> Result<String, Error> {
    let path = root.join(file);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::Config(format!("fixture {}: {e}", path.display())))?;
    serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|e| Error::Config(format!("fixture {}: not JSON: {e}", path.display())))?;
    Ok(text.replace("\"$now\"", &now_ms.to_string()))
}

pub fn plan(root: &Path, s: &Screen, now_ms: i64) -> Result<Plan, Error> {
    let mut routes = std::collections::HashMap::new();
    for (key, route) in &s.routes {
        let (file, status) = match route {
            Route::File(f) => (f.as_str(), 200),
            Route::Detailed { file, status } => (file.as_str(), status.unwrap_or(200)),
        };
        routes.insert(
            key.clone(),
            Fixture {
                status,
                body: read_fixture(root, file, now_ms)?,
            },
        );
    }
    Ok(Plan {
        name: s.name.clone(),
        path: s.path.clone(),
        player: s.player.clone(),
        turn: s.turn,
        routes,
        act: s.act.clone(),
        allow: s.allow_console.iter().map(|a| a.contains.clone()).collect(),
    })
}

pub fn now_ms(now: &str) -> Result<i64, Error> {
    now.parse::<jiff::Timestamp>()
        .map(jiff::Timestamp::as_millisecond)
        .map_err(|e| {
            Error::Config(format!(
                "[server].now {now:?}: {e} (use RFC 3339, e.g. 2026-01-15T12:00:00Z)"
            ))
        })
}

/// `sw_vers`: `macOS 27`.
fn os_name() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|v| v.trim().split('.').next().map(|m| format!("macOS {m}")))
        .unwrap_or_else(|| "unknown".into())
}

fn env_json() -> String {
    let mut s = serde_json::to_string_pretty(&json!({
        "chrome": crate::browser::CHROME_VERSION,
        "os": os_name(),
    }))
    .unwrap_or_default();
    s.push('\n');
    s
}

fn select<'a>(config: &'a Config, opts: &Options, refs: &Path) -> Result<Vec<&'a Screen>, Error> {
    let all = || config.screen.iter().collect::<Vec<_>>();
    if opts.screens.is_empty() {
        if opts.mode == Mode::Update {
            return Err(Error::Config(
                "update needs --screens a,b (the screens whose references change; `all` only for a first generation)".into(),
            ));
        }
        return Ok(all());
    }
    if opts.screens.iter().any(|s| s == "all") {
        let has_refs = std::fs::read_dir(refs).is_ok_and(|d| {
            d.filter_map(Result::ok)
                .any(|e| e.path().extension().is_some_and(|x| x == "png"))
        });
        if opts.mode == Mode::Update && has_refs {
            return Err(Error::Config(
                "update --screens all is for a first generation; references already exist, so name the screens that changed".into(),
            ));
        }
        return Ok(all());
    }
    let unknown: Vec<&str> = opts
        .screens
        .iter()
        .filter(|n| !config.screen.iter().any(|s| &s.name == *n))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(Error::Config(format!(
            "unknown screen(s): {} (screens: {})",
            unknown.join(", "),
            config
                .screen
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )));
    }
    Ok(opts
        .screens
        .iter()
        .filter_map(|n| config.screen.iter().find(|s| &s.name == n))
        .collect())
}

/// One run: a fresh Chrome draws every job and is thrown away.
async fn one_run(chrome: &Path, env: &Arc<Env>, jobs: &[Job], n: u32) -> Result<Vec<Shot>, Error> {
    let count = capture::browsers().min(jobs.len().max(1));
    let mut chromes = capture::Chromes::default();
    for i in 0..count {
        let profile =
            std::env::temp_dir().join(format!("shakedown-chrome-{}-{n}-{i}", std::process::id()));
        chromes.launch(chrome, &profile).await?;
    }
    let shots = capture::shoot_all(chromes.browsers(), env.clone(), jobs.to_vec()).await;
    for browser in chromes.take_browsers() {
        if let Ok(mut b) = Arc::try_unwrap(browser) {
            // Nothing in the profile is worth a graceful exit (it is thrown away): a clean close takes seconds.
            let _ = b.kill().await;
            let _ = b.wait().await;
        }
    }
    Ok(shots)
}

fn build_jobs(
    config: &Config,
    root: &Path,
    opts: &Options,
    screens: &[&Screen],
    ms: i64,
) -> Result<Vec<Job>, Error> {
    let mut jobs = Vec::new();
    for s in screens {
        let mut names = s
            .devices
            .clone()
            .unwrap_or_else(|| config.devices.ui.clone());
        if !opts.devices.is_empty() {
            names.retain(|n| opts.devices.contains(n));
        }
        let list: Vec<Device> = devices::pick(&names)?;
        let p = Arc::new(plan(root, s, ms)?);
        for d in &list {
            for t in &config.devices.themes {
                jobs.push(Job {
                    plan: p.clone(),
                    device: d.clone(),
                    theme: t.clone(),
                });
            }
        }
    }
    Ok(jobs)
}

/// Runs the screens: draws every shot (`repeat` times), then checks or updates.
///
/// # Errors
///
/// Config or usage problems, a missing environment (Chrome, the server), or a failed check.
pub fn run(config: &Config, root: &Path, opts: &Options) -> Result<Outcome, Error> {
    config.require_ui()?;
    if opts.mode == Mode::Update && !opts.devices.is_empty() {
        return Err(Error::Config(
            "update writes every device of a screen; --devices is for ui only".into(),
        ));
    }
    let known = devices::all();
    if let Some(bad) = opts
        .devices
        .iter()
        .find(|d| !known.iter().any(|k| &k.name == *d))
    {
        return Err(Error::Config(format!(
            "unknown device {bad:?} (see: shakedown devices)"
        )));
    }
    let started = Instant::now();
    let server = config
        .server
        .clone()
        .ok_or_else(|| Error::Config("no [server]".into()))?;
    let refs = refs_dir(config, root);
    let screens = select(config, opts, &refs)?;
    let ms = now_ms(&server.now)?;
    let jobs = build_jobs(config, root, opts, &screens, ms)?;
    let out = out_dir(root);
    let _ = std::fs::remove_dir_all(&out);
    let runs = draw_all(config, root, &out, &jobs, opts.repeat.max(1), started, None)?;
    let first = runs.first().cloned().unwrap_or_default();
    let mut outcome = Outcome {
        shots: first.len(),
        ..Outcome::default()
    };
    flaky_note(&runs, &mut outcome);
    let (drift, noisy) = repeat_failures(&runs, &out);
    if !noisy.is_empty() {
        outcome.notes.push(format!(
            "note: {} of {} shots differ between runs by antialiasing noise, inside the compare tolerance (the pixel verdict is the same): {}",
            noisy.len(),
            outcome.shots,
            noisy.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    match opts.mode {
        Mode::Check => check(root, &refs, &out, &first, &mut outcome)?,
        Mode::Update => update(&refs, &first, &mut outcome)?,
    }
    for d in drift {
        match outcome.failures.iter_mut().find(|f| f.id == d.id) {
            Some(f) => f.reasons.extend(d.reasons),
            None => outcome.failures.push(d),
        }
    }
    outcome.failures.sort_by(|a, b| a.id.cmp(&b.id));
    outcome.secs = started.elapsed().as_secs_f64();
    Ok(outcome)
}

/// Starts the world (or uses `base`, an already running local server), draws every job `repeat` times, each run in
/// a fresh Chrome, and tears everything down. One list of shots per run, each sorted by id.
///
/// # Errors
///
/// A missing environment (Chrome, the server), or an interrupt.
pub fn draw_all(
    config: &Config,
    root: &Path,
    out: &Path,
    jobs: &[Job],
    repeat: u32,
    started: Instant,
    base: Option<&str>,
) -> Result<Vec<Vec<Shot>>, Error> {
    let server = config.server.clone();
    let chrome = crate::browser::install()?;
    let ignores = serde_json::to_string(&config.layout.ignore).unwrap_or_else(|_| "[]".into());
    let ms = now_ms(
        server
            .as_ref()
            .map_or("2026-01-15T12:00:00Z", |s| s.now.as_str()),
    )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Environment(format!("runtime: {e}")))?;
    let signals = world::watch_signals();
    runtime.block_on(async {
        let trace = |what: &str| {
            if std::env::var_os("SHAKEDOWN_TRACE").is_some() {
                eprintln!("trace {what} {}ms", started.elapsed().as_millis());
            }
        };
        let running = match (base, &server) {
            (Some(_), _) => None,
            (None, Some(s)) => Some(world::start(s, root, &config.project, out, &signals)?),
            (None, None) => return Err(Error::Config("no [server]".into())),
        };
        trace("server up");
        let env = Arc::new(Env {
            base: base.map_or_else(
                || running.as_ref().map_or_else(String::new, |r| r.base.clone()),
                str::to_string,
            ),
            api: server
                .as_ref()
                .map_or_else(|| "/api".into(), |s| s.api.clone()),
            world: server.as_ref().map_or(World::Seeded, |s| s.world),
            now_ms: ms,
            signin: config.players.as_ref().map(|p| p.signin.clone()),
            ignores,
            player_limit: config.players.as_ref().map_or(3, |p| p.parallel),
        });
        let work = async {
            let mut runs = Vec::new();
            for n in 0..repeat {
                // A fresh Chrome every run: nothing one run drew (decoded images, caches) can colour the next.
                runs.push(one_run(&chrome, &env, jobs, n).await?);
            }
            Ok::<_, Error>(runs)
        };
        let result = tokio::select! {
            r = work => r,
            () = async { while !signals.interrupted() { tokio::time::sleep(Duration::from_millis(50)).await; } } => Err(Error::Failed("interrupted".into())),
        };
        trace("shots done");
        drop(running);
        trace("server down");
        result
    })
}

/// Shots that failed once and passed on the retry are reported, never passed silently.
fn flaky_note(runs: &[Vec<Shot>], outcome: &mut Outcome) {
    for run in runs {
        for shot in run {
            if let Some(why) = &shot.flaky
                && !outcome.flaky.iter().any(|(id, _)| id == &shot.id)
            {
                outcome.flaky.push((shot.id.clone(), why.clone()));
            }
        }
    }
    outcome.flaky.sort();
    if !outcome.flaky.is_empty() {
        outcome.notes.push(format!(
            "warning: flaky: {} shot(s) failed once and passed on the retry (a page that does not draw the same way every time): {}",
            outcome.flaky.len(),
            outcome
                .flaky
                .iter()
                .map(|(id, why)| format!("{id} ({why})"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
}

/// `--repeat N`: every run must give the same pixels and the same findings as the first.
/// Failures between runs, and the shots whose pixels moved only within the compare tolerance. Chrome rescales
/// and clips images (avatars in circles) with an antialiasing race that moves a few edge pixels about one run
/// in a hundred; the gate is on the verdict the compare gives, which is what a reference check sees.
fn repeat_failures(runs: &[Vec<Shot>], out_dir: &Path) -> (Vec<Failure>, Vec<String>) {
    let Some(first) = runs.first() else {
        return (Vec::new(), Vec::new());
    };
    let mut noisy: Vec<String> = Vec::new();
    let mut out: Vec<Failure> = Vec::new();
    for (n, run) in runs.iter().enumerate().skip(1) {
        for (a, b) in first.iter().zip(run) {
            let why = if b.png.is_empty() != a.png.is_empty() {
                let lines: Vec<String> = [a, b]
                    .iter()
                    .flat_map(|x| x.findings.iter().map(Finding::line))
                    .collect();
                Some(format!(
                    "one run could not draw the page ({})",
                    lines.join("; ")
                ))
            } else if a.png != b.png
                && !decode_png(&a.png)
                    .ok()
                    .zip(decode_png(&b.png).ok())
                    .is_some_and(|(x, y)| pixels::compare(&x, &y).passes())
            {
                Some("pixels differ".to_string())
            } else if a.findings != b.findings {
                let lines = |s: &Shot| s.findings.iter().map(Finding::line).collect::<Vec<_>>();
                let (before, after) = (lines(a), lines(b));
                let gone: Vec<&String> = before.iter().filter(|l| !after.contains(l)).collect();
                let new: Vec<&String> = after.iter().filter(|l| !before.contains(l)).collect();
                Some(format!("findings differ (gone: {gone:?}, new: {new:?})"))
            } else {
                None
            };
            if why.is_none() && a.png != b.png && !noisy.contains(&a.id) {
                noisy.push(a.id.clone());
            }
            if let Some(why) = why {
                // Keep the first pair that differs per shot, with the diff, so the cause can be looked at.
                let dir = out_dir.join("repeat");
                let first_for_shot = !out.iter().any(|f: &Failure| f.id == a.id);
                if first_for_shot && a.png != b.png {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join(format!("{}.run1.png", a.id)), &a.png);
                    let _ = std::fs::write(dir.join(format!("{}.run{}.png", a.id, n + 1)), &b.png);
                    if let (Ok(x), Ok(y)) = (decode_png(&a.png), decode_png(&b.png))
                        && let Some(d) = pixels::compare(&x, &y).diff
                        && let Ok(bytes) = pixels::encode(&d, 1)
                    {
                        let _ = std::fs::write(dir.join(format!("{}.diff.png", a.id)), bytes);
                    }
                }
                out.push(Failure {
                    id: a.id.clone(),
                    reasons: vec![format!("repeat: run {} {why} from run 1", n + 1)],
                    findings: Vec::new(),
                });
            }
        }
    }
    noisy.sort();
    (out, noisy)
}

fn decode_png(png: &[u8]) -> Result<Image, Error> {
    pixels::decode(png)
}

fn check(
    root: &Path,
    refs: &Path,
    out: &Path,
    shots: &[Shot],
    outcome: &mut Outcome,
) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::Environment(e.to_string());
    let mut html = Vec::new();
    let env_file = refs.join("env.json");
    if let Ok(text) = std::fs::read_to_string(&env_file)
        && text != env_json()
    {
        outcome.notes.push(format!(
            "note: references were made with {}; this run is {}. A pixel failure may be the environment, not the code",
            text.split_whitespace().collect::<Vec<_>>().join(" "),
            env_json().split_whitespace().collect::<Vec<_>>().join(" ")
        ));
    }
    for shot in shots {
        let mut reasons: Vec<String> = shot.findings.iter().map(Finding::line).collect();
        let mut failure = PixelFailure {
            id: shot.id.clone(),
            reason: String::new(),
            reference: None,
            actual: None,
            diff: None,
            notes: Vec::new(),
            boxes: shot.findings.iter().flat_map(|f| f.boxes.clone()).collect(),
            size: Some(shot.size),
        };
        if !shot.png.is_empty() {
            let reference = refs.join(format!("{}.png", shot.id));
            if reference.is_file() {
                let before = decode_png(&std::fs::read(&reference).map_err(io)?)?;
                let after = decode_png(&shot.png)?;
                let c = pixels::compare(&before, &after);
                if !c.passes() {
                    let why = if c.same_size {
                        format!(
                            "pixels: {:.2}% differ (limit {:.1}%), worst block {:.0}% (limit {:.0}%)",
                            c.ratio * 100.0,
                            pixels::TOL_RATIO * 100.0,
                            c.block * 100.0,
                            pixels::TOL_BLOCK * 100.0
                        )
                    } else {
                        format!(
                            "pixels: size changed {}x{} to {}x{}",
                            before.w, before.h, after.w, after.h
                        )
                    };
                    reasons.push(why);
                    if let Some(diff) = &c.diff {
                        let file = format!("{}.diff.png", shot.id);
                        std::fs::create_dir_all(out).map_err(io)?;
                        std::fs::write(out.join(&file), pixels::encode(diff, 1)?).map_err(io)?;
                        failure.diff = Some(file);
                    }
                }
                if !reasons.is_empty() {
                    let file = format!("{}.reference.png", shot.id);
                    std::fs::create_dir_all(out).map_err(io)?;
                    std::fs::copy(&reference, out.join(&file)).map_err(io)?;
                    failure.reference = Some(file);
                }
            } else {
                reasons.push(format!(
                    "pixels: no reference (shakedown update --screens {})",
                    shot.id.split('@').next().unwrap_or_default()
                ));
            }
            if !reasons.is_empty() {
                let file = format!("{}.actual.png", shot.id);
                std::fs::create_dir_all(out).map_err(io)?;
                std::fs::write(out.join(&file), pixels::encode(&decode_png(&shot.png)?, 1)?)
                    .map_err(io)?;
                failure.actual = Some(file);
            }
        }
        if !reasons.is_empty() {
            failure.reason = reasons.join("; ");
            failure.notes.clone_from(&reasons);
            outcome.failures.push(Failure {
                id: shot.id.clone(),
                reasons,
                findings: shot.findings.clone(),
            });
            html.push(failure);
        }
    }
    if !html.is_empty() {
        std::fs::create_dir_all(out).map_err(io)?;
        std::fs::write(out.join("report.html"), report::pixels_html(&html)).map_err(io)?;
        outcome.report = Some(rel(root, &out.join("report.html")));
    }
    Ok(())
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

fn update(refs: &Path, shots: &[Shot], outcome: &mut Outcome) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::Environment(e.to_string());
    std::fs::create_dir_all(refs).map_err(io)?;
    for shot in shots {
        if shot.png.is_empty() {
            outcome.failures.push(Failure {
                id: shot.id.clone(),
                reasons: shot.findings.iter().map(Finding::line).collect(),
                findings: shot.findings.clone(),
            });
            continue;
        }
        let new = pixels::encode(&decode_png(&shot.png)?, pixels::REF_STEP)?;
        let path = refs.join(format!("{}.png", shot.id));
        // A reference is rewritten only when it is missing or really fails: antialiasing noise inside the
        // compare tolerance leaves the file alone, so a re-run changes nothing in git. The drawn pixels, not the
        // quantized ones, are compared: that is what `check` compares, so `update` and `check` agree.
        let same = std::fs::read(&path)
            .ok()
            .and_then(|old| decode_png(&old).ok())
            .zip(decode_png(&shot.png).ok())
            .is_some_and(|(a, b)| a == b || pixels::compare(&a, &b).passes());
        if same {
            outcome.unchanged += 1;
        } else {
            std::fs::write(&path, new).map_err(io)?;
            outcome.written += 1;
        }
        if !shot.findings.is_empty() {
            outcome.failures.push(Failure {
                id: shot.id.clone(),
                reasons: shot.findings.iter().map(Finding::line).collect(),
                findings: shot.findings.clone(),
            });
        }
    }
    let env = refs.join("env.json");
    if std::fs::read_to_string(&env).ok().as_deref() != Some(&env_json()) {
        std::fs::write(&env, env_json()).map_err(io)?;
    }
    Ok(())
}

/// `summary.json` text for a run: stable, no times, no absolute paths.
pub fn summary_json(project: &str, outcome: &Outcome) -> String {
    let failures: Vec<_> = outcome
        .failures
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "reasons": f.reasons,
                "findings": f.findings.iter().map(|x| json!({"rule": x.rule, "message": x.message})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&json!({
        "project": project,
        "platform": "web",
        "shots": outcome.shots,
        "passed": outcome.failures.is_empty(),
        "flaky": outcome.flaky.iter().map(|(id, why)| json!({"id": id, "first_failure": why})).collect::<Vec<_>>(),
        "failures": failures,
    }))
    .unwrap_or_default();
    s.push('\n');
    s
}

/// The printed lines for a run: failures first (at most `LIMIT`), then the verdict line.
pub fn lines(outcome: &Outcome, mode: Mode) -> Vec<String> {
    const LIMIT: usize = 20;
    let mut out: Vec<String> = outcome.notes.clone();
    for f in outcome.failures.iter().take(LIMIT) {
        let shown = f
            .reasons
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let extra = f.reasons.len().saturating_sub(3);
        out.push(format!(
            "  {}: {shown}{}",
            f.id,
            if extra > 0 {
                format!(" (+{extra} more)")
            } else {
                String::new()
            }
        ));
    }
    if outcome.failures.len() > LIMIT {
        out.push(format!(
            "  ... and {} more (see the report)",
            outcome.failures.len() - LIMIT
        ));
    }
    out.push(match (mode, outcome.failures.is_empty()) {
        (Mode::Check, true) => format!("ok ui.web {} shots {:.1}s", outcome.shots, outcome.secs),
        (Mode::Check, false) => format!(
            "FAIL ui.web {} of {} shots {:.1}s",
            outcome.failures.len(),
            outcome.shots,
            outcome.secs
        ),
        (Mode::Update, _) => format!(
            "update web: {} written, {} unchanged{} {:.1}s",
            outcome.written,
            outcome.unchanged,
            if outcome.failures.is_empty() {
                String::new()
            } else {
                format!(", {} with problems", outcome.failures.len())
            },
            outcome.secs
        ),
    });
    if let Some(r) = &outcome.report {
        out.push(format!("report: {r}"));
    }
    out
}

/// A check step: one line of detail, the failure lines as output.
pub fn step(config: &Config, root: &Path) -> (crate::steps::Status, String, String) {
    let opts = Options {
        screens: Vec::new(),
        devices: Vec::new(),
        repeat: 1,
        mode: Mode::Check,
    };
    match run(config, root, &opts) {
        Ok(o) => {
            let status = if o.failures.is_empty() {
                crate::steps::Status::Ok
            } else {
                crate::steps::Status::Fail(format!("{} of {} shots", o.failures.len(), o.shots))
            };
            (
                status,
                format!("{} shots", o.shots),
                lines(&o, Mode::Check).join("\n"),
            )
        }
        Err(e) => (
            crate::steps::Status::Fail(e.to_string()),
            String::new(),
            e.to_string(),
        ),
    }
}

#[cfg(test)]
mod flaky_tests {
    use super::*;

    #[test]
    fn flaky_shots_are_in_the_json_summary() {
        let outcome = Outcome {
            shots: 2,
            flaky: vec![("home@phone-light".into(), "took longer than 60s".into())],
            ..Outcome::default()
        };
        let text = summary_json("p", &outcome);
        assert!(text.contains("\"flaky\""), "{text}");
        assert!(text.contains("home@phone-light"), "{text}");
        assert!(text.contains("took longer than 60s"), "{text}");
    }
}
