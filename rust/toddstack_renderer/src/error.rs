//! The one error type.

/// Everything the renderer can refuse to do.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No backend in the chain produced a usable adapter; the CPU backend is always last, so this
    /// only happens when the caller forced a GPU backend that is not there.
    #[error("no usable backend (tried: {tried}); drop the forced backend to fall back to the CPU")]
    NoBackend {
        /// The backends attempted, in order.
        tried: String,
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
    /// Reading a GPU buffer back failed.
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

/// `Result` with the crate error.
pub type Result<T> = core::result::Result<T, Error>;
