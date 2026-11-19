# Shakedown: the shared testing tool (design record)

Status: proposed, 2026-11-19. Design only; nothing is built yet. Order of work is in "Phased plan" below.

## Goal

One local tool, written in Rust, that every project uses to run all of its checks:

- unit tests and headless core scenarios;
- UI screenshot checks on web, Android and iOS, against approved reference images;
- layout checks on every platform (one rule set);
- the device sweep (every device, both themes, every route), headless, with a Sonnet review step;
- size and speed budgets;
- the smoke test of the production Docker image;
- simulators and emulators: create, boot, pin, install, run, tear down.

Fast, deterministic, local only. No paid CI, ever: there is no hosted runner to target.

Non-goals: Linux or Windows hosts (Todd's Mac only), cloud device farms, any check that writes to production.

## Where it comes from

- Boggledygook (BDG) has the proven pieces: `make ui-check` (web through playwright-core headless Chromium with a
  faked `/api`, Android through Robolectric + Roborazzi on the JVM, iOS through XCTest in a pinned simulator),
  approved PNG references with a two-level tolerance, an HTML failure report, web-only layout and layout-shift
  checks, byte budgets and vitals. All of it is per-project Node and Python glue.
- `goddtriffin/infra` `scripts/devices/` has the device list (desktop to foldables, landscape), text-size and
  tap-target checks, and a `sweep()` that an agent drives by hand through a browser.

Gaps this closes:

1. Two device lists and two copies of the layout rules. Now: one of each, in the tool.
2. The device sweep has no layout-shift check and no pixel diffs. Now: both.
3. The sweep is manual. Now: headless, one command, plus a packaged Sonnet review.
4. Native screens get no layout checks. Now: the same rules on a layout tree each harness dumps.
5. Device frames share one signed-in account. Now: each frame is its own player.

## 1. Name

| candidate | for | against |
|---|---|---|
| **shakedown** (recommended) | A shakedown run is the trial before real service: exactly the smoke test plus everything before it. Short, one word, free on crates.io (checked 2026-11-19). | Slang meaning (extortion) exists, but the testing sense is standard. |
| rehearsal | Dress rehearsal before opening night; free on crates.io. | Longer to type; reads as one step, not a whole toolbox. |
| toddstack-check | Matches the `toddstack-*` crate names. | Long for a command typed many times a day; `check` collides with `cargo check` in conversation. |

Rejected: `proofsheet` (taken on crates.io), `gauntlet` (taken), `soundcheck` and `pitwall` (taken).

Recommendation: **shakedown**. Binary `shakedown`, config `shakedown.toml`, output `target/shakedown/`.

## 2. Shape: one Rust binary, thin harnesses

Decision: everything that decides pass or fail lives in Rust. Each platform keeps only what must run inside it.

| piece | language | what it does | how it ships |
|---|---|---|---|
| `shakedown` | Rust | config, orchestration, Chrome driving, fake world, layout rules, pixel compare, reports, budgets, smoke, simulators | crates.io, `cargo install shakedown@x.y.z --locked` |
| page collector | JS, about 150 lines | runs in the page: freezes clock and random, records layout shifts, dumps the layout tree | embedded in the binary (`include_str!`), injected over CDP |
| Android harness | Kotlin, about 200 lines | renders the real Compose screens under Robolectric with fixed core views, writes PNG + layout tree per device and theme | vendored into the project by `shakedown init android` |
| iOS harness | Swift, about 250 lines | renders the real SwiftUI screens in the pinned simulator (XCTest), writes PNG + layout tree | vendored by `shakedown init ios` |

- The harnesses are files the tool writes into the project, with a header naming the tool version and harness
  protocol. `shakedown check` fails if a vendored harness is older than the tool's. Rejected: publishing Kotlin and
  Swift packages. Todd deleted the Swift and Kotlin SDKs (2026-11-18, Rust only); a second packaging pipeline for
  400 lines is not worth it.
- Rejected: keeping Node as the orchestrator (today's BDG `ui/*.mjs`). It works, but it is a per-project copy,
  needs a `node_modules`, and leaves the rules in JS where native cannot use them.
- Rejected: Python (`devices.py`, `vitals.py`, `core_size.py`). Same reasons; also slower to start and harder to keep
  deterministic.

### Harness protocol (version 1)

The tool writes `target/shakedown/<platform>/input.json`: screens (name plus path to the fixed core views), devices
(name, width, height, density, orientation), themes, output directory, fixed clock and seed. The harness writes
`<screen>@<device>-<theme>.png` and `<screen>@<device>-<theme>.layout.json` (schema in section 6) and nothing else.
The tool compares, checks and reports. A harness never decides pass or fail.

## 3. Crate layout in toddstack

One new crate, `rust/shakedown/`, a workspace member with the workspace lints. Library plus binary, so the modules
are unit-tested and the frontend engine's testing feature can be reused.

```
rust/shakedown/
  Cargo.toml
  README.md                 consumer reference (every command and config key)
  src/
    main.rs                 clap CLI, exit codes
    config.rs               shakedown.toml: parse, validate, includes
    steps.rs                the check runner: a DAG of steps, parallel, one line each
    devices.rs              the one device list (data in devices.toml, compiled in)
    layout/                 the layout tree type and every rule
    pixels.rs               PNG decode/encode, quantize, compare, diff image
    report/                 HTML report and summary.json
    browser/                pinned Chrome install, CDP session, contexts, fake world, collector.js
    world.rs                fixtures, fixed clock, players
    ui.rs                   ui and update
    sweep.rs                sweep, baselines, review pack, contact sheets
    budgets.rs              sizes (gzip, brotli, stripped) and timing
    smoke.rs                production image smoke test
    sims/                   simctl and avdmanager/emulator/adb wrappers
    native.rs               harness protocol, vendored harness files (harness/android/*.kt, harness/ios/*.swift)
```

- One crate, not five. Rejected: `shakedown-pixels`, `shakedown-layout` and so on as separate published crates.
  Nobody else consumes them yet; splitting later is cheap, merging published crates is not. Tie-breaker: fewer
  moving parts.
- Core scenarios: the tool does not run them itself, because only the project's own CLI knows the app factory
  (BDG's `bdg-cli`). It calls the configured command. It does depend on `toddstack-frontend-engine` (feature
  `testing`) for the scenario and snapshot file formats, so `shakedown` can list scenarios and report failures in
  one format.
- Results go to `target/shakedown/` (already gitignored in every Rust repo, removed by `cargo clean`). Downloaded
  browsers go to `~/Library/Caches/shakedown/` and are shared across projects by exact version.

### Direct dependencies (exact versions, current on 2026-11-19)

Workspace-shared: `tokio` 1.54.1, `serde` 1.0.229, `serde_json` 1.0.154, `reqwest` 0.13.5 (rustls), `thiserror`
2.0.21. New:

| crate | version | why |
|---|---|---|
| `clap` | 4.6.8 | CLI (derive) |
| `toml` | 1.2.1 | config |
| `chromiumoxide` | 0.9.1 | CDP, `default-features = false` (no fetcher: the tool installs Chrome itself) |
| `png` | 0.18.1 | decode and encode; the compare is our own |
| `flate2` | 1.1.11 | gzip budgets |
| `brotli` | 9.1.3 | brotli budgets |
| `sha2` | 0.11.1 | verifies the Chrome download, names cache entries |
| `toddstack-frontend-engine` | 0.1.0 (path + version) | scenario and snapshot formats |

Chrome archives are unpacked with macOS `ditto`, not the `zip` crate: `.app` bundles hold symlinks and code
signatures that `ditto` keeps. Rejected: `image` and `image-compare` (large, and SSIM would change BDG's proven
pass/fail semantics).

## 4. The per-project config: `shakedown.toml`

At the repo root. TOML because it takes comments and every project already reads Cargo.toml. Large screen lists
split out with `include = ["ui/screens/*.toml"]`. Sketch (keys final at implementation, documented in the README):

```toml
shakedown = "0.1.0"            # the tool version this repo expects; a mismatch is an error with the install command
project = "example"

[check]                        # the whole local gate, run by `shakedown check`
steps = [
  { name = "fmt",       run = "cargo fmt --check" },
  { name = "clippy",    run = "cargo clippy --all-targets -- -D warnings" },
  { name = "test",      run = "cargo test" },
  { name = "scenarios", run = "cargo run -q -p app-cli -- run scenarios", after = ["test"] },
]
builtin = ["budgets.sizes", "ui.web", "ui.android"]   # tool-run steps that join the same DAG

[server]                       # the local server that ui and sweep draw against
start = "make ui_server PORT={port} DB={db}"   # the tool picks the free port and a per-checkout database name
ready = "/healthz"
world = "fake"                 # fake: /api answered from fixtures; seeded: real local server and database
seed  = "make ui_seed DB={db}" # seeded worlds only
now   = "2026-11-17T12:00:00Z" # fixed clock: given to the server (SHAKEDOWN_NOW) and to the page

[devices]
ui     = ["phone", "tablet", "desktop"]  # devices with committed references
sweep  = "all"
themes = ["light", "dark"]

[players]                      # seeded local dev accounts; never production
accounts = ["ann", "ben", "cai", "dee"]
signin   = "/__dev/signin?as={player}"

[[screen]]
name   = "home-guest"
path   = "/"
player = "guest"
routes = { "GET /api/v1/me" = "fixtures/me-guest.json" }

[[screen]]
name   = "menu-open"
path   = "/"
player = "ann"
routes = { "GET /api/v1/me" = "fixtures/me-ann.json" }
act    = [{ click = "[data-acct] button" }, { wait_text = "Your profile" }]

[sweep]
routes = ["/", "/account", "/friends"]
groups = [{ name = "lobby", path = "/lobby/test", players = 4 }]  # each frame its own player, same lobby

[layout]
ignore = [{ rule = "clipped-text", match = ".marquee", why = "scrolls by design" }]  # every ignore needs a why

[budgets.sizes]
"web main.css gzip" = { file = "bin/static/main.css", gzip = 41000 }
"iOS stripped"      = { file = "target/aarch64-apple-ios/release/libapp_ffi.a", strip = true, max = 2900000 }

[budgets.timing]               # medians of `shakedown vitals`
"web.home.cold.lcp" = 1000
"web.home.cold.cls" = 0

[smoke]
up     = "make smoke_up PORT={port}"   # Docker stays behind the repo's Makefile targets
down   = "make smoke_down"
ready  = "/healthz"
checks = [
  { path = "/", status = 200, contains = "<title>" },
  { path = "/old", status = 301, location = "/new" },
]
sweep = true

[android]
views   = "cargo run -q -p app-cli -- run --app app ui/scenarios"  # writes the fixed core views
harness = "android/app/src/test/kotlin/ShakedownUi.kt"
run     = "./gradlew -q :app:testDebugUnitTest --tests '*ShakedownUi*' -Pshakedown={input}"
emulator = { image = "system-images;android-36;google_apis;arm64-v8a", device = "pixel_7" }

[ios]
views     = "cargo run -q -p app-cli -- run --app app ui/scenarios"
run       = "xcodebuild test -scheme App -only-testing:AppTests/ShakedownUi -destination {destination}"
simulator = { device = "iPhone 17", runtime = "iOS 26.5" }
```

Decisions:

- Screens are data, not code. BDG's `act` functions become a small fixed action list (`click`, `type`, `press`,
  `wait_text`, `wait_gone`, `scroll`). Rejected: a JS escape hatch; it would reintroduce per-project page code
  the tool cannot check.
- Two world modes. `fake` intercepts every `/api` call and answers from fixtures (BDG today); an unfaked call fails
  the screen. `seeded` runs the real local server on a throwaway per-checkout database with a seed command
  (server-rendered apps such as accounts-service). Both fix the clock.
- Fixtures stay where the project keeps them (BDG reuses its scenario fixtures); the config only points at them.

## 5. Commands

| command | does |
|---|---|
| `shakedown check [--only a,b]` | the whole local gate: configured steps plus built-in ones, as a DAG, in parallel. One line per step; full detail only for failures. Replaces the body of `make test`. |
| `shakedown ui [web\|android\|ios\|all] [--screens a,b]` | renders every screen on the `ui` devices and themes, checks layout and shifts, compares with references; report on failure |
| `shakedown update --screens a,b [--platform p]` | rewrites only the named references (`all` only for a first generation). Writes a file only when its pixels changed. A major deviation is Todd's call. |
| `shakedown sweep [--base URL] [--routes a,b] [--pack]` | every device, every theme, every sweep route and group, headless. Layout rules, layout shift, pixel diff against the baseline sweep. `--pack` writes the review pack (section 11). |
| `shakedown budgets [sizes\|timing]` | sizes are exact and instant; timing reads the last `vitals` results |
| `shakedown vitals [web\|ios\|android]` | measures speed (LCP, CLS, INP, long tasks; launch to first frame and to full content; hitches) with throttling, paired runs, medians |
| `shakedown smoke` | the production-image smoke test (section 10) |
| `shakedown devices [list\|json\|serve]` | the device list; `serve` is the interactive side-by-side viewer that replaces `scripts/devices/devices.py` |
| `shakedown sims [status\|up\|install\|run\|down\|rm\|runtimes]` | simulators and emulators (section 9) |
| `shakedown browser [install\|path]` | the pinned Chrome |
| `shakedown doctor` | checks Xcode, Android SDK, Java, the pinned Chrome and simulator runtimes, disk use; prints the fix for each miss |
| `shakedown init [web\|android\|ios]` | writes a starter `shakedown.toml` and the vendored harness files |
| `shakedown clean` | removes `target/shakedown/` and this project's own simulators and emulators |

Output contract, for people and agents alike: stdout is one line per step (`ok ui.web 48 shots 6.1s`, `FAIL ui.web
2 of 48`), then one line per failure with its reason, then the report path. `--json` also writes
`target/shakedown/summary.json`. Exit codes: 0 pass, 1 a check failed, 2 config or usage error, 3 environment
missing (with the fix command). Short output on success keeps agent contexts small.

## 6. One device list, one set of layout rules

### Devices

`src/devices.toml` in the tool is the only list. It carries over all 11 devices of `scripts/devices` (desktop
1440x900, small laptop 1280x720, tablet 820x1180, phone 390x844, small phone 375x667, small Android 360x740,
Galaxy Z Fold 5 folded 344x882 and unfolded 673x841, iPhone Duo folded 382x580 and unfolded 669x867) with their
landscape forms (measured, not swapped, where the browser toolbar moves). Each entry has CSS pixels, density and the
platforms it applies to.

- Web uses every device. Android maps each to Robolectric qualifiers (`w360dp-h740dp-xhdpi-land`), so the JVM
  harness renders foldables and landscape too. iOS renders a window of each size in points.
- `shakedown devices json` prints the list; nothing else keeps a copy.
- Adding a device means a tool release; projects pick from the list by name.

### The layout tree

Every platform produces the same JSON: viewport, content width, and a flat list of nodes with `id` (stable across
captures), `kind` (`text`, `tap`, `image`, `progress`, `container`), rect, text line rects, font size in the
platform's points, `clipped` (text truncated or overflowing), `on_top` (not covered by a dialog), and opt-out marks.

- Web: `collector.js` walks the DOM (the logic of BDG's `layout.mjs` and `devices.html`, moved in one place).
- Android: the Compose semantics tree (`onRoot().fetchSemanticsNode()`): bounds, text layout results (font size,
  line boxes, overflow), click actions, progress semantics.
- iOS: the accessibility hierarchy plus `UIView` frames. SwiftUI does not expose font size or truncation directly;
  see open question 4.

### Rules (Rust, run on the tree)

| rule | fails when |
|---|---|
| `sideways-scroll` | content is wider than the viewport |
| `off-screen` | a visible element sits partly outside the viewport (unless inside a marked horizontal scroller) |
| `clipped-text` | text is cut off |
| `text-overlap` | two on-top text lines overlap by more than 2 px both ways |
| `text-min-size` | text under 11 px (points, dp) |
| `tap-min-size` | a tap target under 24 px either way (WCAG 2.2 target size); the platform guidance (44 pt iOS, 48 dp Android) is a warning |
| `layout-shift` | anything moves after it first appears. Web: the browser's layout-shift entries, budget 0 (BDG's threshold 0.0005 for font-swap noise). Native: the tree at first frame against the settled tree, by node id |
| `loading-indicator` | a spinner, progress bar or "Loading" text is on screen (canonical pattern section 7: no loading indicators) |

Opt-outs are per element (`data-shakedown-ignore="rule"` on web, a test tag or accessibility identifier prefix on
native) or per config entry, and every one needs a reason string.

Rejected: rules in each platform's language (three copies drift; that is today's problem). Rejected: rules on
pixels (slow, and cannot say which element is wrong).

## 7. Driving Chromium from Rust

- Pinned browser: **Chrome for Testing 149.0.7827.55** (mac-arm64), the build BDG's references were made with.
  `shakedown browser install` downloads it from Google's Chrome for Testing bucket, checks a SHA-256 kept in the
  tool, unpacks it with `ditto` into `~/Library/Caches/shakedown/chrome/149.0.7827.55/`. The version is set by the
  tool release. Bumping it is a tool release plus a reviewed `update --screens all` in each project.
- Full Chrome in new headless mode, not `chrome-headless-shell`: same renderer users get, same as BDG's references.
  One browser process per run, many tabs; startup is paid once.
- CDP crate: **chromiumoxide 0.9.1**, async on tokio with generated protocol types. Wrapped behind a small internal
  `Browser` trait (about 15 calls), so replacing it is a contained change.
  - Rejected: `headless_chrome` (synchronous, thread per tab, slower to follow the protocol, weak worker sessions).
  - Rejected: WebDriver crates (`fantoccini`, `thirtyfour`): need chromedriver, no request interception, no
    layout-shift entries.
  - Rejected: Playwright through Node: the dependency we are removing.
  - Fallback, not chosen now: our own minimal CDP client on `tokio-tungstenite`. Fewer dependencies, but we would
    own protocol types. Take it only if chromiumoxide falls behind the pinned Chrome.
- CDP used: `Target.createBrowserContext` (one isolated context per frame: own cookies and storage, so each frame is
  its own player), `Emulation.setDeviceMetricsOverride` (size, density 1, mobile), touch emulation, emulated media
  (`prefers-color-scheme`, `prefers-reduced-motion`), timezone UTC and locale en-US overrides, `Fetch` interception
  for the fake world and for blocking every off-origin request, `Page.addScriptToEvaluateOnNewDocument` for the
  collector, `Page.captureScreenshot`, and for vitals `Network.emulateNetworkConditions` and
  `Emulation.setCPUThrottlingRate`.
- Workers: the frozen clock and seeded random must reach the core's module worker too. The tool auto-attaches to
  worker targets (paused at start), injects the same prelude, then resumes them. Today's page-only override misses
  the worker.
- Rejected: Chrome's virtual time. The frozen `Date` plus capped timers is proven in BDG; virtual time interacts
  badly with workers, wasm and network waits.

## 8. Pixel compare and report

A straight port of BDG's `png.mjs`, so BDG's existing references stay valid without regeneration:

- A pixel differs when any channel moves by more than 16. A screen passes when at most 0.1% of pixels differ and no
  24x24 block has more than 20% changed (a small element that changed fails even if the screen barely moved).
- Size change fails. Missing reference fails with the `update` command to run.
- References are stored RGB, channels rounded to multiples of 8 (under 4 of 255 error, about half the bytes),
  rows filtered by minimum sum of absolute differences, best compression. Names: `<screen>@<device>-<theme>.png`.
- Settled means two captures in a row are byte-identical (BDG's rule, all platforms).
- Report: `target/shakedown/<platform>/report.html`, written only on failure (the sweep always writes one). Per
  failure: reference, actual and diff side by side, a toggle to flip between reference and actual, and the failing
  layout nodes drawn as boxes over the actual image. `summary.json` lists the same in machine form.

## 9. Simulators and emulators

All through Apple's and Google's own command-line tools (`xcrun simctl`, `avdmanager`, `sdkmanager`, `emulator`,
`adb`), called from Rust. Pinned in `shakedown.toml`.

- Naming: every device the tool creates is `shakedown-<project>-<device>`. The tool only touches devices with its
  prefix and this project's name: several sessions share the Mac, and another project's simulator is never ours.
- `sims up`: creates the device if missing (iOS: device type + runtime; Android: AVD from the pinned system
  image), boots it headless (`emulator -no-window -no-audio -no-snapshot-save -gpu swiftshader_indirect` on a
  free console port), waits for boot (`simctl bootstatus -b`; `sys.boot_completed`), then pins the look: status bar
  overridden (`simctl status_bar override --time 9:41 --batteryLevel 100`; Android demo mode), animations off on
  Android (`settings put global *_animation_scale 0`), appearance set per theme (`simctl ui appearance`;
  `cmd uimode night`). Idempotent: an existing booted device is reused.
- `sims install` / `sims run`: install the built app and launch it pointed at the local server (`adb reverse` for
  Android). `sims down` shuts down only what this tool booted (recorded in `target/shakedown/sims.json`); `sims rm`
  deletes this project's own devices.
- Missing runtimes or system images: the tool prints the exact install command and stops. Installing an SDK package
  accepts a license, so it asks first.
- `sims runtimes` lists installed iOS runtimes, Android system images and AVDs, and which repos pin each (it reads
  every `shakedown.toml` under `~/Documents/code`). It reports what is safe to delete per the global disk rules;
  `--delete` removes them, and only when nothing is booted on them.
- Screenshots for `ui` do not need a booted emulator: Android renders on the JVM (Robolectric, seconds), iOS needs
  the simulator because XCTest runs in it. The emulator is for scenarios on device, instrumented tests and vitals.

## 10. Smoke test of the production Docker image

Canonical pattern 6d: run the exact image that will ship, not the dev server, and check it works, including the
device review.

1. `make <smoke.up> PORT=<free port>`: the repo's own target runs the already-built image with `--rm`, the repo
   label, bound to `127.0.0.1`, with local fake secrets and a throwaway local database. Docker stays behind
   Makefile targets (global rule); the tool never runs `docker` itself. The target prints the image digest, which
   the report records so the push step can confirm the same digest shipped.
2. Wait for `ready` (30 s limit).
3. Built-in HTTP checks on every sweep route: no 5xx, security headers present, every asset the HTML references
   answers 200 with a long cache lifetime, no `localhost` or dev URLs in served pages.
4. The project's checks (status, body contains, redirect target, content type, max bytes).
5. A sweep against the container: layout rules and layout shift (on live content, not fixtures), plus a pixel diff
   against the dev-server sweep of the same commit. Image and dev server should draw the same pages; a difference
   points at the build (missing asset, wrong env).
6. The container's logs are scanned for panics and error lines.
7. `make <smoke.down>`, always: on success, failure or Ctrl-C (a drop guard and a signal handler).

## 11. The Claude skill and the Sonnet reviewer

Canonical pattern 7: automated checks first, then a Sonnet subagent looks at the screenshots and reports text only,
keeping images out of the main context.

- Skill `shakedown-review` lives in `goddtriffin/skills` (shared by every project), not in this repo.
- The main agent runs `shakedown sweep --pack` and reads one line back: the pass/fail count and the pack path.
- The pack, `target/shakedown/review/`: `manifest.json` (routes, devices, themes, every automated finding, every
  pixel change against the baseline) and contact sheets. A contact sheet is one route in one theme, every device
  tiled at one scale with its name burned in, at most 1568 px on the long edge (the size Claude reads without
  downscaling). The sheet is laid out as HTML and captured with the same pinned Chrome, so no font or drawing crate
  is needed. About routes x 2 sheets instead of routes x 22 x 2 single images: far fewer image tokens.
- The skill tells the main agent to start a fresh Sonnet helper with only the pack path and the brief: look for
  what the rules miss (overlap the rules did not catch, too busy, odd spacing, contrast, broken images, anything that
  looks wrong), one line per finding as `<route> <device> <theme>: <problem>`, at most 30 lines, then
  `verdict: pass` or `verdict: fix`. The helper never returns images. The main agent never opens a PNG.
- Rejected: calling the Claude API from the tool. It would need a key, cost money per run, and duplicate what the
  session's own helpers already do.

## 12. Distinct signed-in players per frame

Canonical pattern 7: never several frames as the same account.

- Headless: each frame gets its own browser context, so cookies and storage are separate. Frame `i` signs in as
  `players.accounts[i mod n]` through `players.signin` against local accounts-service dev accounts. A `group`
  (one lobby or game across frames) needs at least as many players as frames, or the sweep refuses to start.
- Fake world: the player is the `/me` fixture each screen names.
- Interactive viewer (`shakedown devices serve`): frames load from `p1.localhost:<port>`, `p2.localhost:<port>` and
  so on. Chrome resolves `*.localhost` to 127.0.0.1 and keeps cookies per host, so each frame has its own jar. The
  frames are then cross-origin to the viewer, so the proxy injects the collector into each HTML response and the
  frame posts its layout tree to the parent. This replaces today's same-origin trick that shares one sign-in.

## 13. Budgets

- Sizes: exact bytes, no device. Raw, gzip level 9, brotli 11, and stripped static libraries (via `strip`). An
  artifact that was not built is reported as skipped, never as passing (BDG's rule), and `check` fails if a skipped
  artifact is listed in `builtin`.
- Timing: `shakedown vitals` measures, `budgets timing` judges medians. Web through CDP with fixed throttling (4x
  CPU, 150 ms RTT, 1.6 Mbps), cold and warm, paired against a baseline commit when asked. Native through the
  harness or the app's own timing log (launch to first frame and to full content, hitches). Ceilings are the worst
  run seen plus headroom; anything not measured is labeled "not measured".

## 14. Determinism

Same inputs give byte-identical outputs and the same verdict.

- Clock fixed (`server.now` to the server, a frozen `Date` in page and workers); timers over 1 s never fire;
  `setInterval` off. Native: the harness pins `now`, seed and motion through the input file (BDG's `Motion`).
- Random seeded (`Math.random`, `crypto.getRandomValues`, the core's seed).
- No network: every off-origin request is blocked or answered with a fixed image; in the fake world every `/api`
  call must be faked; answers arrive after a fixed 150 ms so late-data shifts show.
- Animations, transitions and the caret frozen; reduced motion on; fonts loaded before capture; images decoded;
  settled when two captures match.
- Pinned: Chrome version, device metrics at density 1, UTC, en-US, simulator runtime, Robolectric SDK level, Android
  system image. References record the environment (`refs/env.json`: Chrome, macOS, Xcode, runtime); a mismatch
  warns with the cause before any diff is blamed on the code. Projects self-host their fonts so system fonts never
  reach a reference.
- Output: sorted, no timestamps, no absolute paths in committed files; parallel runs write in a fixed order.
- `update` rewrites a reference only when its pixels changed, so a re-run is a no-op in git.
- `--repeat N` runs everything N times and fails if any run differs (BDG's flakiness gate, for every check).

## 15. Speed

Targets, not measured yet:

- `shakedown ui web` on about 50 shots: under 10 s warm (one Chrome, 8 tabs in parallel, contexts reused per device
  where the world allows).
- `shakedown check` on a clean tree: no slower than the `make test` it replaces; independent steps run in parallel.
- Machine sharing: one Chrome per run, at most one sweep at a time on the Mac (a lock in the cache directory; a
  second run waits and says so), one simulator boot at a time per device.

## 16. Migration

1. **accounts-service, pilot.** Web only, server-rendered: `world = "seeded"` against the local database, players
   are its own dev accounts. Adopt in order: `check` (wraps today's `make test`), `budgets.sizes`, `ui web` on the
   Account Center and sign-in pages, `sweep`, `smoke` against the production image. The Makefile targets become
   thin calls to `shakedown`. Done when `make test` and the pre-merge smoke run through the tool and the old ad hoc
   steps are deleted.
2. **The interactive viewer.** `shakedown devices serve` reaches parity with `scripts/devices/devices.py`; infra's
   README then points to it and the Python script is deleted.
3. **Boggledygook, before its store release** (BDG #74), done by the BDG session, which owns the repo. Each check moves
   only when it passes the Acceptance rule below. `ui/web/screens.mjs` becomes `[[screen]]` entries; the existing
   references carry over unchanged (same compare, same quantization, same names); `ui/*.mjs`, `png.mjs`, `report.mjs`,
   `layout.mjs`, `core_size.py` and the vitals scripts are deleted; the Kotlin and Swift `UiCheck` tests are replaced by
   the vendored harnesses, which add the layout tree. Everything game-specific (screens, fixtures, budgets) stays in
   BDG's own `shakedown.toml`.
4. **Every other project**, one at a time, as each is next touched.

## Acceptance: when shakedown may replace a project's check

A project's existing check (`make test`, `ui-check`, `vitals`, a script) is replaced by shakedown only when both hold:

- **No slower.** Shakedown is no slower than today's equivalent, measured cold (nothing built or cached) and warm
  (second run), on the same machine and commit. The numbers go in the PR body; a slower result needs the reason and
  Todd's go-ahead.
- **Nothing lost.** It covers every check the old one did, and each is proven by a planted-break test: plant a known
  break (a 1 px shift, an overflow, a blown budget, a missing string) and shakedown must fail; plant a harmless
  change (sub-threshold drift) and it must pass. A check with no planted-break test does not count as covered.

Planted-break tests live with the code (`rust/shakedown/tests/planted_breaks.rs` for the tool's own checks) or in the
project, and each phase adds its own before the old check is deleted.

## 17. Phased plan

| phase | delivers | done when |
|---|---|---|
| 0 | this design; open questions decided | merged |
| 1 | crate skeleton, config, `check` step runner, `budgets sizes`, `pixels` (ported, with tests on synthetic images), report, `doctor` | accounts-service `make test` runs through `shakedown check` |
| 2 | pinned Chrome, CDP session, fake and seeded worlds, collector, layout rules in Rust, `ui web`, `update` | accounts-service web references committed; repeat-5 run identical |
| 3 | `sweep` with players, layout shift, baseline diffs, `--pack`, the `shakedown-review` skill, `devices serve` | accounts-service swept on all devices with a Sonnet review; infra's Python viewer retired |
| 4 | `smoke` | accounts-service's production image smoke-tested before a deploy |
| 5 | `sims`, Android and iOS harnesses with layout trees, native rules | a scratch app (frontend engine's counter example) checked on all three platforms |
| 6 | `vitals` and `budgets timing` | timing budgets enforced for one project |
| 7 | BDG migration, before its store release (Todd) | BDG's `ui/` and scripts deleted, same references pass |

Each phase is its own PR with a patch release on crates.io.

## Decided (2026-11-19)

1. Name: `shakedown` (Todd).
2. Pilot order: accounts-service first for the web phases (1-4). BDG moves onto shakedown BEFORE its store
   release (Todd, 2026-11-19): native phases (5) are built against BDG's harnesses, and the BDG migration is part
   of the pre-release work, not after it.
3. Sweep pixel diffs compare against a local, uncommitted baseline in `target/`; only layout rules and shifts fail
   the sweep. Committed references live only in `ui`.
4. iOS layout data: (a), a tiny `.shakedownText()` modifier.
5. Tap targets: fail under 24 px on every platform; warn under the platform guidance (44 pt iOS, 48 dp Android).

## Phase 2 notes (2026-11-19, shakedown 0.2.0)

Built as designed, with these differences:

- No `Browser` trait: `capture.rs` talks to chromiumoxide 0.9.1 directly (about 15 calls); it is the only file that
  would change. Chrome is downloaded with `curl` (no `reqwest`) and unpacked with `ditto`.
- Workers (the frozen clock reaching a module worker) are not done: no pilot has one. Phase 7 (BDG) needs them.
- Layout-shift entries are taken whatever `hadRecentInput` says: mobile emulation marks a plain page load as
  input-driven, and no person is typing. Screens with actions are judged on the shifts before the first action.
- The freeze style is a constructable stylesheet, not a `<style>` element: a page's own CSP (`style-src 'self'`)
  blocks the latter. Text covered by a painted element (a dialog, a dropdown) is not "on top"; text over text is.
- A signed-in player is in at most `[players].parallel` shots at once (default 3; accounts-service sets 4, as it caps
  a person's open event streams at 5).
- `/favicon.ico` console errors are ignored (new headless Chrome asks for it by itself); off-origin image and icon
  requests get a flat stand-in, every other off-origin request fails the screen.
- `loading-indicator` is not implemented yet (it was not in the phase 2 list).
- Chrome flags for stable pixels: software raster and compositing, no checker imaging, one raster thread. A shot is
  settled when every image is decoded and two frames have drawn, then one capture is identical to the one before
  (event-driven, no fixed sleep). `--repeat` uses a fresh Chrome per run. Even so, about 7% of
  shots differ between runs by up to about 50 edge pixels around images clipped to a circle: the repeat gate is on
  the compare's verdict and the findings, with the noisy shots listed. Strict byte identity needs a different
  renderer and is not met.

`update` rewrites a reference only when the drawn pixels fail the compare against the stored one (raw pixels, not the
quantized ones, so `update` and `check` agree); a re-run is a no-op in git.

## Sweep speed notes (2026-11-19, shakedown 0.4.0)

An accounts-service sweep (288 shots) was 121 to 133 s. What changed, from a `SHAKEDOWN_TRACE` profile:

- A shot takes its player's turn before a shared slot, and holds neither while it waits for the other. Before, a
  shot waiting for its player's turn kept its slot (about 4 of 16 slots idle). Pixel-identical clone accounts
  (`ann2`, ...) are not possible in accounts-service (the username, email and friend code are unique and drawn on the
  page), so the cap is met another way: `turn = false` on a `[sweep]` route or `[[screen]]` says the page opens none
  of the connections the server caps per person, so its shots never take a turn (accounts-service: only the
  friends page opens an event stream). The default stays `true`.
- One browser context per player and browser, for the whole run, signed in once by the first shot that needs it
  (the others wait for it). Every device and theme of the player is a tab in that context with its own emulation;
  nothing is wiped between shots, and the contexts end with the browser. Each tab is told it has focus
  (`Emulation.setFocusEmulationEnabled`), or a tab behind its siblings draws no frames. The guard now attributes a
  violation to the top-level page that holds the frame, worker or popup, not to the context (tabs share it).
  The gate is order independence: `SHAKEDOWN_SHUFFLE=<seed>` draws the shots in a seeded order, and two sweeps with
  different seeds must show no pixel change against the baseline of an unshuffled run before this change. A shot
  found to depend on order is isolated in a context of its own; none was.
- Each sweep shot is judged (re-encoded with fast PNG compression, compared, written) the moment it is drawn, on its
  own thread after its slot is free; identical files skip the compare, identical pixels skip the counting. The
  baseline and `current/` are fast-compressed (they stay on this machine); older High-compressed baselines still compare.
  The `ui` references stay High-compressed (they are committed).
- The pack's sheet HTML is built on all cores and the sheets are captured by several pages at once.

## Phase 3 notes (2026-11-19, shakedown 0.3.0)

Built as designed, with these differences:

- `[sweep].routes` entries are a path (a guest) or a table `{ path, name, player, turn, routes, allow_console }`; a
  signed-in route names its one account. A group's `players = N` is the number of accounts that take turns: frame
  `i` (the `i`th device, both themes) is `accounts[i mod N]`, and `N` may not exceed `[players].accounts`. The design's
  "at least as many players as frames" is not applicable: a group is drawn on 18 devices, not 18 simultaneous frames.
- `[devices].sweep` is `"all"` (default) or a list of names.
- The baseline lives in `target/shakedown/sweep/baseline/`, one PNG per shot, written when missing and never
  replaced by a plain sweep (so a change stays visible until accepted). `--rebase` (an addition to the command line)
  makes the current run the baseline. A pixel change is the compare's verdict (past the tolerance), a resize counts.
- `--base URL` skips the `[server]` world and refuses any host that is not loopback or `*.localhost`; `sweep` without
  `--base` needs `[server]`. A `--base` run takes the world mode and clock from `[server]` when there is one.
- The review pack is `target/shakedown/review/`: `manifest.json` (routes with their sheets and players, devices,
  themes, findings, pixel changes, count of new baselines) and `<route>-<theme>.png`. One shared scale per sheet
  (shelf packing, tallest first, largest scale that fits 1568 x 1568); a tile with findings is outlined red. Sheets
  are captured from HTML with data URIs in the pinned Chrome (`sheet.rs`).
- `devices serve` replaces `devices.py` and `devices.html`: one proxy thread per connection on `std::net`, no HTTP
  dependency. Frames are `p<i+1>.localhost:<port>` (own cookie jar each); the injected collector (`collector.js`
  with `__FIXED__ = false`, so the clock is live) posts the layout tree to the viewer, which sends it to
  `/__devices/check` where the Rust rules judge it. The old `sweep()` and the in-page `problems()` are gone: the
  headless `sweep` is the automation. A target must be loopback; the proxy rejects any Host that is not `localhost`,
  `127.0.0.1` or `*.localhost`. The theme toggle sets `color-scheme` on the frame (as before); a cross-origin frame
  cannot be given a different `prefers-color-scheme`, so dark mode is checked exactly only by `sweep`.
- Not done: the viewer's per-frame "own player" sign-in is a button that loads `[players].signin` (it needs a
  `shakedown.toml`); `loading-indicator` is still not implemented.

## Phase 4 notes (2026-11-21, shakedown 0.4.2)

`shakedown smoke` is built as section 10 describes, with these differences:

- Step 5 (the sweep against the container) is wired. `smoke::sweep_container` calls `sweep::run` with
  `base = http://127.0.0.1:{port}` (no world of its own, no pack, no rebase) on the project's `[sweep]` pages;
  layout findings fail the step, pixel changes against the dev-server baseline stay reported by `shakedown sweep`
  only. `run_on` takes the sweeper as a parameter, so the tests inject findings without Chrome. A config without a
  `[sweep]` section reports `skip smoke.sweep` (a skip is not a pass: it is on the output line). A sweep that cannot
  run (no Chrome) fails the step. `routes` of `[smoke]` do not select sweep pages: the whole `[sweep]` list is drawn.
- One extra optional key, `logs`: a third `make` target that prints the container's output. Docker stays behind the
  Makefile, and a container run with `--rm` that crashes at boot leaves nothing for `docker logs`, so the repo's
  `smoke_up` attaches the run to a log file and `smoke_logs` prints it. `down` and `logs` take `{port}` like `up`
  (container, database and network names carry the port, so two runs never share them).
- Other keys: `ready` (default `/`), `timeout` (30 s), `routes` (the paths the built-in checks cover, besides the
  `checks` paths), `headers`, `min_cache`, `log_ignore`.
- The up target must print the image digest (`sha256:` and 64 hex digits); a run without one fails, so the push
  step can always confirm the same image shipped. The digest is in the output, `summary.json` and `smoke.up`'s line.
- The HTTP client is about 40 lines over `std::net` (loopback, HTTP/1.0, no redirects, no TLS): no new dependency.
- Security headers default to CSP, `X-Content-Type-Options` and `Referrer-Policy`. HSTS is left to the edge
  (accounts-service sends none; Cloudflare adds it).
- Planted breaks proven on accounts-service: a crashing image (bad env var) fails `smoke.ready` with the container's
  log shown, and a page that references a missing asset fails `smoke.http`.
