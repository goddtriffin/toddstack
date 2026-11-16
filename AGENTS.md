# AGENTS.md

Working rules for this repository. `CLAUDE.md` only imports this file.

This file is a router. It holds no rules of its own — every rule lives in the
skill that owns it, listed below.

## Skills

| skill | use it when |
|---|---|
| [`contribution-guide`](.claude/skills/contribution-guide/SKILL.md) | writing, reviewing, refactoring, testing or documenting **any** code here — invoke it at the start of every change, however small |
| [`rollout`](.claude/skills/rollout/SKILL.md) | shipping a finished change: patch bump, E2E smoke test, Docker push, pull request, tag, GitHub release, crates.io and JSR publishes |

**Invoke `contribution-guide` before touching anything in this repo.** Most of
what it points to exists because breaking it fails silently, remotely, or only
for consumers of the published crate — so the cost of skipping it is not visible
at the time you skip it.

This repo is **toddstack**: shared code for all of Todd's projects, one
top-level directory per language (`rust/`, `swift/`, `kotlin/`, `typescript/`). The root `README.md`
maps the packages; each package's own README (`rust/README.md`,
`typescript/README.md`) is its consumer-facing reference: keep it current as
part of any feature change. Run everything from the repo root via the
`Makefile`.

## Releasing the native SDKs

Swift and Kotlin share the **bare `X.Y.Z` tag** (no `v`, no prefix): SwiftPM reads it, and JitPack builds the Kotlin
SDK from it (`jitpack.yml`), as `com.github.goddtriffin.toddstack:toddstack-accounts:X.Y.Z`. Rust and TypeScript
`toddstack-accounts` keep their own `toddstack-accounts-vX.Y.Z` tags, made by CI.

**How to release Kotlin:** set `version` in `kotlin/accounts/build.gradle.kts` and `toddstackAccountsVersion` in
`swift/accounts/.../Version.swift` to the new `X.Y.Z`, `make lint test`, merge, then `git tag X.Y.Z && git push origin
X.Y.Z` and warm JitPack at <https://jitpack.io/#goddtriffin/toddstack>. Never tag from a branch. Maven Central comes
later (it waits on Todd: Sonatype namespace, DNS TXT record, GPG key, token); until then JitPack is the source.
