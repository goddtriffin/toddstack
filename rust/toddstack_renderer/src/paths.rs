//! Path helpers: the rounded-rect path and the flattening the GPU backend fills with a stencil.

use tiny_skia::{Path, PathBuilder, PathSegment, Point, Rect};

/// Flatness (pixels) the GPU backend flattens curves to.
const TOLERANCE: f32 = 0.05;

/// A rounded rectangle as a path (`r` of 0 is a plain rectangle); `None` for an empty one.
#[must_use]
#[expect(
    clippy::many_single_char_names,
    reason = "x, y, w, h, r are the geometry"
)]
pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut b: PathBuilder = PathBuilder::new();
    if r <= 0.0 {
        b.push_rect(Rect::from_xywh(x, y, w, h)?);
        return b.finish();
    }
    let r: f32 = r.min(w / 2.0).min(h / 2.0);
    let k: f32 = r * 0.552_284_8;
    let (x1, y1): (f32, f32) = (x + w, y + h);
    b.move_to(x + r, y);
    b.line_to(x1 - r, y);
    b.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    b.line_to(x1, y1 - r);
    b.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    b.line_to(x + r, y1);
    b.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    b.line_to(x, y + r);
    b.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    b.close();
    b.finish()
}

fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point::from_xy(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a small positive segment count"
)]
fn segments(err: f32) -> u32 {
    (err / TOLERANCE).sqrt().ceil().clamp(1.0, 200.0) as u32
}

fn second_diff(a: Point, b: Point, c: Point) -> f32 {
    (a.x - 2.0 * b.x + c.x).hypot(a.y - 2.0 * b.y + c.y)
}

/// A fan of triangles per subpath (from its first point), as a flat triangle list. With the
/// stencil's increment/decrement of front and back faces they give the non-zero winding fill.
#[expect(
    clippy::many_single_char_names,
    reason = "bezier maths: control points and a parameter"
)]
pub fn fan_triangles(path: &Path, out: &mut Vec<[f32; 2]>) {
    let mut hub: Point = Point::zero();
    let mut cur: Point = Point::zero();
    let line = |to: Point, cur: &mut Point, hub: Point, out: &mut Vec<[f32; 2]>| {
        if *cur != hub && to != hub {
            out.extend([[hub.x, hub.y], [cur.x, cur.y], [to.x, to.y]]);
        }
        *cur = to;
    };
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                hub = p;
                cur = p;
            }
            PathSegment::LineTo(p) => line(p, &mut cur, hub, out),
            PathSegment::QuadTo(c, p) => {
                let from: Point = cur;
                let n: u32 = segments(second_diff(from, c, p) / 4.0);
                for i in 1..=n {
                    let t: f32 = f32_from(i) / f32_from(n);
                    let (a, b): (Point, Point) = (lerp(from, c, t), lerp(c, p, t));
                    line(lerp(a, b, t), &mut cur, hub, out);
                }
            }
            PathSegment::CubicTo(c1, c2, p) => {
                let from: Point = cur;
                let m: f32 = second_diff(from, c1, c2).max(second_diff(c1, c2, p));
                let n: u32 = segments(0.75 * m);
                for i in 1..=n {
                    let t: f32 = f32_from(i) / f32_from(n);
                    let (a, b, c): (Point, Point, Point) =
                        (lerp(from, c1, t), lerp(c1, c2, t), lerp(c2, p, t));
                    line(lerp(lerp(a, b, t), lerp(b, c, t), t), &mut cur, hub, out);
                }
            }
            PathSegment::Close => {}
        }
    }
}

#[expect(clippy::cast_precision_loss, reason = "a small segment index")]
const fn f32_from(v: u32) -> f32 {
    v as f32
}
