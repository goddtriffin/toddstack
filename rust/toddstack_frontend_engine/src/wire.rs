use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct Resolve {
    pub id: u64,
    pub ok: Option<Value>,
    pub err: Option<String>,
}

#[derive(Deserialize)]
pub struct PointerIn {
    pub phase: u8,
    pub x: f32,
    pub y: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Act(Value),
    Resolve(Resolve),
    Pointer(PointerIn),
    View(String),
    Introspect {},
    /// Return queued dirty keys and effects (left over from the typed pointer path or boot).
    Flush {},
}

#[derive(Deserialize)]
pub struct Input {
    pub at: Option<u64>,
    #[serde(flatten)]
    pub verb: Verb,
}
