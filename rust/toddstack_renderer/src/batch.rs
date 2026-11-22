//! Custom batches: the app's own instance layout and shader, drawn inside a scene.

use std::sync::Arc;
use tiny_skia::Pixmap;

/// The CPU form of a batch: draws what the WGSL draws, into the target. It is also what runs when
/// no GPU backend is available.
pub trait CpuBatch: Send + Sync {
    /// `data` is the batch's current instance data, `params` its current parameters.
    fn draw(&self, data: &[u32], params: [f32; 4], target: &mut Pixmap);
}

impl<F> CpuBatch for F
where
    F: Fn(&[u32], [f32; 4], &mut Pixmap) + Send + Sync,
{
    fn draw(&self, data: &[u32], params: [f32; 4], target: &mut Pixmap) {
        self(data, params, target);
    }
}

/// What the app supplies to add a batch.
///
/// The WGSL gets this prelude (so one source runs with a storage buffer or a data texture):
///
/// ```wgsl
/// struct Globals { view: vec4f, params: vec4f }   // view.xy = target size in pixels
/// @group(0) @binding(0) var<uniform> globals: Globals;
/// fn data_at(i: u32) -> u32                       // the i-th u32 of the data
/// fn to_clip(px: vec2f) -> vec4f                  // pixel position -> clip space
/// ```
///
/// and must define `vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32)` and
/// `fs`, drawing each instance as a 4-vertex triangle strip with alpha blending over the target.
pub struct BatchDesc {
    /// Shown in errors and adapter reports.
    pub name: String,
    /// The shader body (entry points `vs` and `fs`).
    pub wgsl: String,
    /// The initial data, as packed u32 words; its length is fixed for the batch's life.
    pub data: Vec<u32>,
    /// How many instances to draw (changeable later).
    pub instances: u32,
    /// The CPU form.
    pub cpu: Arc<dyn CpuBatch>,
}

pub struct BatchState {
    pub name: String,
    pub wgsl: String,
    pub data: Vec<u32>,
    pub params: [f32; 4],
    pub instances: u32,
    pub cpu: Arc<dyn CpuBatch>,
}

impl From<BatchDesc> for BatchState {
    fn from(d: BatchDesc) -> Self {
        Self { name: d.name, wgsl: d.wgsl, data: d.data, params: [0.0; 4], instances: d.instances, cpu: d.cpu }
    }
}

/// Width of the data texture used where storage buffers are missing (WebGL2).
pub const DATA_TEXTURE_WIDTH: u32 = 2048;

pub fn prelude(data_texture: bool) -> String {
    let head: &str = "struct Globals { view: vec4f, params: vec4f };\n@group(0) @binding(0) var<uniform> globals: Globals;\nfn to_clip(px: vec2f) -> vec4f { return vec4f(px.x / globals.view.x * 2.0 - 1.0, 1.0 - px.y / globals.view.y * 2.0, 0.0, 1.0); }\n";
    if data_texture {
        format!("{head}const DATA_W: u32 = {DATA_TEXTURE_WIDTH}u;\n@group(0) @binding(1) var data_tex: texture_2d<u32>;\nfn data_at(i: u32) -> u32 {{ return textureLoad(data_tex, vec2u(i % DATA_W, i / DATA_W), 0).r; }}\n")
    } else {
        format!("{head}@group(0) @binding(1) var<storage, read> data_buf: array<u32>;\nfn data_at(i: u32) -> u32 {{ return data_buf[i]; }}\n")
    }
}
