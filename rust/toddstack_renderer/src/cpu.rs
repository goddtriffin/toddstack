//! The CPU backend: tiny-skia for shapes, atlas coverage blitted for glyphs. The pixel reference.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    reason = "pixel geometry: small values, and x/y/w/h/r read best as letters"
)]

use crate::batch::BatchState;
use crate::error::{Error, Result};
use crate::glyphs::{ATLAS_SIZE, Flat, Run};
use crate::scene::Rgba;
use tiny_skia::{Color, FillRule, LineCap, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

pub struct Cpu {
    pm: Pixmap,
}

fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut b: PathBuilder = PathBuilder::new();
    if r <= 0.0 {
        b.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
        return b.finish();
    }
    let k: f32 = r * 0.552_284_8;
    let (x1, y1): (f32, f32) = (x + w, y + h);
    b.move_to(x + r, y);
    b.line_to(x1 - r, y);
    b.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    b.line_to(x1, y1 - r);
    b.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    b.line_to(x + r, y1);
    b.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    b.line_to(x, y + r);
    b.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    b.close();
    b.finish()
}

fn to8(v: f32) -> u8 {
    (v * 255.0).round() as u8
}

impl Cpu {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let pm: Pixmap = Pixmap::new(width, height).ok_or_else(|| {
            Error::BadInput(format!("target {width}x{height} is empty or too large"))
        })?;
        Ok(Self { pm })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        *self = Self::new(width, height)?;
        Ok(())
    }

    pub fn render(
        &mut self,
        flat: &Flat,
        atlas: &[u8],
        clear: Rgba,
        batches: &[BatchState],
    ) -> Vec<u8> {
        let (w, h): (i32, i32) = (self.pm.width() as i32, self.pm.height() as i32);
        self.pm
            .fill(Color::from_rgba8(clear[0], clear[1], clear[2], clear[3]));
        let mut paint: Paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        for run in &flat.runs {
            match run {
                Run::Batch(id) => {
                    if let Some(b) = batches.get(id.0) {
                        b.cpu.draw(&b.data, b.params, &mut self.pm);
                    }
                }
                Run::Insts(range) => {
                    for i in &flat.insts[range.start as usize..range.end as usize] {
                        let c: [f32; 4] = i.color;
                        match i.p[1] as u32 {
                            2 => {
                                let (gx, gy, gw, gh): (i32, i32, i32, i32) =
                                    (i.a[0] as i32, i.a[1] as i32, i.a[2] as i32, i.a[3] as i32);
                                let (u, v): (i32, i32) = (i.uv[0] as i32, i.uv[1] as i32);
                                let data: &mut [u8] = self.pm.data_mut();
                                for row in 0..gh {
                                    for col in 0..gw {
                                        let (px, py): (i32, i32) = (gx + col, gy + row);
                                        if px < 0 || py < 0 || px >= w || py >= h {
                                            continue;
                                        }
                                        let cov: f32 = f32::from(
                                            atlas[((v + row) as u32 * ATLAS_SIZE + (u + col) as u32)
                                                as usize],
                                        ) / 255.0;
                                        let a: f32 = cov * c[3];
                                        let o: usize = ((py * w + px) * 4) as usize;
                                        for k in 0..3 {
                                            let d: f32 = f32::from(data[o + k]);
                                            data[o + k] =
                                                (c[k] * 255.0 * a + d * (1.0 - a)).round() as u8;
                                        }
                                    }
                                }
                            }
                            kind => {
                                paint.set_color_rgba8(to8(c[0]), to8(c[1]), to8(c[2]), to8(c[3]));
                                if kind == 0 {
                                    if let Some(p) =
                                        rrect_path(i.a[0], i.a[1], i.a[2], i.a[3], i.p[0])
                                    {
                                        self.pm.fill_path(
                                            &p,
                                            &paint,
                                            FillRule::Winding,
                                            Transform::identity(),
                                            None,
                                        );
                                    }
                                } else {
                                    let mut b: PathBuilder = PathBuilder::new();
                                    b.move_to(i.a[0], i.a[1]);
                                    b.line_to(i.a[2], i.a[3]);
                                    let stroke: Stroke = Stroke {
                                        width: i.p[0] * 2.0,
                                        line_cap: LineCap::Round,
                                        ..Stroke::default()
                                    };
                                    if let Some(p) = b.finish() {
                                        self.pm.stroke_path(
                                            &p,
                                            &paint,
                                            &stroke,
                                            Transform::identity(),
                                            None,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        self.pm.data().to_vec()
    }
}
