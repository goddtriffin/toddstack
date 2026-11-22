struct VO {
    @builtin(position) pos: vec4f,
    @location(0) @interpolate(flat) a: vec4f,
    @location(1) @interpolate(flat) opacity: f32,
};
@group(0) @binding(0) var<uniform> view: vec4f;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) a: vec4f, @location(2) p: vec4f) -> VO {
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let px = a.xy + c * a.zw;
    var o: VO;
    o.pos = vec4f(px.x / view.x * 2.0 - 1.0, 1.0 - px.y / view.y * 2.0, 0.0, 1.0);
    o.a = a;
    o.opacity = p.x;
    return o;
}

// The texture holds premultiplied colour.
@fragment
fn fs_premul(v: VO) -> @location(0) vec4f {
    let uv = (v.pos.xy - v.a.xy) / v.a.zw;
    return textureSampleLevel(tex, samp, uv, 0.0) * v.opacity;
}

// The texture holds straight colour (a kernel's output).
@fragment
fn fs_straight(v: VO) -> @location(0) vec4f {
    let uv = (v.pos.xy - v.a.xy) / v.a.zw;
    let c = textureSampleLevel(tex, samp, uv, 0.0);
    return vec4f(c.rgb * c.a, c.a) * v.opacity;
}
