//! Browser canvases: the 2D contexts the CPU backend and the WebGL2 sheet copy draw into, and
//! canvas sizing. wasm32 only.

use crate::error::{Error, Result};
use crate::renderer::Kind;
use wasm_bindgen::JsCast as _;

/// A canvas's 2D context.
pub enum Ctx2d {
    Html(web_sys::CanvasRenderingContext2d),
    Offscreen(web_sys::OffscreenCanvasRenderingContext2d),
}

impl Ctx2d {
    /// The 2D context of a canvas target; `None` for an image or device-only target.
    pub fn new(kind: &Kind) -> Result<Option<Self>> {
        let none = || {
            Error::Surface(
                "the canvas has no 2D context (a GPU context already took it)".to_owned(),
            )
        };
        Ok(match kind {
            Kind::Image | Kind::None => None,
            Kind::Html(c) => Some(Self::Html(
                c.get_context("2d")
                    .ok()
                    .flatten()
                    .and_then(|o| o.dyn_into().ok())
                    .ok_or_else(none)?,
            )),
            Kind::Offscreen(c) => Some(Self::Offscreen(
                c.get_context("2d")
                    .ok()
                    .flatten()
                    .and_then(|o| o.dyn_into().ok())
                    .ok_or_else(none)?,
            )),
        })
    }

    pub fn put_image_data(&self, img: &web_sys::ImageData) -> Result<()> {
        match self {
            Self::Html(c) => c.put_image_data(img, 0.0, 0.0),
            Self::Offscreen(c) => c.put_image_data(img, 0.0, 0.0),
        }
        .map_err(|e| Error::Surface(format!("{e:?}")))
    }

    /// Replaces the canvas content with the top-left `w` x `h` of `sheet`. Transparent keeps the
    /// sheet's alpha; opaque forces alpha to 255 (the sheet over black), as the other backends do.
    pub fn copy_from(
        &self,
        sheet: &web_sys::OffscreenCanvas,
        w: u32,
        h: u32,
        opaque: bool,
    ) -> Result<()> {
        let (w, h): (f64, f64) = (f64::from(w), f64::from(h));
        let op: &str = if opaque { "source-over" } else { "copy" };
        match self {
            Self::Html(c) => {
                if opaque {
                    c.set_global_composite_operation("copy")
                        .map_err(|e| Error::Surface(format!("{e:?}")))?;
                    c.set_fill_style_str("#000");
                    c.fill_rect(0.0, 0.0, w, h);
                }
                c.set_global_composite_operation(op)
                    .map_err(|e| Error::Surface(format!("{e:?}")))?;
                c.draw_image_with_offscreen_canvas_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    sheet, 0.0, 0.0, w, h, 0.0, 0.0, w, h,
                )
            }
            Self::Offscreen(c) => {
                if opaque {
                    c.set_global_composite_operation("copy")
                        .map_err(|e| Error::Surface(format!("{e:?}")))?;
                    c.set_fill_style_str("#000");
                    c.fill_rect(0.0, 0.0, w, h);
                }
                c.set_global_composite_operation(op)
                    .map_err(|e| Error::Surface(format!("{e:?}")))?;
                c.draw_image_with_offscreen_canvas_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    sheet, 0.0, 0.0, w, h, 0.0, 0.0, w, h,
                )
            }
        }
        .map_err(|e| Error::Surface(format!("{e:?}")))
    }

    pub fn set_size(&self, width: u32, height: u32) {
        match self {
            Self::Html(c) => {
                if let Some(cv) = c.canvas() {
                    cv.set_width(width);
                    cv.set_height(height);
                }
            }
            Self::Offscreen(c) => {
                let cv = c.canvas();
                cv.set_width(width);
                cv.set_height(height);
            }
        }
    }
}

/// Sets the pixel size of a canvas target.
pub fn set_kind_size(kind: &Kind, width: u32, height: u32) {
    match kind {
        Kind::Html(c) => {
            c.set_width(width);
            c.set_height(height);
        }
        Kind::Offscreen(c) => {
            c.set_width(width);
            c.set_height(height);
        }
        Kind::Image | Kind::None => {}
    }
}
