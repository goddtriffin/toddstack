# DESIGN.md

Decisions for toddstack, in the form "we do X because Y, not Z because W". The deep rules live in
`.claude/skills/contribution-guide/reference/architecture.md`; this file is the short index shiftboss reads first.

- **Rust gets new features, TypeScript is frozen (Todd, 2026-11-18).** One SDK to keep right, not feature parity
  across languages. TypeScript takes security fixes only until SDD's server is ported to Rust.
- **webserver-base is opinionated.** It solves a problem once for every project, so a feature gets one
  non-configurable default, not a knob that can only hold one correct value.
- **Asset hashing happens at build time, never at startup.** An in-place restart that renamed files 404'd every asset.
- **The server writes nothing to disk.** A production image must run read-only.
- **Generated files are never hand-edited.** A failure in one is a bug in the generator under
  `rust/webserver_base/src/assets/`; fix it there and regenerate.
- **Every feature is in its package README in the same change.** An undocumented feature is a documentation bug.
- **Docker is for the rollout smoke test only.** A cold `linux/amd64` build compiles the whole tree under
  emulation; local work uses `make dev`.
- **Rebase-only merges (the repo setting).** PRs land as rebased commits; no merge or squash commits on main.
- **shiftboss runs only `@factory` Makefile targets** (`lint`, `test`). `dev`, `docker_*`, `publish_*` and anything
  long-running are never tagged.
