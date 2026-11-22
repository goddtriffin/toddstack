//! wasm-bindgen surface for the TypeScript wrapper in `web/renderer.ts`: build a scene with calls,
//! present it to a canvas (or read RGBA bytes back from an offscreen target).

use crate::{Backend, BatchId, Error, FontId, Options, Renderer, Scene, Target, demo};
use std::collections::HashMap;
use wasm_bindgen::prelude::{JsError, wasm_bindgen};

fn js(e: &Error) -> JsError {
    JsError::new(&e.to_string())
}

/// A renderer plus the scene being built.
#[wasm_bindgen]
pub struct WebRenderer {
    inner: Renderer,
    scene: Scene,
    font: FontId,
    grid: Option<BatchId>,
    /// Demos that upload images or kernel output, built once.
    built: HashMap<String, Scene>,
}

#[wasm_bindgen]
impl WebRenderer {
    /// Draws straight to `canvas` (its current `width` and `height`; the renderer takes its
    /// context). `backend` is an optional override: webgpu, webgl2 or cpu.
    pub async fn create(
        canvas: web_sys::HtmlCanvasElement,
        font: Vec<u8>,
        backend: Option<String>,
    ) -> Result<WebRenderer, JsError> {
        Self::make(Target::canvas(canvas), font, backend).await
    }

    /// As `create`, for an `OffscreenCanvas` (a worker).
    #[wasm_bindgen(js_name = createOffscreen)]
    pub async fn create_offscreen(
        canvas: web_sys::OffscreenCanvas,
        font: Vec<u8>,
        backend: Option<String>,
    ) -> Result<WebRenderer, JsError> {
        Self::make(Target::offscreen_canvas(canvas), font, backend).await
    }

    /// An offscreen target of this size, for `render()` (readback) instead of `present()`.
    #[wasm_bindgen(js_name = createReadback)]
    pub async fn create_readback(
        width: u32,
        height: u32,
        font: Vec<u8>,
        backend: Option<String>,
    ) -> Result<WebRenderer, JsError> {
        Self::make(Target::offscreen(width, height), font, backend).await
    }

    async fn make(
        target: Target,
        font: Vec<u8>,
        backend: Option<String>,
    ) -> Result<WebRenderer, JsError> {
        let options: Options = Options {
            force: backend.as_deref().and_then(Backend::parse),
            data_texture: None,
        };
        let mut inner: Renderer = Renderer::with_options(target, options)
            .await
            .map_err(|e| js(&e))?;
        let font: FontId = inner.add_font(&font).map_err(|e| js(&e))?;
        Ok(Self {
            inner,
            scene: Scene::new([255, 255, 255, 255]),
            font,
            grid: None,
            built: HashMap::new(),
        })
    }

    pub fn backend(&self) -> String {
        self.inner.backend().name().to_owned()
    }

    pub fn adapter(&self) -> String {
        self.inner.capabilities().adapter.clone()
    }

    pub fn compute(&self) -> bool {
        self.inner.capabilities().compute
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), JsError> {
        self.inner.resize(width, height).map_err(|e| js(&e))
    }

    /// Starts a new empty scene.
    pub fn begin(&mut self, r: u8, g: u8, b: u8, a: u8) {
        self.scene = Scene::new([r, g, b, a]);
    }

    #[expect(
        clippy::too_many_arguments,
        clippy::many_single_char_names,
        reason = "mirrors Scene::rrect for the JS call"
    )]
    pub fn rrect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        cr: u8,
        cg: u8,
        cb: u8,
        ca: u8,
    ) {
        self.scene.rrect(x, y, w, h, r, [cr, cg, cb, ca]);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors Scene::line for the JS call"
    )]
    pub fn line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        cr: u8,
        cg: u8,
        cb: u8,
        ca: u8,
    ) {
        self.scene.line(x0, y0, x1, y1, width, [cr, cg, cb, ca]);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors Scene::text for the JS call"
    )]
    pub fn text(&mut self, text: &str, x: f32, y: f32, size: f32, cr: u8, cg: u8, cb: u8, ca: u8) {
        self.scene
            .text(self.font, text, x, y, size, [cr, cg, cb, ca]);
    }

    /// Replaces the scene with a built-in demo: `board`, `chart`, `grid` (a 1000x1000 batch), `images`, `paths` or `texture` (a kernel's output as an image).
    pub fn demo(&mut self, name: &str) -> Result<(), JsError> {
        self.scene = match name {
            "board" => demo::board_scene(self.font),
            "chart" => demo::chart_scene(self.font),
            "grid" => {
                let id: BatchId = if let Some(id) = self.grid {
                    id
                } else {
                    {
                        let id: BatchId = self
                            .inner
                            .add_batch(demo::grid_batch(1000, 1000))
                            .map_err(|e| js(&e))?;
                        self.inner
                            .set_batch_params(id, [1000.0, 1.0, 0.0, 0.0])
                            .map_err(|e| js(&e))?;
                        self.grid = Some(id);
                        id
                    }
                };
                let mut s: Scene = Scene::new([255, 255, 255, 255]);
                s.batch(id);
                s
            }
            "paths" => demo::paths_scene(self.font),
            "images" | "texture" => {
                if let Some(s) = self.built.get(name) {
                    s.clone()
                } else {
                    let s: Scene = if name == "images" {
                        demo::images_scene(&mut self.inner, self.font)
                    } else {
                        demo::texture_scene(&mut self.inner, self.font)
                    }
                    .map_err(|e| js(&e))?;
                    self.built.insert(name.to_owned(), s.clone());
                    s
                }
            }
            other => {
                return Err(JsError::new(&format!(
                    "unknown demo `{other}`: board, chart, grid, images, paths or texture"
                )));
            }
        };
        Ok(())
    }

    /// Draws the scene to the canvas: no readback, nothing to await.
    pub fn present(&mut self) -> Result<(), JsError> {
        self.inner.present(&self.scene).map_err(|e| js(&e))
    }

    /// Readback targets only: draws the scene and returns `width * height * 4` RGBA bytes.
    pub async fn render(&mut self) -> Result<Vec<u8>, JsError> {
        self.inner.render(&self.scene).await.map_err(|e| js(&e))
    }
}
