//! Filled paths on the GPU: the stencil-then-cover fill into a 4x multisampled transparent layer,
//! resolved and composited over the target as one premultiplied quad.
//!
//! Stencil: each subpath's fan of triangles increments the stencil where it is wound one way and
//! decrements it the other way, so non-zero means inside. Cover: a box over the path, drawn where
//! the stencil is non-zero, which also clears it for the next path.

use crate::glyphs::PathDraw;
use crate::gpu::FORMAT;
use crate::gpu_image::premul_blend;
use crate::paths::fan_triangles;
use std::ops::Range;

pub const SAMPLES: u32 = 4;
/// The layer is this many times the target on each side; the composite averages it down.
pub const SCALE: u32 = 2;
const DEPTH_STENCIL: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PVert {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

/// Where one path's vertices sit: its fan, then its cover box.
pub struct PathRange {
    pub fan: Range<u32>,
    pub cover: Range<u32>,
}

/// Vertices for every path in the frame, and where each path's parts are.
#[expect(clippy::cast_possible_truncation, reason = "vertex counts fit in u32")]
pub fn build(paths: &[PathDraw]) -> (Vec<PVert>, Vec<PathRange>) {
    let mut verts: Vec<PVert> = Vec::new();
    let mut ranges: Vec<PathRange> = Vec::new();
    let mut tris: Vec<[f32; 2]> = Vec::new();
    for d in paths {
        tris.clear();
        fan_triangles(&d.outline, &mut tris);
        let fan_start: u32 = verts.len() as u32;
        verts.extend(tris.iter().map(|p| PVert {
            pos: *p,
            color: d.color,
        }));
        let cover_start: u32 = verts.len() as u32;
        let b: tiny_skia::Rect = d.outline.bounds();
        let (l, t, r, bo): (f32, f32, f32, f32) = (
            b.left() - 2.0,
            b.top() - 2.0,
            b.right() + 2.0,
            b.bottom() + 2.0,
        );
        for p in [[l, t], [r, t], [l, bo], [r, t], [r, bo], [l, bo]] {
            verts.push(PVert {
                pos: p,
                color: d.color,
            });
        }
        ranges.push(PathRange {
            fan: fan_start..cover_start,
            cover: cover_start..verts.len() as u32,
        });
    }
    (verts, ranges)
}

pub struct PathPipes {
    pub bind: wgpu::BindGroup,
    pub stencil: wgpu::RenderPipeline,
    pub cover: wgpu::RenderPipeline,
}

impl PathPipes {
    pub fn new(device: &wgpu::Device, view_buf: &wgpu::Buffer) -> Self {
        let shader: wgpu::ShaderModule =
            device.create_shader_module(wgpu::include_wgsl!("paths.wgsl"));
        let bgl: wgpu::BindGroupLayout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("paths"),
                entries: &[crate::gpu::uniform_entry(0, wgpu::ShaderStages::VERTEX)],
            });
        let bind: wgpu::BindGroup = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("paths"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: view_buf.as_entire_binding(),
            }],
        });
        let layout: wgpu::PipelineLayout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
        let attrs: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];
        let make = |label: &'static str, stencil: wgpu::StencilState, writes: wgpu::ColorWrites| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: size_of::<PVert>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attrs,
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(premul_blend()),
                        write_mask: writes,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_STENCIL,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil,
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: SAMPLES,
                    ..Default::default()
                },
                multiview_mask: None,
                cache: None,
            })
        };
        let face = |compare: wgpu::CompareFunction, pass_op: wgpu::StencilOperation| {
            wgpu::StencilFaceState {
                compare,
                fail_op: wgpu::StencilOperation::Keep,
                depth_fail_op: wgpu::StencilOperation::Keep,
                pass_op,
            }
        };
        let always = wgpu::CompareFunction::Always;
        let stencil_state: wgpu::StencilState = wgpu::StencilState {
            front: face(always, wgpu::StencilOperation::IncrementWrap),
            back: face(always, wgpu::StencilOperation::DecrementWrap),
            read_mask: 0xff,
            write_mask: 0xff,
        };
        let cover_face = face(
            wgpu::CompareFunction::NotEqual,
            wgpu::StencilOperation::Zero,
        );
        let cover_state: wgpu::StencilState = wgpu::StencilState {
            front: cover_face,
            back: cover_face,
            read_mask: 0xff,
            write_mask: 0xff,
        };
        Self {
            stencil: make("path stencil", stencil_state, wgpu::ColorWrites::empty()),
            cover: make("path cover", cover_state, wgpu::ColorWrites::ALL),
            bind,
        }
    }
}

/// The multisampled layer paths are drawn into, its resolve, and the stencil.
pub struct Layer {
    msaa: wgpu::TextureView,
    depth_stencil: wgpu::TextureView,
    resolve: wgpu::TextureView,
    pub texture: wgpu::Texture,
}

impl Layer {
    /// `max_side` is the largest texture side the device allows; past half of it the layer drops to
    /// the target's own size (4 samples a pixel).
    pub fn new(device: &wgpu::Device, w: u32, h: u32, max_side: u32) -> Self {
        let scale: u32 = if w.max(h) * SCALE <= max_side {
            SCALE
        } else {
            1
        };
        let make = |format: wgpu::TextureFormat, samples: u32, usage: wgpu::TextureUsages| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("path layer"),
                size: wgpu::Extent3d {
                    width: w * scale,
                    height: h * scale,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let msaa: wgpu::Texture = make(FORMAT, SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let ds: wgpu::Texture = make(
            DEPTH_STENCIL,
            SAMPLES,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let resolve: wgpu::Texture = make(
            FORMAT,
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        Self {
            msaa: view(&msaa),
            depth_stencil: view(&ds),
            resolve: view(&resolve),
            texture: resolve,
        }
    }

    /// Draws `ranges` of `verts` into the layer (cleared transparent) and resolves it.
    pub fn draw(
        &self,
        enc: &mut wgpu::CommandEncoder,
        pipes: &PathPipes,
        verts: &wgpu::Buffer,
        ranges: &[PathRange],
    ) {
        let mut pass: wgpu::RenderPass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("path layer"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.msaa,
                depth_slice: None,
                resolve_target: Some(&self.resolve),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &self.depth_stencil,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &pipes.bind, &[]);
        pass.set_vertex_buffer(0, verts.slice(..));
        pass.set_stencil_reference(0);
        for r in ranges {
            if !r.fan.is_empty() {
                pass.set_pipeline(&pipes.stencil);
                pass.draw(r.fan.clone(), 0..1);
                pass.set_pipeline(&pipes.cover);
                pass.draw(r.cover.clone(), 0..1);
            }
        }
    }
}
