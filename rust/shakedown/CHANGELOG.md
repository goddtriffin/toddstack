# Changelog

## 0.4.4

- `smoke.http` flags `localhost`, `127.0.0.1`, `0.0.0.0` and `[::1]` only in URL context (after `//`), no longer as a
  bare string, so hostname comparisons in vendored bundles (the Sentry browser SDK's `"localhost"===i`) pass. A real
  `http://localhost:3000/api` still fails.
- `[smoke].http_allow = ["<path glob>"]`: paths whose local-host URLs the scan skips (`*` matches any characters).

## 0.4.3

- `budgets sizes --skip-unbuilt` (also on `check`) and `[budgets].skip_unbuilt = true`: a budget whose artifact does
  not exist prints `skipped (not built) <label>: <file>` and does not fail. Off by default; an over-budget artifact and
  an artifact that cannot be measured still fail.
- `[budgets].file`: ceilings are read from one shared JSON file (`"sizes"`, as boggledygook's `scripts/budgets.json`
  has it). A numeric label is measured by the artifact `[budgets.files]` maps it to; an object entry is a full
  `[budgets.sizes]` table. Numbers with no mapping are another tool's and are ignored. Existing configs work unchanged.
- Sweep baselines are per operating system: `target/shakedown/sweep/baseline/<macos|linux|windows>/`. On macOS the
  old `baseline/<id>.png` is still read and is moved to the new path on the next run (a note says how many); other
  systems never read it, so a Mac baseline no longer produces false pixel changes on Linux.
- `[layout].text_tolerance_px` (off by default): clipped text that overflows its box by no more than this, and a
  layout shift in which no element moves more, are not findings. The collector now reports `clip_px` and `max_px`.

## 0.4.2

- `shakedown smoke`, the production-image smoke test (phase 4): starts the image through the repo's own `make`
  targets, checks it over HTTP, always stops it.
- Review fixes: teardown on every exit path, no false passes, and smoke runs use their own sweep directory so the dev
  sweep's baselines and pictures are left alone.

## 0.4.1

- The pinned Chrome installs on Linux x86_64 (`unzip` into `~/.cache/shakedown/chrome/`).

## 0.4.0

- One browser context per player, signed in once, with tabs per device and theme; act shots sign in fresh, with the
  player's cookies copied in; no state leaks between shots and tabs are always closed.
- A shot takes its player's turn before a shared slot; routes that open no capped connection can set `turn = false`.
- `SHAKEDOWN_SHUFFLE` order gate; the guard reports per page, prunes armed pages and drops pending arming on detach.
- The sweep baseline is written after the run; a retry resets the player; no cookies is an error and Chrome timeouts
  keep cookies.
- The viewer injects its collector after the doctype (standards mode stays), frame reports retry, and frames that never
  report show no report.
- Pack sheets are built and captured in parallel; shots are judged as they are drawn, and identical pictures skip the
  compare.
