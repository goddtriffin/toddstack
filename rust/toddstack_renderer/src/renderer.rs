//! `Renderer`: backend selection, scenes, custom batches and kernels behind one type.

use crate::backend::{Backend, Capabilities};
use crate::batch::{BatchDesc, BatchState};
use crate::cpu::Cpu;
use crate::error::{Error, Result};
use crate::fallback::{self, Callback, FallbackEvent, FallbackReason, Step};
use crate::glyphs::Atlas;
use crate::gpu::Gpu;
use crate::kernel::{Kernel, KernelOutput, KernelPath};
use crate::scene::{BatchId, FontId, ImageId, Sampling, Scene};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where a target draws.
#[derive(Clone, Debug)]
pub(crate) enum Kind {
    /// An RGBA8 image; [`Renderer::render`] reads it back.
    Image,
    /// No target of its own: canvases are attached later.
    None,
    #[cfg(target_arch = "wasm32")]
    Html(web_sys::HtmlCanvasElement),
    #[cfg(target_arch = "wasm32")]
    Offscreen(web_sys::OffscreenCanvas),
}

impl Kind {
    /// Whether both are the same browser canvas.
    fn is_same_canvas(&self, other: &Self) -> bool {
        match (self, other) {
            #[cfg(target_arch = "wasm32")]
            (Self::Html(a), Self::Html(b)) => a == b,
            #[cfg(target_arch = "wasm32")]
            (Self::Offscreen(a), Self::Offscreen(b)) => a == b,
            _ => false,
        }
    }
}

/// What to render into: an offscreen RGBA8 image (readback), nothing yet (a device-only renderer
/// for [`Renderer::attach`]), or on wasm32 a canvas that [`Renderer::present`] draws to directly.
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

    /// No target: a renderer that owns the device and fonts, images and batches, and draws only to
    /// canvases added with [`Renderer::attach`] (see [`Renderer::present_to`]). Its width and
    /// height are 0; [`Renderer::present`], [`Renderer::render`] and [`Renderer::resize`] refuse.
    #[must_use]
    pub const fn device_only() -> Self {
        Self {
            width: 0,
            height: 0,
            kind: Kind::None,
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
        !matches!(self.kind, Kind::Image | Kind::None)
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

/// How long a WebGPU adapter request may take before it counts as failed.
pub const DEFAULT_ADAPTER_TIMEOUT: Duration = Duration::from_secs(2);

/// Overrides for backend selection.
#[derive(Clone)]
#[non_exhaustive]
pub struct Options {
    /// Use only this backend (fails with `NoRendererAvailable` if it does not work, except
    /// `Cpu`). `None` walks [`Backend::chain`].
    pub force: Option<Backend>,
    /// Try this backend first (see [`Options::with_preferred`]); ignored if `force` is set or the
    /// platform's chain does not have it.
    pub preferred: Option<Backend>,
    /// How long the WebGPU adapter request may take (web only); default 2 s.
    pub adapter_timeout: Duration,
    /// Called for every step down the chain and for a lost device.
    pub(crate) on_fallback: Option<Callback>,
    /// Force batches onto data textures (`Some(true)`) or storage buffers (`Some(false)`);
    /// `None` picks by capability. Mostly for testing the WebGL2 path on a desktop GPU.
    pub data_texture: Option<bool>,
    /// The output keeps its alpha: a canvas lets the page show through where the scene is
    /// transparent, and readback is premultiplied. `false` (the default) is an opaque output.
    pub transparent: bool,
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("force", &self.force)
            .field("preferred", &self.preferred)
            .field("adapter_timeout", &self.adapter_timeout)
            .field(
                "on_fallback",
                &self.on_fallback.as_ref().map(|_| "<callback>"),
            )
            .field("data_texture", &self.data_texture)
            .field("transparent", &self.transparent)
            .finish()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

impl Options {
    /// No overrides: the platform's fallback chain, capability-picked batch data.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            force: None,
            preferred: None,
            adapter_timeout: DEFAULT_ADAPTER_TIMEOUT,
            on_fallback: None,
            data_texture: None,
            transparent: false,
        }
    }

    /// Use only this backend (or the chain again with `None`).
    #[must_use]
    pub fn with_force(mut self, force: impl Into<Option<Backend>>) -> Self {
        self.force = force.into();
        self
    }

    /// Start the chain at this backend, for a device that remembered its winner. It is used only
    /// if the platform's chain has it (otherwise ignored). If it fails, the whole chain runs from
    /// the top, with the usual events. Store [`Renderer::backend`] per device and invalidate it
    /// when [`MEMORY_VERSION`](crate::MEMORY_VERSION) changes.
    #[must_use]
    pub fn with_preferred(mut self, preferred: impl Into<Option<Backend>>) -> Self {
        self.preferred = preferred.into();
        self
    }

    /// How long the WebGPU adapter request may take before it counts as failed with
    /// [`FallbackReason::Timeout`] and WebGL2 is tried (default 2 s). Web only: native adapter
    /// requests answer or fail.
    #[must_use]
    pub const fn with_adapter_timeout(mut self, timeout: Duration) -> Self {
        self.adapter_timeout = timeout;
        self
    }

    /// Registers `callback`, called for every step down the chain, with `to: None` on the final
    /// failure ([`Error::NoRendererAvailable`]), and with a `DeviceLost` event when the device is
    /// lost. The crate has no error-reporting dependency: the host forwards events to Sentry.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn with_fallback_callback(
        mut self,
        callback: impl Fn(&FallbackEvent) + Send + Sync + 'static,
    ) -> Self {
        self.on_fallback = Some(std::sync::Arc::new(callback));
        self
    }

    /// As on native, without the `Send + Sync` bound (JS values are not `Send`).
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn with_fallback_callback(mut self, callback: impl Fn(&FallbackEvent) + 'static) -> Self {
        self.on_fallback = Some(std::sync::Arc::new(callback));
        self
    }

    /// Force batches onto data textures (`true`) or storage buffers (`false`), or `None` to pick.
    #[must_use]
    pub fn with_data_texture(mut self, data_texture: impl Into<Option<bool>>) -> Self {
        self.data_texture = data_texture.into();
        self
    }

    /// A transparent output (`true`) or an opaque one (`false`, the default). Transparent: canvas
    /// surfaces are configured premultiplied (falling back to inherit, then to whatever the
    /// surface offers: a WebGL2 canvas has only one mode and keeps its alpha anyway), the CPU
    /// backend unpremultiplies for `putImageData`, scenes may clear to alpha 0, and readback is
    /// premultiplied RGBA8. Opaque: alpha is ignored when a canvas shows the frame.
    #[must_use]
    pub const fn with_transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
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

/// Builds the renderer on one backend, or says why it cannot.
async fn attempt(
    backend: Backend,
    target: &Target,
    options: &Options,
) -> core::result::Result<Inner, Step> {
    let into_step = |e: Error| match e {
        Error::Surface(m) => Step::Fall(FallbackReason::Surface(m)),
        Error::Device(m) => Step::Fall(FallbackReason::RequestDevice(m)),
        other => Step::Fatal(other),
    };
    if backend == Backend::Cpu {
        return Cpu::new(target, options.transparent)
            .map(Inner::Cpu)
            .map_err(into_step);
    }
    let probe = Gpu::probe(backend, target, options.adapter_timeout)
        .await
        .map_err(Step::Fall)?;
    Gpu::create(probe, target, options.data_texture, options.transparent)
        .await
        .map(|g| Inner::Gpu(Box::new(g)))
        .map_err(|f| {
            // On the web, once the canvas has given this backend its context it keeps that type
            // for life: the next backend cannot get one, so the walk ends. A failure before any
            // context was obtained (`getContext` null) still falls to the next backend.
            let taken: bool = cfg!(target_arch = "wasm32") && f.taken;
            match into_step(f.error) {
                Step::Fall(reason) if taken => Step::Stop(reason),
                other => other,
            }
        })
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
    /// The target has pixels of its own (not device-only).
    primary: bool,
    /// Size of each attached canvas, by `SurfaceId`; `None` once detached.
    surfaces: Vec<Option<(u32, u32)>>,
    /// The primary canvas and each attached one, to refuse a canvas that is already in use.
    kinds: Vec<Option<Kind>>,
    on_fallback: Option<Callback>,
    /// The device-lost event has been sent.
    lost_reported: AtomicBool,
}

/// A canvas attached to a renderer with [`Renderer::attach`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub(crate) usize);

impl Renderer {
    /// Picks the most capable backend and falls back down the chain, ending at the CPU.
    ///
    /// # Errors
    /// `BadInput` for an empty or oversized target; `NoRendererAvailable` if nothing works.
    pub async fn new(target: Target) -> Result<Self> {
        Self::with_options(target, Options::default()).await
    }

    /// As [`Renderer::new`], with overrides.
    ///
    /// A backend is given up on only after a real failure (no adapter, a timed-out adapter
    /// request, a device request or surface failure), each reported through
    /// [`Options::with_fallback_callback`].
    ///
    /// # Errors
    /// `NoRendererAvailable` if every backend failed (a forced one counts); `BadInput` for a bad
    /// target.
    pub async fn with_options(target: Target, options: Options) -> Result<Self> {
        let (width, height): (u32, u32) = (target.width, target.height);
        let canvas: bool = target.is_canvas();
        let primary: bool = !matches!(target.kind, Kind::None);
        // The upper bound is the chosen backend's texture limit, checked once it is known.
        if primary {
            check_size(width, height, u32::MAX)?;
        }
        let order: Vec<Backend> =
            fallback::plan(Backend::chain(), options.force, options.preferred);
        let emit = |e: &FallbackEvent| {
            if let Some(cb) = &options.on_fallback {
                cb(e);
            }
        };
        // Each GPU candidate is probed off the real canvas before it is built on it.
        let (_, inner): (Backend, Inner) =
            fallback::run_chain(&order, |b| attempt(b, &target, &options), &emit).await?;
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
            primary,
            surfaces: Vec::new(),
            kinds: vec![Some(target.kind.clone())],
            on_fallback: options.on_fallback,
            lost_reported: AtomicBool::new(false),
        })
    }

    /// Whether the GPU device has been lost: every draw then returns [`Error::DeviceLost`] (and a
    /// `DeviceLost` event is reported once). Build a new `Renderer`; see the README.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        matches!(&self.inner, Inner::Gpu(g) if g.lost().is_some())
    }

    /// `DeviceLost` if the device is gone, reporting the event the first time.
    fn check_lost(&self) -> Result<()> {
        let Inner::Gpu(g) = &self.inner else {
            return Ok(());
        };
        let Some((reason, message)) = g.lost() else {
            return Ok(());
        };
        if !self.lost_reported.swap(true, Ordering::Relaxed)
            && let Some(cb) = &self.on_fallback
        {
            cb(&FallbackEvent {
                from: self.caps.backend,
                to: None,
                reason: FallbackReason::DeviceLost(format!("{reason}: {message}")),
            });
        }
        Err(Error::DeviceLost { reason, message })
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
        self.need_primary()?;
        check_size(width, height, self.caps.max_texture_dimension_2d)?;
        match &mut self.inner {
            Inner::Gpu(g) => g.resize(width, height)?,
            Inner::Cpu(c) => c.resize(width, height)?,
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn need_primary(&self) -> Result<()> {
        if self.primary {
            Ok(())
        } else {
            Err(Error::BadInput(
                "this renderer has no primary target: attach a canvas and use present_to"
                    .to_owned(),
            ))
        }
    }

    /// Attaches a canvas (`Target::canvas` or `Target::offscreen_canvas`, wasm32 only) to this
    /// renderer's device: any number of canvases share one set of fonts, images and batches. Draw
    /// to it with [`Renderer::present_to`]. Make the renderer with [`Target::device_only`] (or an
    /// offscreen target) so the backend is chosen before any canvas is touched.
    ///
    /// A canvas that is already this renderer's target or attached (and not detached) is refused.
    ///
    /// - WebGPU: one device, one surface per canvas.
    /// - WebGL2: one GL context cannot drive many canvases, so frames are drawn on one internal
    ///   sheet and each is copied into its canvas with a 2D `drawImage` (alpha kept). A WebGL2
    ///   renderer whose primary target is a canvas has no sheet: attach is refused there.
    /// - CPU: each canvas gets its own 2D context and `putImageData`.
    ///
    /// # Errors
    /// `BadInput` for an image target, an empty or oversized canvas, or off the web; `Surface` if
    /// the canvas cannot be used (another context type took it).
    pub fn attach(&mut self, target: Target) -> Result<SurfaceId> {
        if !target.is_canvas() {
            return Err(Error::BadInput(
                "attach takes a canvas: Target::canvas or Target::offscreen_canvas (wasm32)"
                    .to_owned(),
            ));
        }
        check_size(
            target.width,
            target.height,
            self.caps.max_texture_dimension_2d,
        )?;
        if self
            .kinds
            .iter()
            .flatten()
            .any(|k| k.is_same_canvas(&target.kind))
        {
            return Err(Error::BadInput(
                "this canvas is already the target or attached: detach it first".to_owned(),
            ));
        }
        let slot: usize = match &mut self.inner {
            Inner::Gpu(g) => g.attach(&target)?,
            Inner::Cpu(c) => c.attach(&target)?,
        };
        debug_assert_eq!(slot, self.surfaces.len(), "slots and sizes stay in step");
        self.surfaces.push(Some((target.width, target.height)));
        self.kinds.push(Some(target.kind));
        Ok(SurfaceId(slot))
    }

    /// Resizes an attached canvas (device pixels; its `width` and `height` are set).
    ///
    /// # Errors
    /// `UnknownSurface`; `BadInput` for an empty or oversized size.
    pub fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32) -> Result<()> {
        self.surface_size(id)?;
        check_size(width, height, self.caps.max_texture_dimension_2d)?;
        match &mut self.inner {
            Inner::Gpu(g) => g.resize_slot(id.0, width, height)?,
            Inner::Cpu(c) => c.resize_slot(id.0, width, height)?,
        }
        self.surfaces[id.0] = Some((width, height));
        Ok(())
    }

    /// The size of an attached canvas, if it is still attached.
    ///
    /// # Errors
    /// `UnknownSurface`.
    pub fn surface_size(&self, id: SurfaceId) -> Result<(u32, u32)> {
        self.surfaces
            .get(id.0)
            .copied()
            .flatten()
            .ok_or(Error::UnknownSurface)
    }

    /// Frees an attached canvas's surface or context (the canvas itself is the caller's). The id
    /// is never reused.
    ///
    /// # Errors
    /// `UnknownSurface`.
    pub fn detach(&mut self, id: SurfaceId) -> Result<()> {
        self.surface_size(id)?;
        match &mut self.inner {
            Inner::Gpu(g) => g.detach(id.0),
            Inner::Cpu(c) => c.detach(id.0),
        }
        self.surfaces[id.0] = None;
        self.kinds[id.0 + 1] = None;
        Ok(())
    }

    /// Draws the scene to an attached canvas with no readback. Synchronous. Backends: see
    /// [`Renderer::attach`].
    ///
    /// # Errors
    /// `UnknownSurface`, `AtlasFull`, `UnknownFont`, `DeviceLost`, or `Surface` if the canvas cannot be recovered.
    pub fn present_to(&mut self, id: SurfaceId, scene: &Scene) -> Result<()> {
        self.surface_size(id)?;
        self.check_lost()?;
        let flat = self.atlas.flatten(scene, &self.images)?;
        match &mut self.inner {
            Inner::Cpu(c) => {
                c.present_to(id.0, &flat, &self.atlas.pixels, scene.clear, &self.batches)
            }
            Inner::Gpu(g) => {
                let upload: Option<&[u8]> = if self.atlas.dirty {
                    Some(&self.atlas.pixels)
                } else {
                    None
                };
                g.present(Some(id.0), &flat, upload, scene.clear, &self.batches)?;
                self.atlas.dirty = false;
                Ok(())
            }
        }
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
        self.check_lost()?;
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
    /// The count is not checked against the batch's data length: the renderer cannot know how many
    /// words an instance reads, and a batch may draw from `instance_index` and `params` alone. A
    /// shader that reads past the data gets what its backend's bounds checks give (clamped or
    /// zero), and the CPU form is the caller's own function, so it must bound itself.
    ///
    /// # Errors
    /// `UnknownBatch`.
    pub fn set_batch_instances(&mut self, id: BatchId, instances: u32) -> Result<()> {
        self.batches
            .get_mut(id.0)
            .ok_or(Error::UnknownBatch)?
            .instances = instances;
        Ok(())
    }

    /// Draws the scene and returns the image as RGBA8 (`width * height * 4` bytes): a readback,
    /// for tests and for callers that want pixels. Offscreen targets only; use
    /// [`Renderer::present`] for a canvas.
    ///
    /// The pixels are **premultiplied** (colour already multiplied by alpha), whatever the
    /// backend. With an opaque scene (alpha 255 everywhere) premultiplied and straight are the
    /// same bytes; for a transparent one use [`unpremultiply_rgba8`] to get straight RGBA.
    ///
    /// # Errors
    /// `BadInput` on a canvas or device-only target; `AtlasFull`, `UnknownFont`, or a readback
    /// failure.
    pub async fn render(&mut self, scene: &Scene) -> Result<Vec<u8>> {
        self.need_primary()?;
        self.check_lost()?;
        if self.canvas {
            return Err(Error::BadInput(
                "a canvas target has no readback: use present, or an offscreen target".to_owned(),
            ));
        }
        let flat = self.atlas.flatten(scene, &self.images)?;
        match &mut self.inner {
            Inner::Cpu(c) => c.render(&flat, &self.atlas.pixels, scene.clear, &self.batches),
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
        self.need_primary()?;
        self.check_lost()?;
        let flat = self.atlas.flatten(scene, &self.images)?;
        match &mut self.inner {
            Inner::Cpu(c) => c.present(&flat, &self.atlas.pixels, scene.clear, &self.batches),
            Inner::Gpu(g) => {
                let upload: Option<&[u8]> = if self.atlas.dirty {
                    Some(&self.atlas.pixels)
                } else {
                    None
                };
                g.present(None, &flat, upload, scene.clear, &self.batches)?;
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
        self.check_lost()?;
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
        self.check_lost()?;
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
        self.check_lost()?;
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
        self.check_lost()?;
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

/// Premultiplied RGBA8 to straight, rounded (what a canvas `putImageData` wants, and what a PNG
/// holds). Opaque pixels are unchanged.
#[must_use]
pub fn unpremultiply_rgba8(rgba8: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = rgba8.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        let a: u32 = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    out
}
