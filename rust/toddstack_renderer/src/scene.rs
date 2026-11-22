//! The backend-neutral scene: plain data, drawn in order.

/// Straight (non-premultiplied) RGBA, 8 bits per channel.
pub type Rgba = [u8; 4];

/// A font added to a renderer with `Renderer::add_font`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontId(pub(crate) usize);

/// A custom batch added to a renderer with `Renderer::add_batch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BatchId(pub(crate) usize);

/// One thing to draw. Coordinates are pixels, origin top-left, y down.
#[derive(Clone, Debug)]
pub enum Prim {
    /// Rounded rectangle (`r` of 0 is a plain rectangle).
    RRect {
        /// Left edge.
        x: f32,
        /// Top edge.
        y: f32,
        /// Width.
        w: f32,
        /// Height.
        h: f32,
        /// Corner radius.
        r: f32,
        /// Fill.
        color: Rgba,
    },
    /// Line segment with round caps; `r` is half the line width.
    Capsule {
        /// Start x.
        x0: f32,
        /// Start y.
        y0: f32,
        /// End x.
        x1: f32,
        /// End y.
        y1: f32,
        /// Half width.
        r: f32,
        /// Fill.
        color: Rgba,
    },
    /// Text centred on `x`, sitting on the baseline `y`, from the shared glyph atlas.
    Text {
        /// Font from `Renderer::add_font`.
        font: FontId,
        /// The text.
        text: String,
        /// Horizontal centre.
        x: f32,
        /// Baseline.
        y: f32,
        /// Pixel size.
        size: f32,
        /// Fill.
        color: Rgba,
    },
    /// A custom batch drawn at this point in paint order.
    Batch(BatchId),
}

/// An ordered list of primitives over a clear colour.
#[derive(Clone, Debug)]
pub struct Scene {
    /// What the target is cleared to first.
    pub clear: Rgba,
    /// Painted first to last.
    pub prims: Vec<Prim>,
}

impl Scene {
    /// An empty scene.
    #[must_use]
    pub const fn new(clear: Rgba) -> Self {
        Self { clear, prims: Vec::new() }
    }

    /// Adds a rounded rectangle.
    #[expect(clippy::many_single_char_names, reason = "x, y, w, h, r are the geometry")]
    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgba) {
        self.prims.push(Prim::RRect { x, y, w, h, r, color });
    }

    /// Adds a plain rectangle.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
        self.rrect(x, y, w, h, 0.0, color);
    }

    /// Adds a line of the given width with round caps.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgba) {
        self.prims.push(Prim::Capsule { x0, y0, x1, y1, r: width / 2.0, color });
    }

    /// Adds centred text on a baseline.
    pub fn text(&mut self, font: FontId, text: &str, x: f32, y: f32, size: f32, color: Rgba) {
        self.prims.push(Prim::Text { font, text: text.to_owned(), x, y, size, color });
    }

    /// Draws a custom batch here in paint order.
    pub fn batch(&mut self, id: BatchId) {
        self.prims.push(Prim::Batch(id));
    }
}
