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
top-level directory per language (`rust/`, and the frozen `typescript/`). The root `README.md`
maps the packages; each package's own README (`rust/README.md`,
`typescript/README.md`) is its consumer-facing reference: keep it current as
part of any feature change. Run everything from the repo root via the
`Makefile`.

## SDK policy (Todd, 2026-11-18)

Rust is the only SDK that gets new features. The TypeScript SDK (`typescript/accounts`, JSR `@todd/toddstack-accounts`,
npm `toddstack-accounts`) is **frozen: security fixes only**, to be deleted once Swarm-Driven Development's server is
ported to Rust. Swift and Kotlin SDKs are deleted. The Rust crate is `toddstack-accounts-server-sdk` (formerly
`toddstack-accounts`; the old name stays on crates.io at 0.9.0).
