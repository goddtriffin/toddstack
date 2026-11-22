//! Image quads on the GPU: one pipeline per colour convention, one sampler per `Sampling`.

use crate::glyphs::Inst;
use crate::gpu::{FORMAT, INST_ATTRS, texture_entry, uniform_entry};
use crate::scene::Sampling;

/// An image on the GPU. Uploaded images are premultiplied; a kernel's output texture is straight.
pub struct GpuImage {
    pub bind: wgpu::BindGroup,
    pub straight: bool,
    _texture: wgpu::Texture,
}

pub struct ImagePipes {
    bgl: wgpu::BindGroupLayout,
    /// Premultiplied, then straight colour.
    pub pipes: [wgpu::RenderPipeline; 2],
    /// Nearest, then linear.
    samplers: [wgpu::Sampler; 2],
}

/// Premultiplied source over the target; alpha accumulates as it should.
pub fn premul_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

impl ImagePipes {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader: wgpu::ShaderModule =
            device.create_shader_module(wgpu::include_wgsl!("image.wgsl"));
        let bgl: wgpu::BindGroupLayout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("image"),
                entries: &[
                    uniform_entry(0, wgpu::ShaderStages::VERTEX),
                    texture_entry(
                        1,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::TextureSampleType::Float { filterable: true },
                    ),
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let layout: wgpu::PipelineLayout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
        let make = |fs: &'static str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
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
                    entry_point: Some(fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(premul_blend()),
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
            })
        };
        let sampler = |f: wgpu::FilterMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: f,
                min_filter: f,
                ..Default::default()
            })
        };
        Self {
            pipes: [make("fs_premul"), make("fs_straight")],
            samplers: [
                sampler(wgpu::FilterMode::Nearest),
                sampler(wgpu::FilterMode::Linear),
            ],
            bgl,
        }
    }

    /// Binds `texture` for drawing with the target-size uniform `view_buf`.
    pub fn image(
        &self,
        device: &wgpu::Device,
        view_buf: &wgpu::Buffer,
        texture: wgpu::Texture,
        sampling: Sampling,
        straight: bool,
    ) -> GpuImage {
        let view: wgpu::TextureView = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler: &wgpu::Sampler = &self.samplers[usize::from(sampling == Sampling::Linear)];
        let bind: wgpu::BindGroup = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: view_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        GpuImage {
            bind,
            straight,
            _texture: texture,
        }
    }
}
