//! Drawing screens in the pinned Chrome over CDP: one browser context per player (own cookies and storage, signed in
//! once; every device and theme of the player is a tab in it), a deterministic page, the fake world's `/api` answers, the settled screenshot
//! and the layout tree. Nothing here decides pass or fail; it returns pixels and findings.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
    SetFocusEmulationEnabledParams, SetLocaleOverrideParams, SetTimezoneOverrideParams,
    SetTouchEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, ContinueRequestParams, EventRequestPaused, FailRequestParams, FulfillRequestParams,
    HeaderEntry, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::log::{EventEntryAdded, LogEntryLevel};
use chromiumoxide::cdp::browser_protocol::network::{
    self, ErrorReason, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
    ResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    ConsoleApiCalledType, EvaluateParams, EventConsoleApiCalled, EventExceptionThrown,
};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt as _;
use tokio::sync::Semaphore;

use crate::config::{Action, ScrollTo, World};
use crate::devices::Device;
use crate::error::Error;
use crate::guard::{self, Guard};
use crate::layout::{self, Finding, Tree};

/// The fake world's answers arrive after this long, as on a phone, so content that lands after the first frame
/// is seen moving things (a real link is slower, never faster).
const API_DELAY: Duration = Duration::from_millis(150);
/// Browsers one run starts when `SHAKEDOWN_BROWSERS` is not set: one per this many pages drawn at once.
const PAGES_PER_BROWSER: usize = 2;
/// Pages drawn at once when `SHAKEDOWN_PARALLEL` is not set: one per core, at most this many.
const PARALLEL_MAX: usize = 16;
/// A shot that takes longer than this is a failure.
const SHOT_TIMEOUT: Duration = Duration::from_secs(60);
/// Stops animation, transitions and the caret: references must not depend on when the capture happened.
const FREEZE: &str = "*,*::before,*::after{animation:none!important;transition:none!important;caret-color:transparent!important;scroll-behavior:auto!important}";
/// The seed of the page's random numbers.
/// Identical captures, this far apart, that make a shot settled.
const STABLE_MATCHES: u32 = 1;
const SEED: u32 = 0x02f6_e2b1;

/// One answer in a fake world.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// HTTP status.
    pub status: u16,
    /// Body, JSON, with `"$now"` already replaced.
    pub body: String,
}

/// A screen ready to draw.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Screen name.
    pub name: String,
    /// Path to open.
    pub path: String,
    /// `guest` or an account.
    pub player: String,
    /// Whether the shot takes one of the player's turns (see `[players].parallel`).
    pub turn: bool,
    /// `"METHOD /path"` to its answer.
    pub routes: HashMap<String, Fixture>,
    /// Steps before the shot.
    pub act: Vec<Action>,
    /// Console messages that are expected.
    pub allow: Vec<String>,
}

/// What every shot shares.
pub struct Env {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    /// `[server].api`.
    pub api: String,
    /// Fake or seeded.
    pub world: World,
    /// The page clock, epoch ms.
    pub now_ms: i64,
    /// Sign-in path with `{player}`.
    pub signin: Option<String>,
    /// `[layout].ignore` as JSON for the collector.
    pub ignores: String,
    /// `[players].parallel`.
    pub player_limit: usize,
}

/// One screen on one device in one theme.
#[derive(Debug, Clone)]
pub struct Job {
    /// The screen.
    pub plan: Arc<Plan>,
    /// The device.
    pub device: Device,
    /// `light` or `dark`.
    pub theme: String,
}

impl Job {
    /// `<screen>@<device>-<theme>`.
    pub fn id(&self) -> String {
        format!("{}@{}-{}", self.plan.name, self.device.name, self.theme)
    }
}

/// A finished shot.
#[derive(Debug, Clone)]
pub struct Shot {
    /// `<screen>@<device>-<theme>`.
    pub id: String,
    /// The settled screenshot (empty if the page could not be drawn).
    pub png: Vec<u8>,
    /// Everything wrong with it, sorted.
    pub findings: Vec<Finding>,
    /// Viewport size.
    pub size: (u32, u32),
    /// Why the first try failed, when the retry drew it: a flaky shot, always reported.
    pub flaky: Option<String>,
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A flat 64x64 gray image: every off-origin image (an avatar on a CDN) is answered with it.
fn avatar() -> Vec<u8> {
    let px: Vec<u8> = (0..64 * 64).flat_map(|_| [200_u8, 200, 200, 255]).collect();
    crate::pixels::encode(&crate::pixels::Image { w: 64, h: 64, px }, 1).unwrap_or_default()
}

/// The Chromes of one run and their temp profiles. Dropping it (an error, a panic, a failed launch of a later
/// Chrome, a future dropped on a caught signal) kills every browser launched so far (chromiumoxide kills the child
/// on drop), stops the handlers and removes the profiles.
#[derive(Default)]
pub struct Chromes {
    browsers: Vec<Arc<Browser>>,
    handlers: Vec<tokio::task::JoinHandle<()>>,
    profiles: Vec<std::path::PathBuf>,
}

impl Chromes {
    /// Launches one Chrome with `profile` and keeps it under this guard.
    ///
    /// # Errors
    ///
    /// Chrome is missing or does not start.
    pub async fn launch(
        &mut self,
        chrome: &std::path::Path,
        profile: &std::path::Path,
    ) -> Result<Arc<Browser>, Error> {
        // Recorded before the launch: a failed launch may already have made the profile.
        self.profiles.push(profile.to_path_buf());
        let (browser, handler) = launch(chrome, profile).await?;
        let browser = Arc::new(browser);
        self.browsers.push(browser.clone());
        self.handlers.push(handler);
        Ok(browser)
    }

    /// Every browser launched so far.
    pub fn browsers(&self) -> Vec<Arc<Browser>> {
        self.browsers.clone()
    }

    /// Hands the browsers over (to be closed), leaving the profiles and handlers to the drop.
    pub fn take_browsers(&mut self) -> Vec<Arc<Browser>> {
        std::mem::take(&mut self.browsers)
    }
}

impl Drop for Chromes {
    fn drop(&mut self) {
        for h in &self.handlers {
            h.abort();
        }
        self.browsers.clear();
        for p in &self.profiles {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// The dead proxy (port 9 is discard; nothing listens) every browser context and the browser itself use.
const PROXY: &str = "127.0.0.1:9";

/// Launches the pinned Chrome: new headless mode, one profile directory per run.
///
/// # Errors
///
/// Chrome is missing or does not start.
pub async fn launch(
    chrome: &std::path::Path,
    profile: &std::path::Path,
) -> Result<(Browser, tokio::task::JoinHandle<()>), Error> {
    let _ = std::fs::remove_dir_all(profile);
    let proxy_flag = format!("proxy-server={PROXY}");
    let config = BrowserConfig::builder()
        .chrome_executable(chrome)
        .new_headless_mode()
        .user_data_dir(profile)
        .viewport(None)
        .request_timeout(Duration::from_secs(30))
        .launch_timeout(Duration::from_secs(30))
        // `chromiumoxide` writes `--` itself, so every key here has no dashes: `"--x"` would reach Chrome as
        // `----x`, which Chrome ignores. `hide-scrollbars`, `mute-audio` and `force-color-profile=srgb` are
        // already in its headless and default sets. Chrome reads only the last `--disable-features`, so every
        // feature is one entry: `chromiumoxide` merges same-key values into one flag.
        .args([
            "font-render-hinting=none".to_string(),
            "disable-lcd-text".to_string(),
            // Nothing leaves the machine, not even a WebSocket (CDP cannot refuse one before it connects): every
            // non-loopback request goes to a proxy nobody listens on (port 9, discard) and fails there. Chrome's
            // implicit `<-loopback>` bypass stays, so the local server is reached directly; do not pass
            // `--proxy-bypass-list`, which would change that.
            proxy_flag,
            // Same pixels every run: software raster, images decoded before the frame, every compositor stage
            // finished before it is drawn.
            "disable-gpu".to_string(),
            "disable-gpu-compositing".to_string(),
            "disable-checker-imaging".to_string(),
            "run-all-compositor-stages-before-draw".to_string(),
            "disable-partial-raster".to_string(),
            "num-raster-threads=1".to_string(),
            "disable-oop-rasterization".to_string(),
            "disable-gpu-rasterization".to_string(),
            "disable-skia-runtime-opts".to_string(),
            // Sandboxed frames stay in their parent's process: Chrome would otherwise move a sandboxed srcdoc
            // frame to its own process after its first document started, past any script the page session adds.
        ])
        .arg((
            "disable-features",
            &["Vulkan", "SkiaGraphite", "IsolateSandboxedIframes"][..],
        ))
        .build()
        .map_err(Error::Environment)?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| Error::Environment(format!("cannot start Chrome: {e}")))?;
    let task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    Ok((browser, task))
}

/// Draws every job, one per core (`SHAKEDOWN_PARALLEL` overrides) at a time. Results come back sorted by id.
pub fn parallel() -> usize {
    std::env::var("SHAKEDOWN_PARALLEL")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(8, |n| n.get().clamp(2, PARALLEL_MAX))
        })
}

/// How many browsers to start.
pub fn browsers() -> usize {
    std::env::var("SHAKEDOWN_BROWSERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| parallel().div_ceil(PAGES_PER_BROWSER))
}

/// Called with each shot the moment it is finished (on its own thread, after its slot is free).
pub type Sink = Arc<dyn Fn(&Shot) + Send + Sync>;

#[expect(clippy::too_many_lines, reason = "one fan-out, read top to bottom")]
pub async fn shoot_all(
    browsers: Vec<Arc<Browser>>,
    env: Arc<Env>,
    jobs: Vec<Job>,
    sink: Option<Sink>,
) -> Vec<Shot> {
    let mut jobs = jobs;
    if let Some(seed) = std::env::var("SHAKEDOWN_SHUFFLE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        shuffle(&mut jobs, seed);
    }
    let parallel = parallel();
    let gate = Arc::new(Semaphore::new(parallel));
    // Each browser has its own connection that arms every frame, popup and worker (see `guard`).
    let mut guards = Vec::new();
    for browser in &browsers {
        match guard::start(browser.websocket_address()).await {
            Ok(g) => guards.push(Arc::new(g)),
            Err(why) => {
                let mut shots: Vec<Shot> = jobs
                    .iter()
                    .map(|j| Shot {
                        id: j.id(),
                        png: Vec::new(),
                        findings: vec![Finding::new("page-failed", why.clone())],
                        size: (j.device.width, j.device.height),
                        flaky: None,
                    })
                    .collect();
                shots.sort_by(|a, b| a.id.cmp(&b.id));
                return shots;
            }
        }
    }
    let pools: Vec<Arc<Players>> = browsers.iter().map(|_| Arc::default()).collect();
    // A signed-in player is in at most `player_limit` shots at once: servers cap a person's open connections.
    let players: HashMap<String, Arc<Semaphore>> = jobs
        .iter()
        .filter(|j| j.plan.player != "guest" && j.plan.turn)
        .map(|j| {
            (
                j.plan.player.clone(),
                Arc::new(Semaphore::new(env.player_limit)),
            )
        })
        .collect();
    let mut tasks = Vec::new();
    for (n, job) in jobs.into_iter().enumerate() {
        // Jobs are dealt out to the browsers in turn: one browser's main thread and connection do not carry them all.
        let at = n % browsers.len();
        let (browser, env, gate, guard, pool) = (
            browsers[at].clone(),
            env.clone(),
            gate.clone(),
            guards[at].clone(),
            pools[at].clone(),
        );
        let mine = if job.plan.turn {
            players.get(&job.plan.player).cloned()
        } else {
            None
        };
        let sink = sink.clone();
        tasks.push(tokio::spawn(async move {
            // The player's turn first, then a place among the pages open at once: a shot waiting for its player's
            // turn holds no place, so the places go to shots that can run. Both are kept across the retry.
            let turn = match &mine {
                Some(t) => t.clone().acquire_owned().await.ok(),
                None => None,
            };
            let permit = gate.acquire().await;
            let size = (job.device.width, job.device.height);
            let id = job.id();
            // The browser, not the page, failed (a command timed out on a starved machine): one more try.
            // A page that fails twice is a failure.
            // A pass on the retry is still reported (`Shot::flaky`): a retry never hides a flake.
            let mut last = String::new();
            let mut first_failure: Option<String> = None;
            let mut done = None;
            for _ in 0..2 {
                match shoot(&browser, &guard, &pool, &env, &job).await {
                    Ok(mut shot) => {
                        shot.flaky = first_failure;
                        done = Some(shot);
                        break;
                    }
                    Err(why) => last = why,
                }
                first_failure.get_or_insert_with(|| last.clone());
            }
            let shot = done.unwrap_or_else(|| Shot {
                id,
                png: Vec::new(),
                findings: vec![Finding::new("page-failed", last)],
                size,
                flaky: None,
            });
            // The slot and the turn are free for the next shot before this one is judged.
            drop(permit);
            drop(turn);
            if let Some(sink) = sink {
                let shot = Arc::new(shot);
                let seen = shot.clone();
                let _ = tokio::task::spawn_blocking(move || sink(&seen)).await;
                return Arc::try_unwrap(shot).unwrap_or_else(|s| (*s).clone());
            }
            shot
        }));
    }
    let mut shots = Vec::new();
    for t in tasks {
        if let Ok(shot) = t.await {
            shots.push(shot);
        }
    }
    shots.sort_by(|a, b| a.id.cmp(&b.id));
    shots
}

fn err<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Evaluates an expression (promises awaited) and returns its JSON value.
async fn eval(page: &Page, expression: &str) -> Result<serde_json::Value, String> {
    let params = EvaluateParams::builder()
        .expression(expression)
        .await_promise(true)
        .return_by_value(true)
        .build()
        .map_err(|e| e.clone())?;
    let out = page.execute(params).await.map_err(err("evaluate"))?;
    if let Some(ex) = out.result.exception_details {
        let text = ex.exception.and_then(|e| e.description).unwrap_or(ex.text);
        return Err(format!("page script: {text}"));
    }
    Ok(out.result.result.value.unwrap_or(serde_json::Value::Null))
}

/// One player's place in one browser: a single browser context for the whole run (the player's cookies and storage
/// build up and are never wiped between shots), signed in once. Every device and theme of the player is a tab in it,
/// with its own emulation. A context ends with the browser at the end of the run.
#[derive(Default)]
struct PlayerSlot {
    ctx: tokio::sync::OnceCell<BrowserContextId>,
    signed_in: tokio::sync::OnceCell<()>,
}

/// A browser's players.
#[derive(Default)]
struct Players {
    slots: Mutex<HashMap<String, Arc<PlayerSlot>>>,
}

impl Players {
    fn slot(&self, player: &str) -> Arc<PlayerSlot> {
        match self.slots.lock() {
            Ok(mut m) => m.entry(player.to_string()).or_default().clone(),
            Err(_) => Arc::default(),
        }
    }
}

/// `items` in an order made by `seed` (the same seed, the same order): `SHAKEDOWN_SHUFFLE=<seed>` draws the shots in
/// another order, to show that no shot depends on what ran before it.
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    for i in (1..items.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = usize::try_from(x % (i as u64 + 1)).unwrap_or(0);
        items.swap(i, j);
    }
}

async fn shoot(
    browser: &Browser,
    guard: &Guard,
    players: &Players,
    env: &Env,
    job: &Job,
) -> Result<Shot, String> {
    let t0 = std::time::Instant::now();
    let lap = |what: &str| {
        if std::env::var_os("SHAKEDOWN_TRACE").is_some() {
            eprintln!("trace {} {what} {}ms", job.id(), t0.elapsed().as_millis());
        }
    };
    let slow = || format!("took longer than {}s", SHOT_TIMEOUT.as_secs());
    let mine = players.slot(&job.plan.player);
    let ctx = mine
        .ctx
        .get_or_try_init(|| async {
            browser
                .create_browser_context(CreateBrowserContextParams {
                    // A context does not take the command-line proxy: it needs its own dead one (see `PROXY`).
                    proxy_server: Some(PROXY.into()),
                    ..CreateBrowserContextParams::default()
                })
                .await
                .map_err(err("context"))
        })
        .await?
        .clone();
    let collected = Arc::new(Collected::default());
    let prepared = tokio::time::timeout(SHOT_TIMEOUT, async {
        let target = CreateTargetParams::builder()
            .url("about:blank")
            .browser_context_id(ctx.clone())
            .build()
            .map_err(|e| e.clone())?;
        let page = browser.new_page(target).await.map_err(err("new page"))?;
        let to = collected.clone();
        guard.register(
            page.target_id().as_ref(),
            Arc::new(move |rule: &str, message: &str| to.note(rule, message)),
        );
        // Tabs share a window: a tab behind the others would draw no frames. Every tab is told it is in front.
        page.execute(SetFocusEmulationEnabledParams::new(true))
            .await
            .map_err(err("focus"))?;
        // The page is not drawn before the guard is on it (it waits at the debugger until then).
        match guard.armed(page.target_id().as_ref()).await {
            Ok(()) => Ok(page),
            Err(why) => {
                let _ = page.close().await;
                Err(why)
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err(slow()));
    lap("page ready");
    match prepared {
        Ok(page) => {
            let outcome = tokio::time::timeout(
                SHOT_TIMEOUT,
                draw(&page, env, job, &collected, &mine.signed_in),
            )
            .await
            .unwrap_or_else(|_| Err(slow()));
            lap("drawn");
            guard.unregister(page.target_id().as_ref());
            let _ = page.close().await;
            outcome
        }
        Err(why) => Err(why),
    }
}

/// What the page's event streams collect.
#[derive(Default)]
struct Collected {
    findings: Mutex<Vec<Finding>>,
    pending: Mutex<HashSet<String>>,
    served: Mutex<BTreeSet<String>>,
    /// Request id to a short description, for the requests in flight.
    urls: Mutex<std::collections::HashMap<String, String>>,
    /// Requests whose end was heard before their start (the two come on different channels).
    ended_early: Mutex<HashSet<String>>,
}

impl Collected {
    fn started(&self, id: &str, what: String) {
        if self.ended_early.lock().is_ok_and(|mut e| e.remove(id)) {
            return;
        }
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id.to_string());
        }
        if let Ok(mut u) = self.urls.lock() {
            u.insert(id.to_string(), what);
        }
    }

    fn ended(&self, id: &str) {
        let was_pending = self.pending.lock().is_ok_and(|mut p| p.remove(id));
        if let Ok(mut u) = self.urls.lock() {
            u.remove(id);
        }
        if !was_pending && let Ok(mut e) = self.ended_early.lock() {
            e.insert(id.to_string());
        }
    }
}

impl Collected {
    fn note(&self, rule: &str, message: impl Into<String>) {
        if let Ok(mut f) = self.findings.lock() {
            let finding = Finding::new(rule, message);
            if !f.contains(&finding) {
                f.push(finding);
            }
        }
    }
    fn pending(&self) -> usize {
        self.pending.lock().map_or(0, |p| p.len())
    }
}

#[expect(clippy::too_many_lines, reason = "one page flow, read top to bottom")]
async fn draw(
    page: &Page,
    env: &Env,
    job: &Job,
    collected: &Arc<Collected>,
    signed_in: &tokio::sync::OnceCell<()>,
) -> Result<Shot, String> {
    let t0 = std::time::Instant::now();
    let lap = |what: &str| {
        if std::env::var_os("SHAKEDOWN_TRACE").is_some() {
            eprintln!("trace {} {what} {}ms", job.id(), t0.elapsed().as_millis());
        }
    };
    let d = &job.device;
    // The environment: size, density 1, touch, theme, motion, timezone, locale.
    page.execute(
        SetDeviceMetricsOverrideParams::builder()
            .width(i64::from(d.width))
            .height(i64::from(d.height))
            .device_scale_factor(1.0)
            .mobile(d.mobile)
            .build()
            .map_err(|e| e.clone())?,
    )
    .await
    .map_err(err("device"))?;
    page.execute(SetTouchEmulationEnabledParams::new(d.mobile))
        .await
        .map_err(err("touch"))?;
    page.execute(SetEmulatedMediaParams {
        media: None,
        features: Some(vec![
            MediaFeature::new("prefers-color-scheme", job.theme.as_str()),
            MediaFeature::new("prefers-reduced-motion", "reduce"),
        ]),
    })
    .await
    .map_err(err("media"))?;
    page.execute(SetTimezoneOverrideParams::new("UTC"))
        .await
        .map_err(err("timezone"))?;
    page.execute(SetLocaleOverrideParams {
        locale: Some("en-US".into()),
    })
    .await
    .map_err(err("locale"))?;

    lap("emulation set");
    // The deterministic page and the collector, before any page script.
    let prelude = include_str!("collector.js")
        .replace("__AT__", &env.now_ms.to_string())
        .replace("__SEED__", &SEED.to_string())
        .replace("__FIXED__", "true");
    page.evaluate_on_new_document(prelude)
        .await
        .map_err(err("collector"))?;

    // Listeners first, then switch the domains on.
    watch_console(page, collected, &job.plan.allow).await?;
    watch_requests(page, collected, &env.base).await?;
    intercept(page, env, &job.plan, collected).await?;
    page.enable_log().await.map_err(err("log"))?;
    page.enable_runtime().await.map_err(err("runtime"))?;

    lap("listeners on");
    if job.plan.player != "guest" {
        let Some(template) = &env.signin else {
            return Err("no [players].signin".into());
        };
        let path = template.replace("{player}", &job.plan.player);
        // Signed in once per player and browser, by the first shot to get here; the others wait for it.
        signed_in
            .get_or_try_init(|| async {
                page.goto(format!("{}{path}", env.base))
                    .await
                    .map_err(err("sign-in"))?;
                wait_loaded(page).await
            })
            .await?;
    }
    page.goto(format!("{}{}", env.base, job.plan.path))
        .await
        .map_err(err("navigation"))?;
    wait_loaded(page).await?;

    lap("loaded");
    // Ready: every call the screen's world answers has been made and drawn.
    let want: Vec<&String> = job.plan.routes.keys().collect();
    let mut waited = 0;
    while want
        .iter()
        .any(|k| !collected.served.lock().is_ok_and(|s| s.contains(*k)))
    {
        if waited > 3000 {
            let missing: Vec<&str> = want
                .iter()
                .filter(|k| !collected.served.lock().is_ok_and(|s| s.contains(**k)))
                .map(|k| k.as_str())
                .collect();
            collected.note("never-called", missing.join(", "));
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 20;
    }
    eval(page, "document.fonts.ready.then(() => true)").await?;
    idle(page, collected).await?;
    lap("idle");
    // Layout shift: nothing may move once it appears. Taken before the actions, which move things on purpose.
    let shifts = eval(page, "JSON.stringify(window.__shakedown.shifts)").await?;
    let before_acts: Vec<layout::Shift> =
        serde_json::from_str(shifts.as_str().unwrap_or("[]")).unwrap_or_default();

    // A constructable stylesheet, not a <style> element: a page's own CSP (style-src 'self') would block that.
    eval(
        page,
        &format!(
            "(() => {{ const s = new CSSStyleSheet(); s.replaceSync({}); document.adoptedStyleSheets = [...document.adoptedStyleSheets, s]; return true; }})()",
            serde_json::Value::String(FREEZE.into())
        ),
    )
    .await?;

    for action in &job.plan.act {
        if let Err(why) = act(page, action).await {
            collected.note("action-failed", why);
            break;
        }
    }
    idle(page, collected).await?;

    lap("acted");
    // Settled means several captures in a row, a moment apart, are byte-identical (images decoded and rescaled at
    // their final quality, layers composited): Chrome upgrades a scaled image a little after its first draw.
    let grab = || async {
        // `optimize_for_speed`: Chrome spends most of a capture compressing the PNG; the pixels are the same, and
        // everything downstream decodes the PNG (the reference files are re-encoded by `pixels::encode`).
        page.screenshot(ScreenshotParams::from(
            CaptureScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .optimize_for_speed(true)
                .build(),
        ))
        .await
        .map_err(err("screenshot"))
    };
    let mut png = grab().await?;
    lap("first grab");
    let mut same = 0;
    for i in 0..=30 {
        // Event-driven: every image decoded, then two frames (the compositor has drawn what the decode finished),
        // instead of a fixed sleep. Each round starts the moment the last one ends.
        eval(page, "Promise.all([...document.images].map((im) => im.decode().catch(() => {}))).then(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(true)))))").await?;
        let again = grab().await?;
        if again == png {
            same += 1;
            if same >= STABLE_MATCHES {
                break;
            }
        } else {
            same = 0;
            png = again;
        }
        if i == 30 {
            // An Err, not a finding: the retry draws the shot again, and a pass there is reported as flaky.
            return Err("never-settled: screenshots keep changing".into());
        }
    }
    lap("settled");
    let raw = eval(
        page,
        &format!("JSON.stringify(window.__shakedown.tree({}))", env.ignores),
    )
    .await?;
    let mut tree: Tree =
        serde_json::from_str(raw.as_str().unwrap_or("{}")).map_err(err("layout tree"))?;
    // Actions move things on purpose, so a screen with actions is judged on the shifts before them; without
    // actions everything up to the settled shot counts (late content that arrives after the first idle too).
    if !job.plan.act.is_empty() {
        tree.shifts = before_acts;
    }
    let mut findings = layout::check(&tree);
    if let Ok(f) = collected.findings.lock() {
        findings.extend(f.iter().cloned());
    }
    findings.sort_by(|a, b| (&a.rule, &a.message).cmp(&(&b.rule, &b.message)));
    Ok(Shot {
        id: job.id(),
        png,
        findings,
        size: (d.width, d.height),
        flaky: None,
    })
}

async fn wait_loaded(page: &Page) -> Result<(), String> {
    for _ in 0..500 {
        if eval(page, "document.readyState").await?.as_str() == Some("complete") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err("the page never finished loading".into())
}

/// No request in flight for 100 ms, then two frames, so late fetches and the renderer's flush have landed.
async fn idle(page: &Page, c: &Collected) -> Result<(), String> {
    let mut quiet = 0;
    for tick in 0..500 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Chrome occasionally never reports the end of a request the page has long finished with. Once a second,
        // forget requests the page itself says are complete (its resource timing), so a lost event is not a hang.
        if tick % 50 == 49 && c.pending() > 0 {
            heal(page, c).await;
        }
        quiet = if c.pending() == 0 { quiet + 20 } else { 0 };
        if quiet >= 100 {
            break;
        }
    }
    if quiet < 100 {
        let mut open: Vec<String> = c
            .urls
            .lock()
            .map_or_else(|_| Vec::new(), |u| u.values().cloned().collect());
        open.sort();
        // An Err, not a finding: the retry draws the shot again, and a pass there is reported as flaky.
        return Err(format!(
            "never-settled: requests keep running: {}",
            open.join(", ")
        ));
    }
    eval(
        page,
        "new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))",
    )
    .await?;
    Ok(())
}

async fn heal(page: &Page, c: &Collected) {
    let Ok(done) = eval(
        page,
        "JSON.stringify(performance.getEntriesByType('resource').filter((e) => e.responseEnd > 0).map((e) => e.name))",
    )
    .await
    else {
        return;
    };
    let done: Vec<String> = serde_json::from_str(done.as_str().unwrap_or("[]")).unwrap_or_default();
    let stuck: Vec<String> = c.urls.lock().map_or_else(
        |_| Vec::new(),
        |u| {
            u.iter()
                .filter(|(_, what)| {
                    let path = what.split_once(' ').map_or("", |(_, p)| p);
                    done.iter().any(|d| d.ends_with(path))
                })
                .map(|(id, _)| id.clone())
                .collect()
        },
    );
    for id in stuck {
        c.ended(&id);
    }
}

async fn watch_console(page: &Page, c: &Arc<Collected>, allow: &[String]) -> Result<(), String> {
    let allowed = |text: &str| allow.iter().any(|a| text.contains(a.as_str()));
    let allow: Vec<String> = allow.to_vec();
    let _ = allowed;
    let mut api = page
        .event_listener::<EventConsoleApiCalled>()
        .await
        .map_err(err("console"))?;
    let mut exc = page
        .event_listener::<EventExceptionThrown>()
        .await
        .map_err(err("exceptions"))?;
    let mut log = page
        .event_listener::<EventEntryAdded>()
        .await
        .map_err(err("log"))?;
    let ok = move |text: &str| !allow.iter().any(|a| text.contains(a.as_str()));
    let (c1, c2, c3) = (c.clone(), c.clone(), c.clone());
    let (ok1, ok2, ok3) = (ok.clone(), ok.clone(), ok);
    tokio::spawn(async move {
        while let Some(e) = api.next().await {
            if matches!(
                e.r#type,
                ConsoleApiCalledType::Error | ConsoleApiCalledType::Assert
            ) {
                let text: Vec<String> = e
                    .args
                    .iter()
                    .map(|a| {
                        a.value
                            .as_ref()
                            .map(|v| v.as_str().map_or_else(|| v.to_string(), String::from))
                            .or_else(|| a.description.clone())
                            .unwrap_or_default()
                    })
                    .collect();
                let text = text.join(" ");
                if ok1(&text) {
                    c1.note("console-error", text);
                }
            }
        }
    });
    tokio::spawn(async move {
        while let Some(e) = exc.next().await {
            let d = &e.exception_details;
            let text = d
                .exception
                .as_ref()
                .and_then(|x| x.description.clone())
                .unwrap_or_else(|| d.text.clone());
            let first = text.lines().next().unwrap_or_default().to_string();
            if ok2(&first) {
                c2.note("page-error", first);
            }
        }
    });
    tokio::spawn(async move {
        while let Some(e) = log.next().await {
            // New headless Chrome asks for /favicon.ico by itself (a browser fetch, not the page's); a page that
            // wants an icon links one, and that request is checked like any other.
            let favicon = e
                .entry
                .url
                .as_deref()
                .is_some_and(|u| strip_origin(u) == "/favicon.ico");
            let whole = format!(
                "{} {}",
                e.entry.text,
                e.entry
                    .url
                    .as_deref()
                    .map_or_else(String::new, strip_origin)
            );
            if e.entry.level == LogEntryLevel::Error && !favicon && ok3(&whole) {
                let url = e
                    .entry
                    .url
                    .as_deref()
                    .map_or_else(String::new, |u| format!(" ({})", strip_origin(u)));
                c3.note("console-error", format!("{}{url}", e.entry.text));
            }
        }
    });
    Ok(())
}

/// Chrome refuses to fulfil a request without a reason phrase for codes it does not know.
fn phrase(status: u16) -> &'static str {
    if status < 400 { "OK" } else { "Error" }
}

fn query_of(u: &url::Url) -> String {
    u.query().map_or_else(String::new, |q| format!("?{q}"))
}

/// Messages name URLs; the port changes every run, so it never goes in a message.
fn strip_origin(u: &str) -> String {
    url::Url::parse(u).map_or_else(
        |_| u.to_string(),
        |p| format!("{}{}", p.path(), query_of(&p)),
    )
}

/// Is `u` (a `ws`/`wss` URL) on the local server (`base` is `http://127.0.0.1:<port>`) under any loopback name
/// (`localhost`, `127.0.0.1`, `[::1]`)? Another port, even on loopback, is not the page's world.
fn on_the_local_server(u: &url::Url, base: &url::Url) -> bool {
    let loopback = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(a)) => a.is_loopback(),
        Some(url::Host::Ipv6(a)) => a.is_loopback(),
        None => false,
    };
    matches!(u.scheme(), "ws" | "wss")
        && loopback
        && u.port_or_known_default() == base.port_or_known_default()
}

/// The finding for a WebSocket URL that leaves the machine; `None` for one on the local server.
fn socket_leaves(ws_url: &str, base: &str) -> Option<String> {
    let b = url::Url::parse(base).ok()?;
    match url::Url::parse(ws_url) {
        Ok(u) if on_the_local_server(&u, &b) => None,
        Ok(u) => Some(format!(
            "request leaves the machine: {}{}",
            u.origin().ascii_serialization(),
            u.path()
        )),
        Err(_) => Some(format!("request leaves the machine: {ws_url}")),
    }
}

/// Which requests are in flight, so the page is known to be idle. Streams that never end do not count.
async fn watch_requests(page: &Page, c: &Arc<Collected>, base: &str) -> Result<(), String> {
    page.execute(network::EnableParams::default())
        .await
        .map_err(err("network"))?;
    let mut sent = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .map_err(err("network"))?;
    let mut done = page
        .event_listener::<EventLoadingFinished>()
        .await
        .map_err(err("network"))?;
    let mut failed = page
        .event_listener::<EventLoadingFailed>()
        .await
        .map_err(err("network"))?;
    let mut cached = page
        .event_listener::<network::EventRequestServedFromCache>()
        .await
        .map_err(err("network"))?;
    // Fetch never pauses WebSockets, and `Network.setBlockedURLs` does not stop them either (tried with allow
    // and block patterns: the socket still connects), so a socket cannot be refused before it connects: one
    // opened off this origin fails the screen here instead.
    let mut sockets = page
        .event_listener::<network::EventWebSocketCreated>()
        .await
        .map_err(err("network"))?;
    let (cs, base) = (c.clone(), base.to_string());
    tokio::spawn(async move {
        while let Some(e) = sockets.next().await {
            if let Some(what) = socket_leaves(&e.url, &base) {
                cs.note("network", what);
            }
        }
    });
    let c4 = c.clone();
    tokio::spawn(async move {
        while let Some(e) = cached.next().await {
            c4.ended(e.request_id.inner());
        }
    });
    let (c1, c2, c3) = (c.clone(), c.clone(), c.clone());
    tokio::spawn(async move {
        while let Some(e) = sent.next().await {
            let endless = matches!(
                e.r#type,
                Some(
                    ResourceType::EventSource
                        | ResourceType::WebSocket
                        | ResourceType::Ping
                        | ResourceType::Prefetch
                        | ResourceType::CspViolationReport
                )
            );
            // Only GETs count: a POST (a form, a time-zone ping, a beacon) is something the page does, not
            // something it is waiting for, and a keepalive one can outlive the page's idea of "done".
            if !endless && e.request.method == "GET" {
                c1.started(
                    e.request_id.inner(),
                    format!("{:?} {}", e.r#type, strip_origin(&e.request.url)),
                );
            }
        }
    });
    tokio::spawn(async move {
        while let Some(e) = done.next().await {
            c2.ended(e.request_id.inner());
        }
    });
    tokio::spawn(async move {
        while let Some(e) = failed.next().await {
            c3.ended(e.request_id.inner());
        }
    });
    Ok(())
}

/// Refuses a request that would leave the machine, and fails the screen for it.
async fn block(page: &Page, c: &Collected, id: fetch::RequestId, what: String) {
    c.note("network", format!("request leaves the machine: {what}"));
    let _ = page
        .execute(FailRequestParams::new(id, ErrorReason::BlockedByClient))
        .await;
}

/// The fake world, and the wall around every world: nothing leaves the machine.
async fn intercept(
    page: &Page,
    env: &Env,
    plan: &Arc<Plan>,
    c: &Arc<Collected>,
) -> Result<(), String> {
    let mut paused = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(err("intercept"))?;
    page.execute(
        fetch::EnableParams::builder()
            .pattern(RequestPattern::builder().url_pattern("*").build())
            .build(),
    )
    .await
    .map_err(err("intercept"))?;
    let (page, plan, c) = (page.clone(), plan.clone(), c.clone());
    let (base, api, world) = (env.base.clone(), env.api.clone(), env.world);
    let image = base64(&avatar());
    tokio::spawn(async move {
        while let Some(ev) = paused.next().await {
            let (page, plan, c, base, api, image) = (
                page.clone(),
                plan.clone(),
                c.clone(),
                base.clone(),
                api.clone(),
                image.clone(),
            );
            tokio::spawn(async move {
                let id = ev.request_id.clone();
                let Ok(u) = url::Url::parse(&ev.request.url) else {
                    block(&page, &c, id, "unparseable url".into()).await;
                    return;
                };
                let origin = u.origin().ascii_serialization();
                let json_headers = vec![HeaderEntry::new("Content-Type", "application/json")];
                if matches!(u.scheme(), "data" | "blob" | "about") {
                    // Made in the page itself: nothing travels.
                    let _ = page.execute(ContinueRequestParams::new(id)).await;
                } else if !matches!(u.scheme(), "http" | "https") {
                    // ftp:, file:, anything else a request could carry out: refused. (WebSockets never reach
                    // Fetch; `watch_requests` fails those.)
                    block(&page, &c, id, u.scheme().to_string()).await;
                } else if origin != base {
                    // An avatar on a CDN: one flat, fixed image stands in. Anything else off-origin is a failure.
                    // (a page icon is requested as type Other, with an image Accept header)
                    let wants_image = ev.resource_type == ResourceType::Image
                        || ev
                            .request
                            .headers
                            .inner()
                            .get("Accept")
                            .and_then(|v| v.as_str())
                            .is_some_and(|a| a.starts_with("image/"));
                    if wants_image {
                        let mut f = FulfillRequestParams::new(id, 200_i64);
                        f.response_headers =
                            Some(vec![HeaderEntry::new("Content-Type", "image/png")]);
                        f.body = Some(image.into());
                        let _ = page.execute(f).await;
                    } else {
                        block(&page, &c, id, format!("{origin}{}", u.path())).await;
                    }
                } else {
                    let key = format!("{} {}", ev.request.method, u.path());
                    let with_query = u.query().map(|q| format!("{key}?{q}"));
                    let hit = with_query
                        .as_ref()
                        .and_then(|k| plan.routes.get(k).map(|f| (k.clone(), f)))
                        .or_else(|| plan.routes.get(&key).map(|f| (key.clone(), f)));
                    if let Some((matched, fixture)) = hit {
                        // Answers arrive late, as on a phone.
                        tokio::time::sleep(API_DELAY).await;
                        if let Ok(mut s) = c.served.lock() {
                            s.insert(matched);
                        }
                        let mut f = FulfillRequestParams::new(id, i64::from(fixture.status));
                        f.response_phrase = Some(phrase(fixture.status).into());
                        f.response_headers = Some(json_headers);
                        f.body = Some(base64(fixture.body.as_bytes()).into());
                        let _ = page.execute(f).await;
                    } else if world == World::Fake && u.path().starts_with(api.as_str()) {
                        c.note("unfaked-call", format!("{key}{}", query_of(&u)));
                        let mut f = FulfillRequestParams::new(id, 599_i64);
                        f.response_phrase = Some("Unfaked".into());
                        f.response_headers = Some(json_headers);
                        f.body = Some(base64(br#"{"error":"unfaked"}"#).into());
                        let _ = page.execute(f).await;
                    } else {
                        let _ = page.execute(ContinueRequestParams::new(id)).await;
                    }
                }
            });
        }
    });
    Ok(())
}

const POLL: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(8);

async fn poll(page: &Page, expression: &str, what: &str) -> Result<serde_json::Value, String> {
    let mut waited = Duration::ZERO;
    loop {
        let v = eval(page, expression).await?;
        if !matches!(v, serde_json::Value::Null | serde_json::Value::Bool(false)) {
            return Ok(v);
        }
        if waited > WAIT {
            return Err(format!("timed out waiting for {what}"));
        }
        tokio::time::sleep(POLL).await;
        waited += POLL;
    }
}

fn js(s: &str) -> String {
    serde_json::Value::String(s.into()).to_string()
}

/// The centre of the first match, scrolled into view.
async fn centre(page: &Page, selector: &str) -> Result<(f64, f64), String> {
    let found = poll(
        page,
        &format!(
            "(() => {{ const e = document.querySelector({sel}); if (!e) return null; e.scrollIntoView({{ block: 'center', inline: 'center' }}); const r = e.getBoundingClientRect(); return r.width || r.height ? [r.left + r.width / 2, r.top + r.height / 2] : null; }})()",
            sel = js(selector)
        ),
        &format!("{selector} to appear"),
    )
    .await?;
    let xy = found.as_array().ok_or("bad position")?;
    Ok((xy[0].as_f64().unwrap_or(0.0), xy[1].as_f64().unwrap_or(0.0)))
}

async fn click_at(page: &Page, x: f64, y: f64) -> Result<(), String> {
    for kind in [
        DispatchMouseEventType::MouseMoved,
        DispatchMouseEventType::MousePressed,
        DispatchMouseEventType::MouseReleased,
    ] {
        let mut p = DispatchMouseEventParams::new(kind.clone(), x, y);
        if kind != DispatchMouseEventType::MouseMoved {
            p.button = Some(MouseButton::Left);
            p.click_count = Some(1);
        }
        page.execute(p).await.map_err(err("mouse"))?;
    }
    Ok(())
}

async fn act(page: &Page, action: &Action) -> Result<(), String> {
    match action {
        Action::Click(sel) => {
            let (x, y) = centre(page, sel).await?;
            click_at(page, x, y).await
        }
        Action::Type(t) => {
            let (x, y) = centre(page, &t.selector).await?;
            click_at(page, x, y).await?;
            page.execute(InsertTextParams::new(t.text.as_str()))
                .await
                .map_err(err("type"))?;
            Ok(())
        }
        Action::Press(key) => {
            let (code, text) = match key.as_str() {
                "Enter" => ("Enter", Some("\r")),
                "Tab" => ("Tab", None),
                "Escape" => ("Escape", None),
                other => (other, None),
            };
            for kind in [
                DispatchKeyEventType::RawKeyDown,
                DispatchKeyEventType::KeyUp,
            ] {
                let mut p = DispatchKeyEventParams::new(kind.clone());
                p.key = Some(key.clone());
                p.code = Some(code.to_string());
                if kind == DispatchKeyEventType::RawKeyDown {
                    p.text = text.map(String::from);
                }
                page.execute(p).await.map_err(err("key"))?;
            }
            Ok(())
        }
        Action::WaitText(text) => {
            poll(
                page,
                &format!("document.body.innerText.includes({})", js(text)),
                &format!("text {text:?}"),
            )
            .await?;
            Ok(())
        }
        Action::WaitGone(sel) => {
            poll(
                page,
                &format!("!document.querySelector({})", js(sel)),
                &format!("{sel} to go"),
            )
            .await?;
            Ok(())
        }
        Action::Scroll(ScrollTo::Selector(sel)) => {
            centre(page, sel).await?;
            Ok(())
        }
        Action::Scroll(ScrollTo::Pixels(px)) => {
            eval(page, &format!("window.scrollBy(0, {px}); true")).await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_makes_one_order_and_every_item_stays() {
        let base: Vec<u32> = (0..50).collect();
        let (mut a, mut b, mut c) = (base.clone(), base.clone(), base.clone());
        shuffle(&mut a, 1);
        shuffle(&mut b, 1);
        shuffle(&mut c, 2);
        assert_eq!(a, b, "the same seed, the same order");
        assert_ne!(a, c, "another seed, another order");
        assert_ne!(a, base);
        a.sort_unstable();
        assert_eq!(a, base, "nothing lost or doubled");
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn origins_never_reach_messages() {
        assert_eq!(strip_origin("http://127.0.0.1:5555/a.js?x=1"), "/a.js?x=1");
    }

    #[test]
    fn every_loopback_name_on_the_servers_port_is_on_the_machine() {
        let base = "http://127.0.0.1:5555";
        for url in [
            "ws://localhost:5555/live",
            "ws://127.0.0.1:5555/live",
            "ws://[::1]:5555/live",
            "wss://LOCALHOST:5555/live",
        ] {
            assert_eq!(socket_leaves(url, base), None, "{url}");
        }
        for url in [
            "wss://example.com/live",
            "ws://localhost:6666/live",
            "ws://192.168.1.5:5555/live",
            "ws://localhost.example.com:5555/live",
        ] {
            assert!(socket_leaves(url, base).is_some(), "{url}");
        }
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;

    /// Every intended flag reaches the real Chrome, spelled once with two dashes (a `----flag` is ignored).
    #[tokio::test]
    async fn every_launch_flag_reaches_chrome_well_formed() {
        if !crate::browser::installed() {
            eprintln!("pinned Chrome not installed: skipped");
            return;
        }
        let profile = std::env::temp_dir().join(format!("shakedown-flags-{}", std::process::id()));
        let (mut browser, task) = launch(&crate::browser::binary(), &profile).await.unwrap();
        let line = browser
            .execute(
                chromiumoxide::cdp::browser_protocol::browser::GetBrowserCommandLineParams::default(
                ),
            )
            .await
            .unwrap();
        let _ = browser.close().await;
        task.abort();
        let _ = std::fs::remove_dir_all(&profile);
        let args = line.result.arguments;
        for a in &args {
            assert!(!a.starts_with("---"), "malformed flag {a}");
        }
        let count = |flag: &str| args.iter().filter(|a| a.as_str() == flag).count();
        for flag in [
            "--font-render-hinting=none",
            "--disable-lcd-text",
            "--force-color-profile=srgb",
            "--hide-scrollbars",
            "--mute-audio",
            "--proxy-server=127.0.0.1:9",
            "--disable-gpu",
            "--disable-gpu-compositing",
            "--disable-checker-imaging",
            "--run-all-compositor-stages-before-draw",
            "--disable-partial-raster",
            "--num-raster-threads=1",
            "--disable-oop-rasterization",
            "--disable-gpu-rasterization",
            "--disable-skia-runtime-opts",
        ] {
            assert_eq!(count(flag), 1, "{flag} in {args:?}");
        }
        let features: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("--disable-features="))
            .collect();
        assert_eq!(features.len(), 1, "one --disable-features: {features:?}");
        for f in ["Vulkan", "SkiaGraphite", "IsolateSandboxedIframes"] {
            assert!(
                features[0].split(['=', ',']).any(|x| x == f),
                "{f} in {features:?}"
            );
        }
    }
}

/// The collector as `devices serve` runs it: inside an iframe (cross-origin and same-origin) of a tall parent page.
#[cfg(test)]
mod frame_tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    fn serve() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut c in listener.incoming().flatten() {
                // A thread per connection: Chrome opens spare connections that never send a request.
                std::thread::spawn(move || {
                    let mut buf = [0_u8; 4096];
                    let n = c.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                    let body = if path.starts_with("/child") {
                        // A tall page: real text far below the fold, a decorative icon painted over it.
                        format!(
                            "<!doctype html><meta charset=utf-8><body style=\"margin:0;height:3000px;font:16px sans-serif\">\
<div style=\"position:absolute;top:2000px;left:10px\">Real line of text</div>\
<div aria-hidden=\"true\" style=\"position:absolute;top:2003px;left:15px;z-index:2;pointer-events:none\">Icon label</div>\
<script>{}</script><script>addEventListener('load',()=>setTimeout(()=>parent.postMessage({{ frame: true, y: scrollY, x: scrollX }}, '*'), 1500));</script>",
                            crate::viewer::frame_js("[]")
                        )
                    } else {
                        let host = path.trim_start_matches("/parent/");
                        format!(
                            "<!doctype html><meta charset=utf-8><body style=\"margin:0;height:5000px\"><iframe style=\"position:absolute;top:1500px;left:0;width:400px;height:300px;border:0\" src=\"http://{host}:{port}/child\"></iframe>\
<script>window.__m=[];addEventListener('message',(e)=>window.__m.push(e.data));addEventListener('load',()=>scrollTo(0,1450));</script>"
                        )
                    };
                    let _ = write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        port
    }

    /// The viewer path: an app serves a page with a doctype and no `<head>`; the viewer proxy injects the collector; a frame loads it.
    #[tokio::test]
    async fn the_viewer_keeps_a_headless_doctype_page_in_standards_mode_and_catches_ariafold() {
        viewer_case(0, "", "<!doctype html>", "CSS1Compat").await;
    }

    /// Whatever starts the document, the appended collector leaves the document mode alone.
    #[tokio::test]
    async fn the_viewer_never_changes_the_document_mode_whatever_the_page_starts_with() {
        let cases = [
            ("\u{feff}", "<!doctype html>", "CSS1Compat"),
            ("<?xml version=\"1.0\"?>", "<!doctype html>", "CSS1Compat"),
            ("<!-->", "<!doctype html>", "CSS1Compat"),
            // Stray text before the doctype is quirks mode on its own; injecting must not change that either way.
            ("--!>", "<!doctype html>", "BackCompat"),
            ("<!-- a > b -->\n", "<!DOCTYPE HTML>", "CSS1Compat"),
            ("<!-- <!doctype html> -->", "", "BackCompat"),
            (
                "",
                "<!doctype html><script>var a = \"<html>\";</script>",
                "CSS1Compat",
            ),
        ];
        for (i, (prefix, doctype, mode)) in cases.into_iter().enumerate() {
            viewer_case(100 + u32::try_from(i).unwrap(), prefix, doctype, mode).await;
        }
    }

    /// Same, with the frame far below the fold at load time and nobody scrolling or reloading.
    #[tokio::test]
    async fn a_frame_that_starts_offscreen_still_reports_without_a_reload() {
        viewer_case(4000, "", "<!doctype html>", "CSS1Compat").await;
    }

    async fn viewer_case(top: u32, prefix: &'static str, doctype: &'static str, want: &str) {
        if !crate::browser::installed() {
            eprintln!("pinned Chrome not installed: skipped");
            return;
        }
        let app = TcpListener::bind("127.0.0.1:0").unwrap();
        let app_port = app.local_addr().unwrap().port();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = free.local_addr().unwrap().port();
        drop(free);
        std::thread::spawn(move || {
            for mut c in app.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0_u8; 4096];
                    let n = c.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                    let body = if path.starts_with("/parent") {
                        format!(
                            "<!doctype html><body style=\"height:6000px\"><iframe style=\"margin-top:{top}px;width:1000px;height:900px;border:0\" src=\"http://127.0.0.1:{proxy_port}/fold\"></iframe>\
<script>window.__m=[];addEventListener('message',(e)=>window.__m.push(e.data));</script>"
                        )
                    } else {
                        // The old ariafold fixture: a doctype, no head, an aria-hidden icon over text below the fold.
                        format!(
                            "{prefix}{doctype}<meta charset=utf-8><body style=\"margin:0;font:16px sans-serif;height:3000px\">\
<p style=\"position:absolute;top:2000px;left:50px;margin:0\">Score text</p>\
<span aria-hidden=\"true\" style=\"position:absolute;top:2000px;left:50px;z-index:2\">ICON</span>\
<script>addEventListener('load',()=>setTimeout(()=>parent.postMessage({{ mode: document.compatMode }}, '*'), 1500));</script>"
                        )
                    };
                    let _ = write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        let settings = crate::viewer::Settings {
            port: proxy_port,
            target: format!("127.0.0.1:{app_port}"),
            routes_file: None,
            config_routes: Vec::new(),
            players: None,
            ignores: "[]".into(),
        };
        std::thread::spawn(move || {
            let _ = crate::viewer::serve(&settings);
        });
        let profile = std::env::temp_dir().join(format!(
            "shakedown-frame-viewer-{}-{top}",
            std::process::id()
        ));
        let mut chromes = Chromes::default();
        let browser = chromes
            .launch(&crate::browser::binary(), &profile)
            .await
            .unwrap();
        let page = browser.new_page("about:blank").await.unwrap();
        page.goto(format!("http://localhost:{app_port}/parent"))
            .await
            .unwrap();
        let mut msgs = Vec::new();
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let v: Vec<serde_json::Value> = page
                .evaluate("JSON.stringify(window.__m || [])")
                .await
                .unwrap()
                .into_value::<String>()
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if v.iter().any(|e| e["mode"].is_string())
                && v.iter().any(|e| e["shakedown"] == "loaded")
            {
                msgs = v;
                break;
            }
        }
        assert!(!msgs.is_empty(), "the frame never reported");
        let mode = msgs.iter().find(|e| e["mode"].is_string()).unwrap();
        assert_eq!(
            mode["mode"], want,
            "the page must keep its document mode ({prefix:?})"
        );
        let loaded = msgs.iter().find(|e| e["shakedown"] == "loaded").unwrap();
        let nodes = loaded["tree"]["nodes"].as_array().unwrap();
        assert!(
            want != "CSS1Compat"
                || nodes.iter().any(|n| n["decor"] == true
                    && n["top_lines"].as_array().is_some_and(|l| !l.is_empty())),
            "ariafold must be caught through the viewer ({prefix:?}, {doctype:?}): {nodes:?}"
        );
    }

    /// The real viewer page with every device frame (most start below the fold): each must show a verdict without a Reload.
    #[tokio::test]
    async fn every_viewer_frame_reports_without_a_reload_even_below_the_fold() {
        if !crate::browser::installed() {
            eprintln!("pinned Chrome not installed: skipped");
            return;
        }
        let app = TcpListener::bind("127.0.0.1:0").unwrap();
        let app_port = app.local_addr().unwrap().port();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = free.local_addr().unwrap().port();
        drop(free);
        std::thread::spawn(move || {
            for mut c in app.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0_u8; 4096];
                    let _ = c.read(&mut buf);
                    let body = "<!doctype html><meta charset=utf-8><body style=\"margin:0;font:16px sans-serif\"><p>Hello</p>";
                    let _ = write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        let settings = crate::viewer::Settings {
            port: proxy_port,
            target: format!("127.0.0.1:{app_port}"),
            routes_file: None,
            config_routes: Vec::new(),
            players: None,
            ignores: "[]".into(),
        };
        std::thread::spawn(move || {
            let _ = crate::viewer::serve(&settings);
        });
        let profile =
            std::env::temp_dir().join(format!("shakedown-all-frames-{}", std::process::id()));
        let mut chromes = Chromes::default();
        let browser = chromes
            .launch(&crate::browser::binary(), &profile)
            .await
            .unwrap();
        let page = browser.new_page("about:blank").await.unwrap();
        page.goto(format!("http://localhost:{proxy_port}/__devices"))
            .await
            .unwrap();
        let mut last = String::new();
        let mut done = false;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            last = page
                .evaluate(
                    "JSON.stringify([...document.querySelectorAll('.st')].map(e => e.textContent))",
                )
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap_or_default();
            let v: Vec<String> = serde_json::from_str(&last).unwrap_or_default();
            if v.len() > 5 && v.iter().all(|t| !t.is_empty() && !t.contains("checking")) {
                done = true;
                break;
            }
        }
        assert!(done, "some frames never reported: {last}");
    }

    /// An app that drops every connection for some frames and hangs for another: the page must never say clean.
    #[tokio::test]
    #[expect(clippy::too_many_lines)]
    async fn frames_that_never_report_are_listed_and_the_summary_is_never_clean() {
        if !crate::browser::installed() {
            eprintln!("pinned Chrome not installed: skipped");
            return;
        }
        let app = TcpListener::bind("127.0.0.1:0").unwrap();
        let app_port = app.local_addr().unwrap().port();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = free.local_addr().unwrap().port();
        drop(free);
        std::thread::spawn(move || {
            for mut c in app.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0_u8; 4096];
                    let n = c.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    let host = head
                        .lines()
                        .find_map(|l| l.strip_prefix("host: "))
                        .unwrap_or("")
                        .to_string();
                    if host.starts_with("p3.") || host.starts_with("p7.") {
                        return; // dropped
                    }
                    if host.starts_with("p5.") {
                        std::thread::sleep(Duration::from_secs(120));
                        return; // never answers
                    }
                    let body = "<!doctype html><meta charset=utf-8><body style=\"margin:0;font:16px sans-serif\"><p>Hello</p>";
                    let _ = write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        let settings = crate::viewer::Settings {
            port: proxy_port,
            target: format!("127.0.0.1:{app_port}"),
            routes_file: None,
            config_routes: Vec::new(),
            players: None,
            ignores: "[]".into(),
        };
        std::thread::spawn(move || {
            let _ = crate::viewer::serve(&settings);
        });
        let profile =
            std::env::temp_dir().join(format!("shakedown-silent-frames-{}", std::process::id()));
        let mut chromes = Chromes::default();
        let browser = chromes
            .launch(&crate::browser::binary(), &profile)
            .await
            .unwrap();
        let page = browser.new_page("about:blank").await.unwrap();
        page.goto(format!(
            "http://localhost:{proxy_port}/__devices?timeout=6000"
        ))
        .await
        .unwrap();
        let mut last = String::new();
        let mut done = false;
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            last = page
                .evaluate(
                    "JSON.stringify({ st: [...document.querySelectorAll('.st')].map(e => e.textContent), report: document.querySelector('#report').textContent, retry: document.querySelectorAll('.st button').length })",
                )
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap_or_default();
            let v: serde_json::Value = serde_json::from_str(&last).unwrap_or_default();
            let st = v["st"].as_array().cloned().unwrap_or_default();
            let summary = v["report"].as_str().unwrap_or("");
            assert!(
                !summary.contains("no problems found"),
                "a run with silent frames said clean: {last}"
            );
            if st.len() > 5
                && st
                    .iter()
                    .all(|t| !t.as_str().unwrap_or("").contains("checking"))
            {
                done = true;
                break;
            }
        }
        assert!(done, "frames stayed undecided: {last}");
        let v: serde_json::Value = serde_json::from_str(&last).unwrap();
        let silent = v["st"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t.as_str().unwrap_or("").starts_with("no report:"))
            .count();
        assert!(silent >= 3, "{last}");
        assert_eq!(v["retry"].as_u64().unwrap(), silent as u64, "{last}");
        let summary = v["report"].as_str().unwrap();
        assert!(
            summary.contains(&format!("{silent} frames did not report")),
            "{last}"
        );
    }

    async fn run(child_host: &str) {
        let port = serve();
        let profile = std::env::temp_dir().join(format!(
            "shakedown-frame-{}-{}",
            std::process::id(),
            child_host.replace('.', "-")
        ));
        let mut chromes = Chromes::default();
        let browser = chromes
            .launch(&crate::browser::binary(), &profile)
            .await
            .unwrap();
        // A cross-origin frame runs in its own process, paused until the guard (as in a real run) resumes it.
        let guard = guard::start(browser.websocket_address()).await.unwrap();
        let page = browser.new_page("about:blank").await.unwrap();
        guard.armed(page.target_id().as_ref()).await.unwrap();
        page.goto(format!("http://localhost:{port}/parent/{child_host}"))
            .await
            .unwrap();
        let mut got = serde_json::Value::Null;
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let v: serde_json::Value = page
                .evaluate("JSON.stringify({ m: window.__m || [], y: scrollY, x: scrollX })")
                .await
                .unwrap()
                .into_value::<String>()
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if v["m"].as_array().is_some_and(|m| {
                m.iter().any(|e| e["frame"] == true) && m.iter().any(|e| e["shakedown"] == "loaded")
            }) {
                got = v;
                break;
            }
        }
        assert!(!got.is_null(), "the frame never reported ({child_host})");
        let msgs = got["m"].as_array().unwrap();
        let loaded = msgs.iter().find(|e| e["shakedown"] == "loaded").unwrap();
        let nodes = loaded["tree"]["nodes"].as_array().unwrap();
        assert!(
            nodes
                .iter()
                .any(|n| n["decor"] == true
                    && n["top_lines"].as_array().is_some_and(|l| !l.is_empty())),
            "ariafold must be caught inside a frame ({child_host}): {nodes:?}"
        );
        let frame = msgs.iter().find(|e| e["frame"] == true).unwrap();
        assert_eq!(
            frame["y"], 0,
            "the frame's scroll must be restored ({child_host})"
        );
        assert_eq!(frame["x"], 0);
        assert_eq!(
            got["y"], 1450,
            "the parent's scroll must be restored ({child_host})"
        );
        assert_eq!(got["x"], 0);
    }

    #[tokio::test]
    async fn an_aria_hidden_icon_below_the_fold_is_caught_in_a_cross_origin_frame_and_nothing_scrolls()
     {
        if !crate::browser::installed() {
            eprintln!("pinned Chrome not installed: skipped");
            return;
        }
        run("127.0.0.1").await;
    }

    #[tokio::test]
    async fn the_same_holds_in_a_same_origin_frame() {
        if !crate::browser::installed() {
            eprintln!("pinned Chrome not installed: skipped");
            return;
        }
        run("localhost").await;
    }
}
