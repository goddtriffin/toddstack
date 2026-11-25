//! The one error type.

use crate::fallback::Attempt;

/// Everything the renderer can refuse to do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Every backend in the chain failed (the CPU last); the final fallback event, with
    /// `to: None`, was reported too. Also the result of a forced backend that is unavailable.
    #[error("no renderer works here (tried: {})", tried(.attempts))]
    NoRendererAvailable {
        /// Each backend tried, in order, and why it failed.
        attempts: Vec<Attempt>,
    },
    /// The GPU device was lost (the renderer reported a `DeviceLost` event). It cannot draw any
    /// more: build a new `Renderer`. See the README ("Device loss").
    #[error("the GPU device was lost ({reason}): {message}")]
    DeviceLost {
        /// `destroyed` (someone destroyed the device) or `unknown` (a driver or GPU reset).
        reason: String,
        /// The platform's message.
        message: String,
    },
    /// The adapter refused the device request.
    #[error("the GPU device request failed: {0}")]
    Device(String),
    /// The shared glyph atlas has no room left for a new glyph.
    #[error("the glyph atlas is full; use fewer distinct glyph sizes")]
    AtlasFull,
    /// A `FontId` that this renderer never issued.
    #[error("unknown font: add it with Renderer::add_font first")]
    UnknownFont,
    /// A `BatchId` that this renderer never issued.
    #[error("unknown batch: add it with Renderer::add_batch first")]
    UnknownBatch,
    /// An `ImageId` that this renderer never issued, or one already removed.
    #[error("unknown image: add it with Renderer::add_image first")]
    UnknownImage,
    /// A `SurfaceId` that this renderer never issued, or one already detached.
    #[error("unknown surface: attach a canvas first, and do not use it after detach")]
    UnknownSurface,
    /// The font bytes did not parse.
    #[error("the font did not parse: {0}")]
    Font(String),
    /// The caller's input is inconsistent (a size, an offset, a binding).
    #[error("bad input: {0}")]
    BadInput(String),
    /// No path of the kernel can run on this backend.
    #[error(
        "kernel `{kernel}` has no form this backend can run: add a fragment form or a CPU function"
    )]
    KernelUnavailable {
        /// The kernel's name.
        kernel: String,
    },
    /// The canvas surface could not be recovered after being lost or outdated.
    #[error("the canvas surface is unusable: {0}")]
    Surface(String),
    /// The GPU rejected WGSL or a pipeline built from it (the validation message).
    #[error("shader or pipeline rejected: {0}")]
    Shader(String),
    /// Reading a GPU buffer back failed.
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

/// `Result` with the crate error.
pub type Result<T> = core::result::Result<T, Error>;

fn tried(attempts: &[Attempt]) -> String {
    attempts
        .iter()
        .map(|a| format!("{}: {}", a.backend.name(), a.reason))
        .collect::<Vec<_>>()
        .join("; ")
}
