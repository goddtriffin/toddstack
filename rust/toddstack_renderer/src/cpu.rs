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
use crate::paths::rounded_rect;
use crate::scene::Rgba;
use crate::scene::{ImageId, Sampling};
use tiny_skia::{
    Color, FillRule, FilterQuality, LineCap, Paint, PathBuilder, Pattern, Pixmap, Rect, SpreadMode,
    Stroke, Transform,
};

/// The canvas's 2D context, when the target is a canvas.
#[cfg(target_arch = "wasm32")]
enum Ctx2d {
    Html(web_sys::CanvasRenderingContext2d),
    Offscreen(web_sys::OffscreenCanvasRenderingContext2d),
}

pub struct Cpu {
    pm: Pixmap,
    #[cfg(target_arch = "wasm32")]
    ctx: Option<Ctx2d>,
    /// Premultiplied pixels and how to sample them, by image id.
    images: Vec<Option<(Pixmap, FilterQuality)>>,
}

fn to8(v: f32) -> u8 {
    (v * 255.0).round() as u8
}

/// Stretches `img` over the quad in `i`, in the instance's opacity.
#[expect(clippy::cast_precision_loss, reason = "image sizes are far below 2^23")]
fn draw_image(pm: &mut Pixmap, img: &Pixmap, quality: FilterQuality, i: &crate::glyphs::Inst) {
    let (x, y, w, h): (f32, f32, f32, f32) = (i.a[0], i.a[1], i.a[2], i.a[3]);
    let Some(rect) = Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let shader = Pattern::new(
        img.as_ref(),
        SpreadMode::Pad,
        quality,
        i.p[0],
        Transform::from_row(
            w / img.width() as f32,
            0.0,
            0.0,
            h / img.height() as f32,
            x,
            y,
        ),
    );
    let paint: Paint = Paint {
        shader,
        anti_alias: false,
        ..Paint::default()
    };
    pm.fill_rect(rect, &paint, Transform::identity(), None);
}

fn fill_paths(pm: &mut Pixmap, paths: &[crate::glyphs::PathDraw]) {
    let mut paint: Paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    for d in paths {
        paint.set_color_rgba8(
            to8(d.color[0]),
            to8(d.color[1]),
            to8(d.color[2]),
            to8(d.color[3]),
        );
        pm.fill_path(
            &d.outline,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

impl Cpu {
    pub fn new(target: &crate::renderer::Target) -> Result<Self> {
        let (width, height): (u32, u32) = (target.width, target.height);
        let pm: Pixmap = Pixmap::new(width, height).ok_or_else(|| {
            Error::BadInput(format!("target {width}x{height} is empty or too large"))
        })?;
        Ok(Self {
            pm,
            #[cfg(target_arch = "wasm32")]
            ctx: Self::context(&target.kind)?,
            images: Vec::new(),
        })
    }

    #[cfg(target_arch = "wasm32")]
    fn context(kind: &crate::renderer::Kind) -> Result<Option<Ctx2d>> {
        use crate::renderer::Kind;
        use wasm_bindgen::JsCast as _;
        let none = || {
            Error::Surface(
                "the canvas has no 2D context (a GPU context already took it)".to_owned(),
            )
        };
        Ok(match kind {
            Kind::Image => None,
            Kind::Html(c) => Some(Ctx2d::Html(
                c.get_context("2d")
                    .ok()
                    .flatten()
                    .and_then(|o| o.dyn_into().ok())
                    .ok_or_else(none)?,
            )),
            Kind::Offscreen(c) => Some(Ctx2d::Offscreen(
                c.get_context("2d")
                    .ok()
                    .flatten()
                    .and_then(|o| o.dyn_into().ok())
                    .ok_or_else(none)?,
            )),
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.pm = Pixmap::new(width, height).ok_or_else(|| {
            Error::BadInput(format!("target {width}x{height} is empty or too large"))
        })?;
        #[cfg(target_arch = "wasm32")]
        self.set_canvas_size(width, height);
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn set_canvas_size(&self, width: u32, height: u32) {
        match &self.ctx {
            Some(Ctx2d::Html(c)) => {
                if let Some(cv) = c.canvas() {
                    cv.set_width(width);
                    cv.set_height(height);
                }
            }
            Some(Ctx2d::Offscreen(c)) => {
                let cv = c.canvas();
                cv.set_width(width);
                cv.set_height(height);
            }
            None => {}
        }
    }

    #[cfg_attr(
        not(target_arch = "wasm32"),
        expect(
            clippy::unnecessary_wraps,
            reason = "fallible on wasm32, where it writes to a canvas"
        )
    )]
    /// Draws the frame, then `putImageData`s it into the canvas (a no-op for an offscreen target).
    pub fn present(
        &mut self,
        flat: &Flat,
        atlas: &[u8],
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<()> {
        self.draw(flat, atlas, clear, batches);
        #[cfg(target_arch = "wasm32")]
        {
            use wasm_bindgen::Clamped;
            let img: web_sys::ImageData = web_sys::ImageData::new_with_u8_clamped_array_and_sh(
                Clamped(self.pm.data()),
                self.pm.width(),
                self.pm.height(),
            )
            .map_err(|e| Error::Surface(format!("{e:?}")))?;
            let put = match &self.ctx {
                Some(Ctx2d::Html(c)) => c.put_image_data(&img, 0.0, 0.0),
                Some(Ctx2d::Offscreen(c)) => c.put_image_data(&img, 0.0, 0.0),
                None => Ok(()),
            };
            put.map_err(|e| Error::Surface(format!("{e:?}")))?;
        }
        Ok(())
    }

    /// Stores an image from premultiplied RGBA8 bytes.
    pub fn set_image(&mut self, id: ImageId, w: u32, h: u32, premul: Vec<u8>, sampling: Sampling) {
        let pm: Option<Pixmap> =
            tiny_skia::IntSize::from_wh(w, h).and_then(|s| Pixmap::from_vec(premul, s));
        if self.images.len() <= id.0 {
            self.images.resize_with(id.0 + 1, || None);
        }
        self.images[id.0] = pm.map(|pm| {
            (
                pm,
                match sampling {
                    Sampling::Linear => FilterQuality::Bilinear,
                    Sampling::Nearest => FilterQuality::Nearest,
                },
            )
        });
    }

    pub fn remove_image(&mut self, id: ImageId) {
        if let Some(slot) = self.images.get_mut(id.0) {
            *slot = None;
        }
    }

    pub fn render(
        &mut self,
        flat: &Flat,
        atlas: &[u8],
        clear: Rgba,
        batches: &[BatchState],
    ) -> Vec<u8> {
        self.draw(flat, atlas, clear, batches);
        self.pm.data().to_vec()
    }

    fn draw(&mut self, flat: &Flat, atlas: &[u8], clear: Rgba, batches: &[BatchState]) {
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
                Run::Image { id, inst } => {
                    if let Some(Some((img, quality))) = self.images.get(id.0) {
                        draw_image(&mut self.pm, img, *quality, &flat.insts[*inst as usize]);
                    }
                }
                Run::Paths(range) => {
                    fill_paths(
                        &mut self.pm,
                        &flat.paths[range.start as usize..range.end as usize],
                    );
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
                                        rounded_rect(i.a[0], i.a[1], i.a[2], i.a[3], i.p[0])
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
    }
}
