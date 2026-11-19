//! App-agnostic frontend core: the Elm-style loop `update(model, msg) -> effects`.
//!
//! The core is pure: it reads no clock, network, disk or randomness. Effects are data; the shell
//! (browser, iOS, Android, a test harness) runs them and answers with `resolve`. Shells only draw
//! view models and run effects. See the README for the pattern and a minimal app.

mod app;
mod effect;
mod policy;
mod runtime;
mod wire;

#[cfg(feature = "example")]
pub mod example;
#[cfg(feature = "ffi")]
pub mod ffi;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use app::{ActionSpec, App, Ctx, FieldType, Pending, Pointer};
pub use effect::{Effect, EffectReq, HttpReply, NavigateOp, NoEffect};
pub use policy::{ClockOffset, RetryPolicy};
pub use runtime::{BRIDGE_PROTOCOL, Bridge, Runtime};

/// Runs the README's Rust examples as doctests, so they cannot rot.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
