# shiftboss: design (v1)

Status: design, not built. 2026-11-20. Crate and binary `shiftboss` in `rust/shiftboss/`.

**Goal.** Ship as many features and fixes to prod as possible across Todd's servers, web apps and native apps, for
the fewest tokens. shiftboss is Todd's software factory: GitHub issues are the queue, deterministic Rust moves every
item through its stages, and an agent runs only where judgment is needed, as one fresh, short, headless CLI run.

**Scope.** shiftboss stops at "PR merged". CI on a PR and build/deploy after merge belong to Bellwether (onboarded
projects) or deploy-webhook (the rest). shiftboss only *listens* to their results: a red check becomes a fix step,
a failed deploy becomes a needs-Todd item. Nothing factory-specific goes into Bellwether or its GitHub App.

**Where it runs.** Todd's Windows rig (WSL Ubuntu, always on) and his Mac. Never the droplet.

Marked **[unverified]**: not checked against a live system.

## 1. Code vs agent boundary

The rule: if a step can be written as a function of its inputs, it is Rust. An agent gets a step only when the
output needs judgment, and the agent's only output is a JSON result plus (for edit steps) a diff in its worktree.

| Work | Who | Notes |
|---|---|---|
| Receive, verify, dedupe webhooks; normalize to events | code | HMAC, `X-GitHub-Delivery` dedupe |
| Decide the next stage of an item | code | pure `decide(item, event, policy) -> (item', commands)` |
| Labels, comments, issue body edits, PR open/update, merge | code | GitHub App installation token; agents never hold one |
| Pick the next item, host and model | code | priority, stage, traits, budget, repo tier |
| Worktrees, sandbox user, CLI argv, settings, timeouts, retries | code | |
| Allowed commands for an agent | code | derived from `@factory` Makefile targets on `origin/main` |
| Run verify (`make test`, `make lint`) | code | through `stokehold`, never by the agent's word |
| Check the diff (denied paths, size), commit as Todd, push | code | the agent's diff is data |
| Rebase a PR on a moved `main` | code | conflict becomes a fix step |
| Watch CI and deploy results | code | `check_run`, `status` events |
| Budget, stalls, needs-Todd list, 9am report, onboarding checks | code | |
| Discord message -> verbatim issue | code | via `botyard` |
| **Refine** an idea: flesh out, push back, ask questions, size it | agent (Opus, every tier) | writes the spec block, never code |
| **Build** the change | agent | edits files in its worktree only |
| **Review** the PR diff | agent (one per PR) | read-only, returns verdict + findings |
| **Fix** review findings, red CI, or a rebase conflict | agent (Sonnet) | edits files only |
| **Re-review** the fix diff only | agent (Sonnet) | read-only |
| Record a lesson worth keeping | agent proposes, code appends | `lessons` field, deduped into `LESSONS.md` in the PR |

We do everything else in code because code is free, repeatable and testable, not in an agent because each agent
turn re-reads its context at model rates and can be talked into things.

## 2. Events and the state machine

### Event sources (all push, no polling)

| Source | Event | Effect |
|---|---|---|
| GitHub App webhook | `issues` opened / labeled / unlabeled / edited / closed | new item, priority change, Todd override, cancel |
| | `issue_comment` created | Todd answers a question or sends `/shiftboss <stage>` |
| | `pull_request` closed+merged, synchronize, closed | merged -> `done`; outside pushes noted; closed -> cancel |
| | `pull_request_review` submitted (by Todd) | Todd's own review counts as findings |
| | `check_run` / `check_suite` completed | CI result on the PR head (GitHub Actions, Bellwether checks) |
| | `status` | Bellwether's `bellwether/build/<recipe>` and `bellwether/<app>/deploy` on the merge commit |
| | `push` to the default branch | open PRs need a rebase |
| | `installation_repositories` | repo added/removed; onboarding check |
| Local | step finished / timed out / crashed | next stage, retry or stall |
| Local | host joined / left (worker connection) | place waiting items |
| Local | rate-limit reading from a worker run | budget guard update |
| Local | timer fired (backoff, five-hour reset, 9am report, cleanup) | scheduled, never a loop |
| `botyard watch` stream | Todd's message in `#factory` (Todd Dev) | verbatim issue (repo from a `repo:` prefix, default toddstack), issue link posted in a thread; idempotent on the message id |

Events from the App itself (`sender` is our bot) are ignored except CI and status events. Labels and commands count
only from Todd's login (`goddtriffin`); anyone else's are reverted and ignored.

### State per issue

```
new ─► refine ─► ready ─► build ─► review ───────────────► ci ─► merge ─► done
                                     └─changes─► fix ─► re-review ─┘
                                     (red CI or rebase conflict also go to fix)
```

| From | Event | To |
|---|---|---|
| new | issue labeled `factory`, no valid spec block | refine |
| new | valid spec block | ready |
| refine | result: ready | ready (spec block written) |
| refine | result: questions or push-back | needs-todd (`spec`); Todd's reply -> refine (max 2 rounds) |
| ready | placed on a capable host, budget allows | build |
| build | diff, verify green | review (PR opened) |
| review | approve | ci |
| review | changes | fix (round 1) |
| fix | diff, verify green | re-review |
| re-review | approve | ci |
| re-review | changes | needs-todd (`review-cap`): never a third round |
| ci | every gating check green on head | merge |
| ci | a check red | fix (max 2 CI fixes, then `ci-red`); its diff gets the re-review if round 2 is unused |
| any open PR | `push` to main, rebase conflicts | fix (conflict); clean rebase is pushed by code |
| merge | label `approval` or repo auto-merge off | needs-todd (`approval`) |
| merge | merged | done |
| done | Bellwether deploy status `failure` on the merge commit | needs-todd (`deploy`), issue reopened |
| any agent step | denied command | needs-todd (`permission`) |
| any step | crash / timeout / verify red, 3rd attempt | failed |
| any | issue closed by Todd | cancelled: step killed, PR closed, branch deleted |
| needs-todd, failed | Todd: `/shiftboss <stage>` or relabel | that stage |

Waiting is not a state change: no capable host online, budget window, CI still running.

| Stage | Who | Model (tier `important` / `low`) | Output |
|---|---|---|---|
| refine | agent | Opus / Opus | spec block, size, required traits, questions or push-back |
| build | agent | Opus if `size:normal`, Sonnet if `trivial` / Grok | diff in worktree; code verifies, commits, pushes `shiftboss/<n>-<slug>`, opens PR `Closes #n`, assignee Todd |
| review | agent | Opus if normal, Sonnet if trivial / Grok | verdict, findings (posted as one PR comment by code) |
| fix | agent | Sonnet / Grok | diff on the PR branch |
| re-review | agent | Sonnet / Grok | verdict on `<reviewed sha>..HEAD` only |
| ci | code | none | waits for every check on the head SHA |
| merge | code | none | re-verify, `PUT /pulls/{n}/merge` squash with `sha` = verified head |

Models follow infra `docs/ai-platforms.md`. We build non-trivial work on important repos with Opus because quality
on those repos is Todd's rule, not Sonnet because it is cheaper (an Opus PR cost ~3x a Sonnet one; code doing every
non-judgment step pays for it). Low-tier repos build on Grok so Claude's week stays for important repos. Refine is
Opus for every tier because a wrong spec wastes every later step (Todd's rule for idea work). A `model:` label
overrides per item.

**Idempotency.** Every command carries a key `(item, stage, attempt, kind)`. Labels are set to a target set
(diff, not toggles); comments carry `<!-- shiftboss:<key> -->` and are edited, never duplicated; pushes use
`--force-with-lease=<branch>:<start sha>`; merges pass the expected head `sha`. Replaying any event, or the whole
history, converges on the same GitHub state. The hidden counters (attempt, round, verified sha) live in one
bot-owned comment `<!-- shiftboss:state {json} -->` so a lost local database loses metrics only.

## 3. Queue schema

**Labels** (created idempotently by `shiftboss onboard`):

| Label | Values | Meaning |
|---|---|---|
| `factory` | | opt-in; nothing else is touched |
| priority | `P0` `P1` `P2` `P3` | P0 now, P3 when idle; default P2 |
| `stage:` | `refine` `ready` `build` `review` `fix` `re-review` `ci` `merge` `done` `needs-todd` `failed` | exactly one; written by code |
| `size:` | `trivial` `normal` | picks review model; set by refine, Todd may override |
| `model:` | `opus` `sonnet` `grok` | override for build/fix |
| `needs:` | `macos` `xcode` `ios-sim` `android` `docker` | extra traits beyond what the verify targets imply |
| `stall:` | see section 9 | set with `stage:needs-todd` |
| `approval` | | stop before merge (prod data, deletions, payments, project moves) |
| `waiting:host` | | visible while no capable host is online |

**Body.** Todd's text stays verbatim at the top. Refine appends one fenced block, the only part code parses:

````markdown
<Todd's words, verbatim>

```toml shiftboss
goal = "One sentence: the outcome, not the steps."
done_when = ["Observable check 1", "Observable check 2"]
verify = ["test", "lint"]   # @factory Makefile targets
size = "normal"
budget_usd = 4.0            # per step, list price; --max-budget-usd
```
````

Todd or the coordinator may write the block directly; an item with a valid block skips refine. A block naming an
untagged target, or missing fields, goes to `needs-todd(spec)` with a comment saying exactly what is missing.

## 4. Worker lifecycle

- **Clone and worktree.** Each host keeps a bare-ish clone per repo in `~/.local/share/shiftboss/repos/<repo>`
  (owned by the `shiftboss` user, fetched with a read-only token). A step gets
  `~/.local/share/shiftboss/work/<repo>-<n>-<step>-<attempt>/`, a worktree at `origin/main` (build) or the PR
  head (fix, review), owned by the sandbox user. It is removed when the step ends; leftovers of steps not running
  are removed at daemon start (scheduled cleanup). `CARGO_TARGET_DIR` is shared per repo per host.
- **Sandbox.** Two Unix users per host. `shiftboss` runs the daemon and holds the App key and tokens.
  `shiftboss-worker` runs the agent CLI and `make`; its home has the CLI's subscription login and nothing else: no
  SSH keys, no `gh` auth, no `op`, AWS or Cloudflare logins. Test code the agent writes runs as that user, so
  permission rules are a second layer, not the sandbox. The daemon reads the diff from a pipe
  (`git -c core.hooksPath=/dev/null -c core.fsmonitor=false diff --binary` run *as the worker user*) and never
  runs git in a worktree the agent controlled. We use a separate user because the worker owns `.git` config and
  hooks; denying them by rule is an endless list, not one boundary.
- **Argv** (built from allowlisted values only):
  `claude -p --model <m> --output-format stream-json --verbose --json-schema <stage schema>
  --permission-mode dontAsk --setting-sources project --settings <generated> --strict-mcp-config
  --tools Read,Edit,Write,Glob,Grep,Bash --max-budget-usd <cap> --no-session-persistence`, prompt on stdin.
  Review and refine get no edit tools. Grok (`grok` 1.0.89): `grok --prompt-file <f> -m <grok-4.7|grok-4.7-build-fast>
  --cwd <worktree> --output-format json --json-schema <s> --permission-mode dontAsk --allow/--deny <rules>
  --disable-web-search --no-subagents --max-turns <n> --rules <fixed rules>`; its rule syntax, silent denial and
  JSON shape are **[unverified]** and get a probe run before the adapter is written. Both sit behind one
  `Worker` trait; the core never names a provider. A step refuses to start if `ANTHROPIC_API_KEY` or `XAI_API_KEY` is set: subscriptions only.
- **Allowlist.** Generated settings allow `stokehold make <t>` for each target whose `##` help contains `@factory`
  in the Makefile **on `origin/main`** (reviewed code), plus read-only `git status|diff|log|show`. Deny edits to
  `Makefile`, `*.mk`, `.claude/`, `.github/`, `bellwether.json`; deny reading `.env*`; no web tools, MCP or
  subagents. Any `permission_denials` entry ends the step as `stall:permission` quoting the command; the fix is a
  reviewed Makefile target, never a wider list. Verified on Claude Code 2.1.329: unlisted and compound commands are
  denied without a prompt and reported.
- **Prompt.** Fixed rules in `--append-system-prompt` (identical across runs, so cached). The user prompt names the
  stage, repo, item, the files to read first (`LESSONS.md`, `DESIGN.md`, `AGENTS.md`/`CLAUDE.md`), the allowed
  targets, and fences the item text as data. Result schema: `outcome` (done | stall), `stall_reason`, `summary`,
  `commit_message` (one line; code strips anything else), `lessons` (0-2 lines), plus `verdict`/`findings` for
  reviews and the spec block for refine.
- **Timeouts.** refine 10 min, build 45, fix 30, review and re-review 15; verify through
  `stokehold --timeout 30m`. Over time: SIGTERM to the process group, 10 s, SIGKILL; counts as an attempt.
- **Retries.** Up to 2 retries per step (3 attempts) for crash, timeout or verify failure, then `failed`. GitHub,
  network and tunnel errors retry with backoff (1, 2, 4 ... 30 min) and do not count. stokehold exit 75 (disk
  full) pauses that host and requeues the item, not an attempt.
- **Concurrency.** One write step (build, fix, merge) per repo at a time; reads (review, refine) may overlap. Per
  host, the budget guard sets how many agent steps run; stokehold sets whether the machine has room.

## 5. Hosts and capabilities

- **One scheduler, on the rig.** The `shiftboss daemon` on the rig owns the event intake, the SQLite store
  (`~/.local/state/shiftboss/shiftboss.db`) and every placement. The Mac runs `shiftboss worker`, which dials the
  rig over Tailscale (outbound from the Mac; WebSocket, bearer token) and runs steps it is given. We do one
  scheduler because placement then needs no distributed lock, not leases in a shared database because that needs
  a server we'd host on the droplet, which shiftboss must not use. The rig is always on; the Mac is often off.
- **Traits** are detected locally by each host at start, with Bellwether's names from #193 so we can switch to
  its host-traits API later: `linux`, `macos`, `docker`, `xcode`, `ios-sim`, `android`, `gpu`.
- **Requirements** of an item = traits tagged on its verify targets (`@factory needs=xcode,ios-sim` in the Makefile
  help) plus any `needs:` label. Placement: an online host whose traits are a superset, budget allowing; stokehold
  on that host then admits CPU and memory. iOS build, simulator, signing and upload need `macos`+`xcode`, so they
  run only on the Mac; Android and everything else run anywhere.
- **Offline Mac = waiting.** The item sits in its stage with `waiting:host`; the report counts this as wait time,
  not a stall. Placement happens on the "host joined" event, not on a timer. After 24 h (2 h for P0) a scheduled
  timer adds "open the laptop" to the needs-Todd list, once.
- **Not Bellwether's admission API (#193) in v1**: stokehold already does admission on both hosts and needs no
  agent key into Robert's shared prod. Revisit if Bellwether's daemon replaces stokehold on Todd's hosts.

## 6. GitHub access: a dedicated shiftboss App

Todd decided: shiftboss gets its own GitHub App, separate from Bellwether's. The manifest is
[`github-app-manifest.json`](github-app-manifest.json) (App name `shiftboss-goddtriffin`, since App names are
global; callback port 8787).

- **Create in one click.** `shiftboss app create` serves a local page that POSTs the manifest to
  `https://github.com/settings/apps/new`. GitHub redirects back to `http://127.0.0.1:<port>/callback?code=...`;
  shiftboss exchanges the code (`POST /app-manifests/{code}/conversions`) and writes the results straight into
  1Password with `op item create`, never to stdout or disk: `PROD - shiftboss - github app private key` (the PEM, a
  document), `PROD - shiftboss - github app webhook secret`, and `PROD - shiftboss - github app id` (App id and
  client id). Add them to infra `docs/secrets.md`. Private App, owned by `goddtriffin`.
- **Installed only on onboarded repos** ("Only select repositories"). Onboarding a repo is ticking it in the
  installation; removal is unticking it.
- **Repository permissions** (nothing at account or org level):

  | Permission | Level | Why |
  |---|---|---|
  | `metadata` | read | mandatory |
  | `contents` | write | push `shiftboss/*` branches, merge PRs, read Makefile |
  | `pull_requests` | write | open/update PRs, review comments |
  | `issues` | write | labels, comments, body edits, issues from Discord |
  | `checks` | read | CI results (`check_run`, `check_suite`) |
  | `statuses` | read | Bellwether build/deploy statuses |

  Not requested: `workflows` (agents can't change CI; `.github/` is denied anyway), `administration`, `actions`,
  `secrets`, `deployments` (Bellwether reports deploys as statuses, not deployments).
- **Events:** `issues`, `issue_comment`, `pull_request`, `pull_request_review`, `check_run`, `check_suite`,
  `status`, `push` (installation events always arrive).
- **Tokens on demand.** The daemon signs a 9-minute RS256 JWT, then mints an installation token per action, scoped
  to one repo and only the permissions it needs (`repositories: [repo]`, e.g. `{"contents":"read"}` for a fetch,
  `{"contents":"write"}` for a push). Tokens live in memory, are reused until 5 minutes before expiry, and are
  passed to git through `GIT_ASKPASS`, never in a URL or argv. Workers never see any token.
- **Key on the hosts.** `make install_secrets` runs `op inject` into `~shiftboss/.config/shiftboss/secrets.env`,
  mode 600, owner `shiftboss` (Todd approves the 1Password prompt once per host). Never printed or logged.
- **Attribution.** Commits are authored as Todd (`Todd Griffin <...>`) and pushed by the App; PRs, comments and
  merges show as the App bot, assigned to Todd. No Co-Authored-By, no "Generated with", no session links or
  trailers: code writes every message from fixed templates and keeps only the first line of an agent's
  `commit_message`.

We use a dedicated App because it is installed per repo, its tokens are short-lived and narrowed per call, its
webhooks come with it, and its activity is separate from Todd's own; not a fine-grained PAT because a PAT is
long-lived, carries Todd's identity, needs per-repo webhooks set up separately, and not Bellwether's App because
Robert ruled out factory permissions there.

## 7. Webhook delivery

**Recommended: a Cloudflare Tunnel from the rig.** `cloudflared` in WSL dials out to Cloudflare; GitHub posts to
`https://shiftboss.toddgriffin.me/github/webhook`, which reaches `127.0.0.1:<port>` on the rig. No inbound port,
no droplet, no third party besides Cloudflare (already in front of every site). Only that path is routed; a
Cloudflare WAF rule allows it only from GitHub's published hook ranges; the daemon verifies
`X-Hub-Signature-256` and dedupes `X-GitHub-Delivery`. The Mac needs no ingress: its worker dials the rig over
Tailscale.

| Option | Verdict |
|---|---|
| **Cloudflare Tunnel on the rig** | **Chosen.** Outbound only, no droplet, one hostname, path-limited. Needs one DNS record (Todd's OK). |
| Relay on the droplet (webhooks in, SSE out) | Works, but puts shiftboss on the droplet, which Todd ruled out, and adds a prod service. |
| Tailscale Funnel | Public exposure via Funnel is banned in infra (`runbooks/tailscale.md`). Plain Tailscale can't receive from GitHub. |
| `gh event listen` | Preview; PR events only (no issues, labels), no replay. Fine for one-off waits, can't carry the queue. |
| `gh webhook forward` | Dev tool, one per repo, needs repo admin, no replay. |
| smee.io / other relays | A third party sees every payload. |
| Events API / `gh api` loops | Polling. Banned. |

**Missed events.** While the rig or tunnel is down, GitHub records failed deliveries. On daemon start and on tunnel
reconnect, shiftboss lists the App's deliveries since the last one it saw (`GET /app/hook/deliveries`) and asks
GitHub to redeliver the failed ones (`POST /app/hook/deliveries/{id}/attempts`); then it reconciles once from
labels (`factory` issues open in installed repos). Both run on an event (start, reconnect), so they are recovery,
not polling. GitHub keeps deliveries about 3 days **[unverified]**; the label reconcile covers anything older.

## 8. Budget guard

Input: every Claude worker run's `stream-json` includes a `rate_limit_event` with `five_hour` and `seven_day`
utilization (0-1) and `resetsAt` (verified on 2.1.329; undocumented, so a missing one keeps the last reading). Each
run also reports `total_cost_usd` and per-model tokens, stored per run. Nothing polls for usage.

Burn = `seven_day` utilization / share of the week elapsed since its reset. Todd keeps 15% of the week.

| Condition | Agent steps per host | Priorities started | Opus |
|---|---|---|---|
| burn < 0.9 | 3 | P0-P3 | yes |
| 0.9-1.1 | 2 | P0-P2 | yes |
| 1.1-1.4 | 1 | P0-P1 | yes |
| burn ≥ 1.4 or seven_day ≥ 0.85 | 1 | P0 | review only; builds drop to Sonnet |
| five_hour ≥ 0.9 | 0 until `resetsAt` (a timer) | | |

- Steps already running finish. Held items are waiting, not stalled.
- Per step, `--max-budget-usd` stops a runaway; ending over it is `stall:budget`.
- Grok shows no usage from the shell yet; v1 caps Grok at a configured number of steps per day per host.
- CPU and memory stay stokehold's job; shiftboss limits subscription spend.

## 9. Stalls and reporting

A **stall** is an item that cannot move without Todd: it sits in `stage:needs-todd` with exactly one `stall:` label
and one bot comment: the question, the options, a recommendation. Waiting (host offline, CI running, budget
window) is not a stall.

| `stall:` | Cause | Typical fix |
|---|---|---|
| `permission` | agent needed a command off the allowlist (quoted) | add a reviewed `@factory` target |
| `secret` | missing secret or logged-out CLI / login | Todd unlocks 1Password, logs in |
| `spec` | refine has questions, pushed back, or the spec block is invalid | Todd answers in the thread |
| `approval` | item labeled `approval` reached merge | Todd merges or relabels |
| `review-cap` | re-review still requests changes | Todd decides, or splits the item |
| `ci-red` | CI still red after 2 fix attempts | |
| `flaky` | a verify target failed then passed 3 times in 7 days | fix or quarantine the test |
| `deploy` | Bellwether deploy status failed on the merge commit | Todd looks at the deploy |
| `budget` | a step hit its `--max-budget-usd` | raise the budget or split the item |
| `conflict` | fix step could not resolve a rebase conflict | |

`failed` (retries exhausted on crash, timeout, verify) shows in the same list.

- Refine questions go to the Discord thread (if any) and the needs-Todd list; the first answer wins, is mirrored
  to the issue, and a later conflicting answer is shown to Todd, not applied.
- `shiftboss todd` prints the needs-Todd and failed list from SQLite: repo, number, title, reason, the question,
  the recommendation, age. The coordinator (rig, Remote Control) reads it; Todd answers on the issue
  (`/shiftboss <stage>` or a plain comment for `spec`) and the webhook resumes the item in seconds.
- **9am report** (systemd timer, 09:00 America/Los_Angeles, covering the last 24 h): PRs merged, deploys after
  them, tasks done, tokens and list cost per task (median, p90, by model and stage), review rounds, stalls by
  reason with Todd-time, wait time by kind, budget burn. Posted to a pinned `factory: reports` issue in toddstack
  and to Discord through `botyard`. Mondays (UTC weeks) it files a P1 item to fix the top stall cause of the week.

## 10. Onboarding a repo

The repo must have:
1. `DESIGN.md` (decisions, "we do X because Y, not Z because W") and `LESSONS.md` (one line per gotcha, at most
   60 lines; code files a P3 trim item past that).
2. `AGENTS.md` or `CLAUDE.md` with the repo rules.
3. A Makefile whose safe targets are tagged `@factory` in their `##` help, at least `test` and `lint`, plus
   `needs=` traits on platform targets (`ios_build ## ... @factory needs=xcode`). Never tag `dev`, `docker_push`,
   `publish*` or anything long-running.
4. Branch protection that lets the App merge (a PR required is fine; required approving reviews must be 0 or the
   item ends at `approval`).

Steps (one repo at a time):
1. Add the repo to `rust/shiftboss/repos.toml` (reviewed): tier (`important` | `low`), provider, CI (which check
   names gate merge), deploy (`bellwether` | `deploy-webhook` | `none`), auto-merge (bool).
2. Tick the repo in the App installation.
3. `shiftboss onboard <repo>`: creates the labels, checks items 1-4, and opens a PR adding whatever stub is
   missing. Idempotent.
4. Pilot with 3 P3 items; widen when they merge clean.

Order: toddstack, then SDD (Grok), then one repo at a time. Boggledygook and Bellwether move last, with their
sessions' owners.

## 11. v1 scope: toddstack PRs end to end this week

In: one daemon on the rig with local workers; toddstack only; Claude only; the App and the Cloudflare tunnel;
build -> review -> fix -> re-review -> ci -> merge, with verify through stokehold; labels and the state comment;
idempotent commands; startup redelivery and reconcile; the budget guard; `shiftboss todd`; a runs table.
Items are written with a spec block by Todd or the coordinator (no refine yet).

Out of v1 (in order): refine; deploy-status watch; the Mac worker and trait placement; Grok and SDD; Discord
intake; the 9am report; CLI install checks on hosts; a commit status from deploy-webhook so non-Bellwether deploys
can be watched (a deploy-webhook change, not shiftboss); self-deploy through Bellwether.

v1 installs with `cargo install --locked shiftboss@x.y.z` plus a systemd user unit in WSL (Makefile targets). We
don't deploy it through Bellwether yet because shiftboss drives host-level tools (agent CLIs, a second Unix user,
worktrees, stokehold), which a container would have to tunnel back out to, not because Bellwether can't run it.

## 12. Implementation PRs

Each sized for one Sonnet helper; v1 is 1-9.

1. **shiftboss: core types and `decide`.** Item, Stage, Event, Command, label and spec-block parse/render, the
   pure state machine for build -> merge with retries and review cap; table tests.
2. **shiftboss: GitHub App client.** JWT, per-repo narrowed installation tokens with an in-memory cache, REST calls
   for labels, comments, body, PRs, merge, deliveries; trait with a mock.
3. **shiftboss: `app create` manifest flow.** Local callback, code conversion, writes to 1Password via `op`, never
   prints; plus the infra `docs/secrets.md` entries.
4. **shiftboss: store.** SQLite schema and migrations: items cache, delivery ids, runs, stalls, usage.
5. **shiftboss: webhook ingress.** axum endpoint, HMAC, dedupe, event normalization, startup redelivery and label
   reconcile.
6. **shiftboss: worker runner.** Worktrees, `shiftboss-worker` user, settings from `@factory` targets, argv,
   timeouts, stream-json parse (result, denials, rate limits), diff extraction as the worker user.
7. **shiftboss: committer.** Diff checks (denied paths, size), apply and commit as Todd in the daemon's clone, push
   with lease, open/update PR, merge with expected sha, rebase on `push`.
8. **shiftboss: daemon.** Event loop, idempotent command executor, backoff, one-write-per-repo, budget guard,
   `shiftboss todd`.
9. **Ops and toddstack onboarding.** Install Makefile targets and systemd unit; cloudflared config and DNS record
   (infra PR, Todd's OK); `repos.toml`; `@factory` tags and `LESSONS.md` in toddstack.

After v1: 10. refine stage; 11. deploy-status watch; 12. Mac worker over Tailscale with trait placement;
13. Grok adapter and SDD onboarding; 14. Discord intake via botyard; 15. 9am report and weekly stall item;
16. ensure `claude` and `grok` CLIs on hosts; 17. `shiftboss onboard`.

## Open questions for Todd

1. **Cloudflare Tunnel** with DNS record `shiftboss.toddgriffin.me` on the rig: OK to create (Cloudflare change)?
2. **The rig as the only scheduler**: OK that nothing moves while the rig is off (the Mac only runs steps)?
3. **Auto-merge**: shiftboss merges reviewed, verified, green PRs in toddstack on its own, and `approval` items stop
   for you. OK?
4. **Two Unix users per host** (`shiftboss`, `shiftboss-worker`), each with its own Claude login on your Max
   account: OK?
5. **Budget**: keep 15% of the week for you and the thresholds in section 8? Grok: a daily step cap until xAI
   exposes limits?
6. **Coordinator model** (rig, Remote Control): Opus, now that intake and bookkeeping are code and its context
   stays small? Alternative: Sonnet by default.
