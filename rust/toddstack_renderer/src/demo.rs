//! The demo scenes, batch and kernel the examples and the parity test share.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::many_single_char_names,
    reason = "demo geometry and colours: small values, letters for coordinates"
)]

use crate::batch::BatchDesc;
use crate::kernel::{Binding, Kernel};
use crate::paths::rounded_rect;
use crate::renderer::Renderer;
use crate::scene::{FontId, Sampling, Scene};
use std::sync::Arc;
use tiny_skia::{LineCap, LineJoin, Path, PathBuilder, Pixmap, Stroke, StrokeDash};

/// Background of the board and chart scenes.
pub const BG: [u8; 4] = [239, 233, 221, 255];

const LETTERS: [&str; 25] = [
    "T", "N", "E", "A", "R", "I", "S", "O", "L", "D", "C", "Qu", "H", "U", "G", "P", "B", "W", "K",
    "F", "Y", "V", "X", "Z", "M",
];

fn board_into(v: &mut Scene, font: FontId) {
    v.rrect(16.0, 16.0, 488.0, 488.0, 24.0, [91, 74, 58, 255]);
    let (cell, gap, ox, oy): (f32, f32, f32, f32) = (84.0, 10.0, 30.0, 30.0);
    let lit: [usize; 3] = [6, 7, 12];
    let pos = |i: usize| {
        (
            ox + (i % 5) as f32 * (cell + gap),
            oy + (i / 5) as f32 * (cell + gap),
        )
    };
    for i in 0..25 {
        let (x, y): (f32, f32) = pos(i);
        v.rrect(x, y, cell, cell, 14.0, [61, 49, 38, 255]);
        v.rrect(x + 4.0, y + 6.0, 76.0, 76.0, 12.0, [0, 0, 0, 90]);
        let body: [u8; 4] = if lit.contains(&i) {
            [255, 210, 74, 255]
        } else {
            [246, 241, 230, 255]
        };
        v.rrect(x + 4.0, y + 2.0, 76.0, 76.0, 12.0, body);
    }
    let centre = |i: usize| {
        let (x, y): (f32, f32) = pos(i);
        (x + cell / 2.0, y + cell / 2.0 - 2.0)
    };
    for w in lit.windows(2) {
        let (a, b): ((f32, f32), (f32, f32)) = (centre(w[0]), centre(w[1]));
        v.line(a.0, a.1, b.0, b.1, 18.0, [40, 160, 200, 150]);
    }
    for (i, l) in LETTERS.iter().enumerate() {
        let (cx, cy): (f32, f32) = centre(i);
        let size: f32 = if l.len() > 1 { 38.0 } else { 46.0 };
        v.text(font, l, cx, cy + size * 0.36, size, [42, 33, 24, 255]);
    }
}

fn arena_chart_into(v: &mut Scene, font: FontId) {
    v.rrect(528.0, 16.0, 256.0, 300.0, 16.0, [255, 250, 240, 255]);
    v.text(
        font,
        "Words by length",
        656.0,
        46.0,
        16.0,
        [42, 33, 24, 255],
    );
    for (g, lab) in ["100", "50", "0"].iter().enumerate() {
        let y: f32 = 90.0 + g as f32 * 95.0;
        v.rect(546.0, y, 222.0, 1.0, [0, 0, 0, 40]);
        v.text(font, lab, 540.0, y + 5.0, 11.0, [90, 80, 70, 255]);
    }
    for (i, val) in [0.9_f32, 0.75, 0.55, 0.35, 0.18, 0.07].iter().enumerate() {
        let x: f32 = 566.0 + i as f32 * 34.0;
        let h: f32 = val * 190.0;
        v.rrect(x, 280.0 - h, 26.0, h, 4.0, [63, 184, 196, 255]);
        v.text(
            font,
            &format!("{}", i + 3),
            x + 13.0,
            298.0,
            14.0,
            [42, 33, 24, 255],
        );
    }
    v.rect(556.0, 280.0, 212.0, 2.0, [68, 68, 68, 255]);
    v.rect(556.0, 90.0, 2.0, 192.0, [68, 68, 68, 255]);
}

/// The arena's fixed scene (800x520): a Big 5x5 board and a bar chart. The parity test scene.
#[must_use]
pub fn arena_scene(font: FontId) -> Scene {
    let mut v: Scene = Scene::new(BG);
    board_into(&mut v, font);
    arena_chart_into(&mut v, font);
    v
}

/// Just the dice board (520x520).
#[must_use]
pub fn board_scene(font: FontId) -> Scene {
    let mut v: Scene = Scene::new(BG);
    board_into(&mut v, font);
    v
}

/// A bar chart with a line over it (640x400): bars are rounded rects, the line is capsules.
#[must_use]
pub fn chart_scene(font: FontId) -> Scene {
    let mut v: Scene = Scene::new(BG);
    v.rrect(12.0, 12.0, 616.0, 376.0, 16.0, [255, 250, 240, 255]);
    v.text(
        font,
        "Words found per day",
        320.0,
        46.0,
        20.0,
        [42, 33, 24, 255],
    );
    for (g, lab) in ["200", "150", "100", "50", "0"].iter().enumerate() {
        let y: f32 = 80.0 + g as f32 * 66.0;
        v.rect(70.0, y, 540.0, 1.0, [0, 0, 0, 40]);
        v.text(font, lab, 48.0, y + 5.0, 13.0, [90, 80, 70, 255]);
    }
    let vals: [f32; 7] = [88.0, 132.0, 110.0, 176.0, 150.0, 198.0, 121.0];
    let days: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let mut prev: Option<(f32, f32)> = None;
    for (i, val) in vals.iter().enumerate() {
        let x: f32 = 90.0 + i as f32 * 74.0;
        let h: f32 = val / 200.0 * 264.0;
        v.rrect(x, 344.0 - h, 48.0, h, 6.0, [63, 184, 196, 255]);
        v.text(font, days[i], x + 24.0, 366.0, 14.0, [42, 33, 24, 255]);
        let pt: (f32, f32) = (x + 24.0, 344.0 - h);
        if let Some(p) = prev {
            v.line(p.0, p.1, pt.0, pt.1, 4.0, [214, 90, 60, 255]);
        }
        prev = Some(pt);
    }
    v.rect(70.0, 344.0, 540.0, 2.0, [68, 68, 68, 255]);
    v
}

const GRID_PALETTE: [[u8; 3]; 9] = [
    [226, 226, 226],
    [66, 133, 244],
    [52, 168, 83],
    [234, 67, 53],
    [103, 58, 183],
    [191, 54, 12],
    [0, 150, 136],
    [33, 33, 33],
    [120, 120, 120],
];

/// Packed tile: bits 0-7 neighbour count (0-8), bit 8 revealed, bit 9 flagged, bit 10 mine.
#[must_use]
pub fn grid_tiles(cols: u32, rows: u32) -> Vec<u32> {
    (0..cols * rows)
        .map(|i| {
            let h: u32 = i.wrapping_mul(0x9E37_79B1) ^ (i >> 7).wrapping_mul(0x85EB_CA6B);
            let count: u32 = (h >> 8) % 9;
            let kind: u32 = (h >> 16) % 8;
            let state: u32 = match kind {
                0 | 1 => 0,
                2 => 1 << 9,
                3 => (1 << 8) | (1 << 10),
                _ => 1 << 8,
            };
            count | state
        })
        .collect()
}

const GRID_WGSL: &str = "
struct VO { @builtin(position) pos: vec4f, @location(0) @interpolate(flat) tile: u32, @location(1) @interpolate(flat) xy: vec2u };
@vertex fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VO {
    let cols = u32(globals.params.x);
    let t = globals.params.y;
    let cell = vec2u(ii % cols, ii / cols);
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    var o: VO;
    o.pos = to_clip((vec2f(cell) + c) * t);
    o.tile = data_at(ii);
    o.xy = cell;
    return o;
}
const PAL = array<vec3u, 9>(vec3u(226u,226u,226u), vec3u(66u,133u,244u), vec3u(52u,168u,83u), vec3u(234u,67u,53u), vec3u(103u,58u,183u), vec3u(191u,54u,12u), vec3u(0u,150u,136u), vec3u(33u,33u,33u), vec3u(120u,120u,120u));
@fragment fn fs(v: VO) -> @location(0) vec4f {
    var rgb: vec3u;
    if (v.tile & 512u) != 0u { rgb = vec3u(214u, 58u, 58u); }
    else if (v.tile & 256u) == 0u { rgb = select(vec3u(140u, 150u, 170u), vec3u(150u, 160u, 180u), ((v.xy.x + v.xy.y) & 1u) == 0u); }
    else if (v.tile & 1024u) != 0u { rgb = vec3u(20u, 20u, 20u); }
    else { rgb = PAL[v.tile & 255u]; }
    return vec4f(vec3f(rgb) / 255.0, 1.0);
}
";

fn grid_color(tile: u32, x: u32, y: u32) -> [u8; 3] {
    if tile & 512 != 0 {
        [214, 58, 58]
    } else if tile & 256 == 0 {
        if (x + y) & 1 == 0 {
            [150, 160, 180]
        } else {
            [140, 150, 170]
        }
    } else if tile & 1024 != 0 {
        [20, 20, 20]
    } else {
        GRID_PALETTE[(tile & 255) as usize]
    }
}

/// An instanced grid of packed tiles: one u32 per tile, `cols * rows` instances. Set the batch
/// params to `[cols, tile_px, 0, 0]`.
#[must_use]
pub fn grid_batch(cols: u32, rows: u32) -> BatchDesc {
    BatchDesc {
        name: "grid".to_owned(),
        wgsl: GRID_WGSL.to_owned(),
        data: grid_tiles(cols, rows),
        instances: cols * rows,
        cpu: Arc::new(|data: &[u32], params: [f32; 4], pm: &mut Pixmap| {
            let (cols, t): (u32, u32) = (params[0] as u32, params[1] as u32);
            let (w, h): (u32, u32) = (pm.width(), pm.height());
            let px: &mut [u8] = pm.data_mut();
            for (i, tile) in data.iter().enumerate() {
                let (cx, cy): (u32, u32) = (i as u32 % cols, i as u32 / cols);
                let c: [u8; 3] = grid_color(*tile, cx, cy);
                for dy in 0..t {
                    for dx in 0..t {
                        let (x, y): (u32, u32) = (cx * t + dx, cy * t + dy);
                        if x < w && y < h {
                            let o: usize = ((y * w + x) * 4) as usize;
                            px[o..o + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
                        }
                    }
                }
            }
        }),
    }
}

/// 16 colours (a cross-stitch floss subset), as RGB bytes.
pub const PALETTE: [[u8; 3]; 16] = [
    [0, 0, 0],
    [255, 255, 255],
    [200, 30, 40],
    [240, 120, 40],
    [250, 210, 50],
    [120, 190, 60],
    [30, 130, 70],
    [60, 190, 200],
    [40, 90, 190],
    [90, 60, 160],
    [200, 90, 170],
    [120, 70, 40],
    [230, 190, 160],
    [150, 150, 150],
    [80, 80, 90],
    [250, 160, 170],
];

/// A deterministic test image: gradients, rings and a checker.
#[must_use]
pub fn test_image(w: u32, h: u32) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (dx, dy): (f32, f32) = (x as f32 - w as f32 * 0.5, y as f32 - h as f32 * 0.5);
            let ring: f32 = ((dx * dx + dy * dy).sqrt() * 0.12).sin() * 0.5 + 0.5;
            let checker: f32 = if ((x / 24) + (y / 24)) % 2 == 0 {
                0.15
            } else {
                0.0
            };
            let r: f32 = x as f32 / w as f32;
            let g: f32 = y as f32 / h as f32;
            let b: f32 = (ring * 0.8 + checker).min(1.0);
            v.extend_from_slice(&[(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, 255]);
        }
    }
    v
}

const QUANT_COMPUTE: &str = "
@group(0) @binding(0) var img: texture_2d<f32>;
@group(0) @binding(1) var<storage, read> palette: array<vec4f>;
@group(0) @binding(2) var out: texture_storage_2d<rgba8unorm, write>;
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3u) {
    let dim = textureDimensions(img);
    if gid.x >= dim.x || gid.y >= dim.y { return; }
    let c = textureLoad(img, vec2i(gid.xy), 0).rgb;
    var best = 0u;
    var bd = 1e9;
    for (var i = 0u; i < 16u; i++) {
        let d = c - palette[i].rgb;
        let e = dot(d, d);
        if e < bd { bd = e; best = i; }
    }
    textureStore(out, vec2i(gid.xy), vec4f(palette[best].rgb, 1.0));
}
";

const QUANT_FRAGMENT: &str = "
@group(0) @binding(0) var img: texture_2d<f32>;
@group(0) @binding(1) var<uniform> palette: array<vec4f, 16>;
@fragment
fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let c = textureLoad(img, vec2i(pos.xy), 0).rgb;
    var best = 0u;
    var bd = 1e9;
    for (var i = 0u; i < 16u; i++) {
        let d = c - palette[i].rgb;
        let e = dot(d, d);
        if e < bd { bd = e; best = i; }
    }
    return vec4f(palette[best].rgb, 1.0);
}
";

/// Nearest-palette quantize (the core of image-to-cross-stitch) with all three forms.
#[must_use]
pub fn quantize_kernel(width: u32, height: u32) -> Kernel {
    let pal: Vec<[f32; 4]> = PALETTE
        .iter()
        .map(|c| {
            [
                f32::from(c[0]) / 255.0,
                f32::from(c[1]) / 255.0,
                f32::from(c[2]) / 255.0,
                1.0,
            ]
        })
        .collect();
    Kernel {
        name: "quantize".to_owned(),
        compute: Some(QUANT_COMPUTE.to_owned()),
        fragment: Some(QUANT_FRAGMENT.to_owned()),
        cpu: Some(Arc::new(|inputs: &[Binding], w: u32, h: u32| {
            let (Binding::Texture { rgba8, .. }, Binding::Storage(p)) = (&inputs[0], &inputs[1])
            else {
                return Vec::new();
            };
            let pal: &[f32] = bytemuck::cast_slice(p);
            let mut out: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
            for px in rgba8.as_chunks::<4>().0 {
                let c: [f32; 3] = [
                    f32::from(px[0]) / 255.0,
                    f32::from(px[1]) / 255.0,
                    f32::from(px[2]) / 255.0,
                ];
                let (mut best, mut bd): (usize, f32) = (0, 1e9);
                for i in 0..16 {
                    let d: [f32; 3] = [
                        c[0] - pal[i * 4],
                        c[1] - pal[i * 4 + 1],
                        c[2] - pal[i * 4 + 2],
                    ];
                    let e: f32 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
                    if e < bd {
                        bd = e;
                        best = i;
                    }
                }
                out.extend_from_slice(&PALETTE[best]);
                out.push(255);
            }
            out
        })),
        workgroup: [8, 8],
        inputs: vec![
            Binding::texture(width, height, test_image(width, height)),
            Binding::storage(&pal),
        ],
        width,
        height,
    }
}

/// A 64x64 avatar-like RGBA image: a gradient disc with a soft edge, two eyes, transparent corners.
#[must_use]
pub fn avatar_image() -> Vec<u8> {
    let mut v: Vec<u8> = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64_u32 {
        for x in 0..64_u32 {
            let (dx, dy): (f32, f32) = (x as f32 - 31.5, y as f32 - 31.5);
            let alpha: f32 = (32.0 - dx.hypot(dy)).clamp(0.0, 1.0);
            let eye: bool = [(-10.0_f32, -6.0_f32), (10.0, -6.0)]
                .iter()
                .any(|(ex, ey)| (dx - ex).hypot(dy - ey) < 5.0);
            let rgb: [u8; 3] = if eye {
                [42, 33, 24]
            } else {
                [(x * 4).min(255) as u8, (y * 3 + 60).min(255) as u8, 190]
            };
            v.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (alpha * 255.0).round() as u8]);
        }
    }
    v
}

/// An 8x8 pixel-art heart, opaque red on transparent.
#[must_use]
pub fn heart_image() -> Vec<u8> {
    const ROWS: [&str; 8] = [
        "..XX.XX.", ".XXXXXXX", ".XXXXXXX", ".XXXXXXX", "..XXXXX.", "...XXX..", "....X...",
        "........",
    ];
    let mut v: Vec<u8> = Vec::with_capacity(8 * 8 * 4);
    for row in ROWS {
        for ch in row.bytes() {
            v.extend_from_slice(if ch == b'X' {
                &[214, 60, 80, 255]
            } else {
                &[0, 0, 0, 0]
            });
        }
    }
    v
}

/// Image quads (640x260): an avatar at 0.5x, 1x and 2x, the 2x one nearest-sampled; a pixel icon
/// nearest and linear; the avatar at half opacity over a bar.
///
/// # Errors
/// Anything adding the images or the font fails with.
pub fn images_scene(r: &mut Renderer, font: FontId) -> crate::Result<Scene> {
    let avatar = r.add_image(64, 64, &avatar_image(), Sampling::Linear)?;
    let blocky = r.add_image(64, 64, &avatar_image(), Sampling::Nearest)?;
    let heart = r.add_image(8, 8, &heart_image(), Sampling::Nearest)?;
    let soft = r.add_image(8, 8, &heart_image(), Sampling::Linear)?;
    let mut v: Scene = Scene::new(BG);
    v.rrect(12.0, 12.0, 616.0, 236.0, 16.0, [255, 250, 240, 255]);
    v.image(avatar, 28.0, 28.0, 32.0, 32.0, 1.0);
    v.image(avatar, 72.0, 28.0, 64.0, 64.0, 1.0);
    v.image(avatar, 28.0, 104.0, 128.0, 128.0, 1.0);
    v.image(blocky, 172.0, 104.0, 128.0, 128.0, 1.0);
    v.text(
        font,
        "0.5x 1x 2x linear | 2x nearest",
        164.0,
        100.0,
        11.0,
        [90, 80, 70, 255],
    );
    v.image(heart, 332.0, 28.0, 96.0, 96.0, 1.0);
    v.image(soft, 444.0, 28.0, 96.0, 96.0, 1.0);
    v.image(heart, 560.0, 28.0, 24.0, 24.0, 1.0);
    v.rect(332.0, 160.0, 280.0, 40.0, [63, 184, 196, 255]);
    v.image(avatar, 440.0, 136.0, 88.0, 88.0, 0.5);
    v.image(avatar, 340.0, 136.0, 88.0, 88.0, 1.0);
    Ok(v)
}

/// A Catmull-Rom spline through `pts`, as cubics.
fn spline(pts: &[(f32, f32)]) -> Option<Path> {
    let mut b: PathBuilder = PathBuilder::new();
    b.move_to(pts.first()?.0, pts.first()?.1);
    for i in 0..pts.len() - 1 {
        let p0: (f32, f32) = pts[i.saturating_sub(1)];
        let (p1, p2): ((f32, f32), (f32, f32)) = (pts[i], pts[i + 1]);
        let p3: (f32, f32) = pts[(i + 2).min(pts.len() - 1)];
        b.cubic_to(
            p1.0 + (p2.0 - p0.0) / 6.0,
            p1.1 + (p2.1 - p0.1) / 6.0,
            p2.0 - (p3.0 - p1.0) / 6.0,
            p2.1 - (p3.1 - p1.1) / 6.0,
            p2.0,
            p2.1,
        );
    }
    b.finish()
}

/// Paths (640x400): a smooth line chart with a translucent area, dots, a dashed guide, a pentagram,
/// a frame with a hole, a zig-zag with miter and bevel joins, rounded tiles; text between runs.
#[must_use]
pub fn paths_scene(font: FontId) -> Scene {
    let ink: [u8; 4] = [42, 33, 24, 255];
    let mut v: Scene = Scene::new(BG);
    if let Some(card) = rounded_rect(12.0, 12.0, 616.0, 376.0, 16.0) {
        v.fill_path(&card, [255, 250, 240, 255]);
        v.stroke_path(
            &card,
            &stroke(2.0, LineCap::Butt, LineJoin::Miter),
            [0, 0, 0, 40],
        );
    }
    v.text(font, "Words found per day", 190.0, 44.0, 18.0, ink);
    let pts: Vec<(f32, f32)> = [88.0_f32, 132.0, 110.0, 176.0, 150.0, 198.0, 121.0]
        .iter()
        .enumerate()
        .map(|(i, val)| (40.0 + i as f32 * 50.0, 330.0 - val * 1.3))
        .collect();
    if let Some(line) = spline(&pts) {
        let mut area: PathBuilder = PathBuilder::new();
        area.move_to(pts[0].0, 330.0);
        area.line_to(pts[0].0, pts[0].1);
        for seg in line.segments().skip(1) {
            if let tiny_skia::PathSegment::CubicTo(a, b, c) = seg {
                area.cubic_to(a.x, a.y, b.x, b.y, c.x, c.y);
            }
        }
        area.line_to(pts[6].0, 330.0);
        area.close();
        if let Some(area) = area.finish() {
            v.fill_path(&area, [63, 184, 196, 110]);
        }
        v.stroke_path(
            &line,
            &stroke(4.0, LineCap::Round, LineJoin::Round),
            [214, 90, 60, 255],
        );
    }
    for (x, y) in &pts {
        if let Some(dot) = rounded_rect(x - 6.0, y - 6.0, 12.0, 12.0, 6.0) {
            v.fill_path(&dot, [255, 250, 240, 255]);
            v.stroke_path(&dot, &stroke(3.0, LineCap::Butt, LineJoin::Round), ink);
        }
    }
    v.text(font, "Mon to Sun", 190.0, 356.0, 13.0, [90, 80, 70, 255]);
    let mut guide: PathBuilder = PathBuilder::new();
    guide.move_to(40.0, 330.0 - 150.0 * 1.3);
    guide.line_to(340.0, 330.0 - 150.0 * 1.3);
    if let Some(g) = guide.finish() {
        let mut st: Stroke = stroke(2.0, LineCap::Butt, LineJoin::Miter);
        st.dash = StrokeDash::new(vec![10.0, 6.0], 0.0);
        v.stroke_path(&g, &st, [90, 80, 70, 200]);
    }
    shapes_into(&mut v);
    v
}

fn shapes_into(v: &mut Scene) {
    let ink: [u8; 4] = [42, 33, 24, 255];
    let mut star: PathBuilder = PathBuilder::new();
    for k in 0..5_u32 {
        let a: f32 = (k * 2) as f32 * std::f32::consts::TAU / 5.0 - std::f32::consts::FRAC_PI_2;
        let (x, y): (f32, f32) = (500.0 + 64.0 * a.cos(), 110.0 + 64.0 * a.sin());
        if k == 0 {
            star.move_to(x, y);
        } else {
            star.line_to(x, y);
        }
    }
    star.close();
    if let Some(star) = star.finish() {
        v.fill_path(&star, [255, 210, 74, 255]);
        v.stroke_path(&star, &stroke(3.0, LineCap::Butt, LineJoin::Miter), ink);
    }
    let mut frame: PathBuilder = PathBuilder::new();
    frame.move_to(380.0, 210.0);
    frame.line_to(470.0, 210.0);
    frame.line_to(470.0, 290.0);
    frame.line_to(380.0, 290.0);
    frame.close();
    frame.move_to(398.0, 226.0);
    frame.line_to(398.0, 274.0);
    frame.line_to(452.0, 274.0);
    frame.line_to(452.0, 226.0);
    frame.close();
    if let Some(frame) = frame.finish() {
        v.fill_path(&frame, [91, 74, 58, 255]);
    }
    let mut zig: PathBuilder = PathBuilder::new();
    zig.move_to(496.0, 250.0);
    zig.line_to(520.0, 190.0);
    zig.line_to(544.0, 250.0);
    zig.line_to(568.0, 190.0);
    zig.line_to(592.0, 250.0);
    if let Some(zig) = zig.finish() {
        v.stroke_path(
            &zig,
            &stroke(8.0, LineCap::Square, LineJoin::Miter),
            [40, 160, 200, 255],
        );
        let moved: Path = zig
            .clone()
            .transform(tiny_skia::Transform::from_translate(0.0, 60.0))
            .unwrap_or_else(|| zig.clone());
        v.stroke_path(
            &moved,
            &stroke(8.0, LineCap::Butt, LineJoin::Bevel),
            [103, 58, 183, 255],
        );
    }
    for (i, color) in [[63, 184, 196, 255], [255, 210, 74, 255], [214, 90, 60, 255]]
        .iter()
        .enumerate()
    {
        if let Some(tile) = rounded_rect(380.0 + i as f32 * 72.0, 326.0, 60.0, 44.0, 12.0) {
            v.fill_path(&tile, *color);
        }
    }
}

fn stroke(width: f32, line_cap: LineCap, line_join: LineJoin) -> Stroke {
    Stroke {
        width,
        line_cap,
        line_join,
        ..Stroke::default()
    }
}

/// A kernel's output as a scene image (640x300): the quantize kernel runs on the GPU and is drawn
/// at full size and half size with no readback, with a frame and text drawn around it.
///
/// # Errors
/// Anything the kernel, the image or the font fails with.
pub fn texture_scene(r: &mut Renderer, font: FontId) -> crate::Result<Scene> {
    let (id, _path) = r.kernel_image(&quantize_kernel(320, 240), Sampling::Linear)?;
    let mut v: Scene = Scene::new(BG);
    v.rrect(8.0, 8.0, 328.0, 248.0, 12.0, [91, 74, 58, 255]);
    v.image(id, 12.0, 12.0, 320.0, 240.0, 1.0);
    v.text(
        font,
        "kernel output, no readback",
        480.0,
        40.0,
        16.0,
        [42, 33, 24, 255],
    );
    v.image(id, 352.0, 56.0, 160.0, 120.0, 1.0);
    v.image(id, 524.0, 56.0, 96.0, 72.0, 0.6);
    v.rrect(352.0, 190.0, 268.0, 40.0, 10.0, [63, 184, 196, 255]);
    Ok(v)
}

/// Renders [`arena_scene`] on `backend` and on the CPU and diffs them. `Ok(None)` when `backend`
/// is not available on this machine.
///
/// # Errors
/// Anything the renderer fails with other than a missing backend.
pub async fn parity(
    backend: crate::Backend,
    font: &[u8],
) -> crate::Result<Option<(crate::Capabilities, crate::Diff)>> {
    parity_scene(backend, crate::Target::offscreen(800, 520), |r| {
        Ok(arena_scene(r.add_font(font)?))
    })
    .await
}

/// Renders the scene `build` makes on `backend` and on the CPU and diffs them. `Ok(None)` when
/// `backend` is not available on this machine.
///
/// # Errors
/// Anything the renderer or `build` fails with other than a missing backend.
pub async fn parity_scene<F>(
    backend: crate::Backend,
    target: crate::Target,
    build: F,
) -> crate::Result<Option<(crate::Capabilities, crate::Diff)>>
where
    F: Fn(&mut Renderer) -> crate::Result<Scene>,
{
    use crate::{Backend, Diff, Error, Options};
    let mut cpu: Renderer = Renderer::with_options(
        target.clone(),
        Options {
            force: Some(Backend::Cpu),
            data_texture: None,
        },
    )
    .await?;
    let scene: Scene = build(&mut cpu)?;
    let reference: Vec<u8> = cpu.render(&scene).await?;
    let mut gpu: Renderer = match Renderer::with_options(
        target,
        Options {
            force: Some(backend),
            data_texture: None,
        },
    )
    .await
    {
        Ok(r) => r,
        Err(Error::NoBackend { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let scene: Scene = build(&mut gpu)?;
    let pixels: Vec<u8> = gpu.render(&scene).await?;
    Ok(Some((
        gpu.capabilities().clone(),
        Diff::between(&reference, &pixels),
    )))
}
