//! A tiny counter app, as a worked example of the pattern: a state-machine enum for the sync
//! flow, an `Http` effect with backoff, a `Store` effect, the pointer fast path, and an
//! app-defined effect (`Vibrate`) next to the built-in ones.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ActionSpec, App, Ctx, Effect, FieldType, HttpReply, Pending, Pointer, RetryPolicy};

/// The app's own effect: what a shell does that the built-in set has no word for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CounterEffect {
    Vibrate { ms: u32 },
}

#[derive(Debug, Deserialize)]
pub enum Action {
    Inc,
    Add {
        n: i64,
    },
    Reset,
    /// `GET /api/count`, retried with backoff while the server is unwell.
    Fetch,
}

/// The whole sync flow, explicit: no booleans that must agree with each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Sync {
    Idle,
    Fetching { attempt: u32 },
    Backoff { attempt: u32 },
    Loaded,
    Failed { reason: String },
}

/// What an in-flight effect was for, so `resolve` is a `match`.
#[derive(Debug, Clone, Copy)]
enum Wait {
    Restore,
    Fetch(u32),
    Retry(u32),
}

const KEY: &str = "example.count";

#[derive(Debug)]
pub struct Counter {
    count: i64,
    taps: u32,
    sync: Sync,
    pending: Pending<Wait>,
}

type C = Ctx<CounterEffect>;

impl Counter {
    fn fetch(&mut self, attempt: u32, ctx: &mut C) {
        self.pending
            .emit(ctx, Effect::http("GET", "/api/count"), Wait::Fetch(attempt));
        self.sync = Sync::Fetching { attempt };
    }

    fn fetched(&mut self, attempt: u32, result: Result<Value, String>, ctx: &mut C) {
        let reply: Option<HttpReply> = result
            .ok()
            .and_then(|v| serde_json::from_value::<HttpReply>(v).ok());
        match reply {
            Some(r) if r.status == 200 => {
                self.count = r.body["count"].as_i64().unwrap_or(self.count);
                self.sync = Sync::Loaded;
            }
            other => {
                let status: u16 = other.map_or(0, |r| r.status);
                // A fixed draw keeps the example deterministic; a real app asks `Effect::Random`.
                let delay: Option<u64> = RetryPolicy::is_retryable(status)
                    .then(|| RetryPolicy::read().delay_ms(attempt, 0))
                    .flatten();
                if let Some(after_ms) = delay {
                    self.pending
                        .emit(ctx, Effect::Timer { after_ms }, Wait::Retry(attempt + 1));
                    self.sync = Sync::Backoff {
                        attempt: attempt + 1,
                    };
                } else {
                    self.sync = Sync::Failed {
                        reason: format!("status {status}"),
                    };
                }
            }
        }
        ctx.dirty("counter");
    }
}

impl App for Counter {
    type Action = Action;
    type Effect = CounterEffect;

    fn boot(_boot: &Value, ctx: &mut C) -> Self {
        let mut app: Self = Self {
            count: 0,
            taps: 0,
            sync: Sync::Idle,
            pending: Pending::default(),
        };
        app.pending.emit(
            ctx,
            Effect::Store {
                key: KEY.to_owned(),
                value: None,
            },
            Wait::Restore,
        );
        ctx.dirty("counter");
        app
    }

    fn update(&mut self, action: Action, ctx: &mut C) {
        match action {
            Action::Inc => {
                self.count += 1;
                ctx.emit_app(CounterEffect::Vibrate { ms: 10 });
            }
            Action::Add { n } => self.count += n,
            Action::Reset => self.count = 0,
            Action::Fetch => {
                if matches!(self.sync, Sync::Fetching { .. } | Sync::Backoff { .. }) {
                    return;
                }
                self.fetch(0, ctx);
            }
        }
        if !matches!(action, Action::Fetch) {
            ctx.emit(Effect::Store {
                key: KEY.to_owned(),
                value: Some(json!(self.count)),
            });
        }
        ctx.dirty("counter");
    }

    fn resolve(&mut self, id: u64, result: Result<Value, String>, ctx: &mut C) {
        match self.pending.take(id) {
            Some(Wait::Restore) => {
                if let Some(n) = result.ok().and_then(|v| v.as_i64()) {
                    self.count = n;
                }
            }
            Some(Wait::Fetch(attempt)) => return self.fetched(attempt, result, ctx),
            Some(Wait::Retry(attempt)) => return self.fetch(attempt, ctx),
            None => return,
        }
        ctx.dirty("counter");
    }

    fn pointer(&mut self, pointer: Pointer, ctx: &mut C) {
        if pointer.phase == Pointer::UP {
            self.taps += 1;
            ctx.dirty("counter");
        }
    }

    fn view(&self, key: &str) -> Option<Value> {
        (key == "counter")
            .then(|| json!({"count": self.count, "taps": self.taps, "sync": self.sync}))
    }

    fn screen(&self) -> String {
        "counter".to_owned()
    }

    fn actions(&self) -> Vec<ActionSpec> {
        let mut actions: Vec<ActionSpec> = vec![
            ActionSpec::new("Inc"),
            ActionSpec::new("Add").field("n", FieldType::Integer),
            ActionSpec::new("Reset"),
        ];
        if !matches!(self.sync, Sync::Fetching { .. } | Sync::Backoff { .. }) {
            actions.push(ActionSpec::new("Fetch"));
        }
        actions
    }
}
