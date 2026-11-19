# shakedown

One local testing tool for every project. Part of [toddstack](https://github.com/goddtriffin/toddstack).
A shakedown run is the trial before real service: the checks you run before anything ships. Fast, deterministic,
local only (macOS). Design record: [`docs/testing-tool/DESIGN.md`](../../docs/testing-tool/DESIGN.md).

Shipped: the `check` step runner, `budgets sizes`, `pixels` compare, the report, `doctor` (phase 1); the pinned
Chrome, `ui web` with layout rules and approved references, `update`, fake and seeded worlds, `include` (phase 2).
The device sweep, smoke tests and simulators arrive in later phases.

## Install

```sh
cargo install shakedown@0.2.0 --locked          # from crates.io
cargo install --path rust/shakedown --locked    # from a toddstack checkout
shakedown doctor                                # what is installed, what is missing, and the fix for each miss
```

Pin the version in each project's `shakedown.toml`; a mismatch stops with the exact install command.

## Commands

| command | does |
|---|---|
| `shakedown check [--only a,b] [--json]` | runs `[check]`: shell steps as a graph, independent steps in parallel |
| `shakedown budgets sizes` | exact byte ceilings from `[budgets.sizes]` |
| `shakedown pixels compare <reference.png> <actual.png> [--out DIR]` | the pixel compare; on failure writes the diff image and `report.html` to `DIR` (default `target/shakedown/pixels`) |
| `shakedown pixels encode <in.png> <out.png> [--step 8]` | stores a PNG the way references are stored |
| `shakedown ui web [--screens a,b] [--repeat N] [--json]` | draws every `[[screen]]` on every `[devices].ui` device and theme in the pinned Chrome; layout rules, console errors and layout shift, then pixels against the references; report on failure |
| `shakedown update --screens a,b` | rewrites only the named references, and only the files whose pixels changed. `all` only for a first generation |
| `shakedown browser install` / `path` | the pinned Chrome for Testing 149.0.7827.55 (sha256 checked, unpacked with `ditto` into `~/Library/Caches/shakedown/chrome/`). `ui` installs it on first use |
| `shakedown devices` | the one device list (18 devices, portrait and landscape) |
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
shakedown = "0.2.0"        # the tool version this repo expects (exact)
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

Sections of later phases (`[sweep]`, `[smoke]`, ...) are accepted and ignored with a note; typos inside the
sections below are errors.

## UI checks

```toml
include = ["ui/screens/*.toml"]      # more files, holding [[screen]] entries only (`*` in the file name only)

[server]
start = "make ui_server PORT={port} DB={db}"   # stays in the foreground; {port} is free, {db} a per-checkout name
ready = "/healthz"                              # default "/"; any answer under 500
world = "fake"                                  # fake (default): every /api call from fixtures. seeded: the real server
seed  = "make ui_seed DB={db}"                  # optional, runs once the server is ready
now   = "2026-01-15T12:00:00Z"                  # fixed clock: SHAKEDOWN_NOW for the server, frozen Date in the page
api   = "/api/"                                 # fake world: paths under this must be faked
timeout = 120                                   # seconds to wait for ready

[devices]
ui = ["phone", "tablet", "desktop"]   # names from `shakedown devices`
themes = ["light", "dark"]

[players]                              # seeded local accounts, never production
accounts = ["ann", "ben"]
signin = "/__dev/signin?as={player}"   # each shot is its own browser context, so its own player
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

The page is made deterministic: fixed clock and seeded random (`collector.js`, injected before any page script),
timers of a second or more never fire, no intervals, animations and caret frozen, reduced motion, UTC, en-US,
density 1, every off-origin request blocked (images get a flat stand-in), a request for `/favicon.ico` ignored. In a
fake world answers arrive after 150 ms and an unfaked or never-made call fails the screen. A shot is taken when
two captures in a row are byte-identical. Fixtures may hold `"$now"`, replaced by the fixed clock in ms.

Layout rules (Rust, on the tree the page reports): `sideways-scroll`, `off-screen`, `clipped-text`,
`text-overlap`, `text-min-size` (under 11 px), `tap-min-size` (under 24 px, inline links exempt) and
`layout-shift` (shifts of 0.0005 or more, taken before any action). Other findings: `console-error`,
`page-error`, `network`, `unfaked-call`, `never-called`, `never-settled`, `action-failed`, `ignore-without-why`.
Opt out per element with `data-shakedown-ignore="rule" data-shakedown-why="reason"` (a horizontal scroller is
`data-shakedown-scroll-x`), or per selector in `[layout].ignore`.

`ui web --repeat N` runs everything N times and fails if any run differs in pixels or findings. As a check step,
list `ui.web` under `[check].builtin`. Exit codes: 0 pass, 1 a check failed, 2 config or usage, 3 environment
(Chrome, server, seed).

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
Chrome (installed on first use).
