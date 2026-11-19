# toddstack

Shared code for all of Todd Everett Griffin's projects, in Rust (and a frozen TypeScript
package). Each language has its own directory; each package inside it is
published on its own and documented in its own README.

## Packages

| package | language | published as | docs |
|---|---|---|---|
| `webserver-base` | Rust | [crates.io](https://crates.io/crates/webserver-base) | [`rust/README.md`](rust/README.md) |
| `@todd/webserver-base` | TypeScript | [JSR](https://jsr.io/@todd/webserver-base) | [`typescript/README.md`](typescript/README.md) |
| `flyball` | Rust | [crates.io](https://crates.io/crates/flyball) | [`rust/flyball/README.md`](rust/flyball/README.md) |
| `toddstack-accounts-server-sdk` (formerly `toddstack-accounts`) | Rust | [crates.io](https://crates.io/crates/toddstack-accounts-server-sdk) | [`rust/toddstack_accounts_server_sdk/README.md`](rust/toddstack_accounts_server_sdk/README.md) |
| `toddstack-accounts` (**frozen**, security fixes only) | TypeScript | [JSR](https://jsr.io/@todd/toddstack-accounts) `@todd/toddstack-accounts`, [npm](https://www.npmjs.com/package/toddstack-accounts) `toddstack-accounts` | [`typescript/accounts/README.md`](typescript/accounts/README.md) |
| `toddstack-frontend-engine` | Rust | [crates.io](https://crates.io/crates/toddstack-frontend-engine) | [`rust/toddstack_frontend_engine/README.md`](rust/toddstack_frontend_engine/README.md) |
| `shakedown` | Rust | [crates.io](https://crates.io/crates/shakedown) | [`rust/shakedown/README.md`](rust/shakedown/README.md) |
| `template-web-server` | Rust | not published; Docker image for several sites | [`rust/template_web_server/`](rust/template_web_server/) |

## Layout

```
rust/              Cargo workspace: every Rust crate
  webserver_base/        the web server library
  template_web_server/   a complete site built on it (crate + html/ + static/ + Dockerfile)
  flyball/               keyed rate limiting: one trait, in-memory and mock implementations
  toddstack_accounts_server_sdk/  client for the shared accounts: sign-in, tokens, webhooks, friends
  shakedown/             the shared local testing tool (binary): check runner, budgets, pixel compare, doctor
  toddstack_frontend_engine/      app-agnostic Elm-style frontend core: NDJSON bridge, effects, dirty-key views, headless harness, wasm and UniFFI glue (+ js/ worker runner)
typescript/        JSR packages (deno.jsonc is the package config); FROZEN, see below
  webserver-base/        the browser/build-side half of webserver-base
  accounts/              toddstack-accounts for TypeScript, frozen (own deno.jsonc + package.json: JSR and npm)
```

## Rust only

**Rust is the only SDK that gets new features** (Todd, 2026-11-18). The Swift and Kotlin SDKs were deleted (nothing
used them). The TypeScript packages are **frozen**: security fixes only, to be deleted once Swarm-Driven
Development's server is ported to Rust. There is no feature parity rule.

## How a project adds in-app friend requests

1. **Server (Rust):** enable `toddstack-accounts-server-sdk`'s `axum` feature, implement `FriendsAuth` over your sessions (the
   caller's accounts access token, or `None` for a guest), and `app.merge(friends_router::router(accounts, auth))`.
   Request the `friends` scope at login. Routes: `/api/v1/friends/...` (table in the Rust README).
2. **Clients:** call the routes with the session the app already holds. Guests get `account_required`: show the
   sign-in upsell.
3. Show `message` from any error to the person as is; it is already worded for them.

## Working on this repo

Everything runs from the repo root through the `Makefile` (`make help`).

| skill | use it when |
|---|---|
| [`contribution-guide`](.claude/skills/contribution-guide/SKILL.md) | writing, reviewing, refactoring, testing or documenting **any** code here — invoke it at the start of every change, however small |
| [`rollout`](.claude/skills/rollout/SKILL.md) | shipping a finished change: patch bump, E2E smoke test, Docker push, pull request, tag, GitHub release, crates.io and JSR publishes |

Rules live in the skills, not here. Invoke `contribution-guide` before any
change to this repo.

Releases are tagged per package: `webserver-base-vX.Y.Z`, `toddstack-accounts-server-sdk-vX.Y.Z`, `toddstack-frontend-engine-vX.Y.Z`,
`toddstack-accounts-vX.Y.Z` (the frozen TypeScript package) (tags up to `v0.3.0` predate the rename from
`goddtriffin/webserver-base`). Bare `X.Y.Z` tags are unused.
