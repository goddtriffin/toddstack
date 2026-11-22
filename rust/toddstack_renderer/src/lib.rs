//! One GPU renderer on wgpu with a tiny-skia CPU fallback that doubles as the pixel reference.
//!
//! Two layers share one [`Renderer`]:
//! - high: a [`Scene`] of rounded rects, lines and text from one shared glyph atlas;
//! - low: custom batches ([`BatchDesc`]: your instance layout, WGSL and data) and compute
//!   [`Kernel`]s (compute, fragment and CPU forms).
//!
//! See the README for the fallback chain and how to run the demos.

mod backend;
mod batch;
mod cpu;
pub mod demo;
mod diff;
mod error;
mod glyphs;
mod gpu;
mod kernel;
mod renderer;
mod scene;
#[cfg(target_arch = "wasm32")]
mod web;

pub use backend::{Backend, Capabilities};
pub use batch::{BatchDesc, CpuBatch};
pub use diff::{Budget, Diff};
pub use error::{Error, Result};
pub use kernel::{Binding, CpuKernel, Kernel, KernelOutput, KernelPath};
pub use renderer::{Options, Renderer, Target};
pub use scene::{BatchId, FontId, Prim, Rgba, Scene};
// Part of the public API (the CPU form of a batch draws into a `Pixmap`).
pub use tiny_skia;
pub use wgpu;
