---
name: rollout
description: Use when shipping finished work in this repo — cutting a version, bumping Cargo.toml or deno.jsonc for a release, running the end-to-end smoke test, building or pushing the Docker image, opening the pull request, tagging, drafting a GitHub release, or publishing webserver-base to crates.io or @todd/webserver-base to JSR. Also use when asked whether a change is ready to ship, whether a version bump or publish is needed at all, when picking the next version number, or when any question arises about which rollout step comes next.
---

# Rollout

Shipping this repo is a **fixed thirteen-step sequence**. The steps never
reorder and never merge. Which of them run is decided by what changed — see
step 0 — but a step that is in scope is never skipped because a change "looks
small."

Writing the code is the **contribution-guide** skill. This one starts once the
change is finished.

## Step 0: what changed decides what runs

**Do not run steps that nothing changed.** Version bumps, smoke tests, image
pushes and publishes are each earned by a change to what they cover. A
docs-only change that bumps a version ships a release consumers must read for
nothing, and re-pushes six production images for nothing.

Diff against `main` and classify:

| what changed | earns |
|---|---|
| `rust/webserver_base/**` (the Rust lib) | Cargo bump, tag, release, crates.io publish — **and** the image steps, because the binary embeds the library |
| `typescript/webserver-base/**` (the TS lib) | `typescript/deno.jsonc` PATCH bump, JSR publish |
| `typescript/accounts/**` (TS `toddstack-accounts`) | PATCH bump of `deno.jsonc`, `package.json` and `version.ts` together (a test checks they agree), CI then publishes to JSR and npm on merge. Check the Rust crate has the same features (parity, see `contribution-guide`) |
| `rust/template_web_server/**` (the crate, its `html/`, `static/`, `Dockerfile`), `rust/Cargo.lock`, `Makefile` | the image steps: build, smoke test, push |
| any `README.md`, `AGENTS.md`, `CLAUDE.md`, `.claude/**`, other docs | nothing but the pull request |

Then run only the steps those rows earn, **in the order below**, skipping the
rest. The order never changes; only which steps are present.

Two consequences worth stating outright:

- **A docs-only change is `make lint test` and a pull request.** No bump, no
  Docker, no tag, no publish.
- **A Rust-lib change always drags the image with it.** `template-web-server`
  compiles `webserver_base` in, so the built image is stale the moment the
  library moves. Never publish the crate without pushing the image.

The tag and the GitHub release track the **Rust crate version**, so they are cut
only when that version moves. A TS-only change is a commit plus a JSR publish,
with no tag — there is no new Rust version to name one after.

When in doubt about a row, run the step. The cost of a needless smoke test is
minutes; the cost of a skipped one is a stale production image.

## The order

The constraint that fixes everything else: **the production deploy watches
commits to `main` and pulls `:latest`.** The image must already be in the
registry when the PR is merged. Push the image, *then* hand over the PR.

Reverse those two and the deploy fires against the new commit while the registry
still holds the old image — production runs stale code, with no error anywhere.

| # | Step | Command |
|---|---|---|
| 1 | **Bump versions** | edit `rust/Cargo.toml` + `typescript/deno.jsonc` |
| 2 | Green gate | `make lint test` |
| 3 | Build the image | `make docker_build` |
| 4 | E2E smoke test | `make docker_run`, then browse |
| 5 | Tear down | `make docker_stop` |
| 6 | **Push the image** | `make docker_push` |
| 7 | **Open the PR** | commit on a branch, `git push -u`, `gh pr create` |
| 8 | **Merge the PR** | `gh pr merge --merge` |
| 9 | **CI publishes** | nothing to run: `.github/workflows/publish.yml` tags, releases and publishes on merge |
| 10 | Release notes | edit the release CI made (`gh release edit`) when there are breaking changes |
| 11 | Verify | see "Verify what actually landed" |

Every change ships through a pull request. Todd has authorized agents to merge
it themselves once steps 2–6 are done (and to commit straight to `main` when a
change earns no image or publish steps, e.g. docs).

---

## 1. Bump versions — first, before anything is built

The two libraries version independently:

- **The Rust crate is bumped by PATCH only**, in `[workspace.package] version`,
  however breaking the change. The user is its only consumer and wants it that
  way. Bump minor or major only when the user explicitly says to; silence means
  patch. Breaking changes are covered by the release notes, not the number.
- **`deno.jsonc` is bumped by PATCH only.** The TypeScript library is small and
  its consumers pin a caret range; a minor or major bump forces every project to
  edit its import map for no benefit.

Bump from what is on `main` now, not from what is published — the two drift when
a version is committed but never released.

**Why this is step 1 and not step 2.** Bumping mutates `Cargo.toml` and
`Cargo.lock`, which are build inputs, and `bootstrap!` resolves
`CARGO_PKG_VERSION` at *compile* time into the Sentry release string. Bump after
the green gate and the artifact you ship was never the artifact you verified.
Step 1 is the point after which nothing about the build changes until it ships.

## 2. Green gate

`make lint test` must be fully clean — see contribution-guide for what it covers
and for the rule that a lint failure in generated output is a bug in the
generator.

## 3. Build the image

`make docker_build`. Slow, because it is an emulated `linux/amd64` cross-build.
That is expected.

## 4. E2E smoke test

`make docker_run` prints the port it chose. `test.toddgriffin.me` maps to
`127.0.0.1` in `/etc/hosts`, so browse **`http://test.toddgriffin.me:PORT`**.

Confirm in the browser, not just with curl:

- **the home page** renders, styled, with the theme applied
- **a 404 URL** renders the real 404 page — this is the load-bearing check,
  because its content-hashed image proves the whole cache-buster pipeline
  survived the build
- the **console is clean**

Then the routes the layout depends on: `/api/v1/health`, `/robots.txt`,
`/humans.txt`, `/site.webmanifest`, `/sitemap.xml`, `/favicon.ico` — all 200,
an unknown path 404.

Finally, check the container's first log line names the version from step 1
(`release=template-web-server@X.Y.Z`). A mismatch means the image predates the
bump: go back to step 3.

## 5. Tear down

`make docker_stop`.

## 6. Push the image — before the PR is merged

`make docker_push` tags the single built image into **six** production Docker
Hub repos and pushes each: `scannable-codes-website`, `turnbased-website`,
`scribble-jump-website`, `video-game-recipe-book-website`,
`5dcheckers-server`, `5ddiplomacy-server`. (`vogue-bot-website` is not one of
them: since 2026-11-05 the `goddtriffin/vogue-bot` repo builds and pushes it.)

Say so plainly when reporting it — six live `:latest` tags are moving, not
one. Confirm every push reports the same digest; a differing digest means a repo
got a stale image.

## 7. Open the PR

Commit on a feature branch, push it, and `gh pr create`. Verify nothing under
`bin/`, `cache-buster.json`, or `static/script/generated/` is staged (anywhere in the tree) — all three
are build output. The PR body follows the same shape as the commit.

Match the existing commit style: a one-line summary, then a body of grouped
bullets that say *why* each change exists and what failure motivated it, closing
with `Bump Rust lib version to X.Y.Z and JSR lib version to A.B.C.`

## 8. Merge the PR

`gh pr merge --merge`. The merge is the deploy, so only merge once the image is
pushed. If anything the image is built from changes after the push, go back to
step 2: the pushed image no longer matches the PR.

Once merged, `git switch main && git pull` before tagging, so the tag names the
merge commit.

## 9. CI publishes

Merging to `main` runs `.github/workflows/publish.yml`, which calls `scripts/publish.sh`. For every library
whose version is not on its registry yet it publishes it (crates.io, JSR, npm), then creates the tag and a
GitHub release with notes generated from the merged PRs. A version already published is skipped, so a
merge without a bump publishes nothing. Pull requests run `scripts/publish.sh check`, which fails when a
library's files changed but its version was not bumped.

No tokens: each registry trusts that workflow file through trusted publishing (set up once per package on
crates.io, npm and JSR). Todd has approved every publish in advance. Publishing is **irreversible**:
crates.io and npm allow yanking or deprecating, never reuse of a version.

Tags name their package (`webserver-base-vX.Y.Z`, `flyball-vX.Y.Z`, `toddstack-accounts-vX.Y.Z`) and follow
the **Rust** version; `@todd/webserver-base` alone (TS) gets no tag. Tags up to `v0.3.0` predate the rename.
Bare `X.Y.Z` tags are reserved: Swift Package Manager reads a repo's plain semver tags as its versions.

If a publish fails, fix the cause and re-run the workflow (`gh workflow run publish.yml`): it skips what is
already out. crates.io **requires** trusted publishing for these crates (Todd's setting), so a crate can only
be published by this workflow, never from a laptop token. npm and JSR can still be published locally in an
emergency, with the user's browser approval (`! scripts/publish.sh`).

## 10. Release notes

The generated notes list the merged PRs. When a consuming project must edit something to upgrade, add a
**Breaking changes** section with `gh release edit <tag> --notes-file ...`.

## 11. Dry run

`make publish_dry_run` still works for a packaging check before merging. Confirm the embedded assets are
in the crate: `(cd rust && cargo package --list -p webserver-base | grep '^assets/')` prints two lines.

---

## Verify what actually landed

Never report a publish from the command's own output alone. crates.io needs a
User-Agent or it returns non-JSON:

```sh
curl -s -H "User-Agent: release-check (tgriffin115@gmail.com)" \
  https://crates.io/api/v1/crates/webserver-base | grep -o '"max_version":"[^"]*"'
curl -s https://jsr.io/@todd/webserver-base/meta.json | grep -o '"latest":"[^"]*"'
```

## If a step fails

Everything through step 5 is free to retry. From step 6 on, each step is visible
to someone else, so stop and report rather than pressing ahead — a half-rolled-out
version is worth a human decision.

The asymmetry worth knowing: steps 6 and 8 are the deploy. If step 12 fails, the
new code is *already in production* and only the library publish is missing,
which is recoverable. That is why step 11 exists.
