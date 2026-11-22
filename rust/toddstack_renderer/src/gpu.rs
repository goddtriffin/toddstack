//! The wgpu backend: one instanced draw for shapes and glyphs, custom batches, compute kernels.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "sizes and counts are small values"
)]

use crate::backend::{Backend, Capabilities};
use crate::batch::{BatchState, DATA_TEXTURE_WIDTH, prelude};
use crate::error::{Error, Result};
use crate::glyphs::{ATLAS_SIZE, Flat, Inst, Run};
use crate::gpu_image::{GpuImage, ImagePipes};
use crate::gpu_path::{self, Layer, PVert, PathPipes};
use crate::kernel::{Binding, FULLSCREEN_VS, Kernel, KernelPath};
#[cfg(target_arch = "wasm32")]
use crate::renderer::Kind;
use crate::renderer::Target;
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
    pub caps: Capabilities,
    data_texture: bool,
    width: u32,
    height: u32,
    /// The offscreen image; `None` for a canvas, which draws into the surface's texture.
    target: Option<wgpu::Texture>,
    surface: Option<Surf>,
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    view_buf: wgpu::Buffer,
    atlas: wgpu::Texture,
    insts: wgpu::Buffer,
    insts_cap: u64,
    batches: Vec<GpuBatch>,
    image_pipes: ImagePipes,
    images: Vec<Option<GpuImage>>,
    /// Made on the first path; its composite bind group; dropped on resize.
    path_pipes: Option<PathPipes>,
    layer: Option<(Layer, GpuImage)>,
    path_buf: wgpu::Buffer,
    path_cap: u64,
}

fn blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
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
}

struct Acquired {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// An adapter and device, or `None` if the backend has none worth using: no adapter, a software
/// one, or a failed device request.
async fn acquire(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
) -> Option<Acquired> {
    let adapter: wgpu::Adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: surface,
            ..Default::default()
        })
        .await
        .ok()?;
    if adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return None;
    }
    let (device, queue): (wgpu::Device, wgpu::Queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_limits: adapter.limits(),
            ..Default::default()
        })
        .await
        .ok()?;
    Some(Acquired {
        adapter,
        device,
        queue,
    })
}

impl Gpu {
    /// Decides whether `backend` can be used, without touching a canvas target's real canvas (it
    /// keeps one context type for life, so a GPU context taken and then rejected would leave the
    /// CPU fallback with none). A WebGL2 canvas target is probed on a 1x1 `OffscreenCanvas`.
    /// `None` when the backend has no usable (non-CPU) adapter here.
    pub async fn probe(backend: Backend, target: &Target) -> Result<Option<Probe>> {
        let Some(backends) = backend.wgpu_backends() else {
            return Ok(None);
        };
        let mut desc: wgpu::InstanceDescriptor =
            wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = backends;
        let instance: wgpu::Instance = wgpu::Instance::new(desc);
        // wgpu's web GL backend only finds an adapter for a given surface: an offscreen target
        // brings its own; a canvas target probes with a 1x1 one. WebGPU needs no surface.
        #[cfg(target_arch = "wasm32")]
        let surface: Option<wgpu::Surface<'static>> = if backend == Backend::WebGl2 {
            let Ok(probe_surface) = web_surface(&instance, &target.kind, target.is_canvas()) else {
                return Ok(None);
            };
            Some(probe_surface)
        } else {
            None
        };
        #[cfg(not(target_arch = "wasm32"))]
        let surface: Option<wgpu::Surface<'static>> = None;
        let Some(acquired) = acquire(&instance, surface.as_ref()).await else {
            return Ok(None);
        };
        #[cfg(target_arch = "wasm32")]
        if let Some(probe_surface) = &surface
            && !surface_takes_format(probe_surface, &acquired.adapter)
        {
            return Ok(None);
        }
        let keep: bool =
            !(cfg!(target_arch = "wasm32") && backend == Backend::WebGl2 && target.is_canvas());
        Ok(Some(Probe {
            backend,
            instance,
            acquired: keep.then_some(acquired),
        }))
    }

    /// Builds the GPU renderer on an accepted probe, creating the surface on the real canvas.
    ///
    /// # Errors
    /// `Surface` or `Device` if the real canvas still fails after acceptance: the one case the
    /// fallback chain cannot recover from.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear device and pipeline setup"
    )]
    pub async fn create(probe: Probe, target: &Target, data_texture: Option<bool>) -> Result<Self> {
        let (width, height): (u32, u32) = (target.width, target.height);
        let Probe {
            backend,
            instance,
            acquired,
        } = probe;
        #[cfg(target_arch = "wasm32")]
        let mut surface: Option<wgpu::Surface<'static>> = None;
        #[cfg(not(target_arch = "wasm32"))]
        let surface: Option<wgpu::Surface<'static>> = None;
        let acquired: Acquired = if let Some(a) = acquired {
            a
        } else {
            #[cfg(target_arch = "wasm32")]
            let real: wgpu::Surface<'static> = web_surface(&instance, &target.kind, false)?;
            #[cfg(target_arch = "wasm32")]
            let a: Option<Acquired> = acquire(&instance, Some(&real)).await;
            #[cfg(not(target_arch = "wasm32"))]
            let a: Option<Acquired> = acquire(&instance, None).await;
            #[cfg(target_arch = "wasm32")]
            {
                surface = Some(real);
            }
            a.ok_or_else(|| Error::Device("no usable adapter or device".to_owned()))?
        };
        let Acquired {
            adapter,
            device,
            queue,
        } = acquired;
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
        crate::renderer::check_size(width, height, caps.max_texture_dimension_2d)?;
        #[cfg(target_arch = "wasm32")]
        if surface.is_none() && target.is_canvas() {
            surface = Some(web_surface(&instance, &target.kind, false)?);
        }
        let surf: Option<Surf> = match surface {
            Some(surface) if target.is_canvas() => {
                let Some(config) = surface_config(
                    &surface,
                    &adapter,
                    width,
                    height,
                    caps.max_texture_dimension_2d,
                )?
                else {
                    return Err(Error::Surface(format!(
                        "the canvas surface cannot take {FORMAT:?}"
                    )));
                };
                surface.configure(&device, &config);
                #[cfg(target_arch = "wasm32")]
                set_canvas_size(&target.kind, width, height);
                Some(Surf {
                    surface,
                    config,
                    #[cfg(target_arch = "wasm32")]
                    kind: target.kind.clone(),
                })
            }
            // The WebGL2 probe for an offscreen target has done its job.
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
        let target: Option<wgpu::Texture> = if surf.is_some() {
            None
        } else {
            Some(target_texture(&device, width, height, false))
        };
        Ok(Self {
            device,
            queue,
            data_texture: data_texture.unwrap_or(!caps.storage_buffers),
            caps,
            width,
            height,
            target,
            surface: surf,
            pipeline,
            bind,
            view_buf,
            atlas,
            insts,
            insts_cap,
            batches: Vec::new(),
            image_pipes,
            images: Vec::new(),
            path_pipes: None,
            layer: None,
            path_buf,
            path_cap,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        crate::renderer::check_size(width, height, self.caps.max_texture_dimension_2d)?;
        if let Some(s) = &mut self.surface {
            s.config.width = width;
            s.config.height = height;
            #[cfg(target_arch = "wasm32")]
            set_canvas_size(&s.kind, width, height);
            s.surface.configure(&self.device, &s.config);
        } else {
            self.target = Some(target_texture(&self.device, width, height, false));
        }
        self.width = width;
        self.height = height;
        self.layer = None;
        self.queue.write_buffer(
            &self.view_buf,
            0,
            bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]),
        );
        for b in &self.batches {
            self.queue.write_buffer(
                &b.globals,
                0,
                bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]),
            );
        }
        Ok(())
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
                        self.width as f32,
                        self.height as f32,
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
    ) -> wgpu::RenderPass<'static> {
        let load: wgpu::LoadOp<wgpu::Color> = clear.map_or(wgpu::LoadOp::Load, |c| {
            wgpu::LoadOp::Clear(wgpu::Color {
                r: f64::from(c[0]) / 255.0,
                g: f64::from(c[1]) / 255.0,
                b: f64::from(c[2]) / 255.0,
                a: f64::from(c[3]) / 255.0,
            })
        });
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
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
        .forget_lifetime()
    }

    /// Draws the frame and reads it back (offscreen targets).
    pub async fn render(
        &mut self,
        flat: &Flat,
        atlas: Option<&[u8]>,
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<Vec<u8>> {
        let view: wgpu::TextureView = self
            .target
            .as_ref()
            .ok_or_else(|| Error::BadInput("a canvas target has no readback".to_owned()))?
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.draw(flat, atlas, clear, batches, &view);
        let target: &wgpu::Texture = self
            .target
            .as_ref()
            .ok_or_else(|| Error::BadInput("a canvas target has no readback".to_owned()))?;
        self.read_texture(target, self.width, self.height).await
    }

    /// Draws the frame with no readback: into the canvas surface and presents it, or (offscreen)
    /// just runs it.
    pub fn present(
        &mut self,
        flat: &Flat,
        atlas: Option<&[u8]>,
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<()> {
        if self.surface.is_none() {
            let view: wgpu::TextureView = self
                .target
                .as_ref()
                .ok_or_else(|| Error::Surface("no target".to_owned()))?
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.draw(flat, atlas, clear, batches, &view);
            return Ok(());
        }
        // Lost or outdated: reconfigure and retry once, then give up.
        let mut last: &str = "";
        for attempt in 0..2 {
            let Some(s) = &self.surface else { break };
            let frame: wgpu::SurfaceTexture = match s.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f)
                | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                // Nothing to show this frame (hidden, or slow): not an error.
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok(());
                }
                other => {
                    last = match other {
                        wgpu::CurrentSurfaceTexture::Outdated => "outdated",
                        wgpu::CurrentSurfaceTexture::Lost => "lost",
                        _ => "validation error",
                    };
                    if attempt == 0 {
                        s.surface.configure(&self.device, &s.config);
                    }
                    continue;
                }
            };
            let view: wgpu::TextureView = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.draw(flat, atlas, clear, batches, &view);
            self.queue.present(frame);
            return Ok(());
        }
        Err(Error::Surface(format!(
            "{last} after a reconfigure; recreate the renderer"
        )))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one linear frame: uploads, then passes in paint order"
    )]
    fn draw(
        &mut self,
        flat: &Flat,
        atlas: Option<&[u8]>,
        clear: Rgba,
        batches: &[BatchState],
        view: &wgpu::TextureView,
    ) {
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
        let quad: Inst = Inst {
            a: [0.0, 0.0, self.width as f32, self.height as f32],
            uv: [0.0; 4],
            p: [1.0, 0.0, 0.0, 0.0],
            color: [1.0; 4],
        };
        self.queue.write_buffer(
            &self.insts,
            u64::from(composite) * size_of::<Inst>() as u64,
            bytemuck::bytes_of(&quad),
        );
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
            if self.layer.is_none() {
                let layer: Layer = Layer::new(
                    &self.device,
                    self.width,
                    self.height,
                    self.caps.max_texture_dimension_2d,
                );
                let comp: GpuImage = self.image_pipes.image(
                    &self.device,
                    &self.view_buf,
                    layer.texture.clone(),
                    Sampling::Linear,
                    false,
                );
                self.layer = Some((layer, comp));
            }
        }
        let mut enc: wgpu::CommandEncoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut pass: Option<wgpu::RenderPass<'static>> =
            Some(Self::main_pass(&mut enc, view, Some(clear)));
        for run in &flat.runs {
            if let Run::Paths(r) = run {
                // The layer draws between two target passes, then lands as one quad.
                pass = None;
                if let (Some(pipes), Some((layer, comp))) = (&self.path_pipes, &self.layer) {
                    layer.draw(
                        &mut enc,
                        pipes,
                        &self.path_buf,
                        &pranges[r.start as usize..r.end as usize],
                    );
                    let p = pass.insert(Self::main_pass(&mut enc, view, None));
                    p.set_pipeline(&self.image_pipes.pipes[0]);
                    p.set_bind_group(0, &comp.bind, &[]);
                    p.set_vertex_buffer(0, self.insts.slice(..));
                    p.draw(0..4, composite..composite + 1);
                }
                continue;
            }
            let p: &mut wgpu::RenderPass<'static> =
                pass.get_or_insert_with(|| Self::main_pass(&mut enc, view, None));
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
                Run::Image { id, inst } => {
                    if let Some(Some(img)) = self.images.get(id.0) {
                        p.set_pipeline(&self.image_pipes.pipes[usize::from(img.straight)]);
                        p.set_bind_group(0, &img.bind, &[]);
                        p.set_vertex_buffer(0, self.insts.slice(..));
                        p.draw(0..4, *inst..*inst + 1);
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

/// The wgpu surface for a canvas target; with `probe`, a throwaway 1x1 canvas instead (an image
/// target always gets one).
#[cfg(target_arch = "wasm32")]
fn web_surface(
    instance: &wgpu::Instance,
    kind: &Kind,
    probe: bool,
) -> Result<wgpu::Surface<'static>> {
    let throwaway = || {
        web_sys::OffscreenCanvas::new(1, 1)
            .map(wgpu::SurfaceTarget::OffscreenCanvas)
            .map_err(|e| Error::Surface(format!("{e:?}")))
    };
    let target: wgpu::SurfaceTarget<'static> = match kind {
        Kind::Image => throwaway()?,
        _ if probe => throwaway()?,
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

#[cfg(target_arch = "wasm32")]
fn set_canvas_size(kind: &Kind, width: u32, height: u32) {
    match kind {
        Kind::Html(c) => {
            c.set_width(width);
            c.set_height(height);
        }
        Kind::Offscreen(c) => {
            c.set_width(width);
            c.set_height(height);
        }
        Kind::Image => {}
    }
}

/// The surface configuration: our one render format, opaque alpha. `None` if the surface cannot
/// take that format.
fn surface_config(
    surface: &wgpu::Surface<'static>,
    adapter: &wgpu::Adapter,
    width: u32,
    height: u32,
    max: u32,
) -> Result<Option<wgpu::SurfaceConfiguration>> {
    crate::renderer::check_size(width, height, max)?;
    let caps: wgpu::SurfaceCapabilities = surface.get_capabilities(adapter);
    if !caps.formats.contains(&FORMAT) {
        return Ok(None);
    }
    let alpha: wgpu::CompositeAlphaMode =
        if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes.first().copied().unwrap_or_default()
        };
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
