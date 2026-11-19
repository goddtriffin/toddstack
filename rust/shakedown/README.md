# shakedown

One local testing tool for every project. Part of [toddstack](https://github.com/goddtriffin/toddstack).
A shakedown run is the trial before real service: the checks you run before anything ships. Fast, deterministic,
local only (macOS). Design record: [`docs/testing-tool/DESIGN.md`](../../docs/testing-tool/DESIGN.md).

Phase 1 ships: the `check` step runner, `budgets sizes`, `pixels` compare, the report and `doctor`. Browser
screenshots, the device sweep, smoke tests and simulators arrive in later phases.

## Install

```sh
cargo install shakedown@0.1.0 --locked          # from crates.io
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
shakedown = "0.1.0"        # the tool version this repo expects (exact)
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

`[check].builtin`: `"budgets.sizes"` is the only one in this version. In `check`, an artifact that was not built is a
failure (a skipped artifact is never a pass); `shakedown budgets sizes` alone reports it as `skipped` and exits 0.

`[budgets.sizes]`: one entry per artifact, keyed by label. `file` is relative to the root; its last component may
hold one `*` for hashed names and must then match exactly one file. Ceilings, all in exact bytes: `raw`, `gzip`
(level 9), `brotli` (quality 11), and `max` (raw size, or the size after `strip -S -x` when `strip = true`).

Any other section in the design (`[server]`, `[devices]`, `[[screen]]`, ...) is accepted and ignored with a note
until the phase that reads it; typos inside the sections above are errors. `include = [...]` is not read yet.

## Pixel compare

A port of the compare used by Boggledygook, so its references stay valid:

- A pixel differs when any channel moves by more than 16.
- A screen passes when at most 0.1% of its pixels differ and no 24x24 block has more than 20% differing pixels.
- A size change fails; a missing reference fails.
- References are 8-bit RGB with channels rounded to multiples of 8. Decoding accepts 8-bit RGB and RGBA PNGs.

## Tests

`cargo test -p shakedown` includes planted-break tests (`tests/planted_breaks.rs`): a 1 px shift, a colour change just
over the threshold, a budget one byte over, an unbuilt artifact and a failing step must each make the real binary
exit 1, and harmless drift must pass.
