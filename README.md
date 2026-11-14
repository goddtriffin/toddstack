# toddstack

Shared code for all of Todd Everett Griffin's projects, in every language they are
written in. Each language has its own directory; each package inside it is
published on its own and documented in its own README.

## Packages

| package | language | published as | docs |
|---|---|---|---|
| `webserver-base` | Rust | [crates.io](https://crates.io/crates/webserver-base) | [`rust/README.md`](rust/README.md) |
| `@todd/webserver-base` | TypeScript | [JSR](https://jsr.io/@todd/webserver-base) | [`typescript/README.md`](typescript/README.md) |
| `flyball` | Rust | [crates.io](https://crates.io/crates/flyball) | [`rust/flyball/README.md`](rust/flyball/README.md) |
| `toddstack-accounts` | Rust | [crates.io](https://crates.io/crates/toddstack-accounts) | [`rust/toddstack_accounts/README.md`](rust/toddstack_accounts/README.md) |
| `toddstack-accounts` | TypeScript | [JSR](https://jsr.io/@todd/toddstack-accounts) `@todd/toddstack-accounts`, [npm](https://www.npmjs.com/package/toddstack-accounts) `toddstack-accounts` | [`typescript/accounts/README.md`](typescript/accounts/README.md) |
| `template-web-server` | Rust | not published; Docker image for several sites | [`rust/template_web_server/`](rust/template_web_server/) |

## Layout

```
rust/              Cargo workspace: every Rust crate
  webserver_base/        the web server library
  template_web_server/   a complete site built on it (crate + html/ + static/ + Dockerfile)
  flyball/               keyed rate limiting: one trait, in-memory and mock implementations
  toddstack_accounts/    client for the shared accounts: sign-in, tokens, webhooks, friends
typescript/        JSR packages (deno.jsonc is the package config)
  webserver-base/        the browser/build-side half of webserver-base
  accounts/              toddstack-accounts for TypeScript (own deno.jsonc + package.json: JSR and npm)
```

## Parity across languages

A package that exists in more than one language has **the same features in every language**, so every project
behaves the same whatever it is written in. Each one is written idiomatically for its language (names, errors,
async style and internals follow that language), but no language gets a feature the others lack. A change to
one adds the feature to the others in the same change, or names the gap in both READMEs.

New languages (Swift, Kotlin, ...) get their own top-level directory when the
first project needs one.

## Working on this repo

Everything runs from the repo root through the `Makefile` (`make help`).

| skill | use it when |
|---|---|
| [`contribution-guide`](.claude/skills/contribution-guide/SKILL.md) | writing, reviewing, refactoring, testing or documenting **any** code here — invoke it at the start of every change, however small |
| [`rollout`](.claude/skills/rollout/SKILL.md) | shipping a finished change: patch bump, E2E smoke test, Docker push, pull request, tag, GitHub release, crates.io and JSR publishes |

Rules live in the skills, not here. Invoke `contribution-guide` before any
change to this repo.

Releases are tagged per package: `webserver-base-vX.Y.Z`, `toddstack-accounts-vX.Y.Z` (tags up to `v0.3.0`
predate the rename from `goddtriffin/webserver-base`).
