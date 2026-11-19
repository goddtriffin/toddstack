use std::collections::VecDeque;

use serde_json::{Value, json};

use super::world::World;
use crate::Bridge;

/// Builds a bridge from boot bytes (one per app / target).
pub type Factory<'a> = &'a dyn Fn(&[u8]) -> Result<Box<dyn Bridge>, String>;

/// A [`Factory`] for an app: `testing::run_file(path, &factory::<MyApp>, &options)`.
///
/// # Errors
/// Never; the signature is the factory's.
pub fn factory<A: crate::App + 'static>(boot: &[u8]) -> Result<Box<dyn Bridge>, String> {
    Ok(Box::new(crate::Runtime::<A>::new(boot)))
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Value,
    pub latency_ms: u64,
    /// The answer reaches the app only after the clock moves this far (a slow
    /// server); 0 answers at once.
    pub delay_ms: u64,
}

/// Answers `Http` effects: scripted in scenarios; an app's CLI may answer for real.
pub trait HttpHandler {
    /// # Errors
    /// A message when no answer exists (unscripted route, network failure).
    fn handle(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        auth: bool,
        now: u64,
    ) -> Result<HttpResponse, String>;
}

/// Answers an app-defined effect (the whole flat effect object, with its `id`): `Some` resolves it
/// now, `None` leaves it recorded and unanswered. A plain `fn`, so scenario options stay `Copy`-able.
pub type Answerer = fn(&Value, &mut World) -> Option<Result<Value, String>>;

/// A bridge plus a fake world that answers its effects. Effects the world cannot answer
/// (the app's own, unless an [`Answerer`] handles them) are only recorded.
pub struct Session {
    bridge: Box<dyn Bridge>,
    pub world: World,
    http: Box<dyn HttpHandler>,
    /// Handles effects the built-in world does not.
    pub answerer: Option<Answerer>,
    /// Make unanswerable HTTP an error instead of resolving it with `err`.
    pub strict: bool,
    /// Effects emitted since the last [`Session::clear`], including cascades.
    pub effects: Vec<Value>,
    /// Dirty keys since the last [`Session::clear`].
    pub dirty: Vec<String>,
    /// Every response of the session, for repeat-run comparison.
    pub log: Vec<Value>,
}

/// A scripted body may say `"$now"` or `"$now+1000"` for the world clock, so a
/// server's `now` can follow the fake clock.
fn with_now(body: Value, now: u64) -> Value {
    match body {
        Value::String(s) if s.starts_with("$now") => {
            let off: i64 = s["$now".len()..].parse().unwrap_or(0);
            json!(i64::try_from(now).unwrap_or(0) + off)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(|v| with_now(v, now)).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (k, with_now(v, now))).collect())
        }
        other => other,
    }
}

fn check_version(v: &Value) -> Result<(), String> {
    match v.get("v").and_then(Value::as_u64) {
        Some(crate::BRIDGE_PROTOCOL) => Ok(()),
        other => Err(format!(
            "response protocol version {other:?} does not match runner v{}",
            crate::BRIDGE_PROTOCOL
        )),
    }
}

const MAX_CASCADE: usize = 1_000_000;

impl Session {
    /// # Errors
    /// When the bridge cannot boot.
    pub fn new(
        factory: Factory<'_>,
        boot: &Value,
        seed: u64,
        http: Box<dyn HttpHandler>,
    ) -> Result<Self, String> {
        match boot.get("v").and_then(Value::as_u64) {
            Some(crate::BRIDGE_PROTOCOL) => {}
            other => {
                return Err(format!(
                    "boot protocol version {other:?} does not match runner v{}",
                    crate::BRIDGE_PROTOCOL
                ));
            }
        }
        let bridge = factory(boot.to_string().as_bytes())?;
        let now = boot.get("at").and_then(Value::as_u64).unwrap_or(0);
        let mut session = Self {
            bridge,
            world: World::new(now, seed),
            http,
            answerer: None,
            strict: false,
            log: Vec::new(),
            effects: Vec::new(),
            dirty: Vec::new(),
        };
        let first = session.dispatch_value(&json!({"flush": {}}))?;
        session.settle(vec![first])?;
        Ok(session)
    }

    pub fn clear(&mut self) {
        self.effects.clear();
        self.dirty.clear();
    }

    #[must_use]
    pub fn bridge(&self) -> &dyn Bridge {
        &*self.bridge
    }

    /// One input straight to the bridge: no effect handling, nothing recorded.
    ///
    /// # Errors
    /// When the bridge returns bad JSON.
    pub fn dispatch_raw(&mut self, input: &Value) -> Result<Value, String> {
        self.dispatch_value(input)
    }

    fn dispatch_value(&mut self, input: &Value) -> Result<Value, String> {
        let out = self.bridge.dispatch(input.to_string().as_bytes());
        let v: Value =
            serde_json::from_slice(&out).map_err(|e| format!("bridge returned bad JSON: {e}"))?;
        check_version(&v)?;
        Ok(v)
    }

    /// Send one input (adds `at` from the world clock when missing). Returns every response,
    /// including those caused by auto-answered effects.
    ///
    /// # Errors
    /// On a protocol error response or an unanswerable effect in strict mode.
    pub fn send(&mut self, mut input: Value) -> Result<Vec<Value>, String> {
        if input.get("at").is_none() {
            input["at"] = json!(self.world.now);
        }
        let first = self.dispatch_value(&input)?;
        self.settle(vec![first])
    }

    /// Hand a named blob to the app.
    ///
    /// # Errors
    /// As [`Session::send`].
    pub fn provide_blob(&mut self, name: &str, bytes: &[u8]) -> Result<Vec<Value>, String> {
        let out = self.bridge.provide_blob(name, bytes);
        let first: Value = serde_json::from_slice(&out).map_err(|e| format!("bad JSON: {e}"))?;
        check_version(&first)?;
        self.settle(vec![first])
    }

    /// Move the clock, firing due timers in order.
    ///
    /// # Errors
    /// As [`Session::send`].
    pub fn advance_ms(&mut self, ms: u64) -> Result<Vec<Value>, String> {
        let target = self.world.now + ms;
        let mut all = Vec::new();
        while let Some((due, id)) = self.world.pop_due(target) {
            self.world.now = self.world.now.max(due);
            let input = match self.world.held.remove(&id) {
                Some(answer) => json!({"at": due, "resolve": answer}),
                None => json!({"at": due, "resolve": {"id": id, "ok": {"fired": true}}}),
            };
            let first = self.dispatch_value(&input)?;
            all.extend(self.settle(vec![first])?);
        }
        self.world.now = target;
        Ok(all)
    }

    /// Record a response, answer its effects, and follow the cascade.
    fn settle(&mut self, first: Vec<Value>) -> Result<Vec<Value>, String> {
        let mut queue: VecDeque<Value> = first.into();
        let mut all = Vec::new();
        let mut steps = 0;
        while let Some(resp) = queue.pop_front() {
            steps += 1;
            if steps > MAX_CASCADE {
                return Err("effect cascade did not settle".to_owned());
            }
            if let Some(err) = resp.get("error") {
                return Err(format!("protocol error: {err}"));
            }
            if let Some(keys) = resp.get("dirty").and_then(Value::as_array) {
                for k in keys.iter().filter_map(Value::as_str) {
                    if !self.dirty.iter().any(|d| d == k) {
                        self.dirty.push(k.to_owned());
                    }
                }
            }
            for eff in resp
                .get("effects")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.effects.push(eff.clone());
                if let Some(answer) = self.answer(eff)? {
                    let next = self.dispatch_value(&answer)?;
                    queue.push_back(next);
                }
            }
            self.log.push(resp.clone());
            all.push(resp);
        }
        Ok(all)
    }

    /// The `resolve` input for an effect the world can answer now.
    fn answer(&mut self, eff: &Value) -> Result<Option<Value>, String> {
        let Some(id) = eff.get("id").and_then(Value::as_u64) else {
            return Ok(None);
        };
        let now = self.world.now;
        let resolve = |body: Result<Value, String>| {
            let r = match body {
                Ok(ok) => json!({"id": id, "ok": ok}),
                Err(err) => json!({"id": id, "err": err}),
            };
            Some(json!({"at": now, "resolve": r}))
        };
        if let Some(h) = eff.get("Http") {
            let method = h["method"].as_str().unwrap_or("GET");
            let path = h["path"].as_str().unwrap_or("");
            let auth = h["auth"].as_bool().unwrap_or(false);
            return match self.http.handle(method, path, h.get("body"), auth, now) {
                Ok(r) if r.delay_ms > 0 => {
                    // Held like a timer; `advance_ms` delivers it when due.
                    let due = now + r.delay_ms;
                    let answer = json!({
                        "id": id,
                        "ok": {"status": r.status, "body": with_now(r.body, due), "sent": now, "recv": due}
                    });
                    self.world.hold(id, r.delay_ms, answer);
                    Ok(None)
                }
                Ok(r) => Ok(resolve(Ok(json!({
                    "status": r.status, "body": with_now(r.body, now), "sent": now, "recv": now + r.latency_ms
                })))),
                Err(e) if self.strict => Err(e),
                Err(e) => Ok(resolve(Err(e))),
            };
        }
        if let Some(t) = eff.get("Timer") {
            self.world
                .add_timer(id, t["after_ms"].as_u64().unwrap_or(0));
            return Ok(None);
        }
        if let Some(c) = eff.get("CancelTimer") {
            self.world.cancel_timer(c["timer_id"].as_u64().unwrap_or(0));
            return Ok(None);
        }
        if let Some(s) = eff.get("Store").or_else(|| eff.get("Secure")) {
            let prefix = if eff.get("Secure").is_some() {
                "secure:"
            } else {
                ""
            };
            let key = format!("{prefix}{}", s["key"].as_str().unwrap_or(""));
            if let Some(v) = s.get("value") {
                self.world.store.insert(key, v.clone());
                return Ok(resolve(Ok(Value::Null)));
            }
            return Ok(resolve(Ok(self
                .world
                .store
                .get(&key)
                .cloned()
                .unwrap_or(Value::Null))));
        }
        if let Some(r) = eff.get("Random") {
            let hex = self
                .world
                .random_hex(u32::try_from(r["n"].as_u64().unwrap_or(0)).unwrap_or(0));
            return Ok(resolve(Ok(json!(hex))));
        }
        if let Some(answerer) = self.answerer
            && let Some(result) = answerer(eff, &mut self.world)
        {
            return Ok(resolve(result));
        }
        Ok(None)
    }
}
