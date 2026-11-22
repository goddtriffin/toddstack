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
use crate::glyphs::{ATLAS_SIZE, Flat, Inst, Run};
use crate::paths::rounded_rect;
use crate::renderer::{Kind, Target};
use crate::scene::Rgba;
use crate::scene::{ImageId, Sampling};
use tiny_skia::{
    Color, FillRule, FilterQuality, IntRect, LineCap, Paint, PathBuilder, Pattern, Pixmap, Rect,
    SpreadMode, Stroke, Transform,
};

/// One drawing surface: the pixels, and on the web the canvas's 2D context they are put into.
struct Out {
    pm: Pixmap,
    #[cfg(target_arch = "wasm32")]
    ctx: Option<crate::canvas::Ctx2d>,
}

pub struct Cpu {
    /// The target `new` was given; `None` for a device-only renderer.
    primary: Option<Out>,
    /// Attached canvases, by `SurfaceId`; `None` once detached.
    slots: Vec<Option<Out>>,
    /// Premultiplied pixels and how to sample them, by image id.
    images: Vec<Option<(Pixmap, FilterQuality)>>,
    transparent: bool,
}

fn to8(v: f32) -> u8 {
    (v * 255.0).round() as u8
}

fn new_pixmap(width: u32, height: u32) -> Result<Pixmap> {
    Pixmap::new(width, height)
        .ok_or_else(|| Error::BadInput(format!("target {width}x{height} is empty or too large")))
}

/// Draws one sprite record: the source rect of `img`, tinted, rotated about the centre of the
/// destination rect. The source is cropped first so that sampling never leaves it, as on the GPU.
#[expect(clippy::cast_precision_loss, reason = "image sizes are far below 2^23")]
fn draw_sprite(pm: &mut Pixmap, img: &Pixmap, quality: FilterQuality, i: &Inst) {
    let (iw, ih): (f32, f32) = (img.width() as f32, img.height() as f32);
    let (sw, sh): (u32, u32) = ((i.uv[2] * iw).round() as u32, (i.uv[3] * ih).round() as u32);
    let Some(src) = IntRect::from_xywh(
        (i.uv[0] * iw).round() as i32,
        (i.uv[1] * ih).round() as i32,
        sw,
        sh,
    ) else {
        return;
    };
    let [x, y, w, h]: [f32; 4] = i.a;
    let Some(rect) = Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let whole: bool = sw == img.width() && sh == img.height();
    let white: bool = i.color[..3].iter().all(|c| *c >= 1.0);
    let mut opacity: f32 = i.color[3];
    let crop: Option<Pixmap> = if whole && white {
        None
    } else {
        let Some(mut c) = img.clone_rect(src) else {
            return;
        };
        if !white {
            // Straight tint on premultiplied pixels: colour by tint colour and alpha, alpha by alpha.
            let m: [f32; 4] = [
                i.color[0] * i.color[3],
                i.color[1] * i.color[3],
                i.color[2] * i.color[3],
                i.color[3],
            ];
            for px in c.data_mut().as_chunks_mut::<4>().0 {
                for (v, k) in px.iter_mut().zip(m) {
                    *v = (f32::from(*v) * k).round().min(255.0) as u8;
                }
            }
            opacity = 1.0;
        }
        Some(c)
    };
    let source: &Pixmap = crop.as_ref().unwrap_or(img);
    let (cx, cy): (f32, f32) = (x + w / 2.0, y + h / 2.0);
    let rotation: f32 = i.p[0].to_degrees();
    let shader = Pattern::new(
        source.as_ref(),
        SpreadMode::Pad,
        quality,
        opacity,
        Transform::from_translate(cx, cy)
            .pre_rotate(rotation)
            .pre_scale(w / source.width() as f32, h / source.height() as f32)
            .pre_translate(
                -(source.width() as f32) / 2.0,
                -(source.height() as f32) / 2.0,
            ),
    );
    let paint: Paint = Paint {
        shader,
        anti_alias: false,
        ..Paint::default()
    };
    if i.p[0] == 0.0 {
        pm.fill_rect(rect, &paint, Transform::identity(), None);
    } else if let Some(quad) =
        PathBuilder::from_rect(rect).transform(Transform::from_rotate_at(rotation, cx, cy))
    {
        pm.fill_path(
            &quad,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
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

impl Out {
    fn new(target: &Target) -> Result<Self> {
        Ok(Self {
            pm: new_pixmap(target.width, target.height)?,
            #[cfg(target_arch = "wasm32")]
            ctx: crate::canvas::Ctx2d::new(&target.kind)?,
        })
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.pm = new_pixmap(width, height)?;
        #[cfg(target_arch = "wasm32")]
        if let Some(c) = &self.ctx {
            c.set_size(width, height);
        }
        Ok(())
    }

    /// `putImageData`s the frame into the canvas (nothing for an image target). The canvas wants
    /// straight alpha and the pixmap is premultiplied: a transparent output is unpremultiplied; an
    /// opaque one keeps its colour and drops its alpha, as an opaque GPU surface does.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "fallible and canvas-bound on wasm32"
        )
    )]
    fn show(&self, transparent: bool) -> Result<()> {
        #[cfg(target_arch = "wasm32")]
        {
            use wasm_bindgen::Clamped;
            let Some(ctx) = &self.ctx else {
                return Ok(());
            };
            let px: Vec<u8> = if transparent {
                crate::renderer::unpremultiply_rgba8(self.pm.data())
            } else {
                let mut o: Vec<u8> = self.pm.data().to_vec();
                for p in o.as_chunks_mut::<4>().0 {
                    p[3] = 255;
                }
                o
            };
            let img: web_sys::ImageData = web_sys::ImageData::new_with_u8_clamped_array_and_sh(
                Clamped(&px),
                self.pm.width(),
                self.pm.height(),
            )
            .map_err(|e| Error::Surface(format!("{e:?}")))?;
            ctx.put_image_data(&img)?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = transparent;
        Ok(())
    }
}

impl Cpu {
    pub fn new(target: &Target, transparent: bool) -> Result<Self> {
        Ok(Self {
            primary: if matches!(target.kind, Kind::None) {
                None
            } else {
                Some(Out::new(target)?)
            },
            slots: Vec::new(),
            images: Vec::new(),
            transparent,
        })
    }

    fn no_primary() -> Error {
        Error::BadInput(
            "this renderer has no primary target: attach a canvas and use present_to".to_owned(),
        )
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.primary
            .as_mut()
            .ok_or_else(Self::no_primary)?
            .resize(width, height)
    }

    /// Gives a canvas its own 2D context and pixels; returns its slot.
    pub fn attach(&mut self, target: &Target) -> Result<usize> {
        self.slots.push(Some(Out::new(target)?));
        Ok(self.slots.len() - 1)
    }

    pub fn resize_slot(&mut self, slot: usize, width: u32, height: u32) -> Result<()> {
        self.slots
            .get_mut(slot)
            .and_then(Option::as_mut)
            .ok_or(Error::UnknownSurface)?
            .resize(width, height)
    }

    pub fn detach(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            *s = None;
        }
    }

    /// Draws the frame, then `putImageData`s it into the canvas (a no-op for an image target).
    pub fn present(
        &mut self,
        flat: &Flat,
        atlas: &[u8],
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<()> {
        let out: &mut Out = self.primary.as_mut().ok_or_else(Self::no_primary)?;
        draw(&self.images, &mut out.pm, flat, atlas, clear, batches);
        out.show(self.transparent)
    }

    /// As [`Cpu::present`], for an attached canvas.
    pub fn present_to(
        &mut self,
        slot: usize,
        flat: &Flat,
        atlas: &[u8],
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<()> {
        let out: &mut Out = self
            .slots
            .get_mut(slot)
            .and_then(Option::as_mut)
            .ok_or(Error::UnknownSurface)?;
        draw(&self.images, &mut out.pm, flat, atlas, clear, batches);
        out.show(self.transparent)
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

    /// The frame as premultiplied RGBA8.
    pub fn render(
        &mut self,
        flat: &Flat,
        atlas: &[u8],
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<Vec<u8>> {
        let out: &mut Out = self.primary.as_mut().ok_or_else(Self::no_primary)?;
        draw(&self.images, &mut out.pm, flat, atlas, clear, batches);
        Ok(out.pm.data().to_vec())
    }
}

type Images = [Option<(Pixmap, FilterQuality)>];

fn draw(
    images: &Images,
    pm: &mut Pixmap,
    flat: &Flat,
    atlas: &[u8],
    clear: Rgba,
    batches: &[BatchState],
) {
    let (w, h): (i32, i32) = (pm.width() as i32, pm.height() as i32);
    pm.fill(Color::from_rgba8(clear[0], clear[1], clear[2], clear[3]));
    let mut paint: Paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    for run in &flat.runs {
        match run {
            Run::Batch(id) => {
                if let Some(b) = batches.get(id.0) {
                    b.cpu.draw(&b.data, b.instances, b.params, pm);
                }
            }
            Run::Image { id, insts } => {
                if let Some(Some((img, quality))) = images.get(id.0) {
                    for i in &flat.insts[insts.start as usize..insts.end as usize] {
                        draw_sprite(pm, img, *quality, i);
                    }
                }
            }
            Run::Paths(range) => {
                fill_paths(pm, &flat.paths[range.start as usize..range.end as usize]);
            }
            Run::Insts(range) => {
                for i in &flat.insts[range.start as usize..range.end as usize] {
                    let c: [f32; 4] = i.color;
                    match i.p[1] as u32 {
                        2 => {
                            let (gx, gy, gw, gh): (i32, i32, i32, i32) =
                                (i.a[0] as i32, i.a[1] as i32, i.a[2] as i32, i.a[3] as i32);
                            let (u, v): (i32, i32) = (i.uv[0] as i32, i.uv[1] as i32);
                            let data: &mut [u8] = pm.data_mut();
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
                                    // Premultiplied over: alpha accumulates too (a no-op over opaque).
                                    let da: f32 = f32::from(data[o + 3]);
                                    data[o + 3] = (255.0 * a + da * (1.0 - a)).round() as u8;
                                }
                            }
                        }
                        kind => {
                            paint.set_color_rgba8(to8(c[0]), to8(c[1]), to8(c[2]), to8(c[3]));
                            if kind == 0 {
                                if let Some(p) =
                                    rounded_rect(i.a[0], i.a[1], i.a[2], i.a[3], i.p[0])
                                {
                                    pm.fill_path(
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
                                    pm.stroke_path(
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
