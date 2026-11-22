//! The shared glyph atlas and the flattening of a scene into instance records.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "pixel and atlas coordinates are small non-negative values"
)]

use crate::error::{Error, Result};
use crate::scene::{BatchId, FontId, Prim, Rgba, Scene};
use std::collections::HashMap;
use std::ops::Range;

/// Side of the square R8 atlas.
pub const ATLAS_SIZE: u32 = 512;

/// One draw record, 64 bytes, four float4 attributes. `p.y` is the kind: 0 rounded rect
/// (`a` = x, y, w, h), 1 capsule (`a` = x0, y0, x1, y1), 2 glyph (`a` = quad, `uv` = atlas x, y).
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
}

pub struct Flat {
    pub insts: Vec<Inst>,
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
    pub fn flatten(&mut self, scene: &Scene) -> Result<Flat> {
        let mut insts: Vec<Inst> = Vec::new();
        let mut runs: Vec<Run> = Vec::new();
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
                    let end: u32 = insts.len() as u32;
                    if end > start {
                        runs.push(Run::Insts(start..end));
                    }
                    start = end;
                    runs.push(Run::Batch(*id));
                }
            }
        }
        let end: u32 = insts.len() as u32;
        if end > start {
            runs.push(Run::Insts(start..end));
        }
        Ok(Flat { insts, runs })
    }
}
