struct VO {
    @builtin(position) pos: vec4f,
    @location(0) @interpolate(flat) color: vec4f,
};
@group(0) @binding(0) var<uniform> view: vec4f;

@vertex
fn vs(@location(0) p: vec2f, @location(1) color: vec4f) -> VO {
    var o: VO;
    o.pos = vec4f(p.x / view.x * 2.0 - 1.0, 1.0 - p.y / view.y * 2.0, 0.0, 1.0);
    o.color = color;
    return o;
}

// Premultiplied, for blending into the transparent layer.
@fragment
fn fs(v: VO) -> @location(0) vec4f {
    return vec4f(v.color.rgb * v.color.a, v.color.a);
}
