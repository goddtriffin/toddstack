//! `shakedown sweep`: every device, every theme, every sweep route and group, headless. Layout rules and layout
//! shift fail the sweep; pixel changes against the local baseline are reported, never failed. `--pack` writes the
//! review pack (design section 11).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;

use crate::capture::{self, Job, Shot};
use crate::config::{Config, Screen, SweepRoute};
use crate::devices::{self, Device};
use crate::error::Error;
use crate::layout::Finding;
use crate::pixels::{self, Image};
use crate::report::{self, PixelFailure};
use crate::sheet::{self, Frame, Picture};
use crate::ui;

/// What the caller asked for.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// A running local server to draw against, instead of starting the config's.
    pub base: Option<String>,
    /// Only these pages (routes and groups, by name); empty means all.
    pub routes: Vec<String>,
    /// Write the review pack.
    pub pack: bool,
    /// Replace the baseline with this run's pictures.
    pub rebase: bool,
}

/// A page to draw: a route or a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Its name in the pack.
    pub name: String,
    /// The path.
    pub path: String,
    /// Who is signed in: one account for a route, the accounts taking turns for a group.
    pub players: Vec<String>,
}

/// The name a page has in the pack: the given one, or made from the path (`/account/friends` is
/// `account-friends`, `/` is `home`, a query adds its words).
pub fn page_name(name: Option<&str>, path: &str) -> String {
    let source = name.unwrap_or(path);
    let mut out = String::new();
    for c in source.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "home".into() } else { out }
}

/// What a shot looks like next to the baseline.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// No baseline yet: this picture becomes it.
    New,
    /// Inside the compare tolerance.
    Same,
    /// Pixels moved past the tolerance.
    Changed {
        /// Share of pixels that differ.
        ratio: f64,
        /// The worst 24x24 block's share.
        block: f64,
    },
    /// The picture's size changed.
    Resized {
        /// Baseline size.
        from: (u32, u32),
        /// This run's size.
        to: (u32, u32),
    },
    /// The page could not be drawn.
    NoImage,
}

impl Change {
    /// The one line for people, `None` for the unremarkable.
    pub fn line(&self) -> Option<String> {
        match self {
            Self::Changed { ratio, block } => Some(format!(
                "{:.2}% of pixels differ, worst block {:.0}%",
                ratio * 100.0,
                block * 100.0
            )),
            Self::Resized { from, to } => Some(format!(
                "size changed {}x{} to {}x{}",
                from.0, from.1, to.0, to.1
            )),
            _ => None,
        }
    }
}

/// Compares a picture with the baseline's (the PNG bytes of each).
pub fn judge(baseline: Option<&[u8]>, current: &[u8]) -> Change {
    if current.is_empty() {
        return Change::NoImage;
    }
    let Some(baseline) = baseline else {
        return Change::New;
    };
    if baseline == current {
        // The very same file: nothing to decode, nothing to compare.
        return Change::Same;
    }
    let (Ok(before), Ok(after)) = (pixels::decode(baseline), pixels::decode(current)) else {
        return Change::NoImage;
    };
    compare_images(&before, &after)
}

fn compare_images(before: &Image, after: &Image) -> Change {
    let c = pixels::compare(before, after);
    if !c.same_size {
        Change::Resized {
            from: (before.w, before.h),
            to: (after.w, after.h),
        }
    } else if c.passes() {
        Change::Same
    } else {
        Change::Changed {
            ratio: c.ratio,
            block: c.block,
        }
    }
}

/// `<route>@<device>-<theme>` into its three parts.
pub fn split_id(id: &str) -> Option<(&str, &str, &str)> {
    let (route, rest) = id.split_once('@')?;
    let (device, theme) = rest.rsplit_once('-')?;
    Some((route, device, theme))
}

/// One shot's result, for the manifest and the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Page name.
    pub route: String,
    /// Device name.
    pub device: String,
    /// `light` or `dark`.
    pub theme: String,
    /// Findings as `rule: message`, sorted.
    pub findings: Vec<String>,
    /// Against the baseline.
    pub change: Change,
}

/// The pack's `manifest.json`: routes, devices, themes, every finding, every pixel change. Deterministic: sorted,
/// no times, no absolute paths.
pub fn manifest(
    project: &str,
    pages: &[Page],
    devices: &[String],
    themes: &[String],
    records: &[Record],
    sheets: &BTreeMap<(String, String), String>,
) -> String {
    let mut sorted: Vec<&Record> = records.iter().collect();
    sorted.sort_by(|a, b| (&a.route, &a.device, &a.theme).cmp(&(&b.route, &b.device, &b.theme)));
    let findings: Vec<_> = sorted
        .iter()
        .flat_map(|r| {
            let mut lines = r.findings.clone();
            lines.sort();
            lines.into_iter().map(move |f| {
                json!({"route": r.route, "device": r.device, "theme": r.theme, "finding": f})
            })
        })
        .collect();
    let changes: Vec<_> = sorted
        .iter()
        .filter_map(|r| {
            r.change.line().map(
                |l| json!({"route": r.route, "device": r.device, "theme": r.theme, "change": l}),
            )
        })
        .collect();
    let count = |want: &Change| {
        records
            .iter()
            .filter(|r| std::mem::discriminant(&r.change) == std::mem::discriminant(want))
            .count()
    };
    let routes: Vec<_> = pages
        .iter()
        .map(|p| {
            let contact: BTreeMap<&str, &String> = sheets
                .iter()
                .filter(|((route, _), _)| route == &p.name)
                .map(|((_, theme), file)| (theme.as_str(), file))
                .collect();
            json!({"name": p.name, "path": p.path, "players": p.players, "sheets": contact})
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&json!({
        "project": project,
        "routes": routes,
        "devices": devices,
        "themes": themes,
        "shots": records.len(),
        "findings": findings,
        "pixel_changes": changes,
        "pixels_new": count(&Change::New),
    }))
    .unwrap_or_default();
    s.push('\n');
    s
}

fn out_dir(root: &Path) -> PathBuf {
    root.join("target/shakedown/sweep")
}

fn pack_dir(root: &Path) -> PathBuf {
    root.join("target/shakedown/review")
}

/// The pages to draw, with the players each one has.
fn pages(config: &Config, only: &[String]) -> Result<Vec<(Page, Arc<Screen>)>, Error> {
    let sweep = config.sweep.clone().unwrap_or_default();
    let mut all: Vec<(Page, Arc<Screen>)> = Vec::new();
    for r in &sweep.routes {
        let (path, name, player, routes, allow, turn) = match r {
            SweepRoute::Path(p) => (
                p.clone(),
                None,
                "guest".to_string(),
                BTreeMap::new(),
                Vec::new(),
                true,
            ),
            SweepRoute::Page(p) => (
                p.path.clone(),
                p.name.clone(),
                p.player.clone(),
                p.routes.clone(),
                p.allow_console.clone(),
                p.turn,
            ),
        };
        let name = page_name(name.as_deref(), &path);
        let screen = Screen {
            name: name.clone(),
            path: path.clone(),
            player: player.clone(),
            turn,
            routes,
            act: Vec::new(),
            devices: None,
            allow_console: allow,
        };
        all.push((
            Page {
                name,
                path,
                players: vec![player],
            },
            Arc::new(screen),
        ));
    }
    let accounts = config
        .players
        .as_ref()
        .map_or_else(Vec::new, |p| p.accounts.clone());
    for g in &sweep.groups {
        let screen = Screen {
            name: page_name(Some(&g.name), &g.path),
            path: g.path.clone(),
            player: "group".into(),
            turn: true,
            routes: g.routes.clone(),
            act: Vec::new(),
            devices: None,
            allow_console: g.allow_console.clone(),
        };
        all.push((
            Page {
                name: screen.name.clone(),
                path: g.path.clone(),
                players: accounts.iter().take(g.players).cloned().collect(),
            },
            Arc::new(screen),
        ));
    }
    if all.is_empty() {
        return Err(Error::Config(
            "nothing to sweep: add routes under [sweep] in shakedown.toml".into(),
        ));
    }
    if !only.is_empty() {
        let unknown: Vec<&str> = only
            .iter()
            .filter(|o| !all.iter().any(|(p, _)| &p.name == *o))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(Error::Config(format!(
                "unknown route(s): {} (routes: {})",
                unknown.join(", "),
                all.iter()
                    .map(|(p, _)| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )));
        }
        all.retain(|(p, _)| only.contains(&p.name));
    }
    Ok(all)
}

/// The jobs: every page on every device in every theme. A group's frame `i` (the `i`th device) is
/// `players[i mod n]`, in both themes.
fn jobs(
    pages: &[(Page, Arc<Screen>)],
    devices: &[Device],
    themes: &[String],
    root: &Path,
    now_ms: i64,
) -> Result<Vec<Job>, Error> {
    let mut out = Vec::new();
    for (page, screen) in pages {
        let grouped = screen.player == "group";
        let base = ui::plan(root, screen, now_ms)?;
        let plans: Vec<Arc<crate::capture::Plan>> = if grouped {
            page.players
                .iter()
                .map(|player| {
                    let mut p = base.clone();
                    p.player.clone_from(player);
                    Arc::new(p)
                })
                .collect()
        } else {
            vec![Arc::new(base)]
        };
        for (i, d) in devices.iter().enumerate() {
            for t in themes {
                out.push(Job {
                    plan: plans[i % plans.len()].clone(),
                    device: d.clone(),
                    theme: t.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// A base URL must be a local server: the sweep never draws anything off this machine.
///
/// # Errors
///
/// Not an http URL, or a host that is not loopback.
pub fn check_base(base: &str) -> Result<String, Error> {
    let url = url::Url::parse(base).map_err(|e| Error::Config(format!("--base {base:?}: {e}")))?;
    if url.scheme() != "http" {
        return Err(Error::Config(format!(
            "--base {base:?}: only http, on this machine"
        )));
    }
    let local = match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if !local {
        return Err(Error::Config(format!(
            "--base {base:?}: not a local address (the sweep only draws against 127.0.0.1 or localhost)"
        )));
    }
    Ok(base.trim_end_matches('/').to_string())
}

/// The result of a sweep.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Shots drawn.
    pub shots: usize,
    /// Shots with findings.
    pub failed: Vec<Record>,
    /// Pixel changes against the baseline (reported, not failed).
    pub changed: usize,
    /// Pictures that became the baseline.
    pub new: usize,
    /// Wall time.
    pub secs: f64,
    /// Notes to print first.
    pub notes: Vec<String>,
    /// The report, relative to the repo.
    pub report: Option<String>,
    /// The pack, relative to the repo.
    pub pack: Option<String>,
    /// Sheets written.
    pub sheets: usize,
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

fn io(e: &std::io::Error) -> Error {
    Error::Environment(e.to_string())
}

fn png_of(shot: &Shot) -> Result<Vec<u8>, Error> {
    if shot.png.is_empty() {
        return Ok(Vec::new());
    }
    // The sweep's pictures stay on this machine (the baseline, `current/`, the pack): speed over size.
    pixels::encode_with(
        &pixels::decode(&shot.png)?,
        pixels::REF_STEP,
        png::Compression::Fast,
    )
}

/// Runs the sweep.
///
/// # Errors
///
/// Config or usage problems, a missing environment (Chrome, the server), or failed layout rules.
pub fn run(config: &Config, root: &Path, opts: &Options) -> Result<Outcome, Error> {
    let started = Instant::now();
    let base = opts.base.as_deref().map(check_base).transpose()?;
    if base.is_none() {
        config.require_server()?;
    }
    let pages = pages(config, &opts.routes)?;
    let device_list = devices::pick(&config.devices.sweep_names())?;
    let now = config
        .server
        .as_ref()
        .map_or("2026-01-15T12:00:00Z", |s| s.now.as_str());
    let jobs = jobs(
        &pages,
        &device_list,
        &config.devices.themes,
        root,
        ui::now_ms(now)?,
    )?;
    let dir = out_dir(root);
    for sub in ["current", "diff", "server"] {
        let _ = std::fs::remove_dir_all(dir.join(sub));
    }
    if opts.pack {
        let _ = std::fs::remove_dir_all(pack_dir(root));
    }
    std::fs::create_dir_all(dir.join("baseline")).map_err(|e| io(&e))?;
    // Each shot is judged on its own thread the moment it is drawn, while the others are still being drawn.
    let judged: Arc<Mutex<Vec<Result<Judged, Error>>>> = Arc::default();
    let sink: capture::Sink = {
        let (dir, judged) = (dir.clone(), judged.clone());
        Arc::new(move |shot: &Shot| {
            let r = judge_shot(&dir, shot);
            if let Ok(mut j) = judged.lock() {
                j.push(r);
            }
        })
    };
    let runs = ui::draw_all(
        config,
        root,
        &dir.join("server"),
        &jobs,
        1,
        started,
        base.as_deref(),
        Some(&sink),
    )?;
    let shots = runs.into_iter().next().unwrap_or_default();

    let mut outcome = Outcome {
        shots: shots.len(),
        ..Outcome::default()
    };
    let flaky: Vec<String> = shots
        .iter()
        .filter_map(|s| s.flaky.as_ref().map(|why| format!("{} ({why})", s.id)))
        .collect();
    if !flaky.is_empty() {
        outcome.notes.push(format!(
            "warning: flaky: {} shot(s) failed once and passed on the retry: {}",
            flaky.len(),
            flaky.join("; ")
        ));
    }
    let judged = std::mem::take(
        &mut *judged
            .lock()
            .map_err(|_| Error::Failed("judge lock".into()))?,
    );
    let (records, pictures) = collect(judged, shots.len())?;
    // The run finished whole: only now do new pictures (or all of them, on `--rebase`) become the baseline, so a
    // failed or interrupted run never leaves a half-written one.
    keep_baseline(&dir, &records, &pictures, opts.rebase)?;
    outcome.new = records.iter().filter(|r| r.change == Change::New).count();
    outcome.changed = records.iter().filter(|r| r.change.line().is_some()).count();
    outcome.failed = records
        .iter()
        .filter(|r| !r.findings.is_empty())
        .cloned()
        .collect();
    write_report(root, &dir, &records, &shots, &mut outcome)?;
    if opts.pack {
        pack(
            config,
            root,
            &pages,
            &device_list,
            &records,
            &pictures,
            &mut outcome,
        )?;
    }
    outcome.secs = started.elapsed().as_secs_f64();
    Ok(outcome)
}

type Pictures = BTreeMap<String, Vec<u8>>;

/// One judged shot: its record and its (re-encoded) picture.
struct Judged {
    record: Record,
    id: String,
    png: Vec<u8>,
}

/// Writes `current/` and compares with `baseline/` (which is only read here: `keep_baseline` writes it once the
/// whole run has finished). Shots are independent: this runs for each as soon as it is drawn.
fn judge_shot(dir: &Path, shot: &Shot) -> Result<Judged, Error> {
    let (current, baseline, diff) = (dir.join("current"), dir.join("baseline"), dir.join("diff"));
    std::fs::create_dir_all(&current).map_err(|e| io(&e))?;
    let (route, device, theme) =
        split_id(&shot.id).ok_or_else(|| Error::Environment(format!("shot id {:?}", shot.id)))?;
    let png = png_of(shot)?;
    let old_path = baseline.join(format!("{}.png", shot.id));
    let old = std::fs::read(&old_path).ok();
    let change = judge(old.as_deref(), &png);
    if !png.is_empty() {
        std::fs::write(current.join(format!("{}.png", shot.id)), &png).map_err(|e| io(&e))?;
    }
    if let (Change::Changed { .. }, Some(old)) = (&change, &old)
        && let (Ok(a), Ok(b)) = (pixels::decode(old), pixels::decode(&png))
        && let Some(d) = pixels::compare(&a, &b).diff
    {
        std::fs::create_dir_all(&diff).map_err(|e| io(&e))?;
        std::fs::write(
            diff.join(format!("{}.png", shot.id)),
            pixels::encode(&d, 1)?,
        )
        .map_err(|e| io(&e))?;
    }
    let mut findings: Vec<String> = shot.findings.iter().map(Finding::line).collect();
    findings.sort();
    Ok(Judged {
        record: Record {
            route: route.into(),
            device: device.into(),
            theme: theme.into(),
            findings,
            change,
        },
        id: shot.id.clone(),
        png,
    })
}

/// Writes `baseline/`: the pictures that have no baseline yet, or every picture on `rebase`.
fn keep_baseline(
    dir: &Path,
    records: &[Record],
    pictures: &Pictures,
    rebase: bool,
) -> Result<(), Error> {
    let baseline = dir.join("baseline");
    std::fs::create_dir_all(&baseline).map_err(|e| io(&e))?;
    for r in records {
        let id = format!("{}@{}-{}", r.route, r.device, r.theme);
        let Some(png) = pictures.get(&id).filter(|p| !p.is_empty()) else {
            continue;
        };
        let path = baseline.join(format!("{id}.png"));
        if rebase || !path.exists() {
            std::fs::write(&path, png).map_err(|e| io(&e))?;
        }
    }
    Ok(())
}

/// The judged shots as sorted records and the pictures by id.
fn collect(
    judged: Vec<Result<Judged, Error>>,
    shots: usize,
) -> Result<(Vec<Record>, Pictures), Error> {
    let mut records = Vec::new();
    let mut pictures = Pictures::new();
    for j in judged {
        let j = j?;
        pictures.insert(j.id, j.png);
        records.push(j.record);
    }
    if records.len() != shots {
        return Err(Error::Failed(format!(
            "{} of {shots} shots were judged",
            records.len()
        )));
    }
    records.sort_by(|a, b| (&a.route, &a.device, &a.theme).cmp(&(&b.route, &b.device, &b.theme)));
    Ok((records, pictures))
}

/// `report.html`: every shot with a finding or a pixel change, baseline beside current beside the diff.
fn write_report(
    root: &Path,
    dir: &Path,
    records: &[Record],
    shots: &[Shot],
    outcome: &mut Outcome,
) -> Result<(), Error> {
    let mut items = Vec::new();
    for r in records {
        if r.findings.is_empty() && r.change.line().is_none() {
            continue;
        }
        let id = format!("{}@{}-{}", r.route, r.device, r.theme);
        let mut notes = r.findings.clone();
        notes.extend(r.change.line().map(|l| format!("pixels: {l}")));
        let file = |sub: &str| {
            dir.join(sub)
                .join(format!("{id}.png"))
                .is_file()
                .then(|| format!("{sub}/{id}.png"))
        };
        let shot = shots.iter().find(|s| s.id == id);
        items.push(PixelFailure {
            id: id.clone(),
            reason: notes.join("; "),
            reference: file("baseline"),
            actual: file("current"),
            diff: file("diff"),
            notes,
            boxes: shot.map_or_else(Vec::new, |s| {
                s.findings.iter().flat_map(|f| f.boxes.clone()).collect()
            }),
            size: shot.map(|s| s.size),
        });
    }
    if items.is_empty() {
        return Ok(());
    }
    let path = dir.join("report.html");
    std::fs::write(&path, report::pixels_html(&items)).map_err(|e| io(&e))?;
    outcome.report = Some(rel(root, &path));
    Ok(())
}

fn pack(
    config: &Config,
    root: &Path,
    pages: &[(Page, Arc<Screen>)],
    device_list: &[Device],
    records: &[Record],
    pictures: &Pictures,
    outcome: &mut Outcome,
) -> Result<(), Error> {
    let dir = pack_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| io(&e))?;
    // One sheet per page and theme; their HTML (every picture base64'd) is built on all cores, in sheet order.
    let sheets: Vec<(&Page, &String)> = pages
        .iter()
        .flat_map(|(page, _)| config.devices.themes.iter().map(move |theme| (page, theme)))
        .collect();
    let build = |(page, theme): &(&Page, &String)| {
        let mut frames = Vec::new();
        let mut pics = Vec::new();
        for d in device_list {
            let id = format!("{}@{}-{theme}", page.name, d.name);
            let png = pictures.get(&id).cloned().unwrap_or_default();
            let flagged = records.iter().any(|r| {
                r.route == page.name
                    && r.device == d.name
                    && &r.theme == *theme
                    && !r.findings.is_empty()
            });
            let frame = Frame {
                name: d.name.clone(),
                w: d.width,
                h: d.height,
            };
            frames.push(frame.clone());
            pics.push(Picture {
                frame,
                png,
                flagged,
            });
        }
        let sheet = sheet::layout(&frames, sheet::MAX_EDGE);
        (sheet::html(&sheet, &pics), sheet.w, sheet.h)
    };
    let workers = capture::parallel().min(sheets.len()).max(1);
    let mut items: Vec<Option<(String, u32, u32)>> = vec![None; sheets.len()];
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|k| {
                let (sheets, build) = (&sheets, &build);
                scope.spawn(move || {
                    sheets
                        .iter()
                        .enumerate()
                        .skip(k)
                        .step_by(workers)
                        .map(|(i, s)| (i, build(s)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            for (i, item) in h.join().unwrap_or_default() {
                items[i] = Some(item);
            }
        }
    });
    let items: Vec<(String, u32, u32)> = items.into_iter().flatten().collect();
    if items.len() != sheets.len() {
        return Err(Error::Failed("a pack sheet could not be built".into()));
    }
    let names: Vec<(String, String)> = sheets
        .iter()
        .map(|(page, theme)| (page.name.clone(), (*theme).clone()))
        .collect();
    let chrome = crate::browser::install()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Environment(format!("runtime: {e}")))?;
    // Dropping the capture on a caught signal drops its Chrome guard: Chrome is killed and its profile removed.
    let signals = crate::world::watch_signals();
    let pngs = runtime.block_on(async {
        tokio::select! {
            r = sheet::capture_all(&chrome, &items) => r,
            () = async { while !signals.interrupted() { tokio::time::sleep(std::time::Duration::from_millis(50)).await; } } => Err(Error::Failed("interrupted".into())),
        }
    })?;
    drop(signals);
    let mut files = BTreeMap::new();
    for ((route, theme), png) in names.into_iter().zip(pngs) {
        let file = format!("{route}-{theme}.png");
        std::fs::write(dir.join(&file), png).map_err(|e| io(&e))?;
        files.insert((route, theme), file);
    }
    outcome.sheets = files.len();
    let page_list: Vec<Page> = pages.iter().map(|(p, _)| p.clone()).collect();
    let device_names: Vec<String> = device_list.iter().map(|d| d.name.clone()).collect();
    std::fs::write(
        dir.join("manifest.json"),
        manifest(
            &config.project,
            &page_list,
            &device_names,
            &config.devices.themes,
            records,
            &files,
        ),
    )
    .map_err(|e| io(&e))?;
    outcome.pack = Some(rel(root, &dir));
    Ok(())
}

/// The printed lines: notes, at most `LIMIT` failing shots, pixel changes, the verdict, then the paths.
pub fn lines(o: &Outcome) -> Vec<String> {
    const LIMIT: usize = 20;
    let mut out = o.notes.clone();
    for r in o.failed.iter().take(LIMIT) {
        let shown = r
            .findings
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let extra = r.findings.len().saturating_sub(3);
        out.push(format!(
            "  {}@{}-{}: {shown}{}",
            r.route,
            r.device,
            r.theme,
            if extra > 0 {
                format!(" (+{extra} more)")
            } else {
                String::new()
            }
        ));
    }
    if o.failed.len() > LIMIT {
        out.push(format!(
            "  ... and {} more (see the report)",
            o.failed.len() - LIMIT
        ));
    }
    if o.changed > 0 || o.new > 0 {
        out.push(format!(
            "pixels: {} changed against the baseline (reported, not failed), {} new",
            o.changed, o.new
        ));
    }
    out.push(if o.failed.is_empty() {
        format!("ok sweep {} shots {:.1}s", o.shots, o.secs)
    } else {
        format!(
            "FAIL sweep {} of {} shots {:.1}s",
            o.failed.len(),
            o.shots,
            o.secs
        )
    });
    if let Some(r) = &o.report {
        out.push(format!("report: {r}"));
    }
    if let Some(p) = &o.pack {
        out.push(format!("pack: {p} ({} contact sheets)", o.sheets));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type Sheets = BTreeMap<(String, String), String>;

    fn rec(route: &str, device: &str, theme: &str, findings: &[&str], change: Change) -> Record {
        Record {
            route: route.into(),
            device: device.into(),
            theme: theme.into(),
            findings: findings.iter().map(|s| (*s).into()).collect(),
            change,
        }
    }

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        let px: Vec<u8> = (0..w * h).flat_map(|_| [v, v, v, 255]).collect();
        pixels::encode(&Image { w, h, px }, 1).unwrap()
    }

    #[test]
    fn names_come_from_paths() {
        assert_eq!(page_name(None, "/"), "home");
        assert_eq!(page_name(None, "/account/friends"), "account-friends");
        assert_eq!(
            page_name(None, "/account?notice=saved"),
            "account-notice-saved"
        );
        assert_eq!(page_name(Some("Lobby Test"), "/lobby/test"), "lobby-test");
    }

    #[test]
    fn ids_split_into_route_device_theme() {
        assert_eq!(
            split_id("account-friends@small-phone-land-dark"),
            Some(("account-friends", "small-phone-land", "dark"))
        );
        assert_eq!(split_id("nope"), None);
    }

    #[test]
    fn baseline_diff_tells_new_same_changed_resized_and_missing_apart() {
        let a = solid(100, 100, 40);
        assert_eq!(judge(None, &a), Change::New);
        assert_eq!(judge(Some(&a), &a), Change::Same);
        assert_eq!(judge(Some(&a), &[]), Change::NoImage);
        // A change of a few channel steps is noise; a real one is a change.
        assert_eq!(judge(Some(&a), &solid(100, 100, 44)), Change::Same);
        match judge(Some(&a), &solid(100, 100, 200)) {
            Change::Changed { ratio, block } => assert!(ratio > 0.99 && block > 0.99),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            judge(Some(&a), &solid(100, 120, 40)),
            Change::Resized {
                from: (100, 100),
                to: (100, 120)
            }
        );
    }

    #[test]
    fn a_small_changed_block_is_a_change_even_when_the_screen_barely_moved() {
        let a = solid(200, 200, 40);
        let mut img = pixels::decode(&a).unwrap();
        for y in 10..34 {
            for x in 10..34 {
                let i = usize::try_from((y * 200 + x) * 4_u32).unwrap();
                img.px[i] = 250;
            }
        }
        let b = pixels::encode(&img, 1).unwrap();
        assert!(matches!(judge(Some(&a), &b), Change::Changed { .. }));
    }

    fn sample() -> (Vec<Page>, Vec<Record>, Sheets) {
        let pages = vec![
            Page {
                name: "home".into(),
                path: "/".into(),
                players: vec!["guest".into()],
            },
            Page {
                name: "lobby".into(),
                path: "/lobby".into(),
                players: vec!["ann".into(), "ben".into()],
            },
        ];
        let records = vec![
            rec("lobby", "phone", "dark", &[], Change::Same),
            rec(
                "home",
                "phone",
                "light",
                &["sideways-scroll: 400 > 390", "clipped-text: x"],
                Change::New,
            ),
            rec(
                "home",
                "desktop",
                "light",
                &[],
                Change::Changed {
                    ratio: 0.0123,
                    block: 0.5,
                },
            ),
        ];
        let mut sheets = BTreeMap::new();
        sheets.insert(
            ("home".to_string(), "light".to_string()),
            "home-light.png".to_string(),
        );
        sheets.insert(
            ("home".to_string(), "dark".to_string()),
            "home-dark.png".to_string(),
        );
        (pages, records, sheets)
    }

    #[test]
    fn the_manifest_lists_routes_devices_themes_findings_and_changes() {
        let (pages, records, sheets) = sample();
        let text = manifest(
            "p",
            &pages,
            &["desktop".into(), "phone".into()],
            &["light".into(), "dark".into()],
            &records,
            &sheets,
        );
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["project"], "p");
        assert_eq!(v["shots"], 3);
        assert_eq!(v["devices"], json!(["desktop", "phone"]));
        assert_eq!(v["themes"], json!(["light", "dark"]));
        assert_eq!(v["routes"][0]["name"], "home");
        assert_eq!(v["routes"][0]["sheets"]["dark"], "home-dark.png");
        assert_eq!(v["routes"][0]["sheets"]["light"], "home-light.png");
        assert_eq!(v["routes"][1]["players"], json!(["ann", "ben"]));
        assert_eq!(v["findings"].as_array().unwrap().len(), 2);
        assert_eq!(v["findings"][0]["finding"], "clipped-text: x");
        assert_eq!(v["findings"][0]["device"], "phone");
        assert_eq!(v["pixel_changes"].as_array().unwrap().len(), 1);
        assert_eq!(v["pixel_changes"][0]["device"], "desktop");
        assert!(
            v["pixel_changes"][0]["change"]
                .as_str()
                .unwrap()
                .contains("1.23%")
        );
        assert_eq!(v["pixels_new"], 1);
    }

    #[test]
    fn the_manifest_is_the_same_whatever_order_the_shots_came_in() {
        let (pages, mut records, sheets) = sample();
        let d = ["desktop".to_string(), "phone".to_string()];
        let t = ["light".to_string(), "dark".to_string()];
        let a = manifest("p", &pages, &d, &t, &records, &sheets);
        records.reverse();
        assert_eq!(a, manifest("p", &pages, &d, &t, &records, &sheets));
        assert!(!a.contains("secs") && !a.contains("/Users"));
    }

    #[test]
    fn only_local_bases_are_allowed() {
        assert_eq!(
            check_base("http://127.0.0.1:8790/").unwrap(),
            "http://127.0.0.1:8790"
        );
        assert!(check_base("http://localhost:1").is_ok());
        assert!(check_base("http://p1.localhost:1").is_ok());
        assert!(check_base("https://accounts.example.com").is_err());
        assert!(check_base("http://example.com").is_err());
        assert!(check_base("http://127.0.0.1.example.com").is_err());
        assert!(check_base("ftp://127.0.0.1").is_err());
    }

    #[test]
    fn the_summary_lines_name_failures_changes_and_the_pack() {
        let o = Outcome {
            shots: 10,
            failed: vec![rec(
                "home",
                "phone",
                "light",
                &["sideways-scroll: wide"],
                Change::Same,
            )],
            changed: 2,
            new: 1,
            secs: 3.0,
            notes: Vec::new(),
            report: Some("target/shakedown/sweep/report.html".into()),
            pack: Some("target/shakedown/review".into()),
            sheets: 4,
        };
        let l = lines(&o);
        assert!(l[0].contains("home@phone-light: sideways-scroll: wide"));
        assert!(l.iter().any(|x| x.starts_with("pixels: 2 changed")));
        assert!(l.iter().any(|x| x == "FAIL sweep 1 of 10 shots 3.0s"));
        assert!(
            l.iter()
                .any(|x| x.starts_with("pack: target/shakedown/review"))
        );
        let ok = lines(&Outcome {
            shots: 4,
            ..Outcome::default()
        });
        assert_eq!(ok, vec!["ok sweep 4 shots 0.0s"]);
    }
}
