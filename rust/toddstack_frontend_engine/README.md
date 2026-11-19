# toddstack-frontend-engine

An app-agnostic frontend core: the Elm-style loop `update(model, msg) -> effects`. Part of
[toddstack](https://github.com/goddtriffin/toddstack).

All client logic of an app lives in one Rust core. Web runs it as wasm in a module worker; iOS and
Android run it through UniFFI. Shells (browser, Swift, Kotlin) only **draw view models** and **run
effects**. The core never reads a clock, network, disk or randomness: everything it needs arrives as
an input, so one headless harness can run it against a fake clock, network and random on every
target and get byte-identical results.

## The pattern

- **`App`** is the one trait: `boot`, `update(action)`, `resolve(effect id, result)`, `view(key)`,
  `screen()`, `actions()`. State machines are plain enums in your model.
- **Effects are data.** `update` calls `ctx.emit(...)`; the shell runs the effect and answers with
  `resolve`. Time is `ctx.at()`, the wall clock of the input being handled.
- **Views are keyed JSON.** `update` marks keys dirty (`ctx.dirty("home")`); the engine hashes each
  dirty view and reports it only when its bytes changed, so shells redraw only what changed.
- **Introspection** lists the screen and the actions legal right now, with JSON schemas. The same
  protocol is the debug protocol: a person (or an agent) can drive an app over NDJSON.
- **Pointer fast path**: `Bridge::pointer(phase, x, y, at_ms)` skips JSON for drags and drawing. The
  units of `x` and `y` are yours.

## Effects

Built in (`Effect<X>`): `Http`, `Timer`, `CancelTimer`, `Store`, `Secure`, `Random`, `Navigate`,
`OpenAuthBrowser`, `Reload`, `LoadBlob`, `Span`, `Log`, `Report`.

Your own go in the app's effect type `X` (`type Effect = MyEffect` on your `App`), emitted with
`ctx.emit_app(...)`. They serialize flat next to the built-ins, so a shell matches on one key:
`{"id":4,"Vibrate":{"ms":10}}`. Haptics, sound, share sheets, push registration and review prompts
are app effects, not built in. An app with none uses `NoEffect`.

`Pending<T>` remembers, per effect id, what the app was doing when it asked, so `resolve` is a `match`
over an enum. `RetryPolicy` (exponential backoff, jitter drawn by the caller) and `ClockOffset`
(server time from the lowest-round-trip sample) are deterministic helpers.

## A minimal app

```rust
use serde::Deserialize;
use serde_json::{Value, json};
use toddstack_frontend_engine::{ActionSpec, App, Ctx, Effect, NoEffect};

#[derive(Deserialize)]
enum Action { Inc, Load }

#[derive(Default)]
struct Counter { count: i64, loading: bool }

impl App for Counter {
    type Action = Action;
    type Effect = NoEffect;

    fn boot(_boot: &Value, ctx: &mut Ctx<NoEffect>) -> Self {
        ctx.dirty("counter");
        Self::default()
    }

    fn update(&mut self, action: Action, ctx: &mut Ctx<NoEffect>) {
        match action {
            Action::Inc => self.count += 1,
            Action::Load => {
                self.loading = true;
                ctx.emit(Effect::http("GET", "/api/count"));
            }
        }
        ctx.dirty("counter");
    }

    fn resolve(&mut self, _id: u64, result: Result<Value, String>, ctx: &mut Ctx<NoEffect>) {
        self.loading = false;
        if let Ok(reply) = result {
            self.count = reply["body"]["count"].as_i64().unwrap_or(self.count);
        }
        ctx.dirty("counter");
    }

    fn view(&self, key: &str) -> Option<Value> {
        (key == "counter").then(|| json!({"count": self.count}))
    }
    fn screen(&self) -> String { "counter".to_owned() }
    fn actions(&self) -> Vec<ActionSpec> { vec![ActionSpec::new("Inc"), ActionSpec::new("Load")] }
}
```

`Runtime::<Counter>::new(boot_bytes)` implements `Bridge`: bytes in (`dispatch`), bytes out. The
`example` feature ships a fuller version (`example::Counter`) with a state-machine enum for a retried
fetch, a `Store` restore and an app-defined `Vibrate` effect; it is the app the tests run.

## The wire (NDJSON)

One JSON object per line in, one per line out; every response carries `"v"` (`BRIDGE_PROTOCOL`).
Inputs: `{"at":ms,"act":<Action>}`, `{"resolve":{"id":3,"ok":<v>}}` or `{"resolve":{"id":3,"err":"msg"}}`,
`{"pointer":{"phase":0,"x":1.0,"y":2.0}}`, `{"view":"key"}`, `{"introspect":{}}`, `{"flush":{}}`.
Responses to `act`, `resolve`, `pointer` and `flush`: `{"dirty":[...],"effects":[{"id":1,...}]}`.

## Features

| feature | adds |
|---|---|
| `testing` | `testing::{World, Session, Script, run_file, run_repl}`: a fake clock, timers, store and seeded random; scripted HTTP with latency and delay; a JSON-lines scenario runner with snapshots; an NDJSON REPL. Uses `std::fs`: dev-dependency only |
| `wasm` | `export_wasm_core!(MyApp)`: the wasm-bindgen `Core` class for a `cdylib` |
| `ffi` | `export_uniffi_core!(MyApp)`: the UniFFI `Core` object for a `staticlib` / `cdylib` |
| `example` | the counter example app |

The consuming crate depends on `wasm-bindgen` (or `uniffi`) itself, at the exact version this crate
pins, and invokes the macro once. Build the wasm with
`cargo build --target wasm32-unknown-unknown --features wasm` and `wasm-bindgen --target web`.

## Scenarios

A scenario is a `.jsonl` file, one step per line (`#` and blank lines skipped):

```text
{"http":{"GET /api/count":[{"status":503},{"status":200,"body":{"count":42}}]}}
{"boot":{"v":1,"at":5000}}
{"send":{"act":"Fetch"}}
{"expect":{"effects":[{"Timer":{"after_ms":250}}],"view":{"counter":{"count":0}},"actions":["Inc"]}}
{"advance_ms":250}
{"snapshot":"counter"}
```

Steps: `boot` (`$APP_VERSION` is replaced from `Options::app_version`), `http` (scripted routes, a list
answers in order and repeats the last; `latency_ms`, `delay_ms`, `body_file`; a body of `"$now+5"`
follows the fake clock), `send`, `pointer`, `blob`, `advance_ms`, `expect` (partial match of
`effects`, `dirty`, `view`, `screen`, `actions`) and `snapshot` (exact match against
`<snapshot_root>/<scenario>/<n>.json`; set `Options::golden` to write them). App-defined effects the
fake world cannot answer are recorded; give `Options::answerer` (or `Session::answerer`) to answer
them. `run_file` returns the response log, so a test can run a scenario twice and compare.

## The JavaScript side

`js/` holds the typed browser half (a Deno-checked TypeScript package, copy or bundle it into the app):
`protocol.ts` (page and worker messages), `worker.ts` (`serveCore`: the module worker that owns the
wasm, transfers each dirty view's bytes, fetches `LoadBlob` blobs and drops replies from a replaced
core) and `client.ts` (`connect` and `CoreClient`: watch a view key, send actions, run timers and hand
every other effect to your `run` function).

## What a native shell implements

Swift and Kotlin runners for the built-in effects are not part of this crate yet. A native shell
needs: an HTTP client for `Http` (answering `{"status","body","sent","recv"}`, `auth` from the
Keychain/Keystore token), timers, key-value and secure storage, a random source, navigation, an auth
browser session, and a fire-and-forget logger; plus a runner for each of the app's own effects. The
UniFFI object is `dispatch` / `pointer` / `view` / `provide_blob`, bytes in and out.
