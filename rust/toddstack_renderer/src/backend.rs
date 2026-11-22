//! Backend names, the fallback chain per platform, and what the chosen one can do.

use std::future::Future;

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
            "gl" | "gles" | "opengl" => Some(if cfg!(target_arch = "wasm32") {
                Self::WebGl2
            } else {
                Self::Gles
            }),
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

/// Walks `chain` in order and returns the first backend `probe` accepts, with what the probe
/// produced. The CPU is always accepted (it needs no probe), so a chain ending in it always
/// selects; a chain without it can select nothing.
///
/// # Errors
/// The first error a probe returns; a rejection is `Ok(None)`, never an error.
pub(crate) async fn select<T, F, Fut>(
    chain: &[Backend],
    mut probe: F,
) -> crate::error::Result<Option<(Backend, Option<T>)>>
where
    F: FnMut(Backend) -> Fut,
    Fut: Future<Output = crate::error::Result<Option<T>>>,
{
    for &b in chain {
        if b == Backend::Cpu {
            return Ok(Some((b, None)));
        }
        if let Some(t) = probe(b).await? {
            return Ok(Some((b, Some(t))));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{Backend, select};
    use crate::error::Error;

    fn run(chain: &[Backend], accept: &[Backend]) -> Option<(Backend, bool)> {
        pollster::block_on(select(chain, |b| {
            let ok = accept.contains(&b);
            async move { Ok(ok.then_some(())) }
        }))
        .expect("no error")
        .map(|(b, t)| (b, t.is_some()))
    }

    #[test]
    fn first_accepted_wins() {
        let c = [Backend::WebGpu, Backend::WebGl2, Backend::Cpu];
        assert_eq!(
            run(&c, &[Backend::WebGpu, Backend::WebGl2]),
            Some((Backend::WebGpu, true))
        );
        assert_eq!(run(&c, &[Backend::WebGl2]), Some((Backend::WebGl2, true)));
    }

    #[test]
    fn all_rejected_reaches_the_cpu() {
        let c = [Backend::WebGpu, Backend::WebGl2, Backend::Cpu];
        assert_eq!(run(&c, &[]), Some((Backend::Cpu, false)));
        assert_eq!(run(&[Backend::Cpu], &[]), Some((Backend::Cpu, false)));
    }

    #[test]
    fn forced_gpu_rejected_selects_nothing() {
        assert_eq!(run(&[Backend::WebGpu], &[]), None);
    }

    #[test]
    fn every_platform_chain_ends_at_the_cpu() {
        assert_eq!(Backend::chain().last(), Some(&Backend::Cpu));
    }

    #[test]
    fn probes_run_in_order_and_stop_at_the_winner() {
        let seen = std::cell::RefCell::new(Vec::new());
        let c = [Backend::Vulkan, Backend::Dx12, Backend::Gles, Backend::Cpu];
        let got = pollster::block_on(select(&c, |b| {
            seen.borrow_mut().push(b);
            async move { Ok((b == Backend::Dx12).then_some(())) }
        }))
        .expect("no error");
        assert_eq!(got.map(|(b, _)| b), Some(Backend::Dx12));
        assert_eq!(*seen.borrow(), [Backend::Vulkan, Backend::Dx12]);
    }

    #[test]
    fn a_probe_error_stops_the_walk() {
        let c = [Backend::WebGpu, Backend::Cpu];
        let got = pollster::block_on(select::<(), _, _>(&c, |_| async {
            Err(Error::BadInput("x".to_owned()))
        }));
        assert!(got.is_err());
    }
}
