//! The backend-neutral scene: plain data, drawn in order.

use tiny_skia::{Path, Stroke};

/// Straight (non-premultiplied) RGBA, 8 bits per channel.
pub type Rgba = [u8; 4];

/// A font added to a renderer with `Renderer::add_font`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontId(pub(crate) usize);

/// A custom batch added to a renderer with `Renderer::add_batch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BatchId(pub(crate) usize);

/// An image added to a renderer with `Renderer::add_image` or `Renderer::kernel_image`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageId(pub(crate) usize);

/// How an image is sampled when drawn at a size other than its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Sampling {
    /// Bilinear: smooth, for avatars and scaled icons.
    #[default]
    Linear,
    /// Nearest texel: hard pixels, for pixel art and exact-size icons.
    Nearest,
}

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
    /// An image stretched over a rectangle; `opacity` multiplies its alpha.
    Image {
        /// Image from `Renderer::add_image` or `Renderer::kernel_image`.
        image: ImageId,
        /// Left edge.
        x: f32,
        /// Top edge.
        y: f32,
        /// Width.
        w: f32,
        /// Height.
        h: f32,
        /// 0 to 1.
        opacity: f32,
    },
    /// A filled path (non-zero winding), antialiased.
    Fill {
        /// The outline; open subpaths close implicitly.
        path: Path,
        /// Fill.
        color: Rgba,
    },
    /// A stroked path, antialiased: the stroker's outline, filled.
    Stroke {
        /// The centre line.
        path: Path,
        /// Width, caps, joins and dashes.
        stroke: Stroke,
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
        Self {
            clear,
            prims: Vec::new(),
        }
    }

    /// Adds a rounded rectangle.
    #[expect(
        clippy::many_single_char_names,
        reason = "x, y, w, h, r are the geometry"
    )]
    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgba) {
        self.prims.push(Prim::RRect {
            x,
            y,
            w,
            h,
            r,
            color,
        });
    }

    /// Adds a plain rectangle.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
        self.rrect(x, y, w, h, 0.0, color);
    }

    /// Adds a line of the given width with round caps.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgba) {
        self.prims.push(Prim::Capsule {
            x0,
            y0,
            x1,
            y1,
            r: width / 2.0,
            color,
        });
    }

    /// Adds centred text on a baseline.
    pub fn text(&mut self, font: FontId, text: &str, x: f32, y: f32, size: f32, color: Rgba) {
        self.prims.push(Prim::Text {
            font,
            text: text.to_owned(),
            x,
            y,
            size,
            color,
        });
    }

    /// Adds an image stretched over a rectangle (`opacity` 0 to 1).
    pub fn image(&mut self, image: ImageId, x: f32, y: f32, w: f32, h: f32, opacity: f32) {
        self.prims.push(Prim::Image {
            image,
            x,
            y,
            w,
            h,
            opacity,
        });
    }

    /// Fills a path (non-zero winding).
    pub fn fill_path(&mut self, path: &Path, color: Rgba) {
        self.prims.push(Prim::Fill {
            path: path.clone(),
            color,
        });
    }

    /// Strokes a path with the given width, caps, joins and dashes.
    pub fn stroke_path(&mut self, path: &Path, stroke: &Stroke, color: Rgba) {
        self.prims.push(Prim::Stroke {
            path: path.clone(),
            stroke: stroke.clone(),
            color,
        });
    }

    /// Draws a custom batch here in paint order.
    pub fn batch(&mut self, id: BatchId) {
        self.prims.push(Prim::Batch(id));
    }
}
