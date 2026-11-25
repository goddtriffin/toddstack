//! The wgpu backend: one instanced draw for shapes and glyphs, custom batches, compute kernels.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "sizes and counts are small values"
)]

use crate::backend::{Backend, Capabilities};
use crate::batch::{BatchState, DATA_TEXTURE_WIDTH, prelude};
#[cfg(target_arch = "wasm32")]
use crate::canvas::Ctx2d;
use crate::error::{Error, Result};
use crate::glyphs::{ATLAS_SIZE, Flat, Inst, Run};
use crate::gpu_image::{GpuImage, ImagePipes};
use crate::gpu_path::{self, Layer, PVert, PathPipes};
use crate::kernel::{Binding, FULLSCREEN_VS, Kernel, KernelPath};
use crate::renderer::{Kind, Target};
use crate::scene::{ImageId, Rgba, Sampling};
use wgpu::util::DeviceExt;

/// The attributes of one [`Inst`], shared by the shape and image pipelines.
pub const INST_ATTRS: [wgpu::VertexAttribute; 4] =
    wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4];

pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A canvas surface and the configuration it was last given.
struct Surf {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    #[cfg(target_arch = "wasm32")]
    kind: Kind,
}

/// Where a frame ends up.
enum Out {
    /// An offscreen image, readable back.
    Texture(wgpu::Texture),
    /// A canvas surface, presented directly (WebGPU, or the one canvas of a WebGL2 renderer).
    Surface(Surf),
    /// WebGL2 with many canvases: drawn on the sheet, then copied into this canvas's 2D context.
    #[cfg(target_arch = "wasm32")]
    Copy(Ctx2d),
}

/// A target and its size.
struct Slot {
    w: u32,
    h: u32,
    out: Out,
}

/// WebGL2's one internal canvas. A GL context drives one canvas, so every attached canvas is drawn
/// here (at the top-left, sized to the frame) and copied out with `drawImage`.
#[cfg(target_arch = "wasm32")]
struct Sheet {
    surface: wgpu::Surface<'static>,
    canvas: web_sys::OffscreenCanvas,
    /// `None` until the first frame; grown, never shrunk.
    config: Option<wgpu::SurfaceConfiguration>,
}

/// One frame's scene data.
struct Frame<'a> {
    flat: &'a Flat,
    atlas: Option<&'a [u8]>,
    clear: Rgba,
    batches: &'a [BatchState],
}

struct GpuBatch {
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    globals: wgpu::Buffer,
    data_buf: Option<wgpu::Buffer>,
    data_tex: Option<wgpu::Texture>,
}

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    #[cfg(target_arch = "wasm32")]
    instance: wgpu::Instance,
    #[cfg(target_arch = "wasm32")]
    adapter: wgpu::Adapter,
    pub caps: Capabilities,
    data_texture: bool,
    /// Canvas surfaces keep their alpha (premultiplied).
    #[cfg_attr(
        not(target_arch = "wasm32"),
        expect(
            dead_code,
            reason = "only canvases, which are wasm32, are configured with it"
        )
    )]
    transparent: bool,
    /// The target size the view uniforms (here and in every batch's globals) currently hold.
    view: (u32, u32),
    /// The target given to `Renderer::new`; `None` for a device-only renderer.
    primary: Option<Slot>,
    /// Attached canvases, by `SurfaceId`.
    slots: Vec<Option<Slot>>,
    #[cfg(target_arch = "wasm32")]
    sheet: Option<Sheet>,
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    view_buf: wgpu::Buffer,
    atlas: wgpu::Texture,
    insts: wgpu::Buffer,
    insts_cap: u64,
    batches: Vec<GpuBatch>,
    image_pipes: ImagePipes,
    /// Set by wgpu's device-lost callback: the reason (`destroyed` or `unknown`) and message.
    lost: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>,
    images: Vec<Option<GpuImage>>,
    /// Made on the first path; the layer and its composite bind group. The layer only grows, to
    /// the largest frame drawn, and each frame uses its top-left.
    path_pipes: Option<PathPipes>,
    layer: Option<(Layer, GpuImage)>,
    path_buf: wgpu::Buffer,
    path_cap: u64,
}

fn no_primary() -> Error {
    Error::BadInput(
        "this renderer has no primary target: attach a canvas and use present_to".to_owned(),
    )
}

/// Straight source colour over a premultiplied target (shapes, glyphs and custom batches).
fn blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        // Premultiplied "over" for alpha, so a transparent target accumulates coverage (over an
        // opaque one it stays 1).
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

fn target_texture(device: &wgpu::Device, w: u32, h: u32, storage: bool) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: if storage {
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING
        },
        view_formats: &[],
    })
}

pub fn uniform_entry(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

pub fn texture_entry(
    binding: u32,
    vis: wgpu::ShaderStages,
    sample_type: wgpu::TextureSampleType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn rgba_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    w: u32,
    h: u32,
    data: &[u8],
) -> wgpu::Texture {
    device.create_texture_with_data(
        queue,
        &wgpu::TextureDescriptor {
            label: Some("input"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        data,
    )
}

fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut v: Vec<u8> = bytes.to_vec();
    v.resize(v.len().div_ceil(16).max(1) * 16, 0);
    v
}

/// A GPU backend that passed every rejection rule, found before the real canvas was touched.
pub struct Probe {
    backend: Backend,
    instance: wgpu::Instance,
    /// The adapter and device to keep. `None` for a WebGL2 canvas: its adapter is bound to the
    /// GL context of the surface it was requested with, so it is requested again on the canvas.
    acquired: Option<Acquired>,
    /// WebGL2 without a canvas target: the probe's surface becomes the sheet that frames for
    /// attached canvases are drawn on.
    #[cfg(target_arch = "wasm32")]
    sheet: Option<(wgpu::Surface<'static>, web_sys::OffscreenCanvas)>,
}

struct Acquired {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Why an adapter or device could not be had.
type Unavailable = crate::fallback::FallbackReason;

/// How long the adapter request may take: only WebGPU on the web is bounded, because a stuck
/// `requestAdapter` is the one failure there that never answers.
fn adapter_timeout(
    backend: Backend,
    configured: std::time::Duration,
) -> Option<std::time::Duration> {
    (cfg!(target_arch = "wasm32") && backend == Backend::WebGpu).then_some(configured)
}

/// An adapter and device, or the reason there is none: no adapter, a timeout, a software
/// adapter, or a failed device request.
async fn acquire(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
    timeout: Option<std::time::Duration>,
) -> std::result::Result<Acquired, Unavailable> {
    let request = instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: surface,
        ..Default::default()
    });
    let adapter: std::result::Result<wgpu::Adapter, wgpu::RequestAdapterError> = match timeout {
        Some(d) => crate::timeout::race(request, crate::timeout::sleep(d))
            .await
            .ok_or(Unavailable::Timeout)?,
        None => request.await,
    };
    let adapter: wgpu::Adapter = adapter.map_err(|_| Unavailable::NoAdapter)?;
    if adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return Err(Unavailable::Rejected(
            "software adapter: the tiny-skia path is the CPU renderer".to_owned(),
        ));
    }
    let (device, queue): (wgpu::Device, wgpu::Queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_limits: adapter.limits(),
            ..Default::default()
        })
        .await
        .map_err(|e| Unavailable::RequestDevice(e.to_string()))?;
    Ok(Acquired {
        adapter,
        device,
        queue,
    })
}

impl Gpu {
    /// The reason and message if the device has been lost.
    pub fn lost(&self) -> Option<(String, String)> {
        self.lost.lock().ok().and_then(|l| l.clone())
    }

    /// Decides whether `backend` can be used, without touching a canvas target's real canvas (it
    /// keeps one context type for life, so a GPU context taken and then rejected would leave the
    /// CPU fallback with none). A WebGL2 canvas target is probed on a 1x1 `OffscreenCanvas`.
    ///
    /// # Errors
    /// The reason when the backend has no usable (non-CPU) adapter here.
    pub async fn probe(
        backend: Backend,
        target: &Target,
        adapter_timeout_for: std::time::Duration,
    ) -> std::result::Result<Probe, Unavailable> {
        let Some(backends) = backend.wgpu_backends() else {
            return Err(Unavailable::NoAdapter);
        };
        let mut desc: wgpu::InstanceDescriptor =
            wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = backends;
        let instance: wgpu::Instance = wgpu::Instance::new(desc);
        // wgpu's web GL backend only finds an adapter for a given surface: an offscreen target
        // brings its own; a canvas target probes with a 1x1 one. WebGPU needs no surface.
        #[cfg(target_arch = "wasm32")]
        let probe_canvas: Option<(wgpu::Surface<'static>, web_sys::OffscreenCanvas)> = if backend
            == Backend::WebGl2
        {
            Some(throwaway_surface(&instance).map_err(|e| Unavailable::Surface(e.to_string()))?)
        } else {
            None
        };
        #[cfg(target_arch = "wasm32")]
        let surface: Option<&wgpu::Surface<'static>> = probe_canvas.as_ref().map(|(s, _)| s);
        #[cfg(not(target_arch = "wasm32"))]
        let surface: Option<&wgpu::Surface<'static>> = None;
        let acquired: Acquired = acquire(
            &instance,
            surface,
            adapter_timeout(backend, adapter_timeout_for),
        )
        .await?;
        #[cfg(target_arch = "wasm32")]
        if let Some(probe_surface) = surface
            && !surface_takes_format(probe_surface, &acquired.adapter)
        {
            return Err(Unavailable::Rejected(format!(
                "the surface cannot take {FORMAT:?}"
            )));
        }
        let keep: bool =
            !(cfg!(target_arch = "wasm32") && backend == Backend::WebGl2 && target.is_canvas());
        Ok(Probe {
            backend,
            instance,
            acquired: keep.then_some(acquired),
            #[cfg(target_arch = "wasm32")]
            sheet: probe_canvas.filter(|_| !target.is_canvas()),
        })
    }

    /// Builds the GPU renderer on an accepted probe, creating the surface on the real canvas.
    ///
    /// # Errors
    /// `Surface` or `Device` if the real canvas still fails after acceptance. On the web the
    /// canvas has then given this backend its context and cannot give another type, so the chain
    /// ends here with the real reason (`Step::Stop`) instead of trying the next backend.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear device and pipeline setup"
    )]
    pub async fn create(
        probe: Probe,
        target: &Target,
        data_texture: Option<bool>,
        transparent: bool,
    ) -> Result<Self> {
        let (width, height): (u32, u32) = (target.width, target.height);
        let Probe {
            backend,
            instance,
            acquired,
            #[cfg(target_arch = "wasm32")]
            sheet,
        } = probe;
        #[cfg(target_arch = "wasm32")]
        let mut surface: Option<wgpu::Surface<'static>> = None;
        #[cfg(not(target_arch = "wasm32"))]
        let surface: Option<wgpu::Surface<'static>> = None;
        let acquired: Acquired = if let Some(a) = acquired {
            a
        } else {
            #[cfg(target_arch = "wasm32")]
            let real: wgpu::Surface<'static> = web_surface(&instance, &target.kind)?;
            #[cfg(target_arch = "wasm32")]
            let a: std::result::Result<Acquired, Unavailable> =
                acquire(&instance, Some(&real), None).await;
            #[cfg(not(target_arch = "wasm32"))]
            let a: std::result::Result<Acquired, Unavailable> =
                acquire(&instance, None, None).await;
            #[cfg(target_arch = "wasm32")]
            {
                surface = Some(real);
            }
            a.map_err(|r| Error::Device(r.to_string()))?
        };
        let Acquired {
            adapter,
            device,
            queue,
        } = acquired;
        let lost: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>> =
            std::sync::Arc::default();
        let lost_slot = std::sync::Arc::clone(&lost);
        device.set_device_lost_callback(move |reason, message| {
            let reason: &str = match reason {
                wgpu::DeviceLostReason::Destroyed => "destroyed",
                wgpu::DeviceLostReason::Unknown => "unknown",
            };
            if let Ok(mut slot) = lost_slot.lock() {
                slot.get_or_insert((reason.to_owned(), message));
            }
        });
        let info: wgpu::AdapterInfo = adapter.get_info();
        let flags: wgpu::DownlevelFlags = adapter.get_downlevel_capabilities().flags;
        let limits: wgpu::Limits = adapter.limits();
        let compute: bool = flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS);
        let storage: bool = flags.contains(wgpu::DownlevelFlags::VERTEX_STORAGE)
            && limits.max_storage_buffers_per_shader_stage > 0;
        let caps: Capabilities = Capabilities {
            backend,
            adapter: format!(
                "{} ({:?}, {}, driver {} {})",
                info.name,
                info.device_type,
                backend.name(),
                info.driver,
                info.driver_info
            ),
            compute,
            storage_buffers: storage,
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
            max_buffer_size: limits.max_buffer_size,
            max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
        };
        let has_target: bool = !matches!(target.kind, Kind::None);
        if has_target {
            crate::renderer::check_size(width, height, caps.max_texture_dimension_2d)?;
        }
        #[cfg(target_arch = "wasm32")]
        if surface.is_none() && target.is_canvas() {
            surface = Some(web_surface(&instance, &target.kind)?);
        }
        let primary: Option<Slot> = match surface {
            Some(surface) if target.is_canvas() => {
                let Some(config) = surface_config(
                    &surface,
                    &adapter,
                    (width, height),
                    caps.max_texture_dimension_2d,
                    transparent,
                )?
                else {
                    return Err(Error::Surface(format!(
                        "the canvas surface cannot take {FORMAT:?}"
                    )));
                };
                surface.configure(&device, &config);
                #[cfg(target_arch = "wasm32")]
                crate::canvas::set_kind_size(&target.kind, width, height);
                Some(Slot {
                    w: width,
                    h: height,
                    out: Out::Surface(Surf {
                        surface,
                        config,
                        #[cfg(target_arch = "wasm32")]
                        kind: target.kind.clone(),
                    }),
                })
            }
            _ if has_target => Some(Slot {
                w: width,
                h: height,
                out: Out::Texture(target_texture(&device, width, height, false)),
            }),
            _ => None,
        };
        let shader: wgpu::ShaderModule =
            device.create_shader_module(wgpu::include_wgsl!("shapes.wgsl"));
        let atlas: wgpu::Texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view_buf: wgpu::Buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("view"),
            contents: bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bgl: wgpu::BindGroupLayout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    uniform_entry(0, wgpu::ShaderStages::VERTEX),
                    texture_entry(
                        1,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::TextureSampleType::Float { filterable: false },
                    ),
                ],
            });
        let bind: wgpu::BindGroup = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: view_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &atlas.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
            ],
        });
        let layout: wgpu::PipelineLayout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
        let pipeline: wgpu::RenderPipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shapes"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: size_of::<Inst>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &INST_ATTRS,
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(blend()),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let insts_cap: u64 = 64 * size_of::<Inst>() as u64;
        let insts: wgpu::Buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: insts_cap,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let path_cap: u64 = 1024 * size_of::<PVert>() as u64;
        let path_buf: wgpu::Buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("path vertices"),
            size: path_cap,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let image_pipes: ImagePipes = ImagePipes::new(&device);
        Ok(Self {
            device,
            queue,
            #[cfg(target_arch = "wasm32")]
            instance,
            #[cfg(target_arch = "wasm32")]
            adapter,
            data_texture: data_texture.unwrap_or(!caps.storage_buffers),
            caps,
            transparent,
            view: (width, height),
            primary,
            slots: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            sheet: sheet.map(|(surface, canvas)| Sheet {
                surface,
                canvas,
                config: None,
            }),
            pipeline,
            bind,
            view_buf,
            atlas,
            insts,
            insts_cap,
            batches: Vec::new(),
            image_pipes,
            lost,
            images: Vec::new(),
            path_pipes: None,
            layer: None,
            path_buf,
            path_cap,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        crate::renderer::check_size(width, height, self.caps.max_texture_dimension_2d)?;
        let slot: &mut Slot = self.primary.as_mut().ok_or_else(no_primary)?;
        Self::resize_out(&self.device, slot, width, height);
        Ok(())
    }

    pub fn resize_slot(&mut self, i: usize, width: u32, height: u32) -> Result<()> {
        crate::renderer::check_size(width, height, self.caps.max_texture_dimension_2d)?;
        let slot: &mut Slot = self
            .slots
            .get_mut(i)
            .and_then(Option::as_mut)
            .ok_or(Error::UnknownSurface)?;
        Self::resize_out(&self.device, slot, width, height);
        Ok(())
    }

    fn resize_out(device: &wgpu::Device, slot: &mut Slot, width: u32, height: u32) {
        match &mut slot.out {
            Out::Texture(t) => *t = target_texture(device, width, height, false),
            Out::Surface(s) => {
                s.config.width = width;
                s.config.height = height;
                #[cfg(target_arch = "wasm32")]
                crate::canvas::set_kind_size(&s.kind, width, height);
                s.surface.configure(device, &s.config);
            }
            #[cfg(target_arch = "wasm32")]
            Out::Copy(ctx) => ctx.set_size(width, height),
        }
        slot.w = width;
        slot.h = height;
    }

    pub fn detach(&mut self, i: usize) {
        if let Some(s) = self.slots.get_mut(i) {
            *s = None;
        }
    }

    /// Attaches a canvas: a surface of its own on WebGPU, a 2D context to copy into on WebGL2.
    pub fn attach(&mut self, target: &Target) -> Result<usize> {
        #[cfg(target_arch = "wasm32")]
        {
            let (w, h): (u32, u32) = (target.width, target.height);
            let out: Out = if self.caps.backend == Backend::WebGl2 {
                if self.sheet.is_none() {
                    return Err(Error::BadInput(
                        "a WebGL2 renderer whose target is a canvas draws to that canvas only: \
                         make the renderer with Target::device_only to attach many"
                            .to_owned(),
                    ));
                }
                Out::Copy(
                    Ctx2d::new(&target.kind)?
                        .ok_or_else(|| Error::BadInput("not a canvas".to_owned()))?,
                )
            } else {
                let surface: wgpu::Surface<'static> = web_surface(&self.instance, &target.kind)?;
                let config: wgpu::SurfaceConfiguration = surface_config(
                    &surface,
                    &self.adapter,
                    (w, h),
                    self.caps.max_texture_dimension_2d,
                    self.transparent,
                )?
                .ok_or_else(|| {
                    Error::Surface(format!("the canvas surface cannot take {FORMAT:?}"))
                })?;
                surface.configure(&self.device, &config);
                crate::canvas::set_kind_size(&target.kind, w, h);
                Out::Surface(Surf {
                    surface,
                    config,
                    kind: target.kind.clone(),
                })
            };
            self.slots.push(Some(Slot { w, h, out }));
            Ok(self.slots.len() - 1)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = target;
            Err(Error::BadInput(format!(
                "attaching canvases is wasm32 only ({} attached)",
                self.slots.len()
            )))
        }
    }

    /// Makes the view uniforms (here and in every batch's globals) hold this target size.
    fn set_view(&mut self, w: u32, h: u32) {
        if self.view == (w, h) {
            return;
        }
        self.view = (w, h);
        let size: [f32; 4] = [w as f32, h as f32, 0.0, 0.0];
        self.queue
            .write_buffer(&self.view_buf, 0, bytemuck::cast_slice(&size));
        for b in &self.batches {
            self.queue
                .write_buffer(&b.globals, 0, bytemuck::cast_slice(&size));
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one linear layout, buffer and pipeline setup"
    )]
    pub async fn add_batch(&mut self, state: &BatchState) -> Result<()> {
        let scope: wgpu::ErrorScopeGuard =
            self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let dt: bool = self.data_texture;
        let wgsl: String = format!("{}{}", prelude(dt), state.wgsl);
        let module: wgpu::ShaderModule =
            self.device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(&state.name),
                    source: wgpu::ShaderSource::Wgsl(wgsl.into()),
                });
        let vis: wgpu::ShaderStages = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let data_entry: wgpu::BindGroupLayoutEntry = if dt {
            texture_entry(1, vis, wgpu::TextureSampleType::Uint)
        } else {
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: vis,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }
        };
        let bgl: wgpu::BindGroupLayout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: None,
                    entries: &[uniform_entry(0, vis), data_entry],
                });
        let globals: wgpu::Buffer =
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("globals"),
                    contents: bytemuck::cast_slice(&[
                        self.view.0 as f32,
                        self.view.1 as f32,
                        0.0,
                        0.0,
                        state.params[0],
                        state.params[1],
                        state.params[2],
                        state.params[3],
                    ]),
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                });
        let (data_buf, data_tex): (Option<wgpu::Buffer>, Option<wgpu::Texture>) = if dt {
            let rows: u32 = (state.data.len() as u32)
                .div_ceil(DATA_TEXTURE_WIDTH)
                .max(1);
            let tex: wgpu::Texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("batch data"),
                size: wgpu::Extent3d {
                    width: DATA_TEXTURE_WIDTH,
                    height: rows,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Uint,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            (None, Some(tex))
        } else {
            let buf: wgpu::Buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("batch data"),
                size: (state.data.len().max(1) * 4) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            (Some(buf), None)
        };
        let tex_view: Option<wgpu::TextureView> = data_tex
            .as_ref()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
        let data_res: wgpu::BindingResource = match (&data_buf, &tex_view) {
            (Some(b), _) => b.as_entire_binding(),
            (None, Some(v)) => wgpu::BindingResource::TextureView(v),
            (None, None) => return Err(Error::BadInput("batch data".into())),
        };
        let bind: wgpu::BindGroup = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: data_res,
                },
            ],
        });
        let layout: wgpu::PipelineLayout =
            self.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: None,
                    bind_group_layouts: &[Some(&bgl)],
                    immediate_size: 0,
                });
        let pipeline: wgpu::RenderPipeline =
            self.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(&state.name),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fs"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: FORMAT,
                            blend: Some(blend()),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
        shader_result(scope).await?;
        self.batches.push(GpuBatch {
            pipeline,
            bind,
            globals,
            data_buf,
            data_tex,
        });
        let idx: usize = self.batches.len() - 1;
        self.write_batch_data(idx, &state.data, 0, state.data.len());
        Ok(())
    }

    /// Uploads the changed words `data[offset..end]`; `data` is the whole mirror.
    pub fn write_batch_data(&self, idx: usize, data: &[u32], offset: usize, end: usize) {
        let Some(b) = self.batches.get(idx) else {
            return;
        };
        if let Some(buf) = &b.data_buf {
            self.queue.write_buffer(
                buf,
                (offset * 4) as u64,
                bytemuck::cast_slice(&data[offset..end]),
            );
        } else if let Some(tex) = &b.data_tex {
            let w: usize = DATA_TEXTURE_WIDTH as usize;
            if end <= offset {
                return;
            }
            // A texture upload is whole rows: those holding `offset..end`, re-sent from the
            // mirror, nothing past the last of them.
            let first: usize = offset / w;
            let rows: usize = end.div_ceil(w);
            let mut words: Vec<u32> = data[first * w..(rows * w).min(data.len())].to_vec();
            words.resize((rows - first) * w, 0);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: first as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&words),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(DATA_TEXTURE_WIDTH * 4),
                    rows_per_image: Some((rows - first) as u32),
                },
                wgpu::Extent3d {
                    width: DATA_TEXTURE_WIDTH,
                    height: (rows - first) as u32,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    pub fn write_batch_params(&self, idx: usize, params: [f32; 4]) {
        if let Some(b) = self.batches.get(idx) {
            self.queue
                .write_buffer(&b.globals, 16, bytemuck::cast_slice(&params));
        }
    }

    /// Uploads premultiplied RGBA8 as image `id`.
    pub fn set_image(&mut self, id: ImageId, w: u32, h: u32, premul: &[u8], sampling: Sampling) {
        let tex: wgpu::Texture = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some("image"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            premul,
        );
        self.put_image(id, tex, sampling, false);
    }

    fn put_image(&mut self, id: ImageId, tex: wgpu::Texture, sampling: Sampling, straight: bool) {
        let img: GpuImage =
            self.image_pipes
                .image(&self.device, &self.view_buf, tex, sampling, straight);
        if self.images.len() <= id.0 {
            self.images.resize_with(id.0 + 1, || None);
        }
        self.images[id.0] = Some(img);
    }

    pub fn remove_image(&mut self, id: ImageId) {
        if let Some(slot) = self.images.get_mut(id.0) {
            *slot = None;
        }
    }

    /// Runs the kernel into a texture and keeps it as image `id`: no readback.
    pub async fn kernel_image(
        &mut self,
        k: &Kernel,
        path: KernelPath,
        id: ImageId,
        sampling: Sampling,
    ) -> Result<()> {
        let tex: wgpu::Texture = self.dispatch_kernel(k, path).await?;
        self.put_image(id, tex, sampling, true);
        Ok(())
    }

    fn grow(device: &wgpu::Device, buf: &mut wgpu::Buffer, cap: &mut u64, need: u64, label: &str) {
        if need > *cap {
            *cap = need.next_power_of_two();
            *buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: *cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
    }

    fn main_pass(
        enc: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        clear: Option<Rgba>,
        size: (u32, u32),
    ) -> wgpu::RenderPass<'static> {
        let load: wgpu::LoadOp<wgpu::Color> = clear.map_or(wgpu::LoadOp::Load, |c| {
            // Premultiplied, as the CPU's pixmap holds it (the target is premultiplied everywhere).
            let a: f64 = f64::from(c[3]) / 255.0;
            wgpu::LoadOp::Clear(wgpu::Color {
                r: f64::from(c[0]) / 255.0 * a,
                g: f64::from(c[1]) / 255.0 * a,
                b: f64::from(c[2]) / 255.0 * a,
                a,
            })
        });
        let mut pass: wgpu::RenderPass<'static> = enc
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        // The frame fills the top-left of a view that may be larger (the WebGL2 sheet).
        pass.set_viewport(0.0, 0.0, size.0 as f32, size.1 as f32, 0.0, 1.0);
        pass
    }

    /// Draws the frame and reads it back (offscreen targets): premultiplied RGBA8.
    pub async fn render(
        &mut self,
        flat: &Flat,
        atlas: Option<&[u8]>,
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<Vec<u8>> {
        let slot: Slot = self.primary.take().ok_or_else(no_primary)?;
        let frame: Frame = Frame {
            flat,
            atlas,
            clear,
            batches,
        };
        let result: Result<Vec<u8>> = if let Out::Texture(t) = &slot.out {
            let view: wgpu::TextureView = t.create_view(&wgpu::TextureViewDescriptor::default());
            self.draw(&frame, &view, (slot.w, slot.h));
            self.read_texture(t, slot.w, slot.h).await
        } else {
            Err(Error::BadInput(
                "a canvas target has no readback".to_owned(),
            ))
        };
        self.primary = Some(slot);
        result
    }

    /// Draws the frame with no readback: into the canvas surface and presents it, or (offscreen)
    /// just runs it. `slot` is an attached canvas; `None` is the primary target.
    pub fn present(
        &mut self,
        slot: Option<usize>,
        flat: &Flat,
        atlas: Option<&[u8]>,
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<()> {
        let taken: Option<Slot> = match slot {
            None => self.primary.take(),
            Some(i) => self.slots.get_mut(i).and_then(Option::take),
        };
        let Some(taken) = taken else {
            return Err(if slot.is_some() {
                Error::UnknownSurface
            } else {
                no_primary()
            });
        };
        let frame: Frame = Frame {
            flat,
            atlas,
            clear,
            batches,
        };
        let result: Result<()> = self.present_slot(&taken, &frame);
        match slot {
            None => self.primary = Some(taken),
            Some(i) => self.slots[i] = Some(taken),
        }
        result
    }

    fn present_slot(&mut self, slot: &Slot, frame: &Frame) -> Result<()> {
        let size: (u32, u32) = (slot.w, slot.h);
        match &slot.out {
            Out::Texture(t) => {
                let view: wgpu::TextureView =
                    t.create_view(&wgpu::TextureViewDescriptor::default());
                self.draw(frame, &view, size);
                Ok(())
            }
            Out::Surface(s) => self
                .present_surface(&s.surface, &s.config, size, frame)
                .map(drop),
            #[cfg(target_arch = "wasm32")]
            Out::Copy(ctx) => self.present_copy(ctx, size, frame),
        }
    }

    /// Draws into the surface's current texture and presents it. `false` when there was no frame
    /// to show (hidden, or slow), which is not an error.
    fn present_surface(
        &mut self,
        surface: &wgpu::Surface<'static>,
        config: &wgpu::SurfaceConfiguration,
        size: (u32, u32),
        frame: &Frame,
    ) -> Result<bool> {
        // Lost or outdated: reconfigure and retry once, then give up.
        let mut last: &str = "";
        for attempt in 0..2 {
            let texture: wgpu::SurfaceTexture = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f)
                | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok(false);
                }
                other => {
                    last = match other {
                        wgpu::CurrentSurfaceTexture::Outdated => "outdated",
                        wgpu::CurrentSurfaceTexture::Lost => "lost",
                        _ => "validation error",
                    };
                    if attempt == 0 {
                        surface.configure(&self.device, config);
                    }
                    continue;
                }
            };
            let view: wgpu::TextureView = texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.draw(frame, &view, size);
            self.queue.present(texture);
            return Ok(true);
        }
        Err(Error::Surface(format!(
            "{last} after a reconfigure; recreate the renderer"
        )))
    }

    /// WebGL2: draws on the sheet (grown to fit), then copies the frame into the canvas.
    #[cfg(target_arch = "wasm32")]
    fn present_copy(&mut self, ctx: &Ctx2d, size: (u32, u32), frame: &Frame) -> Result<()> {
        let mut sheet: Sheet = self
            .sheet
            .take()
            .ok_or_else(|| Error::BadInput("no WebGL2 sheet".to_owned()))?;
        let shown: Result<bool> = match sheet.fit(
            &self.device,
            &self.adapter,
            size,
            self.caps.max_texture_dimension_2d,
            self.transparent,
        ) {
            Ok(config) => self.present_surface(&sheet.surface, &config, size, frame),
            Err(e) => Err(e),
        };
        let canvas: web_sys::OffscreenCanvas = sheet.canvas.clone();
        self.sheet = Some(sheet);
        if shown? {
            ctx.copy_from(&canvas, size.0, size.1, !self.transparent)?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one linear frame: uploads, then passes in paint order"
    )]
    fn draw(&mut self, frame: &Frame, view: &wgpu::TextureView, size: (u32, u32)) {
        let Frame {
            flat,
            atlas,
            clear,
            batches,
        } = *frame;
        let (width, height): (u32, u32) = size;
        self.set_view(width, height);
        if let Some(px) = atlas {
            self.queue.write_texture(
                self.atlas.as_image_copy(),
                px,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(ATLAS_SIZE),
                    rows_per_image: Some(ATLAS_SIZE),
                },
                wgpu::Extent3d {
                    width: ATLAS_SIZE,
                    height: ATLAS_SIZE,
                    depth_or_array_layers: 1,
                },
            );
        }
        // One extra record after the scene's: the quad that composites the path layer.
        let composite: u32 = flat.insts.len() as u32;
        let need: u64 = ((flat.insts.len() + 1) * size_of::<Inst>()) as u64;
        Self::grow(
            &self.device,
            &mut self.insts,
            &mut self.insts_cap,
            need,
            "instances",
        );
        if !flat.insts.is_empty() {
            self.queue
                .write_buffer(&self.insts, 0, bytemuck::cast_slice(&flat.insts));
        }
        let (pverts, pranges): (Vec<PVert>, Vec<gpu_path::PathRange>) =
            gpu_path::build(&flat.paths);
        if !pverts.is_empty() {
            Self::grow(
                &self.device,
                &mut self.path_buf,
                &mut self.path_cap,
                (pverts.len() * size_of::<PVert>()) as u64,
                "path vertices",
            );
            self.queue
                .write_buffer(&self.path_buf, 0, bytemuck::cast_slice(&pverts));
            if self.path_pipes.is_none() {
                self.path_pipes = Some(PathPipes::new(&self.device, &self.view_buf));
            }
            if self
                .layer
                .as_ref()
                .is_none_or(|l| l.0.size.0 < width || l.0.size.1 < height)
            {
                let (lw, lh): (u32, u32) = self.layer.as_ref().map_or((width, height), |l| {
                    (l.0.size.0.max(width), l.0.size.1.max(height))
                });
                let layer: Layer =
                    Layer::new(&self.device, lw, lh, self.caps.max_texture_dimension_2d);
                let comp: GpuImage = self.image_pipes.image(
                    &self.device,
                    &self.view_buf,
                    layer.texture.clone(),
                    Sampling::Linear,
                    false,
                );
                self.layer = Some((layer, comp));
            }
            if let Some((layer, _)) = &self.layer {
                // The quad that composites the path layer, after the scene's records.
                let quad: Inst = Inst {
                    a: [0.0, 0.0, width as f32, height as f32],
                    uv: layer.uv(size),
                    p: [0.0; 4],
                    color: [1.0; 4],
                };
                self.queue.write_buffer(
                    &self.insts,
                    u64::from(composite) * size_of::<Inst>() as u64,
                    bytemuck::bytes_of(&quad),
                );
            }
        }
        let mut enc: wgpu::CommandEncoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut pass: Option<wgpu::RenderPass<'static>> =
            Some(Self::main_pass(&mut enc, view, Some(clear), size));
        for run in &flat.runs {
            if let Run::Paths(r) = run {
                // The layer draws between two target passes, then lands as one quad.
                pass = None;
                if let (Some(pipes), Some((layer, comp))) = (&self.path_pipes, &self.layer) {
                    layer.draw(
                        &mut enc,
                        size,
                        pipes,
                        &self.path_buf,
                        &pranges[r.start as usize..r.end as usize],
                    );
                    let p = pass.insert(Self::main_pass(&mut enc, view, None, size));
                    p.set_pipeline(&self.image_pipes.pipes[0]);
                    p.set_bind_group(0, &comp.bind, &[]);
                    p.set_vertex_buffer(0, self.insts.slice(..));
                    p.draw(0..4, composite..composite + 1);
                }
                continue;
            }
            let p: &mut wgpu::RenderPass<'static> =
                pass.get_or_insert_with(|| Self::main_pass(&mut enc, view, None, size));
            match run {
                Run::Insts(r) => {
                    p.set_pipeline(&self.pipeline);
                    p.set_bind_group(0, &self.bind, &[]);
                    p.set_vertex_buffer(0, self.insts.slice(..));
                    p.draw(0..4, r.clone());
                }
                Run::Batch(id) => {
                    if let (Some(g), Some(s)) = (self.batches.get(id.0), batches.get(id.0)) {
                        p.set_pipeline(&g.pipeline);
                        p.set_bind_group(0, &g.bind, &[]);
                        p.draw(0..4, 0..s.instances);
                    }
                }
                Run::Image { id, insts } => {
                    if let Some(Some(img)) = self.images.get(id.0) {
                        p.set_pipeline(&self.image_pipes.pipes[usize::from(img.straight)]);
                        p.set_bind_group(0, &img.bind, &[]);
                        p.set_vertex_buffer(0, self.insts.slice(..));
                        p.draw(0..4, insts.clone());
                    }
                }
                Run::Paths(_) => {}
            }
        }
        drop(pass);
        self.queue.submit([enc.finish()]);
    }

    async fn read_texture(&self, tex: &wgpu::Texture, w: u32, h: u32) -> Result<Vec<u8>> {
        let row: u32 = (w * 4).div_ceil(256) * 256;
        let out: wgpu::Buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(row) * u64::from(h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc: wgpu::CommandEncoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &out,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([enc.finish()]);
        let (tx, rx) = futures_channel::oneshot::channel();
        out.slice(..)
            .map_async(wgpu::MapMode::Read, move |r| drop(tx.send(r)));
        #[cfg(not(target_arch = "wasm32"))]
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| Error::Readback(e.to_string()))?;
        // WebGPU fires the callback from the JS event loop. WebGL2 has no event for it: wgpu only
        // runs the callback from `poll`, once the GL fence signals, so yield to the loop between polls.
        #[cfg(target_arch = "wasm32")]
        let mut rx = rx;
        #[cfg(target_arch = "wasm32")]
        let mapped = if self.caps.backend == Backend::WebGl2 {
            loop {
                drop(self.device.poll(wgpu::PollType::Poll));
                if let Some(r) = rx.try_recv().map_err(|e| Error::Readback(e.to_string()))? {
                    break r;
                }
                yield_to_event_loop().await;
            }
        } else {
            rx.await.map_err(|e| Error::Readback(e.to_string()))?
        };
        #[cfg(not(target_arch = "wasm32"))]
        let mapped = rx.await.map_err(|e| Error::Readback(e.to_string()))?;
        mapped.map_err(|e| Error::Readback(e.to_string()))?;
        let data = out
            .slice(..)
            .get_mapped_range()
            .map_err(|e| Error::Readback(e.to_string()))?;
        let mut px: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
        for r in 0..h as usize {
            px.extend_from_slice(&data[r * row as usize..r * row as usize + (w * 4) as usize]);
        }
        drop(data);
        out.unmap();
        Ok(px)
    }

    pub async fn run_kernel(&self, k: &Kernel, path: KernelPath) -> Result<Vec<u8>> {
        let tex: wgpu::Texture = self.dispatch_kernel(k, path).await?;
        self.read_texture(&tex, k.width, k.height).await
    }

    /// Runs the kernel and returns its output texture, still on the GPU.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear bind-group and pipeline setup for two forms"
    )]
    async fn dispatch_kernel(&self, k: &Kernel, path: KernelPath) -> Result<wgpu::Texture> {
        let scope: wgpu::ErrorScopeGuard =
            self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let compute: bool = path == KernelPath::Compute;
        let stage: wgpu::ShaderStages = if compute {
            wgpu::ShaderStages::COMPUTE
        } else {
            wgpu::ShaderStages::FRAGMENT
        };
        let src: &str = if compute {
            k.compute.as_deref()
        } else {
            k.fragment.as_deref()
        }
        .ok_or_else(|| Error::KernelUnavailable {
            kernel: k.name.clone(),
        })?;
        let wgsl: String = if compute {
            src.to_owned()
        } else {
            format!("{src}{FULLSCREEN_VS}")
        };
        let module: wgpu::ShaderModule =
            self.device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(&k.name),
                    source: wgpu::ShaderSource::Wgsl(wgsl.into()),
                });
        let mut entries: Vec<wgpu::BindGroupLayoutEntry> = Vec::new();
        let mut views: Vec<Option<wgpu::TextureView>> = Vec::new();
        let mut bufs: Vec<Option<wgpu::Buffer>> = Vec::new();
        for (i, b) in k.inputs.iter().enumerate() {
            let i: u32 = i as u32;
            match b {
                Binding::Texture {
                    width,
                    height,
                    rgba8,
                } => {
                    if rgba8.len() != (*width as usize) * (*height as usize) * 4 {
                        return Err(Error::BadInput(format!(
                            "kernel `{}` texture input {i} has the wrong byte length",
                            k.name
                        )));
                    }
                    entries.push(texture_entry(
                        i,
                        stage,
                        wgpu::TextureSampleType::Float { filterable: false },
                    ));
                    let t: wgpu::Texture =
                        rgba_texture(&self.device, &self.queue, *width, *height, rgba8);
                    views.push(Some(t.create_view(&wgpu::TextureViewDescriptor::default())));
                    bufs.push(None);
                }
                Binding::Storage(bytes) | Binding::Uniform(bytes) => {
                    let as_storage: bool = compute && matches!(b, Binding::Storage(_));
                    entries.push(wgpu::BindGroupLayoutEntry {
                        binding: i,
                        visibility: stage,
                        ty: wgpu::BindingType::Buffer {
                            ty: if as_storage {
                                wgpu::BufferBindingType::Storage { read_only: true }
                            } else {
                                wgpu::BufferBindingType::Uniform
                            },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    });
                    bufs.push(Some(self.device.create_buffer_init(
                        &wgpu::util::BufferInitDescriptor {
                            label: None,
                            contents: &padded(bytes),
                            usage: if as_storage {
                                wgpu::BufferUsages::STORAGE
                            } else {
                                wgpu::BufferUsages::UNIFORM
                            },
                        },
                    )));
                    views.push(None);
                }
            }
        }
        let out_tex: wgpu::Texture = target_texture(&self.device, k.width, k.height, compute);
        let out_view: wgpu::TextureView =
            out_tex.create_view(&wgpu::TextureViewDescriptor::default());
        if compute {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: k.inputs.len() as u32,
                visibility: stage,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: FORMAT,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            });
        }
        let bgl: wgpu::BindGroupLayout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: None,
                    entries: &entries,
                });
        let mut bind_entries: Vec<wgpu::BindGroupEntry> = Vec::new();
        for i in 0..k.inputs.len() {
            let resource: wgpu::BindingResource = match (&bufs[i], &views[i]) {
                (Some(b), _) => b.as_entire_binding(),
                (None, Some(v)) => wgpu::BindingResource::TextureView(v),
                (None, None) => return Err(Error::BadInput("binding".into())),
            };
            bind_entries.push(wgpu::BindGroupEntry {
                binding: i as u32,
                resource,
            });
        }
        if compute {
            bind_entries.push(wgpu::BindGroupEntry {
                binding: k.inputs.len() as u32,
                resource: wgpu::BindingResource::TextureView(&out_view),
            });
        }
        let bind: wgpu::BindGroup = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &bind_entries,
        });
        let layout: wgpu::PipelineLayout =
            self.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: None,
                    bind_group_layouts: &[Some(&bgl)],
                    immediate_size: 0,
                });
        let mut enc: wgpu::CommandEncoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        if compute {
            let pipeline: wgpu::ComputePipeline =
                self.device
                    .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: Some(&k.name),
                        layout: Some(&layout),
                        module: &module,
                        entry_point: Some("main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        cache: None,
                    });
            let mut pass: wgpu::ComputePass =
                enc.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(
                k.width.div_ceil(k.workgroup[0]),
                k.height.div_ceil(k.workgroup[1]),
                1,
            );
        } else {
            let pipeline: wgpu::RenderPipeline =
                self.device
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some(&k.name),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &module,
                            entry_point: Some("vs_full"),
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                            buffers: &[],
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &module,
                            entry_point: Some("fs"),
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                            targets: &[Some(wgpu::ColorTargetState {
                                format: FORMAT,
                                blend: None,
                                write_mask: wgpu::ColorWrites::ALL,
                            })],
                        }),
                        primitive: wgpu::PrimitiveState::default(),
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState::default(),
                        multiview_mask: None,
                        cache: None,
                    });
            let mut pass: wgpu::RenderPass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
        shader_result(scope).await?;
        Ok(out_tex)
    }
}

/// Ends a validation error scope: a captured error (bad WGSL, a pipeline that does not link)
/// becomes `Error::Shader` with wgpu's message.
async fn shader_result(scope: wgpu::ErrorScopeGuard) -> Result<()> {
    match scope.pop().await {
        None => Ok(()),
        Some(e) => Err(Error::Shader(e.to_string())),
    }
}

/// A throwaway 1x1 `OffscreenCanvas` and its wgpu surface: what a WebGL2 adapter is requested
/// against when no real canvas may be touched yet (it becomes the sheet of a device-only renderer).
#[cfg(target_arch = "wasm32")]
fn throwaway_surface(
    instance: &wgpu::Instance,
) -> Result<(wgpu::Surface<'static>, web_sys::OffscreenCanvas)> {
    let canvas: web_sys::OffscreenCanvas =
        web_sys::OffscreenCanvas::new(1, 1).map_err(|e| Error::Surface(format!("{e:?}")))?;
    let surface: wgpu::Surface<'static> = instance
        .create_surface(wgpu::SurfaceTarget::OffscreenCanvas(canvas.clone()))
        .map_err(|e| Error::Surface(e.to_string()))?;
    Ok((surface, canvas))
}

/// The wgpu surface for a canvas target (an image target gets a throwaway one).
#[cfg(target_arch = "wasm32")]
fn web_surface(instance: &wgpu::Instance, kind: &Kind) -> Result<wgpu::Surface<'static>> {
    let target: wgpu::SurfaceTarget<'static> = match kind {
        Kind::Image | Kind::None => return Ok(throwaway_surface(instance)?.0),
        Kind::Html(c) => wgpu::SurfaceTarget::Canvas(c.clone()),
        Kind::Offscreen(c) => wgpu::SurfaceTarget::OffscreenCanvas(c.clone()),
    };
    instance
        .create_surface(target)
        .map_err(|e| Error::Surface(e.to_string()))
}

/// Whether the surface can take our one render format.
#[cfg(target_arch = "wasm32")]
fn surface_takes_format(surface: &wgpu::Surface<'static>, adapter: &wgpu::Adapter) -> bool {
    surface.get_capabilities(adapter).formats.contains(&FORMAT)
}

/// The surface configuration: our one render format. Transparent: premultiplied alpha, else
/// inherit, else whatever the surface offers (WebGL2 offers only `Opaque` and still keeps the
/// canvas's alpha); opaque: `Opaque`. `None` if the surface cannot take the format.
fn surface_config(
    surface: &wgpu::Surface<'static>,
    adapter: &wgpu::Adapter,
    size: (u32, u32),
    max: u32,
    transparent: bool,
) -> Result<Option<wgpu::SurfaceConfiguration>> {
    let (width, height): (u32, u32) = size;
    crate::renderer::check_size(width, height, max)?;
    let caps: wgpu::SurfaceCapabilities = surface.get_capabilities(adapter);
    if !caps.formats.contains(&FORMAT) {
        return Ok(None);
    }
    let wanted: &[wgpu::CompositeAlphaMode] = if transparent {
        &[
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ]
    } else {
        &[wgpu::CompositeAlphaMode::Opaque]
    };
    let alpha: wgpu::CompositeAlphaMode = wanted
        .iter()
        .copied()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or_else(|| caps.alpha_modes.first().copied().unwrap_or_default());
    Ok(Some(wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: FORMAT,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: alpha,
        view_formats: vec![],
    }))
}

#[cfg(target_arch = "wasm32")]
impl Sheet {
    /// Configures the sheet at least `size` big (it only grows) and returns its configuration.
    /// The sheet is as wide as the widest frame drawn and as tall as the tallest, so one wide frame
    /// and one tall frame make it a near-maximum square (up to the device's texture limit squared).
    fn fit(
        &mut self,
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        size: (u32, u32),
        max: u32,
        transparent: bool,
    ) -> Result<wgpu::SurfaceConfiguration> {
        let (w, h): (u32, u32) = self
            .config
            .as_ref()
            .map_or(size, |c| (c.width.max(size.0), c.height.max(size.1)));
        if let Some(c) = &self.config
            && (c.width, c.height) == (w, h)
        {
            return Ok(c.clone());
        }
        let config: wgpu::SurfaceConfiguration =
            surface_config(&self.surface, adapter, (w, h), max, transparent)?.ok_or_else(|| {
                Error::Surface(format!("the sheet surface cannot take {FORMAT:?}"))
            })?;
        self.canvas.set_width(w);
        self.canvas.set_height(h);
        self.surface.configure(device, &config);
        self.config = Some(config.clone());
        Ok(config)
    }
}

/// Yields to the JS event loop with `setTimeout(0)`, in a window or a worker.
#[cfg(target_arch = "wasm32")]
async fn yield_to_event_loop() {
    use wasm_bindgen::JsCast as _;
    let promise: js_sys::Promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let g: wasm_bindgen::JsValue = js_sys::global().into();
        if let Some(w) = g.dyn_ref::<web_sys::Window>() {
            drop(w.set_timeout_with_callback(&resolve));
        } else if let Some(w) = g.dyn_ref::<web_sys::WorkerGlobalScope>() {
            drop(w.set_timeout_with_callback(&resolve));
        } else {
            drop(resolve.call0(&wasm_bindgen::JsValue::UNDEFINED));
        }
    });
    drop(wasm_bindgen_futures::JsFuture::from(promise).await);
}
