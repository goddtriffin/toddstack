//! Compute kernels with a fragment-shader form (where compute is missing) and a CPU form.

use std::sync::Arc;

/// One input binding, at the index of its position in `Kernel::inputs`.
#[derive(Clone, Debug)]
pub enum Binding {
    /// An RGBA8 image, read in WGSL as `texture_2d<f32>` (`textureLoad`).
    Texture {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// `width * height * 4` bytes.
        rgba8: Vec<u8>,
    },
    /// Read-only data. A `var<storage, read>` in the compute form, a `var<uniform>` in the
    /// fragment form (WebGL2 has no storage buffers), so keep it small (16 KB) and 16-byte
    /// aligned in elements, such as `array<vec4f, N>`.
    Storage(Vec<u8>),
    /// A `var<uniform>` in both forms.
    Uniform(Vec<u8>),
}

impl Binding {
    /// A storage binding from typed data.
    #[must_use]
    pub fn storage<T: bytemuck::NoUninit>(data: &[T]) -> Self {
        Self::Storage(bytemuck::cast_slice(data).to_vec())
    }

    /// A uniform binding from typed data.
    #[must_use]
    pub fn uniform<T: bytemuck::NoUninit>(data: &[T]) -> Self {
        Self::Uniform(bytemuck::cast_slice(data).to_vec())
    }

    /// An RGBA8 texture binding; `rgba8` must hold `width * height * 4` bytes.
    #[must_use]
    pub const fn texture(width: u32, height: u32, rgba8: Vec<u8>) -> Self {
        Self::Texture { width, height, rgba8 }
    }
}

/// The CPU form: inputs and output size in, `width * height * 4` RGBA8 bytes out.
pub type CpuKernel = Arc<dyn Fn(&[Binding], u32, u32) -> Vec<u8> + Send + Sync>;

/// Which form ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelPath {
    /// A compute dispatch into a storage texture.
    Compute,
    /// A full-screen fragment pass into a render target.
    Fragment,
    /// The CPU function.
    Cpu,
}

/// WGSL plus typed bindings, producing one RGBA8 image.
///
/// Compute form: input `i` is `@group(0) @binding(i)`, the output is
/// `@group(0) @binding(inputs.len())` as `texture_storage_2d<rgba8unorm, write>`, entry point
/// `main`; `workgroup` must equal the shader's `@workgroup_size`.
/// Fragment form: the same input bindings, entry point `fs(@builtin(position) pos: vec4f) ->
/// @location(0) vec4f`; the full-screen vertex stage is added for you.
#[derive(Clone)]
pub struct Kernel {
    /// Shown in errors.
    pub name: String,
    /// The compute form.
    pub compute: Option<String>,
    /// The fragment form, for backends without compute.
    pub fragment: Option<String>,
    /// The CPU form, the last resort.
    pub cpu: Option<CpuKernel>,
    /// The compute `@workgroup_size` (x, y).
    pub workgroup: [u32; 2],
    /// Inputs, in binding order.
    pub inputs: Vec<Binding>,
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
}

/// The kernel's image.
#[derive(Clone, Debug)]
pub struct KernelOutput {
    /// Which form produced it.
    pub path: KernelPath,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, RGBA8.
    pub rgba8: Vec<u8>,
}

pub const FULLSCREEN_VS: &str = "\n@vertex fn vs_full(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4f {\n    let p = vec2f(f32((vi << 1u) & 2u), f32(vi & 2u));\n    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);\n}\n";
