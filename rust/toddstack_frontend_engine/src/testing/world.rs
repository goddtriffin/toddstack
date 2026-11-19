use std::collections::BTreeMap;
use std::fmt::Write;

use serde_json::Value;

use super::session::{HttpHandler, HttpResponse};

/// Fake capabilities: a clock that moves only when told, timers, an in-memory store and seeded
/// random bytes.
#[derive(Debug)]
pub struct World {
    pub now: u64,
    /// (due, sequence) -> effect id of the timer. Ordered, so firing is deterministic.
    pub(crate) timers: BTreeMap<(u64, u64), u64>,
    seq: u64,
    pub store: BTreeMap<String, Value>,
    /// Answers waiting for their time (a slow server), by effect id.
    pub(crate) held: BTreeMap<u64, Value>,
    rng: u64,
}

impl World {
    #[must_use]
    pub const fn new(now: u64, seed: u64) -> Self {
        Self {
            now,
            timers: BTreeMap::new(),
            seq: 0,
            store: BTreeMap::new(),
            held: BTreeMap::new(),
            rng: seed,
        }
    }

    pub(crate) fn add_timer(&mut self, id: u64, after_ms: u64) {
        self.seq += 1;
        self.timers.insert((self.now + after_ms, self.seq), id);
    }

    /// Deliver `answer` for effect `id` after `after_ms`, in timer order.
    pub(crate) fn hold(&mut self, id: u64, after_ms: u64, answer: Value) {
        self.add_timer(id, after_ms);
        self.held.insert(id, answer);
    }

    pub(crate) fn cancel_timer(&mut self, id: u64) {
        self.timers.retain(|_, v| *v != id);
        self.held.remove(&id);
    }

    /// The next timer due at or before `target`, removed.
    pub(crate) fn pop_due(&mut self, target: u64) -> Option<(u64, u64)> {
        let (&(due, seq), &id) = self.timers.iter().next()?;
        if due > target {
            return None;
        }
        self.timers.remove(&(due, seq));
        Some((due, id))
    }

    /// `SplitMix64`.
    fn next(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn random_hex(&mut self, n: u32) -> String {
        let mut out = String::new();
        for _ in 0..n {
            let _ = write!(out, "{:02x}", self.next() & 0xff);
        }
        out
    }
}

/// Scripted HTTP: responses keyed by `METHOD path`. A list answers in order, repeating the last.
#[derive(Debug, Default)]
pub struct Script {
    routes: BTreeMap<String, (Vec<HttpResponse>, usize)>,
}

impl Script {
    pub fn add(&mut self, key: &str, responses: Vec<HttpResponse>) {
        self.routes.insert(key.to_owned(), (responses, 0));
    }
}

impl HttpHandler for Script {
    fn handle(
        &mut self,
        method: &str,
        path: &str,
        _body: Option<&Value>,
        _auth: bool,
        _now: u64,
    ) -> Result<HttpResponse, String> {
        let key = format!("{method} {path}");
        let (list, next) = self
            .routes
            .get_mut(&key)
            .ok_or_else(|| format!("no scripted http for `{key}`"))?;
        let resp = list
            .get(*next)
            .or_else(|| list.last())
            .cloned()
            .ok_or_else(|| format!("empty script for `{key}`"))?;
        *next += 1;
        Ok(resp)
    }
}

impl HttpHandler for std::rc::Rc<std::cell::RefCell<Script>> {
    fn handle(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        auth: bool,
        now: u64,
    ) -> Result<HttpResponse, String> {
        self.borrow_mut().handle(method, path, body, auth, now)
    }
}
