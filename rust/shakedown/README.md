# shakedown

One local testing tool for every project. Part of [toddstack](https://github.com/goddtriffin/toddstack).
A shakedown run is the trial before real service: the checks you run before anything ships. Fast, deterministic,
local only (macOS). Design record: [`docs/testing-tool/DESIGN.md`](../../docs/testing-tool/DESIGN.md).

Shipped: the `check` step runner, `budgets sizes`, `pixels` compare, the report, `doctor` (phase 1); the pinned
Chrome, `ui web` with layout rules and approved references, `update`, fake and seeded worlds, `include` (phase 2).
`sweep` with the review pack and `devices serve`, the interactive viewer (phase 3). `smoke`, the production-image smoke test (phase 4). Simulators arrive in a later phase.

## Install

```sh
cargo install shakedown@0.4.3 --locked          # from crates.io
cargo install --path rust/shakedown --locked    # from a toddstack checkout
shakedown doctor                                # what is installed, what is missing, and the fix for each miss
```

Pin the version in each project's `shakedown.toml`; a mismatch stops with the exact install command.

## Commands

| command | does |
|---|---|
| `shakedown check [--only a,b] [--json] [--skip-unbuilt]` | runs `[check]`: shell steps as a graph, independent steps in parallel |
| `shakedown budgets sizes [--skip-unbuilt]` | exact byte ceilings from `[budgets.sizes]` and the shared `[budgets].file`; `--skip-unbuilt` skips artifacts that were not built |
| `shakedown pixels compare <reference.png> <actual.png> [--out DIR]` | the pixel compare; on failure writes the diff image and `report.html` to `DIR` (default `target/shakedown/pixels`) |
| `shakedown pixels encode <in.png> <out.png> [--step 8]` | stores a PNG the way references are stored |
| `shakedown ui web [--screens a,b] [--repeat N] [--json]` | draws every `[[screen]]` on every `[devices].ui` device and theme in the pinned Chrome; layout rules, console errors and layout shift, then pixels against the references; report on failure |
| `shakedown update --screens a,b` | rewrites only the named references, and only the files whose pixels changed. `all` only for a first generation |
| `shakedown browser install` / `path` | the pinned Chrome for Testing 149.0.7827.55 (macOS arm64 or Linux x86_64; sha256 checked, unpacked with `ditto` or, on Linux, `unzip` into `~/Library/Caches/shakedown/chrome/` or `~/.cache/shakedown/chrome/`; Linux Chrome needs `sudo apt install libnss3 libnspr4 libasound2t64`). `ui` installs it on first use |
| `shakedown sweep [--base URL] [--routes a,b] [--pack] [--rebase]` | every `[sweep]` page on every sweep device and theme, headless: layout rules and shift fail it; pixels are diffed against a local baseline and only reported. `--pack` writes the review pack |
| `shakedown smoke [--json]` | starts the production Docker image through the repo's own `make` targets and checks it over HTTP; always stops it (`[smoke]`, below) |
| `shakedown devices [list\|json\|serve]` | the one device list (18 devices, portrait and landscape); `serve` is the interactive side-by-side viewer |
| `shakedown doctor` | checks Rust, a shell, `strip`, Xcode, simulator runtimes, Java, the Android SDK, the pinned Chrome and disk use |

`--config PATH` points at a `shakedown.toml`; without it the nearest one (searching upward) is used, and its
directory is the root every command runs in.

Output is one line per step (`ok test 12.3s`, `FAIL clippy exit 101`, `skip x (clippy did not pass)`), then the last
40 lines of each failure, then the report path. Everything goes to `target/shakedown/`: `check/<step>.log` for every
step, `check/report.html` on failure, `summary.json` with `--json` (no times, so it is stable).

Exit codes: 0 pass, 1 a check failed, 2 config or usage error, 3 environment missing.

## `shakedown.toml`

At the repo root.

```toml
shakedown = "0.4.3"        # the tool version this repo expects (exact)
project   = "example"

[check]
steps = [
  { name = "fmt",    run = "cargo fmt --check" },
  { name = "clippy", run = "cargo clippy --all-targets -- -D warnings" },
  { name = "test",   run = "cargo test", after = ["clippy"] },
  { name = "audit",  run = "cargo audit --deny warnings", manual = true },
]
builtin = ["budgets.sizes"]   # tool-run steps that join the same graph

[budgets.sizes]
"web main.css gzip" = { file = "bin/static/stylesheet/main.*.css", gzip = 41000 }
"app raw"           = { file = "bin/app.js", raw = 100000 }
"iOS stripped"      = { file = "target/aarch64-apple-ios/release/libapp.a", strip = true, max = 2900000 }
```

`[check].steps`: each step has a unique `name` and a `run` command, run with `sh -c` from the root, with
`SHAKEDOWN=1` set and stdout and stderr captured together.

- `after = [...]`: steps that must pass first. A step whose dependency failed is skipped, never run.
- `manual = true`: runs only when named by `--only` (for example `shakedown check --only audit`). The default gate
  skips it.
- `--only a,b` runs just those steps (dependencies outside the selection are not waited for). Unknown names are a
  config error.

`[check].builtin`: `"budgets.sizes"` and `"ui.web"`. An artifact that was not built is a failure everywhere (a
skipped artifact is never a pass), in `check` and in `shakedown budgets sizes`. `check --json` reports only the steps
that ran.

`[budgets.sizes]`: one entry per artifact, keyed by label. `file` is relative to the root; its last component may
hold one `*` for hashed names and must then match exactly one file. Ceilings, all in exact bytes: `raw`, `gzip`
(level 9), `brotli` (quality 11), and `max` (raw size, or the size after `strip -S -x` when `strip = true`).

Sections of later phases are accepted and ignored with a note; typos inside the
sections below are errors.

## UI checks

```toml
include = ["ui/screens/*.toml"]      # more files, holding [[screen]] entries only (`*` in the file name only)

[server]
start = "make ui_server PORT={port} DB={db}"   # stays in the foreground; {port} is free, {db} a per-checkout name
ready = "/healthz"                              # default "/"; any answer under 500
world = "fake"                                  # fake (default): every /api call from fixtures. seeded: the real server
seed  = "make ui_seed DB={db}"                  # optional, runs once the server is ready
drop_db  = "make ui_db_drop DB={db}"            # drops the run's database on every way a run ends (success, failure, panic, signal)
list_dbs = "make ui_db_list"                    # prints this machine's throwaway databases; at startup those named
                                                # shakedown_<project>_<hash>_p<pid> with a dead pid are dropped (never a live run's)
now   = "2026-01-15T12:00:00Z"                  # fixed clock: SHAKEDOWN_NOW for the server, frozen Date in the page
api   = "/api/"                                 # fake world: paths under this must be faked
timeout = 120                                   # seconds to wait for ready

[devices]
ui = ["phone", "tablet", "desktop"]   # names from `shakedown devices`
themes = ["light", "dark"]

[players]                              # seeded local accounts, never production
accounts = ["ann", "ben"]
signin = "/__dev/signin?as={player}"   # each shot is its own browser context; a player signs in once per browser (a shot with `act` signs in fresh in its own context)
parallel = 3                           # shots one player is in at once (default 3)

[ui]
refs = "ui/refs"                       # references live in <refs>/web/<screen>@<device>-<theme>.png, plus env.json

[layout]
ignore = [{ rule = "clipped-text", match = ".marquee", why = "scrolls by design" }]   # a why is required

[[screen]]
name = "menu"                          # lower case, digits, -
path = "/"
player = "ann"                         # default "guest"
routes = { "GET /api/v1/me" = "fixtures/me.json", "GET /api/v1/x" = { file = "fixtures/x.json", status = 404 } }
act = [{ click = "[data-acct] button" }, { type = { selector = "input", text = "hi" } }, { press = "Enter" },
       { wait_text = "Your profile" }, { wait_gone = ".spin" }, { scroll = "footer" }]
allow_console = [{ contains = "expected message", why = "the test plants it" }]
devices = ["phone"]                    # optional: only these
```

A shot with `act` signs in fresh instead of using the player's cached cookies, in a context of its own that is
disposed before the shot starts, so the sign-in page's storage and service workers never reach the shot. Every shot
still signs in as the same account as the player's other shots, so act steps must not end every session, delete the
account, or rotate sessions server-side; use a dedicated player for those.

The page is made deterministic: fixed clock and seeded random (`collector.js`, injected before any page script),
timers of a second or more never fire, no intervals, animations and caret frozen, reduced motion, UTC, en-US,
density 1, WebRTC and popups blocked in every frame (a guard on its own CDP connection attaches to every iframe, popup and worker before it runs and reports through a CDP binding that page code cannot reach), every off-origin request blocked (images get a flat stand-in), a request for `/favicon.ico` ignored. In a
fake world answers arrive after 150 ms and an unfaked or never-made call fails the screen. A shot is taken when
two captures in a row are byte-identical. Fixtures may hold `"$now"`, replaced by the fixed clock in ms.

Layout rules (Rust, on the tree the page reports): `sideways-scroll`, `off-screen`, `clipped-text`,
`text-overlap`, `text-min-size` (under 11 px), `tap-min-size` (under 24 px, inline links exempt) and
`layout-shift` (shifts of 0.0005 or more, taken before any action). Other findings: `console-error`,
`page-error`, `network`, `unfaked-call`, `never-called`, `webrtc` (any RTCPeerConnection, in any frame), `popup` (any new window; closed at once, never allow-listable), `action-failed`, `ignore-without-why`. A shot that never settles (requests or pixels keep changing) is retried once like any
browser failure; a pass on the retry is reported as flaky, a second stall is `page-failed`.
Opt out per element with `data-shakedown-ignore="rule" data-shakedown-why="reason"` (a horizontal scroller is
`data-shakedown-scroll-x`), or per selector in `[layout].ignore`.

`ui web --repeat N` runs everything N times, each in a fresh Chrome, and fails if any run differs from the first in
findings or in the pixel verdict (the compare against run 1). Chrome rescales and clips images (avatars in circles)
with an antialiasing race that moves a few edge pixels in about one shot in fifteen; those shots are listed in a
note, not failed. A difference beyond the tolerance keeps both images and the diff in `target/shakedown/web/repeat/`. As a check step,
list `ui.web` under `[check].builtin`. Exit codes: 0 pass, 1 a check failed, 2 config or usage, 3 environment
(Chrome, server, seed). `--devices a,b` limits a `ui` run to some devices for a quick look; `SHAKEDOWN_PARALLEL=N` sets the pages drawn at once (default one per core, at most 16; `SHAKEDOWN_BROWSERS=N` sets the Chromes they are spread over, default one per 2 pages).

## Sweep

```toml
[devices]
sweep = "all"                # default: every device; or a list of names
themes = ["light", "dark"]

[sweep]
routes = [
  "/sign-in",                                          # a guest
  { path = "/account", player = "ann" },               # a signed-in player ([players].accounts)
  { path = "/x?y=1", name = "x-y", allow_console = [{ contains = "status of 404", why = "a 404 by design" }] },
]
groups = [{ name = "lobby", path = "/lobby/test", players = 4 }]   # frame i is accounts[i mod 4]
```

`shakedown sweep` draws every page on every device in both themes, with the same page, rules and settle as `ui web`
(fake worlds take `routes = { "GET /api/v1/me" = "fixtures/me.json" }` on a page). Layout rules and layout shift fail
it. Each picture is also compared with the local baseline in `target/shakedown/sweep/baseline/<os>/` (`macos`, `linux` or `windows`, never committed; font metrics differ per OS, so each has its own. Migration from 0.4.2: on macOS the old `baseline/<id>.png` is still read and moved to `baseline/macos/` on the next run; Linux and Windows start fresh):
the first sweep makes the baseline, later ones report what changed (`pixels: N changed against the baseline`), and
`--rebase` makes the current run the baseline. `report.html` lists every shot with a finding or a change.
`--base http://127.0.0.1:PORT` draws against a server you already run; any other host is refused.
Exit codes as for `ui`.

`--pack` writes `target/shakedown/review/`: `manifest.json` (routes, devices, themes, findings, pixel changes) and one
contact sheet per route per theme, `<route>-<theme>.png`: every device tiled at one scale with its name burned in, at
most 1568 px on the long edge, drawn in the pinned Chrome (a tile with findings is outlined red). The Claude skill
`shakedown-review` (goddtriffin/skills) hands the pack to a fresh Sonnet helper.

## Device viewer

`shakedown devices serve [--target http://127.0.0.1:8790] [--port 8799] [--routes pages.txt]` then open
`http://localhost:8799/__devices?path=/`. Every device in its own iframe (the browser window is never resized),
light and dark, the layout rules per frame (the same Rust ones as the sweep), a page picker (`--routes` file,
`[sweep]` pages), and frames that follow a click made in any frame. It is a proxy bound to 127.0.0.1 only, for local
apps only: it strips `X-Frame-Options` and `frame-ancestors`, and injects the collector into every page. Frame `i`
loads from `p<i+1>.localhost:<port>`, so each frame has its own cookies: "Sign frames in as players" signs frame `i`
in as `[players].accounts[i mod n]` through `[players].signin`. The theme toggle sets the frame's `color-scheme`; the
sweep is the exact check of `prefers-color-scheme`.

## Pixel compare

A port of the compare used by Boggledygook, so its references stay valid:

- A pixel differs when any channel moves by more than 16.
- A screen passes when at most 0.1% of its pixels differ and no 24x24 block has more than 20% differing pixels.
- A size change fails; a missing reference fails.
- References are 8-bit RGB with channels rounded to multiples of 8. Decoding accepts 8-bit RGB and RGBA PNGs.

## Tests

`cargo test -p shakedown` includes planted-break tests (`tests/planted_breaks.rs`): a 1 px shift, a colour change just
over the threshold, a budget one byte over, an unbuilt artifact and a failing step must each make the real binary
exit 1, and harmless drift must pass. `tests/planted_ui.rs` plants a break in a real page for every layout rule,
console and page errors, a request that leaves the machine, an unfaked and a never-made call, a failed action, a
layout shift, a 1 px move, a colour change just over the threshold and a missing reference, and requires `ui web`
to fail naming it; harmless drift, an ignore with a reason and a clean `--repeat 3` must pass. They need the pinned
Chrome (installed on first use). `tests/planted_sweep.rs` sweeps a small site: a page that scrolls sideways fails,
pixel changes are only reported, `--pack` writes a manifest and sheets within 1568 px, and a non-local `--base` is refused.

## `shakedown smoke`

The exact image that will ship, run the way production runs it. The tool never runs `docker`: `up` and `down` are
the repo's `make` targets (they run the already-built image with `--rm`, the repo label, `127.0.0.1` only, fake
local secrets and a throwaway database). Build the image from the commit that will land first, then smoke it.

```toml
[smoke]
up     = "make smoke_up PORT={port}"    # starts the image, prints its digest (sha256: and 64 hex digits)
down   = "make smoke_down PORT={port}"  # removes everything up made; safe after a half-finished up
logs   = "make smoke_logs PORT={port}"  # optional: prints the container's output (kept even after a crash)
ready  = "/health"                      # answers below 500 once the container is up (default "/")
timeout = 30                            # seconds to wait for ready (default 30)
routes = ["/sign-in", "/health"]        # pages the built-in checks cover, besides the paths of `checks`
checks = [
  { path = "/", status = 303, location = "/sign-in" },
  { path = "/sign-in", contains = "Sign in", content_type = "text/html", max_bytes = 60000 },
]
headers   = ["content-security-policy", "x-content-type-options", "referrer-policy"]  # the default; [] turns off
min_cache = 86400                       # least max-age an asset must grant (default; 0 turns off)
log_ignore = ["known noisy text"]       # log lines (substrings) left out of the error scan
sweep = true                            # the layout sweep against the container (the `[sweep]` pages); skipped when there is no `[sweep]` section
```

Steps, one output line each: `smoke.up` (records the digest), `smoke.ready`, `smoke.http`, `smoke.checks`,
`smoke.sweep`, `smoke.logs`, `smoke.down`.

- `smoke.sweep` (with `sweep = true`): every `[sweep]` page on every sweep device and theme, drawn by `shakedown sweep`'s
  code against `http://127.0.0.1:{port}` with no world of its own. A layout finding fails the step; pixel changes
  against the dev-server baseline are left to `shakedown sweep`. Signed-in pages sign in through `[players].signin`,
  so the production image must serve that path (or keep those pages out of `[sweep]`).

- `smoke.http`, on every route and every `checks` path: no 5xx; the `headers` present; no `localhost`, `127.0.0.1`,
  `0.0.0.0` or `[::1]` in served HTML, JSON or XML; and every script, stylesheet, icon, image and media file the
  HTML loads from the container answers 200 with `max-age` of at least `min_cache` (or `immutable`).
  Assets on other hosts are not fetched.
- `smoke.checks`: `status` (default 200; redirects are not followed), `contains` (body), `location` (the
  `Location` header, as a path or a full URL), `content_type` (substring), `max_bytes`.
- `smoke.logs`: a log line with the word `error`, `panic`, `panicked` or `fatal` fails the step.
- `smoke.down` runs on every exit: a pass, a failed check, a panic, a failed `up`, and the first Ctrl-C or SIGTERM.
  A second signal ends the process at once, as in `ui`. A `down` that fails is a failure.

Exit codes: 0 pass, 1 a step failed (including `up` and `down`), 2 no `[smoke]` section or an `up` that names a
`make` target the Makefile does not have, 3 a tool `up` needs is not installed. `--json` writes
`target/shakedown/smoke/summary.json` (with the digest); `up.log`, `down.log`, `container.log` and, on failure,
`report.html` are in the same folder.

## Budgets from a shared file, unbuilt artifacts

```toml
[budgets]
file = "scripts/budgets.json"   # from the directory holding shakedown.toml
skip_unbuilt = false            # true (or --skip-unbuilt): a missing artifact prints "skipped (not built)" and passes
[budgets.files]                 # which artifact each numeric label in the JSON measures
"web wasm gzip" = "web/pkg/*.wasm"                        # metric = the label's last word: raw, gzip or brotli
"iOS stripped per arch" = { file = "ios/libcore.a", strip = true }   # any other label: the plain (or stripped) size
```

The JSON's `"sizes"` object (the shape boggledygook's `budgets.json` uses, which also feeds vitals through its
`"timing"` object, ignored here) maps a label to a byte ceiling. A number is measured only when `[budgets.files]` names
its label; numbers with no mapping belong to another tool. An object value is a complete `[budgets.sizes]` table
(`file`, `raw`, `gzip`, `brotli`, `strip`, `max`). A label in both `[budgets.sizes]` and the file is an error. A
missing artifact fails by default; an over-budget one and one that cannot be measured fail even with `skip_unbuilt`.

## Text-metrics tolerance

```toml
[layout]
text_tolerance_px = 2   # default 0 (off)
```

Fonts measure differently per OS. With a tolerance, `clipped-text` ignores text that overflows its box by no more than
this many px, and `layout-shift` ignores a shift in which no element moves more than this. A shift the browser gave no
element positions for is never forgiven. Applies to `ui`, `sweep` and `devices serve`.
