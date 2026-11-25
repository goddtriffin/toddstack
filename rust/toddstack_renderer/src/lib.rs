//! One GPU renderer on wgpu with a tiny-skia CPU fallback that doubles as the pixel reference.
//!
//! Two layers share one [`Renderer`]:
//! - high: a [`Scene`] of rounded rects, lines, text from one shared glyph atlas, images and
//!   sprites (sub-rects, rotated and tinted; app RGBA or a kernel's output, no readback) and
//!   filled or stroked paths, to one target or many canvases, opaque or transparent;
//! - low: custom batches ([`BatchDesc`]: your instance layout, WGSL and data) and compute
//!   [`Kernel`]s (compute, fragment and CPU forms).
//!
//! See the README for the fallback chain and how to run the demos.

mod backend;
mod batch;
#[cfg(target_arch = "wasm32")]
mod canvas;
mod cpu;
#[cfg(feature = "web-demo")]
pub mod demo;
mod diff;
mod error;
mod fallback;
mod glyphs;
mod gpu;
mod gpu_image;
mod gpu_path;
mod kernel;
mod paths;
mod renderer;
mod scene;
mod timeout;
#[cfg(all(target_arch = "wasm32", feature = "web-demo"))]
#[expect(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "a JS-facing demo surface: errors cross as JsError, the docs are in web/renderer.ts"
)]
mod web;

pub use backend::{Backend, Capabilities};
pub use batch::{BatchDesc, CpuBatch};
pub use diff::{Budget, Diff};
pub use error::{Error, Result};
pub use fallback::{Attempt, FallbackEvent, FallbackReason, MEMORY_VERSION};
pub use kernel::{Binding, CpuKernel, Kernel, KernelOutput, KernelPath};
pub use paths::rounded_rect;
pub use renderer::{Options, Renderer, SurfaceId, Target, unpremultiply_rgba8};
pub use scene::{BatchId, FontId, ImageId, Prim, Rgba, Sampling, Scene};
// Part of the public API (the CPU form of a batch draws into a `Pixmap`).
pub use tiny_skia;
#[cfg(all(target_arch = "wasm32", feature = "web-demo"))]
pub use web::{WebBoards, WebRenderer};
