struct VO {
    @builtin(position) pos: vec4f,
    @location(0) @interpolate(flat) a: vec4f,
    @location(1) @interpolate(flat) uv: vec4f,
    @location(2) @interpolate(flat) p: vec4f,
    @location(3) @interpolate(flat) color: vec4f,
};
@group(0) @binding(0) var<uniform> view: vec4f;
@group(0) @binding(1) var atlas: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) a: vec4f, @location(1) uv: vec4f,
      @location(2) p: vec4f, @location(3) color: vec4f) -> VO {
    var lo = a.xy - 1.0;
    var hi = a.xy + a.zw + 1.0;
    if p.y > 0.5 && p.y < 1.5 {
        lo = min(a.xy, a.zw) - p.x - 1.0;
        hi = max(a.xy, a.zw) + p.x + 1.0;
    } else if p.y > 1.5 {
        lo = a.xy;
        hi = a.xy + a.zw;
    }
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let px = lo + c * (hi - lo);
    var o: VO;
    o.pos = vec4f(px.x / view.x * 2.0 - 1.0, 1.0 - px.y / view.y * 2.0, 0.0, 1.0);
    o.a = a; o.uv = uv; o.p = p; o.color = color;
    return o;
}

@fragment
fn fs(v: VO) -> @location(0) vec4f {
    let q = v.pos.xy;
    var cov = 1.0;
    if v.p.y < 0.5 {
        let h = v.a.zw * 0.5;
        let d = abs(q - (v.a.xy + h)) - (h - v.p.x);
        cov = clamp(0.5 - (length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0) - v.p.x), 0.0, 1.0);
    } else if v.p.y < 1.5 {
        let pa = q - v.a.xy;
        let ba = v.a.zw - v.a.xy;
        let t = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-6), 0.0, 1.0);
        cov = clamp(0.5 - (length(pa - ba * t) - v.p.x), 0.0, 1.0);
    } else {
        cov = textureLoad(atlas, vec2i(floor(q - v.a.xy) + v.uv.xy), 0).r;
    }
    return vec4f(v.color.rgb, v.color.a * cov);
}
