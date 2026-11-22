struct VO {
    @builtin(position) pos: vec4f,
    // Position in the quad, 0..1 on each axis.
    @location(0) c: vec2f,
    // Source rect as fractions of the image: u, v, width, height.
    @location(1) @interpolate(flat) uv: vec4f,
    @location(2) @interpolate(flat) half_texel: vec2f,
    // Straight tint, 0..1.
    @location(3) @interpolate(flat) tint: vec4f,
};
@group(0) @binding(0) var<uniform> view: vec4f;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

// a = destination rect, uv = source rect, p = rotation and half a texel, color = tint.
@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) a: vec4f, @location(1) uv: vec4f,
      @location(2) p: vec4f, @location(3) color: vec4f) -> VO {
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    var l = (c - 0.5) * a.zw;
    if p.x != 0.0 {
        let s = sin(p.x);
        let k = cos(p.x);
        l = vec2f(l.x * k - l.y * s, l.x * s + l.y * k);
    }
    let px = a.xy + a.zw * 0.5 + l;
    var o: VO;
    o.pos = vec4f(px.x / view.x * 2.0 - 1.0, 1.0 - px.y / view.y * 2.0, 0.0, 1.0);
    o.c = c;
    o.uv = uv;
    o.half_texel = p.yz;
    o.tint = color;
    return o;
}

// Sampling never leaves the source rect: the coordinate stops half a texel inside it, which is
// what clamp-to-edge does on a texture holding only that rect.
fn sample_src(v: VO) -> vec4f {
    let t = v.uv.xy + v.c * v.uv.zw;
    let lo = v.uv.xy + v.half_texel;
    let hi = v.uv.xy + v.uv.zw - v.half_texel;
    return textureSampleLevel(tex, samp, clamp(t, min(lo, hi), max(lo, hi)), 0.0);
}

// Straight tint times premultiplied colour: colour by tint colour and tint alpha, alpha by tint alpha.
fn tint_mul(v: VO) -> vec4f {
    return vec4f(v.tint.rgb * v.tint.a, v.tint.a);
}

// The texture holds premultiplied colour.
@fragment
fn fs_premul(v: VO) -> @location(0) vec4f {
    return sample_src(v) * tint_mul(v);
}

// The texture holds straight colour (a kernel's output).
@fragment
fn fs_straight(v: VO) -> @location(0) vec4f {
    let c = sample_src(v);
    return vec4f(c.rgb * c.a, c.a) * tint_mul(v);
}
