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
use crate::scene::{ImageId, Rgba, Sampling};
use wgpu::util::DeviceExt;

/// The attributes of one [`Inst`], shared by the shape and image pipelines.
pub const INST_ATTRS: [wgpu::VertexAttribute; 4] =
    wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4];

pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

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
    target: wgpu::Texture,
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

impl Gpu {
    /// `None` when this backend has no usable (non-CPU) adapter here.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear device and pipeline setup"
    )]
    pub async fn create(
        backend: Backend,
        width: u32,
        height: u32,
        data_texture: Option<bool>,
    ) -> Result<Option<Self>> {
        let Some(backends) = backend.wgpu_backends() else {
            return Ok(None);
        };
        let mut desc: wgpu::InstanceDescriptor =
            wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = backends;
        let instance: wgpu::Instance = wgpu::Instance::new(desc);
        // wgpu's web GL backend only finds an adapter for a given surface, so probe with a 1x1 one.
        #[cfg(target_arch = "wasm32")]
        let probe: Option<wgpu::Surface<'static>> = if backend == Backend::WebGl2 {
            let canvas: web_sys::OffscreenCanvas = web_sys::OffscreenCanvas::new(1, 1)
                .map_err(|e| Error::Device(format!("{e:?}")))?;
            Some(
                instance
                    .create_surface(wgpu::SurfaceTarget::OffscreenCanvas(canvas))
                    .map_err(|e| Error::Device(e.to_string()))?,
            )
        } else {
            None
        };
        #[cfg(not(target_arch = "wasm32"))]
        let probe: Option<wgpu::Surface<'static>> = None;
        let Ok(adapter) = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: probe.as_ref(),
                ..Default::default()
            })
            .await
        else {
            return Ok(None);
        };
        let info: wgpu::AdapterInfo = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            return Ok(None);
        }
        let (device, queue): (wgpu::Device, wgpu::Queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| Error::Device(e.to_string()))?;
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
        let target: wgpu::Texture = target_texture(&device, width, height, false);
        Ok(Some(Self {
            device,
            queue,
            data_texture: data_texture.unwrap_or(!caps.storage_buffers),
            caps,
            width,
            height,
            target,
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
        }))
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.target = target_texture(&self.device, width, height, false);
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
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one linear layout, buffer and pipeline setup"
    )]
    pub fn add_batch(&mut self, state: &BatchState) -> Result<()> {
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
        self.batches.push(GpuBatch {
            pipeline,
            bind,
            globals,
            data_buf,
            data_tex,
        });
        let idx: usize = self.batches.len() - 1;
        self.write_batch_data(idx, &state.data, 0);
        Ok(())
    }

    /// Uploads `state_data[offset..]`'s changed words; `data` is the whole mirror, `offset` the first changed word.
    pub fn write_batch_data(&self, idx: usize, data: &[u32], offset: usize) {
        let Some(b) = self.batches.get(idx) else {
            return;
        };
        if let Some(buf) = &b.data_buf {
            self.queue.write_buffer(
                buf,
                (offset * 4) as u64,
                bytemuck::cast_slice(&data[offset..]),
            );
        } else if let Some(tex) = &b.data_tex {
            let w: usize = DATA_TEXTURE_WIDTH as usize;
            let first: usize = offset / w;
            let rows: usize = data.len().div_ceil(w);
            let mut words: Vec<u32> = data[first * w..].to_vec();
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
    pub fn kernel_image(
        &mut self,
        k: &Kernel,
        path: KernelPath,
        id: ImageId,
        sampling: Sampling,
    ) -> Result<()> {
        let tex: wgpu::Texture = self.dispatch_kernel(k, path)?;
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

    #[expect(
        clippy::too_many_lines,
        reason = "one linear frame: uploads, then passes in paint order"
    )]
    pub async fn render(
        &mut self,
        flat: &Flat,
        atlas: Option<&[u8]>,
        clear: Rgba,
        batches: &[BatchState],
    ) -> Result<Vec<u8>> {
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
        let view: wgpu::TextureView = self
            .target
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc: wgpu::CommandEncoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let mut pass: Option<wgpu::RenderPass<'static>> =
            Some(Self::main_pass(&mut enc, &view, Some(clear)));
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
                    let p = pass.insert(Self::main_pass(&mut enc, &view, None));
                    p.set_pipeline(&self.image_pipes.pipes[0]);
                    p.set_bind_group(0, &comp.bind, &[]);
                    p.set_vertex_buffer(0, self.insts.slice(..));
                    p.draw(0..4, composite..composite + 1);
                }
                continue;
            }
            let p: &mut wgpu::RenderPass<'static> =
                pass.get_or_insert_with(|| Self::main_pass(&mut enc, &view, None));
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
        self.read_texture(&self.target, self.width, self.height)
            .await
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
        let tex: wgpu::Texture = self.dispatch_kernel(k, path)?;
        self.read_texture(&tex, k.width, k.height).await
    }

    /// Runs the kernel and returns its output texture, still on the GPU.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear bind-group and pipeline setup for two forms"
    )]
    fn dispatch_kernel(&self, k: &Kernel, path: KernelPath) -> Result<wgpu::Texture> {
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
        Ok(out_tex)
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = "export function tick() { return new Promise((r) => setTimeout(r, 0)); }")]
extern "C" {
    fn tick() -> js_sys::Promise;
}

#[cfg(target_arch = "wasm32")]
async fn yield_to_event_loop() {
    drop(wasm_bindgen_futures::JsFuture::from(tick()).await);
}
