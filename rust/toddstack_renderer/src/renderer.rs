//! `Renderer`: backend selection, scenes, custom batches and kernels behind one type.

use crate::backend::{self, Backend, Capabilities};
use crate::batch::{BatchDesc, BatchState};
use crate::cpu::Cpu;
use crate::error::{Error, Result};
use crate::glyphs::Atlas;
use crate::gpu::Gpu;
use crate::kernel::{Kernel, KernelOutput, KernelPath};
use crate::scene::{BatchId, FontId, ImageId, Sampling, Scene};

/// Where a target draws.
#[derive(Clone, Debug)]
pub(crate) enum Kind {
    /// An RGBA8 image; [`Renderer::render`] reads it back.
    Image,
    #[cfg(target_arch = "wasm32")]
    Html(web_sys::HtmlCanvasElement),
    #[cfg(target_arch = "wasm32")]
    Offscreen(web_sys::OffscreenCanvas),
}

/// What to render into: an offscreen RGBA8 image (readback), or on wasm32 a canvas that
/// [`Renderer::present`] draws to directly.
#[derive(Clone, Debug)]
pub struct Target {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    pub(crate) kind: Kind,
}

impl Target {
    /// An offscreen target: the only one whose pixels [`Renderer::render`] can read back.
    #[must_use]
    pub const fn offscreen(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            kind: Kind::Image,
        }
    }

    /// A canvas element, sized to its current `width` and `height` attributes. The renderer takes
    /// the canvas's context (WebGPU, WebGL2 or 2D), so give it a canvas nothing else draws on.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn canvas(canvas: web_sys::HtmlCanvasElement) -> Self {
        Self {
            width: canvas.width(),
            height: canvas.height(),
            kind: Kind::Html(canvas),
        }
    }

    /// An `OffscreenCanvas` (for a worker), sized as [`Target::canvas`].
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn offscreen_canvas(canvas: web_sys::OffscreenCanvas) -> Self {
        Self {
            width: canvas.width(),
            height: canvas.height(),
            kind: Kind::Offscreen(canvas),
        }
    }

    pub(crate) const fn is_canvas(&self) -> bool {
        !matches!(self.kind, Kind::Image)
    }
}

/// `BadInput` unless both sides are 1 to `max`.
pub(crate) fn check_size(width: u32, height: u32, max: u32) -> Result<()> {
    if width == 0 || height == 0 || width > max || height > max {
        return Err(Error::BadInput(format!(
            "target {width}x{height} is empty or larger than {max}"
        )));
    }
    Ok(())
}

/// Overrides for backend selection.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct Options {
    /// Use only this backend (fails with `NoBackend` if it is missing, except `Cpu`).
    /// `None` walks [`Backend::chain`].
    pub force: Option<Backend>,
    /// Force batches onto data textures (`Some(true)`) or storage buffers (`Some(false)`);
    /// `None` picks by capability. Mostly for testing the WebGL2 path on a desktop GPU.
    pub data_texture: Option<bool>,
}

impl Options {
    /// No overrides: the platform's fallback chain, capability-picked batch data.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            force: None,
            data_texture: None,
        }
    }

    /// Use only this backend (or the chain again with `None`).
    #[must_use]
    pub fn with_force(mut self, force: impl Into<Option<Backend>>) -> Self {
        self.force = force.into();
        self
    }

    /// Force batches onto data textures (`true`) or storage buffers (`false`), or `None` to pick.
    #[must_use]
    pub fn with_data_texture(mut self, data_texture: impl Into<Option<bool>>) -> Self {
        self.data_texture = data_texture.into();
        self
    }

    /// `WGPU_BACKEND=vulkan|dx12|metal|gl|cpu` (native only; unset or unknown means no override).
    #[must_use]
    pub fn from_env() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let force: Option<Backend> = std::env::var("WGPU_BACKEND")
                .ok()
                .and_then(|s| Backend::parse(&s));
            Self::new().with_force(force)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self::new()
        }
    }
}

enum Inner {
    Gpu(Box<Gpu>),
    Cpu(Cpu),
}

/// One renderer: draws [`Scene`]s, custom batches and [`Kernel`]s on the best backend available.
pub struct Renderer {
    inner: Inner,
    caps: Capabilities,
    atlas: Atlas,
    batches: Vec<BatchState>,
    /// Size of each live image, by id.
    images: Vec<Option<(u32, u32)>>,
    width: u32,
    height: u32,
    /// A canvas target: `present` draws it, `render` (readback) is refused.
    canvas: bool,
}

impl Renderer {
    /// Picks the most capable backend and falls back down the chain, ending at the CPU.
    ///
    /// # Errors
    /// `BadInput` for an empty or oversized target.
    pub async fn new(target: Target) -> Result<Self> {
        Self::with_options(target, Options::default()).await
    }

    /// As [`Renderer::new`], with overrides.
    ///
    /// # Errors
    /// `NoBackend` if a forced GPU backend is unavailable; `BadInput` for a bad target.
    pub async fn with_options(target: Target, options: Options) -> Result<Self> {
        let (width, height): (u32, u32) = (target.width, target.height);
        let canvas: bool = target.is_canvas();
        // The upper bound is the chosen backend's texture limit, checked once it is known.
        check_size(width, height, u32::MAX)?;
        let chain: Vec<Backend> = options
            .force
            .map_or_else(|| Backend::chain().to_vec(), |b| vec![b]);
        // Choose before touching a canvas: each GPU candidate is probed off the real canvas.
        let picked = backend::select(&chain, |b| Gpu::probe(b, &target)).await?;
        let inner: Inner = match picked {
            Some((Backend::Cpu, _)) => Inner::Cpu(Cpu::new(&target)?),
            Some((_, Some(probe))) => Inner::Gpu(Box::new(
                Gpu::create(probe, &target, options.data_texture).await?,
            )),
            _ => {
                return Err(Error::NoBackend {
                    tried: chain
                        .iter()
                        .map(|b| b.name())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        };
        let caps: Capabilities = match &inner {
            Inner::Gpu(g) => g.caps.clone(),
            Inner::Cpu(_) => Capabilities::cpu(),
        };
        Ok(Self {
            inner,
            caps,
            atlas: Atlas::new(),
            batches: Vec::new(),
            images: Vec::new(),
            width,
            height,
            canvas,
        })
    }

    /// The backend in use.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.caps.backend
    }

    /// What the backend can do.
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Target width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Target height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Resizes the target; batch data is kept.
    ///
    /// # Errors
    /// `BadInput` for an empty or oversized target.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        check_size(width, height, self.caps.max_texture_dimension_2d)?;
        match &mut self.inner {
            Inner::Gpu(g) => g.resize(width, height)?,
            Inner::Cpu(c) => c.resize(width, height)?,
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Adds a font (TTF or OTF bytes) for text.
    ///
    /// # Errors
    /// `Font` if the bytes do not parse.
    pub fn add_font(&mut self, bytes: &[u8]) -> Result<FontId> {
        self.atlas.add_font(bytes)
    }

    /// Adds a custom batch; draw it with `Scene::batch`.
    ///
    /// # Errors
    /// `Shader` if the GPU rejects the WGSL or the pipeline (async: the GPU reports it later).
    pub async fn add_batch(&mut self, desc: BatchDesc) -> Result<BatchId> {
        let state: BatchState = desc.into();
        if let Inner::Gpu(g) = &mut self.inner {
            g.add_batch(&state).await?;
        }
        self.batches.push(state);
        Ok(BatchId(self.batches.len() - 1))
    }

    /// Overwrites `words.len()` data words from `offset`; only that range is uploaded.
    ///
    /// # Errors
    /// `UnknownBatch`, or `BadInput` if the range is past the batch's data.
    pub fn update_batch(&mut self, id: BatchId, offset: usize, words: &[u32]) -> Result<()> {
        let b: &mut BatchState = self.batches.get_mut(id.0).ok_or(Error::UnknownBatch)?;
        let end: usize = offset.checked_add(words.len()).ok_or_else(|| {
            Error::BadInput(format!("batch `{}`: offset {offset} overflows", b.name))
        })?;
        if end > b.data.len() {
            return Err(Error::BadInput(format!(
                "batch `{}` has {} words, update ends at {end}",
                b.name,
                b.data.len()
            )));
        }
        b.data[offset..end].copy_from_slice(words);
        if let Inner::Gpu(g) = &self.inner {
            g.write_batch_data(id.0, &self.batches[id.0].data, offset, end);
        }
        Ok(())
    }

    /// Sets the batch's four parameters (`globals.params` in WGSL).
    ///
    /// # Errors
    /// `UnknownBatch`.
    pub fn set_batch_params(&mut self, id: BatchId, params: [f32; 4]) -> Result<()> {
        self.batches
            .get_mut(id.0)
            .ok_or(Error::UnknownBatch)?
            .params = params;
        if let Inner::Gpu(g) = &self.inner {
            g.write_batch_params(id.0, params);
        }
        Ok(())
    }

    /// Sets how many instances the batch draws.
    ///
    /// # Errors
    /// `UnknownBatch`; `BadInput` if `instances` is more than the batch has data words.
    pub fn set_batch_instances(&mut self, id: BatchId, instances: u32) -> Result<()> {
        let b: &mut BatchState = self.batches.get_mut(id.0).ok_or(Error::UnknownBatch)?;
        // Every instance reads at least one data word, so more instances than words is a mistake.
        if instances as usize > b.data.len() {
            return Err(Error::BadInput(format!(
                "batch `{}` has {} words, so it cannot draw {instances} instances",
                b.name,
                b.data.len()
            )));
        }
        b.instances = instances;
        Ok(())
    }

    /// Draws the scene and returns the image as RGBA8 (`width * height * 4` bytes): a readback,
    /// for tests and for callers that want pixels. Offscreen targets only; use
    /// [`Renderer::present`] for a canvas.
    ///
    /// # Errors
    /// `BadInput` on a canvas target; `AtlasFull`, `UnknownFont`, or a readback failure.
    pub async fn render(&mut self, scene: &Scene) -> Result<Vec<u8>> {
        if self.canvas {
            return Err(Error::BadInput(
                "a canvas target has no readback: use present, or an offscreen target".to_owned(),
            ));
        }
        let flat = self.atlas.flatten(scene)?;
        match &mut self.inner {
            Inner::Cpu(c) => Ok(c.render(&flat, &self.atlas.pixels, scene.clear, &self.batches)),
            Inner::Gpu(g) => {
                let upload: Option<&[u8]> = if self.atlas.dirty {
                    Some(&self.atlas.pixels)
                } else {
                    None
                };
                let px: Vec<u8> = g.render(&flat, upload, scene.clear, &self.batches).await?;
                self.atlas.dirty = false;
                Ok(px)
            }
        }
    }

    /// Draws the scene to the target with no readback: a canvas target shows the frame (GPU
    /// backends present the surface; the CPU backend `putImageData`s into the canvas's 2D
    /// context); an offscreen target just runs the frame. Synchronous.
    ///
    /// # Errors
    /// `AtlasFull`, `UnknownFont`, or `Surface` if the canvas surface cannot be recovered.
    pub fn present(&mut self, scene: &Scene) -> Result<()> {
        let flat = self.atlas.flatten(scene)?;
        match &mut self.inner {
            Inner::Cpu(c) => c.present(&flat, &self.atlas.pixels, scene.clear, &self.batches),
            Inner::Gpu(g) => {
                let upload: Option<&[u8]> = if self.atlas.dirty {
                    Some(&self.atlas.pixels)
                } else {
                    None
                };
                g.present(&flat, upload, scene.clear, &self.batches)?;
                self.atlas.dirty = false;
                Ok(())
            }
        }
    }

    /// Runs a kernel on the best form this backend supports: compute, then fragment, then CPU.
    ///
    /// # Errors
    /// `KernelUnavailable` if no form can run here.
    pub async fn run_kernel(&self, kernel: &Kernel) -> Result<KernelOutput> {
        let path: KernelPath = self.best_path(kernel)?;
        self.run_kernel_with(kernel, path).await
    }

    fn best_path(&self, kernel: &Kernel) -> Result<KernelPath> {
        match &self.inner {
            Inner::Gpu(g) if g.caps.compute && kernel.compute.is_some() => Ok(KernelPath::Compute),
            Inner::Gpu(_) if kernel.fragment.is_some() => Ok(KernelPath::Fragment),
            _ if kernel.cpu.is_some() => Ok(KernelPath::Cpu),
            _ => Err(Error::KernelUnavailable {
                kernel: kernel.name.clone(),
            }),
        }
    }

    /// Adds an image from straight RGBA8 (`width * height * 4` bytes); draw it with `Scene::image`.
    ///
    /// # Errors
    /// `BadInput` for an empty image or the wrong byte length.
    pub fn add_image(
        &mut self,
        width: u32,
        height: u32,
        rgba8: &[u8],
        sampling: Sampling,
    ) -> Result<ImageId> {
        if width == 0 || height == 0 || rgba8.len() != (width as usize) * (height as usize) * 4 {
            return Err(Error::BadInput(format!(
                "image {width}x{height} needs {} bytes, got {}",
                (width as usize) * (height as usize) * 4,
                rgba8.len()
            )));
        }
        let id: ImageId = self.next_image(width, height);
        let premul: Vec<u8> = premultiply(rgba8);
        match &mut self.inner {
            Inner::Gpu(g) => g.set_image(id, width, height, &premul, sampling),
            Inner::Cpu(c) => c.set_image(id, width, height, premul, sampling),
        }
        Ok(id)
    }

    /// Runs a kernel and keeps its output as an image for `Scene::image`. On a GPU form the output
    /// stays on the GPU (no readback); the CPU form uploads its bytes. Returns the form that ran.
    ///
    /// # Errors
    /// `KernelUnavailable` if no form can run here.
    pub async fn kernel_image(
        &mut self,
        kernel: &Kernel,
        sampling: Sampling,
    ) -> Result<(ImageId, KernelPath)> {
        let path: KernelPath = self.best_path(kernel)?;
        self.kernel_image_with(kernel, path, sampling).await
    }

    /// As [`Renderer::kernel_image`], on one named form.
    ///
    /// # Errors
    /// `KernelUnavailable` if the kernel lacks that form, or the backend cannot run it.
    pub async fn kernel_image_with(
        &mut self,
        kernel: &Kernel,
        path: KernelPath,
        sampling: Sampling,
    ) -> Result<(ImageId, KernelPath)> {
        let unavailable = || Error::KernelUnavailable {
            kernel: kernel.name.clone(),
        };
        if let (Inner::Gpu(g), true) = (&mut self.inner, path != KernelPath::Cpu) {
            if path == KernelPath::Compute && !g.caps.compute {
                return Err(unavailable());
            }
            let id: ImageId = ImageId(self.images.len());
            g.kernel_image(kernel, path, id, sampling).await?;
            self.images.push(Some((kernel.width, kernel.height)));
            return Ok((id, path));
        }
        if path != KernelPath::Cpu {
            return Err(unavailable());
        }
        let f = kernel.cpu.as_ref().ok_or_else(unavailable)?;
        let rgba8: Vec<u8> = f(&kernel.inputs, kernel.width, kernel.height);
        let id: ImageId = self.add_image(kernel.width, kernel.height, &rgba8, sampling)?;
        Ok((id, KernelPath::Cpu))
    }

    /// Frees an image; scenes that still draw it draw nothing for it.
    ///
    /// # Errors
    /// `UnknownImage`.
    pub fn remove_image(&mut self, id: ImageId) -> Result<()> {
        self.images
            .get_mut(id.0)
            .and_then(Option::take)
            .ok_or(Error::UnknownImage)?;
        match &mut self.inner {
            Inner::Gpu(g) => g.remove_image(id),
            Inner::Cpu(c) => c.remove_image(id),
        }
        Ok(())
    }

    /// The size of an image, if it is live.
    #[must_use]
    pub fn image_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.images.get(id.0).copied().flatten()
    }

    fn next_image(&mut self, width: u32, height: u32) -> ImageId {
        self.images.push(Some((width, height)));
        ImageId(self.images.len() - 1)
    }

    /// Runs one named form, for comparing forms against each other.
    ///
    /// # Errors
    /// `KernelUnavailable` if the kernel lacks that form, or the backend cannot run it.
    pub async fn run_kernel_with(&self, kernel: &Kernel, path: KernelPath) -> Result<KernelOutput> {
        let rgba8: Vec<u8> = match (path, &self.inner) {
            (KernelPath::Cpu, _) => {
                let f = kernel
                    .cpu
                    .as_ref()
                    .ok_or_else(|| Error::KernelUnavailable {
                        kernel: kernel.name.clone(),
                    })?;
                f(&kernel.inputs, kernel.width, kernel.height)
            }
            (KernelPath::Compute, Inner::Gpu(g)) if !g.caps.compute => {
                return Err(Error::KernelUnavailable {
                    kernel: kernel.name.clone(),
                });
            }
            (p, Inner::Gpu(g)) => g.run_kernel(kernel, p).await?,
            (_, Inner::Cpu(_)) => {
                return Err(Error::KernelUnavailable {
                    kernel: kernel.name.clone(),
                });
            }
        };
        Ok(KernelOutput {
            path,
            width: kernel.width,
            height: kernel.height,
            rgba8,
        })
    }
}

/// Straight RGBA8 to premultiplied, rounded.
#[expect(clippy::cast_possible_truncation, reason = "the result is at most 255")]
fn premultiply(rgba8: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = rgba8.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        let a: u32 = u32::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u32::from(*c) * a + 127) / 255) as u8;
        }
    }
    out
}
