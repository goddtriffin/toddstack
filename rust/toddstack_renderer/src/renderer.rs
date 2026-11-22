//! `Renderer`: backend selection, scenes, custom batches and kernels behind one type.

use crate::backend::{Backend, Capabilities};
use crate::batch::{BatchDesc, BatchState};
use crate::cpu::Cpu;
use crate::error::{Error, Result};
use crate::glyphs::Atlas;
use crate::gpu::Gpu;
use crate::kernel::{Kernel, KernelOutput, KernelPath};
use crate::scene::{BatchId, FontId, Scene};

/// What to render into: an offscreen RGBA8 image of this size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Target {
    /// An offscreen target.
    #[must_use]
    pub const fn offscreen(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Overrides for backend selection.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Use only this backend (fails with `NoBackend` if it is missing, except `Cpu`).
    /// `None` walks [`Backend::chain`].
    pub force: Option<Backend>,
    /// Force batches onto data textures (`Some(true)`) or storage buffers (`Some(false)`);
    /// `None` picks by capability. Mostly for testing the WebGL2 path on a desktop GPU.
    pub data_texture: Option<bool>,
}

impl Options {
    /// `WGPU_BACKEND=vulkan|dx12|metal|gl|cpu` (native only; unset or unknown means no override).
    #[must_use]
    pub fn from_env() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let force: Option<Backend> = std::env::var("WGPU_BACKEND").ok().and_then(|s| Backend::parse(&s));
            Self { force, data_texture: None }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self::default()
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
    width: u32,
    height: u32,
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
        let chain: Vec<Backend> = options.force.map_or_else(|| Backend::chain().to_vec(), |b| vec![b]);
        let mut tried: Vec<&str> = Vec::new();
        for b in chain {
            tried.push(b.name());
            if b == Backend::Cpu {
                return Ok(Self { inner: Inner::Cpu(Cpu::new(width, height)?), caps: Capabilities::cpu(), atlas: Atlas::new(), batches: Vec::new(), width, height });
            }
            if let Some(gpu) = Gpu::create(b, width, height, options.data_texture).await? {
                let caps: Capabilities = gpu.caps.clone();
                return Ok(Self { inner: Inner::Gpu(Box::new(gpu)), caps, atlas: Atlas::new(), batches: Vec::new(), width, height });
            }
        }
        Err(Error::NoBackend { tried: tried.join(", ") })
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
        match &mut self.inner {
            Inner::Gpu(g) => g.resize(width, height),
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
    /// Shader or pipeline problems on the GPU.
    pub fn add_batch(&mut self, desc: BatchDesc) -> Result<BatchId> {
        let state: BatchState = desc.into();
        if let Inner::Gpu(g) = &mut self.inner {
            g.add_batch(&state)?;
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
        let end: usize = offset + words.len();
        if end > b.data.len() {
            return Err(Error::BadInput(format!("batch `{}` has {} words, update ends at {end}", b.name, b.data.len())));
        }
        b.data[offset..end].copy_from_slice(words);
        if let Inner::Gpu(g) = &self.inner {
            g.write_batch_data(id.0, &self.batches[id.0].data, offset);
        }
        Ok(())
    }

    /// Sets the batch's four parameters (`globals.params` in WGSL).
    ///
    /// # Errors
    /// `UnknownBatch`.
    pub fn set_batch_params(&mut self, id: BatchId, params: [f32; 4]) -> Result<()> {
        self.batches.get_mut(id.0).ok_or(Error::UnknownBatch)?.params = params;
        if let Inner::Gpu(g) = &self.inner {
            g.write_batch_params(id.0, params);
        }
        Ok(())
    }

    /// Sets how many instances the batch draws.
    ///
    /// # Errors
    /// `UnknownBatch`.
    pub fn set_batch_instances(&mut self, id: BatchId, instances: u32) -> Result<()> {
        self.batches.get_mut(id.0).ok_or(Error::UnknownBatch)?.instances = instances;
        Ok(())
    }

    /// Draws the scene and returns the image as RGBA8 (`width * height * 4` bytes).
    ///
    /// # Errors
    /// `AtlasFull`, `UnknownFont`, or a readback failure.
    pub async fn render(&mut self, scene: &Scene) -> Result<Vec<u8>> {
        let flat = self.atlas.flatten(scene)?;
        match &mut self.inner {
            Inner::Cpu(c) => Ok(c.render(&flat, &self.atlas.pixels, scene.clear, &self.batches)),
            Inner::Gpu(g) => {
                let upload: Option<&[u8]> = if self.atlas.dirty { Some(&self.atlas.pixels) } else { None };
                let px: Vec<u8> = g.render(&flat, upload, scene.clear, &self.batches).await?;
                self.atlas.dirty = false;
                Ok(px)
            }
        }
    }

    /// Runs a kernel on the best form this backend supports: compute, then fragment, then CPU.
    ///
    /// # Errors
    /// `KernelUnavailable` if no form can run here.
    pub async fn run_kernel(&self, kernel: &Kernel) -> Result<KernelOutput> {
        let path: KernelPath = match &self.inner {
            Inner::Gpu(g) if g.caps.compute && kernel.compute.is_some() => KernelPath::Compute,
            Inner::Gpu(_) if kernel.fragment.is_some() => KernelPath::Fragment,
            _ if kernel.cpu.is_some() => KernelPath::Cpu,
            _ => return Err(Error::KernelUnavailable { kernel: kernel.name.clone() }),
        };
        self.run_kernel_with(kernel, path).await
    }

    /// Runs one named form, for comparing forms against each other.
    ///
    /// # Errors
    /// `KernelUnavailable` if the kernel lacks that form, or the backend cannot run it.
    pub async fn run_kernel_with(&self, kernel: &Kernel, path: KernelPath) -> Result<KernelOutput> {
        let rgba8: Vec<u8> = match (path, &self.inner) {
            (KernelPath::Cpu, _) => {
                let f = kernel.cpu.as_ref().ok_or_else(|| Error::KernelUnavailable { kernel: kernel.name.clone() })?;
                f(&kernel.inputs, kernel.width, kernel.height)
            }
            (KernelPath::Compute, Inner::Gpu(g)) if !g.caps.compute => return Err(Error::KernelUnavailable { kernel: kernel.name.clone() }),
            (p, Inner::Gpu(g)) => g.run_kernel(kernel, p).await?,
            (_, Inner::Cpu(_)) => return Err(Error::KernelUnavailable { kernel: kernel.name.clone() }),
        };
        Ok(KernelOutput { path, width: kernel.width, height: kernel.height, rgba8 })
    }
}
