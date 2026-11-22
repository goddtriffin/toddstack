//! The shared glyph atlas and the flattening of a scene into instance records.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "pixel and atlas coordinates are small non-negative values"
)]

use crate::error::{Error, Result};
use crate::scene::{BatchId, FontId, ImageId, Prim, Rgba, Scene};
use std::collections::HashMap;
use std::ops::Range;

/// Side of the square R8 atlas.
pub const ATLAS_SIZE: u32 = 512;

/// One draw record, 64 bytes, four float4 attributes. `p.y` is the kind: 0 rounded rect
/// (`a` = x, y, w, h), 1 capsule (`a` = x0, y0, x1, y1), 2 glyph (`a` = quad, `uv` = atlas x, y).
/// A sprite (its own run, so `p.y` is unused): `a` = destination rect, `uv` = source rect as
/// fractions of the image, `p` = rotation, half a texel in u, half a texel in v, `color` = tint.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Inst {
    pub a: [f32; 4],
    pub uv: [f32; 4],
    /// Radius, kind, 0, 0.
    pub p: [f32; 4],
    /// Straight 0..1.
    pub color: [f32; 4],
}

pub enum Run {
    Insts(Range<u32>),
    Batch(BatchId),
    /// Sprite quads of one image, in order: their records are `insts[insts]`.
    Image {
        id: ImageId,
        insts: Range<u32>,
    },
    /// A range of `Flat::paths`, filled in order.
    Paths(Range<u32>),
}

/// One filled outline (a stroke is already its outline) in a straight 0..1 colour.
pub struct PathDraw {
    pub outline: tiny_skia::Path,
    pub color: [f32; 4],
}

pub struct Flat {
    pub insts: Vec<Inst>,
    pub paths: Vec<PathDraw>,
    pub runs: Vec<Run>,
}

#[derive(Clone, Copy)]
struct Glyph {
    x: u32,
    y: u32,
    xmin: i32,
    ymin: i32,
    w: u32,
    h: u32,
    advance: f32,
}

pub struct Atlas {
    fonts: Vec<fontdue::Font>,
    pub pixels: Vec<u8>,
    cache: HashMap<(usize, char, u32), Glyph>,
    cx: u32,
    cy: u32,
    row_h: u32,
    /// Set when a glyph was added since the last upload.
    pub dirty: bool,
}

/// Closes the open run of plain instances.
fn flush(runs: &mut Vec<Run>, start: &mut u32, len: usize) {
    let end: u32 = len as u32;
    if end > *start {
        runs.push(Run::Insts(*start..end));
    }
    *start = end;
}

/// Adds an outline, extending the previous path run when nothing was drawn between.
fn push_path(runs: &mut Vec<Run>, paths: &mut Vec<PathDraw>, outline: tiny_skia::Path, c: Rgba) {
    let at: u32 = paths.len() as u32;
    paths.push(PathDraw {
        outline,
        color: col(c),
    });
    match runs.last_mut() {
        Some(Run::Paths(r)) if r.end == at => r.end = at + 1,
        _ => runs.push(Run::Paths(at..at + 1)),
    }
}

fn size_of_image(images: &[Option<(u32, u32)>], id: ImageId) -> Option<(u32, u32)> {
    images.get(id.0).copied().flatten()
}

/// Adds one sprite record, extending the previous run when it is the same image drawn straight
/// after. A removed or unknown image, or an empty source, draws nothing.
#[expect(
    clippy::too_many_arguments,
    reason = "the flattening state plus the sprite's own fields"
)]
fn push_sprite(
    runs: &mut Vec<Run>,
    insts: &mut Vec<Inst>,
    start: &mut u32,
    images: &[Option<(u32, u32)>],
    image: ImageId,
    src: Option<[u32; 4]>,
    dst: [f32; 4],
    rotation: f32,
    tint: [f32; 4],
) {
    let (Some((iw, ih)), Some([sx, sy, sw, sh])) = (size_of_image(images, image), src) else {
        return;
    };
    let (sx, sy): (u32, u32) = (sx.min(iw), sy.min(ih));
    let (sw, sh): (u32, u32) = (sw.min(iw - sx), sh.min(ih - sy));
    // An empty, inverted or NaN destination draws nothing, on every backend.
    if sw == 0 || sh == 0 || !(dst[2] > 0.0 && dst[3] > 0.0) {
        return;
    }
    flush(runs, start, insts.len());
    let at: u32 = insts.len() as u32;
    let (fw, fh): (f32, f32) = (iw as f32, ih as f32);
    insts.push(Inst {
        a: dst,
        uv: [
            sx as f32 / fw,
            sy as f32 / fh,
            sw as f32 / fw,
            sh as f32 / fh,
        ],
        p: [rotation, 0.5 / fw, 0.5 / fh, 0.0],
        color: tint,
    });
    match runs.last_mut() {
        Some(Run::Image { id, insts: r }) if *id == image && r.end == at => r.end = at + 1,
        _ => runs.push(Run::Image {
            id: image,
            insts: at..at + 1,
        }),
    }
    *start = insts.len() as u32;
}

fn col(c: Rgba) -> [f32; 4] {
    c.map(|v| f32::from(v) / 255.0)
}

impl Atlas {
    pub fn new() -> Self {
        Self {
            fonts: Vec::new(),
            pixels: vec![0; (ATLAS_SIZE * ATLAS_SIZE) as usize],
            cache: HashMap::new(),
            cx: 1,
            cy: 1,
            row_h: 0,
            dirty: true,
        }
    }

    pub fn add_font(&mut self, bytes: &[u8]) -> Result<FontId> {
        let font: fontdue::Font =
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                .map_err(|e| Error::Font(e.to_owned()))?;
        self.fonts.push(font);
        Ok(FontId(self.fonts.len() - 1))
    }

    fn glyph(&mut self, font: FontId, ch: char, size: f32) -> Result<Glyph> {
        let key: (usize, char, u32) = (font.0, ch, size.to_bits());
        if let Some(g) = self.cache.get(&key) {
            return Ok(*g);
        }
        let face: &fontdue::Font = self.fonts.get(font.0).ok_or(Error::UnknownFont)?;
        let (m, bitmap) = face.rasterize(ch, size);
        let (w, h): (u32, u32) = (m.width as u32, m.height as u32);
        if self.cx + w + 1 > ATLAS_SIZE {
            self.cx = 1;
            self.cy += self.row_h + 1;
            self.row_h = 0;
        }
        if self.cy + h >= ATLAS_SIZE {
            return Err(Error::AtlasFull);
        }
        for row in 0..h {
            let dst: usize = ((self.cy + row) * ATLAS_SIZE + self.cx) as usize;
            let src: usize = (row * w) as usize;
            self.pixels[dst..dst + w as usize].copy_from_slice(&bitmap[src..src + w as usize]);
        }
        let g: Glyph = Glyph {
            x: self.cx,
            y: self.cy,
            xmin: m.xmin,
            ymin: m.ymin,
            w,
            h,
            advance: m.advance_width,
        };
        self.cx += w + 1;
        self.row_h = self.row_h.max(h);
        self.dirty = true;
        self.cache.insert(key, g);
        Ok(g)
    }

    /// Lays text out into the atlas and flattens the scene to instances, in paint order.
    #[expect(clippy::too_many_lines, reason = "one match over every primitive kind")]
    pub fn flatten(&mut self, scene: &Scene, images: &[Option<(u32, u32)>]) -> Result<Flat> {
        let mut insts: Vec<Inst> = Vec::new();
        let mut runs: Vec<Run> = Vec::new();
        let mut paths: Vec<PathDraw> = Vec::new();
        let mut start: u32 = 0;
        for p in &scene.prims {
            match p {
                Prim::RRect {
                    x,
                    y,
                    w,
                    h,
                    r,
                    color,
                } => insts.push(Inst {
                    a: [*x, *y, *w, *h],
                    uv: [0.0; 4],
                    p: [*r, 0.0, 0.0, 0.0],
                    color: col(*color),
                }),
                Prim::Capsule {
                    x0,
                    y0,
                    x1,
                    y1,
                    r,
                    color,
                } => insts.push(Inst {
                    a: [*x0, *y0, *x1, *y1],
                    uv: [0.0; 4],
                    p: [*r, 1.0, 0.0, 0.0],
                    color: col(*color),
                }),
                Prim::Text {
                    font,
                    text,
                    x,
                    y,
                    size,
                    color,
                } => {
                    let mut glyphs: Vec<Glyph> = Vec::with_capacity(text.len());
                    for ch in text.chars() {
                        glyphs.push(self.glyph(*font, ch, *size)?);
                    }
                    let total: f32 = glyphs.iter().map(|g| g.advance).sum();
                    let mut pen: f32 = x - total / 2.0;
                    for g in glyphs {
                        if g.w > 0 {
                            let gx: f32 = (pen + g.xmin as f32).round();
                            let gy: f32 = (y - g.ymin as f32 - g.h as f32).round();
                            insts.push(Inst {
                                a: [gx, gy, g.w as f32, g.h as f32],
                                uv: [g.x as f32, g.y as f32, 0.0, 0.0],
                                p: [0.0, 2.0, 0.0, 0.0],
                                color: col(*color),
                            });
                        }
                        pen += g.advance;
                    }
                }
                Prim::Batch(id) => {
                    flush(&mut runs, &mut start, insts.len());
                    runs.push(Run::Batch(*id));
                }
                Prim::Image {
                    image,
                    x,
                    y,
                    w,
                    h,
                    opacity,
                } => {
                    let tint: [f32; 4] = [1.0, 1.0, 1.0, opacity.clamp(0.0, 1.0)];
                    let whole: Option<[u32; 4]> =
                        size_of_image(images, *image).map(|(iw, ih)| [0, 0, iw, ih]);
                    push_sprite(
                        &mut runs,
                        &mut insts,
                        &mut start,
                        images,
                        *image,
                        whole,
                        [*x, *y, *w, *h],
                        0.0,
                        tint,
                    );
                }
                Prim::Sprite {
                    image,
                    src,
                    dst,
                    rotation,
                    tint,
                } => push_sprite(
                    &mut runs,
                    &mut insts,
                    &mut start,
                    images,
                    *image,
                    Some(*src),
                    *dst,
                    *rotation,
                    col(*tint),
                ),
                Prim::Fill { path, color } => {
                    flush(&mut runs, &mut start, insts.len());
                    push_path(&mut runs, &mut paths, path.clone(), *color);
                }
                Prim::Stroke {
                    path,
                    stroke,
                    color,
                } => {
                    let dashed: Option<tiny_skia::Path> = match &stroke.dash {
                        Some(d) => path.dash(d, 1.0),
                        None => Some(path.clone()),
                    };
                    if let Some(outline) = dashed.and_then(|p| p.stroke(stroke, 1.0)) {
                        flush(&mut runs, &mut start, insts.len());
                        push_path(&mut runs, &mut paths, outline, *color);
                    }
                }
            }
        }
        flush(&mut runs, &mut start, insts.len());
        Ok(Flat { insts, paths, runs })
    }
}
