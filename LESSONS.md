# LESSONS.md

One line per gotcha, newest last, max 60 lines.

- `make test` builds the template site first (`assets`), then runs Rust and Deno suites; it takes minutes, so run it through `stokehold`.
- `make test` runs `npx -y` for the npm build check, so the verify sandbox needs the npm registry in its egress list.
- `make lint` runs `cargo fmt` and `deno fmt`, which rewrite files; commit what they change.
- `assets/**` must stay in `rust/Cargo.toml`'s `include` list, or the crate compiles locally and fails for consumers.
- Never hand-edit `cache-buster.json`, `static/script/generated/` or anything under `bin/`; fix the generator.
- Never reintroduce a fallback to an un-hashed asset path; it turns a loud misconfiguration into a silent 404.
- The wasm glue needs the `wasm32-unknown-unknown` target; `make test` builds it.
- Docs-only changes need no version bump; code changes to a published crate follow the rollout skill.
- Never tag `dev`, `docker_push`, `publish_dry_run` or any server in `@factory`.
