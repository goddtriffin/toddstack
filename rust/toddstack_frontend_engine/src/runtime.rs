use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use serde_json::{Value, json};

use crate::app::{App, Ctx, Pointer};
use crate::effect::{Effect, EffectReq, builtin_collision};
use crate::wire::{Input, Verb};

/// The bridge message format (shell to core): the boot message carries `"v"` and every response
/// echoes it. Not an app's API contract: apps version that themselves.
pub const BRIDGE_PROTOCOL: u64 = 1;

/// The surface every bridge (`UniFFI`, `wasm-bindgen`, a test harness) wraps. Bytes in, bytes out.
pub trait Bridge {
    /// One protocol line in, one response out.
    fn dispatch(&mut self, input: &[u8]) -> Vec<u8>;
    /// Typed fast path, no JSON. Returns a bitmask: bit 0 = dirty views queued, bit 1 = effects pending.
    fn pointer(&mut self, phase: u8, x: f32, y: f32, at_ms: f64) -> u32;
    /// The JSON of one view key (`null` when unknown).
    fn view(&self, key: &str) -> Vec<u8>;
    /// Large binary input off the JSON path, named by the app.
    fn provide_blob(&mut self, name: &str, bytes: &[u8]) -> Vec<u8>;
}

/// Drives an [`App`] with the protocol. Construct with [`Runtime::new`].
pub struct Runtime<A: App> {
    app: A,
    ctx: Ctx<A::Effect>,
    /// Hash of the last bytes reported dirty per view key: an unchanged view is never sent again.
    sent: HashMap<String, u64>,
}

impl<A: App> Runtime<A> {
    /// Boot facts from the shell (JSON). Bad JSON boots with `null`. Boot effects stay queued
    /// until the next response.
    #[must_use]
    pub fn new(boot: &[u8]) -> Self {
        let boot: Value = serde_json::from_slice(boot).unwrap_or(Value::Null);
        let mut ctx: Ctx<A::Effect> = Ctx {
            at: boot.get("at").and_then(Value::as_u64).unwrap_or(0),
            ..Ctx::default()
        };
        let app: A = A::boot(&boot, &mut ctx);
        Self {
            app,
            ctx,
            sent: HashMap::new(),
        }
    }

    fn flush(&mut self) -> Value {
        let marked = std::mem::take(&mut self.ctx.dirty);
        let mut dirty: Vec<String> = Vec::with_capacity(marked.len());
        for key in marked {
            let mut hasher: DefaultHasher = DefaultHasher::new();
            serde_json::to_vec(&self.app.view(&key))
                .unwrap_or_default()
                .hash(&mut hasher);
            let hash: u64 = hasher.finish();
            if self.sent.insert(key.clone(), hash) != Some(hash) {
                dirty.push(key);
            }
        }
        let queued: Vec<EffectReq<A::Effect>> = std::mem::take(&mut self.ctx.effects);
        for req in &queued {
            if let Effect::App(app_effect) = &req.effect {
                debug_assert!(
                    builtin_collision(app_effect).is_none(),
                    "app effect named like a built-in ({:?}); it would read back as the built-in",
                    builtin_collision(app_effect),
                );
            }
        }
        match serde_json::to_value(queued) {
            Ok(effects) => json!({"dirty": dirty, "effects": effects}),
            Err(e) => json!({"error": format!("effects not serializable: {e}")}),
        }
    }

    fn handle(&mut self, verb: Verb) -> Value {
        match verb {
            Verb::Act(value) => match serde_json::from_value::<A::Action>(value) {
                Ok(action) => {
                    self.app.update(action, &mut self.ctx);
                    self.flush()
                }
                Err(e) => json!({"error": format!("bad act: {e}")}),
            },
            Verb::Resolve(r) => {
                let result: Result<Value, String> = match (r.ok, r.err) {
                    (_, Some(err)) => Err(err),
                    (ok, None) => Ok(ok.unwrap_or(Value::Null)),
                };
                self.app.resolve(r.id, result, &mut self.ctx);
                self.flush()
            }
            Verb::Pointer(p) => {
                let pointer: Pointer = Pointer {
                    phase: p.phase,
                    x: p.x,
                    y: p.y,
                    at_ms: self.ctx.at,
                };
                self.app.pointer(pointer, &mut self.ctx);
                self.flush()
            }
            Verb::View(key) => json!({"view": key, "json": self.app.view(&key)}),
            Verb::Introspect {} => {
                let screen: String = self.app.screen();
                let actions: Vec<Value> = self
                    .app
                    .actions()
                    .into_iter()
                    .map(|a| json!({"name": a.name, "schema": a.schema}))
                    .collect();
                json!({"screen": screen, "view": self.app.view(&screen), "actions": actions})
            }
            Verb::Flush {} => self.flush(),
        }
    }
}

fn versioned(mut out: Value) -> Value {
    out["v"] = json!(BRIDGE_PROTOCOL);
    out
}

impl<A: App> Bridge for Runtime<A> {
    fn dispatch(&mut self, input: &[u8]) -> Vec<u8> {
        let out: Value = match serde_json::from_slice::<Input>(input) {
            Ok(parsed) => {
                if let Some(at) = parsed.at {
                    self.ctx.at = at;
                }
                self.handle(parsed.verb)
            }
            Err(e) => json!({"error": format!("bad input: {e}")}),
        };
        serde_json::to_vec(&versioned(out)).unwrap_or_default()
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a millisecond clock reading, clamped non-negative; f64 holds it exactly"
    )]
    fn pointer(&mut self, phase: u8, x: f32, y: f32, at_ms: f64) -> u32 {
        let at: u64 = at_ms.max(0.0) as u64;
        self.ctx.at = at;
        self.app.pointer(
            Pointer {
                phase,
                x,
                y,
                at_ms: at,
            },
            &mut self.ctx,
        );
        u32::from(!self.ctx.dirty.is_empty()) | (u32::from(!self.ctx.effects.is_empty()) << 1)
    }

    fn view(&self, key: &str) -> Vec<u8> {
        serde_json::to_vec(&self.app.view(key)).unwrap_or_default()
    }

    fn provide_blob(&mut self, name: &str, bytes: &[u8]) -> Vec<u8> {
        self.app.blob(name, bytes, &mut self.ctx);
        serde_json::to_vec(&versioned(self.flush())).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, Serializer};

    use super::*;

    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
    }

    struct Bad;

    impl App for Bad {
        type Action = String;
        type Effect = Unserializable;

        fn boot(_: &Value, _: &mut Ctx<Unserializable>) -> Self {
            Self
        }
        fn update(&mut self, _: String, ctx: &mut Ctx<Unserializable>) {
            ctx.emit_app(Unserializable);
        }
        fn resolve(&mut self, _: u64, _: Result<Value, String>, _: &mut Ctx<Unserializable>) {}
        fn view(&self, _: &str) -> Option<Value> {
            None
        }
        fn screen(&self) -> String {
            String::new()
        }
        fn actions(&self) -> Vec<crate::app::ActionSpec> {
            Vec::new()
        }
    }

    #[test]
    fn an_unserializable_effect_is_an_error_frame_not_dropped() {
        let mut rt: Runtime<Bad> = Runtime::new(b"{}");
        let out: Value = serde_json::from_slice(&rt.dispatch(br#"{"act":"go"}"#)).unwrap();
        let error: &str = out["error"].as_str().unwrap();
        assert!(error.contains("effects not serializable"), "{out}");
        assert!(out.get("effects").is_none());
    }
}
