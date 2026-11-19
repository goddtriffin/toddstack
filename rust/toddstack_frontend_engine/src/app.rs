use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::effect::{Effect, EffectReq};

/// One pointer sample on the typed fast path. `x` and `y` are in units the app chooses (cells,
/// pixels, a 0..1 canvas); the engine never interprets them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pointer {
    pub phase: u8,
    pub x: f32,
    pub y: f32,
    pub at_ms: u64,
}

impl Pointer {
    pub const DOWN: u8 = 0;
    pub const MOVE: u8 = 1;
    pub const UP: u8 = 2;
    pub const CANCEL: u8 = 3;
}

/// What `update` may do besides mutating the model: mark views dirty and request effects.
#[derive(Debug)]
pub struct Ctx<X> {
    pub(crate) at: u64,
    pub(crate) next_id: u64,
    pub(crate) dirty: BTreeSet<String>,
    pub(crate) effects: Vec<EffectReq<X>>,
}

impl<X> Default for Ctx<X> {
    fn default() -> Self {
        Self {
            at: 0,
            next_id: 0,
            dirty: BTreeSet::new(),
            effects: Vec::new(),
        }
    }
}

impl<X> Ctx<X> {
    /// Wall clock of the input being handled (ms since epoch, UTC).
    #[must_use]
    pub const fn at(&self) -> u64 {
        self.at
    }

    /// Mark a view key as changed; the shell pulls it.
    pub fn dirty(&mut self, key: impl Into<String>) {
        self.dirty.insert(key.into());
    }

    /// Request an effect. Returns its id; the answer arrives in [`App::resolve`].
    pub fn emit(&mut self, effect: Effect<X>) -> u64 {
        self.next_id += 1;
        let id: u64 = self.next_id;
        self.effects.push(EffectReq { id, effect });
        id
    }

    /// Request an app-defined effect.
    pub fn emit_app(&mut self, effect: X) -> u64 {
        self.emit(Effect::App(effect))
    }
}

/// Effect ids still waiting for their answer, each with what the app was doing when it asked.
/// Keeps `resolve` a `match` on a state-machine enum instead of scattered flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending<T> {
    waiting: BTreeMap<u64, T>,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self {
            waiting: BTreeMap::new(),
        }
    }
}

impl<T> Pending<T> {
    /// Emit `effect` and remember `what` under its id.
    pub fn emit<X>(&mut self, ctx: &mut Ctx<X>, effect: Effect<X>, what: T) -> u64 {
        let id: u64 = ctx.emit(effect);
        self.waiting.insert(id, what);
        id
    }

    /// The remembered state for an answered id; `None` for an id this app never tracked (or a
    /// stale one).
    pub fn take(&mut self, id: u64) -> Option<T> {
        self.waiting.remove(&id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

/// JSON-schema type of one action field.
#[derive(Debug, Clone, Copy)]
pub enum FieldType {
    String,
    Integer,
    Number,
    Boolean,
}

impl FieldType {
    const fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

/// A legal action with a hand-rolled JSON schema (an object whose fields are all required).
#[derive(Debug, Clone)]
pub struct ActionSpec {
    pub name: String,
    pub schema: Value,
}

impl ActionSpec {
    /// An action with no fields.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            schema: json!({}),
        }
    }

    /// Add a required field.
    #[must_use]
    pub fn field(mut self, field: &str, ty: FieldType) -> Self {
        let mut props: Value = self.schema.get("properties").cloned().unwrap_or(json!({}));
        let mut required: Value = self.schema.get("required").cloned().unwrap_or(json!([]));
        props[field] = json!({"type": ty.name()});
        if let Some(list) = required.as_array_mut() {
            list.push(json!(field));
        }
        self.schema = json!({"type": "object", "properties": props, "required": required});
        self
    }
}

/// The app: a model plus pure transitions. All time comes from `ctx.at()`.
pub trait App: Sized {
    /// User intents, decoded from the `act` field (serde external tagging: `"Inc"`, `{"Add":{"n":2}}`).
    type Action: DeserializeOwned;
    /// The app's own effects, next to the built-in ones. [`NoEffect`](crate::NoEffect) for none.
    type Effect: Serialize;

    /// Build the model from the shell's boot facts.
    fn boot(boot: &Value, ctx: &mut Ctx<Self::Effect>) -> Self;
    fn update(&mut self, action: Self::Action, ctx: &mut Ctx<Self::Effect>);
    /// The answer to effect `id`: `Ok(value)` or `Err(message)`.
    fn resolve(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx<Self::Effect>);
    /// Typed pointer fast path. Mark views dirty through `ctx`.
    fn pointer(&mut self, _pointer: Pointer, _ctx: &mut Ctx<Self::Effect>) {}
    /// Large binary input (`provide_blob`).
    fn blob(&mut self, _name: &str, _bytes: &[u8], _ctx: &mut Ctx<Self::Effect>) {}
    /// The JSON of one view key, or `None` when the key is unknown.
    fn view(&self, key: &str) -> Option<Value>;
    /// Current screen name; its view key is the same name.
    fn screen(&self) -> String;
    /// Actions legal right now.
    fn actions(&self) -> Vec<ActionSpec>;
}
