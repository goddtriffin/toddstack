//! Backend names, the fallback chain per platform, and what the chosen one can do.

use wgpu::Backends;

/// A way to draw. The CPU backend is always available and is the pixel reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Browser WebGPU.
    WebGpu,
    /// WebGL2, through wgpu's GL backend.
    WebGl2,
    /// Apple Metal.
    Metal,
    /// Vulkan (Android, Windows, Linux).
    Vulkan,
    /// Direct3D 12 (Windows).
    Dx12,
    /// OpenGL ES (Android, Windows, Linux), through wgpu's GL backend.
    Gles,
    /// tiny-skia on the CPU.
    Cpu,
}

impl Backend {
    /// The most capable first, the CPU last, for this platform.
    #[must_use]
    pub const fn chain() -> &'static [Self] {
        if cfg!(target_arch = "wasm32") {
            &[Self::WebGpu, Self::WebGl2, Self::Cpu]
        } else if cfg!(target_vendor = "apple") {
            &[Self::Metal, Self::Cpu]
        } else if cfg!(target_os = "windows") {
            &[Self::Vulkan, Self::Dx12, Self::Gles, Self::Cpu]
        } else {
            &[Self::Vulkan, Self::Gles, Self::Cpu]
        }
    }

    /// Lowercase name, as accepted by [`Backend::parse`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WebGpu => "webgpu",
            Self::WebGl2 => "webgl2",
            Self::Metal => "metal",
            Self::Vulkan => "vulkan",
            Self::Dx12 => "dx12",
            Self::Gles => "gles",
            Self::Cpu => "cpu",
        }
    }

    /// Parses `webgpu`, `webgl2`, `metal`, `vulkan`, `dx12`, `gl`/`gles` or `cpu`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "webgpu" => Some(Self::WebGpu),
            "webgl2" | "webgl" => Some(Self::WebGl2),
            "metal" => Some(Self::Metal),
            "vulkan" | "vk" => Some(Self::Vulkan),
            "dx12" | "d3d12" => Some(Self::Dx12),
            "gl" | "gles" | "opengl" => Some(if cfg!(target_arch = "wasm32") { Self::WebGl2 } else { Self::Gles }),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }

    pub(crate) const fn wgpu_backends(self) -> Option<Backends> {
        match self {
            Self::WebGpu => Some(Backends::BROWSER_WEBGPU),
            Self::WebGl2 | Self::Gles => Some(Backends::GL),
            Self::Metal => Some(Backends::METAL),
            Self::Vulkan => Some(Backends::VULKAN),
            Self::Dx12 => Some(Backends::DX12),
            Self::Cpu => None,
        }
    }
}

/// What the chosen backend reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The backend in use.
    pub backend: Backend,
    /// Adapter name, type and driver (or `tiny-skia` for the CPU).
    pub adapter: String,
    /// Compute shaders work (false on WebGL2 and the CPU).
    pub compute: bool,
    /// Storage buffers work in vertex shaders (false on WebGL2: batches use a data texture).
    pub storage_buffers: bool,
    /// Largest 2D texture side.
    pub max_texture_dimension_2d: u32,
    /// Largest buffer, in bytes.
    pub max_buffer_size: u64,
    /// Largest storage binding, in bytes.
    pub max_storage_buffer_binding_size: u64,
}

impl Capabilities {
    pub(crate) fn cpu() -> Self {
        Self {
            backend: Backend::Cpu,
            adapter: "tiny-skia (CPU)".to_owned(),
            compute: false,
            storage_buffers: false,
            max_texture_dimension_2d: 16_384,
            max_buffer_size: u64::from(u32::MAX),
            max_storage_buffer_binding_size: 0,
        }
    }
}
