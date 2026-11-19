//! PNG decode, encode and the pixel compare. A straight port of Boggledygook's `ui/web/png.mjs`, so its
//! references stay valid: same thresholds, same block rule, same reference encoding.

use std::collections::BTreeMap;

use crate::error::Error;

/// A pixel differs when any channel moves by more than this (anti-aliasing noise stays below it).
pub const PIXEL: i32 = 16;
/// Up to this share of differing pixels is drift and passes.
pub const TOL_RATIO: f64 = 0.001;
/// No block may have more than this share of differing pixels.
pub const TOL_BLOCK: f64 = 0.2;
/// Block edge in pixels.
pub const BLOCK: u32 = 24;
/// References are stored with channels rounded to a multiple of this.
pub const REF_STEP: u8 = 8;

/// An RGBA image, row-major, 4 bytes per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
    /// RGBA bytes.
    pub px: Vec<u8>,
}

/// What a compare found.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Same width and height.
    pub same_size: bool,
    /// Share of differing pixels.
    pub ratio: f64,
    /// Largest share of differing pixels in any block.
    pub block: f64,
    /// Diff image (grayscale original, differing pixels red); absent on a size change and when nothing differs.
    pub diff: Option<Image>,
}

impl Outcome {
    /// Pass: same size, drift within both tolerances.
    pub fn passes(&self) -> bool {
        self.same_size && self.ratio <= TOL_RATIO && self.block <= TOL_BLOCK
    }
}

/// Decodes an 8-bit, non-interlaced RGB or RGBA PNG.
///
/// # Errors
///
/// Anything else, or a corrupt file.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    let bad = |e: &dyn std::fmt::Display| Error::Config(format!("unsupported PNG: {e}"));
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().map_err(|e| bad(&e))?;
    if reader.info().interlaced {
        return Err(bad(&"interlaced"));
    }
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight || !matches!(color, png::ColorType::Rgb | png::ColorType::Rgba)
    {
        return Err(bad(&"need 8-bit RGB or RGBA"));
    }
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or_else(|| bad(&"too large"))?
    ];
    let info = reader.next_frame(&mut buf).map_err(|e| bad(&e))?;
    let (w, h) = (info.width, info.height);
    let n = (w as usize) * (h as usize);
    let mut px = Vec::with_capacity(n * 4);
    if color == png::ColorType::Rgba {
        px.extend_from_slice(&buf[..n * 4]);
    } else {
        for c in buf[..n * 3].as_chunks::<3>().0 {
            px.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    Ok(Image { w, h, px })
}

/// Encodes RGB (alpha dropped: pages are opaque), rows filtered adaptively, best compression.
/// `step > 1` rounds each channel to a multiple of `step`: lossy by at most `step / 2`, far under the compare
/// tolerance, and much smaller.
///
/// # Errors
///
/// The encoder failed (an image too large for PNG).
pub fn encode(img: &Image, step: u8) -> Result<Vec<u8>, Error> {
    encode_with(img, step, png::Compression::High)
}

/// [`encode`] with a chosen compression: `Fast` for files that are only kept locally (the sweep's baseline), where
/// the encode time matters and the size does not.
///
/// # Errors
///
/// The encoder failed (an image too large for PNG).
pub fn encode_with(img: &Image, step: u8, compression: png::Compression) -> Result<Vec<u8>, Error> {
    let q = |v: u8| -> u8 {
        if step > 1 {
            let s = u32::from(step);
            u8::try_from(((u32::from(v) + s / 2) / s * s).min(255)).unwrap_or(255)
        } else {
            v
        }
    };
    let rgb: Vec<u8> = img
        .px
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [q(p[0]), q(p[1]), q(p[2])])
        .collect();
    let mut out = Vec::new();
    let fail = |e: png::EncodingError| Error::Failed(format!("PNG encode: {e}"));
    let mut enc = png::Encoder::new(&mut out, img.w, img.h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(compression);
    enc.set_filter(png::Filter::Adaptive);
    let mut writer = enc.write_header().map_err(fail)?;
    writer.write_image_data(&rgb).map_err(fail)?;
    writer.finish().map_err(fail)?;
    Ok(out)
}

/// Compares `a` (the reference) with `b` (the actual).
pub fn compare(reference: &Image, actual: &Image) -> Outcome {
    let (a, b) = (reference, actual);
    if a.w != b.w || a.h != b.h {
        return Outcome {
            same_size: false,
            ratio: 1.0,
            block: 1.0,
            diff: None,
        };
    }
    if a.px == b.px {
        // The same pixels: nothing to count, and no diff image to draw.
        return Outcome {
            same_size: true,
            ratio: 0.0,
            block: 0.0,
            diff: None,
        };
    }
    let (cols, rows) = (a.w as usize, a.h as usize);
    let mut diff = vec![0_u8; cols * rows * 4];
    let mut cells: BTreeMap<(usize, usize), u32> = BTreeMap::new();
    let mut bad = 0_u32;
    for i in 0..cols * rows {
        let at = i * 4;
        let worst = (0..3)
            .map(|c| (i32::from(a.px[at + c]) - i32::from(b.px[at + c])).abs())
            .max()
            .unwrap_or(0);
        let sum = u32::from(a.px[at]) + u32::from(a.px[at + 1]) + u32::from(a.px[at + 2]);
        // round(sum / 3 * 0.3 + 178), halves up like JavaScript's Math.round.
        let gray = u8::try_from((sum + 1785) / 10).unwrap_or(255);
        diff[at..at + 4].copy_from_slice(&[gray, gray, gray, 255]);
        if worst > PIXEL {
            bad += 1;
            diff[at..at + 3].copy_from_slice(&[255, 0, 60]);
            *cells
                .entry((i / cols / BLOCK as usize, (i % cols) / BLOCK as usize))
                .or_insert(0) += 1;
        }
    }
    let block = f64::from(cells.values().copied().max().unwrap_or(0)) / f64::from(BLOCK * BLOCK);
    Outcome {
        same_size: true,
        ratio: f64::from(bad) / f64::from(u32::try_from(cols * rows).unwrap_or(u32::MAX)),
        block,
        diff: Some(Image {
            w: a.w,
            h: a.h,
            px: diff,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Image {
        Image {
            w,
            h,
            px: (0..w * h)
                .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
                .collect(),
        }
    }

    fn paint(img: &mut Image, x0: u32, y0: u32, x1: u32, y1: u32, rgb: [u8; 3]) {
        for y in y0..y1 {
            for x in x0..x1 {
                let o = ((y * img.w + x) * 4) as usize;
                img.px[o..o + 3].copy_from_slice(&rgb);
            }
        }
    }

    #[test]
    fn identical_images_pass() {
        let a = solid(100, 100, [200, 200, 200]);
        let o = compare(&a, &a.clone());
        assert!(o.passes());
        assert!(o.ratio.abs() < f64::EPSILON && o.block.abs() < f64::EPSILON);
    }

    #[test]
    fn size_change_fails() {
        let o = compare(&solid(10, 10, [0; 3]), &solid(10, 11, [0; 3]));
        assert!(!o.passes() && !o.same_size && o.diff.is_none());
    }

    #[test]
    fn channel_move_of_16_is_noise_17_is_a_difference() {
        let a = solid(50, 50, [100, 100, 100]);
        let mut b = a.clone();
        paint(&mut b, 0, 0, 50, 50, [116, 100, 100]);
        assert!(
            compare(&a, &b).ratio.abs() < f64::EPSILON,
            "16 is not over the threshold"
        );
        paint(&mut b, 0, 0, 50, 50, [100, 100, 117]);
        assert!((compare(&a, &b).ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn drift_under_a_tenth_of_a_percent_passes() {
        // 1000x1000 = 1e6 px; 1000 differing px is exactly 0.1%, in a strip so no block is over 20%.
        let a = solid(1000, 1000, [255, 255, 255]);
        let mut b = a.clone();
        paint(&mut b, 0, 0, 1000, 1, [0, 0, 0]);
        let o = compare(&a, &b);
        assert!((o.ratio - 0.001).abs() < 1e-12);
        assert!(o.block < TOL_BLOCK);
        assert!(o.passes());
        paint(&mut b, 0, 1, 1, 2, [0, 0, 0]);
        assert!(!compare(&a, &b).passes(), "one pixel past 0.1% fails");
    }

    #[test]
    fn small_changed_element_fails_even_when_the_screen_barely_moved() {
        // 20x20 block of 400 px out of 1e6 is 0.04% overall but 69% of its block.
        let a = solid(1000, 1000, [255, 255, 255]);
        let mut b = a.clone();
        paint(&mut b, 24, 24, 44, 44, [0, 0, 0]);
        let o = compare(&a, &b);
        assert!(o.ratio < TOL_RATIO);
        assert!((o.block - 400.0 / 576.0).abs() < 1e-12);
        assert!(!o.passes());
    }

    #[test]
    fn block_edge_is_exactly_20_percent() {
        // 115 px of 576 is 0.1997 (passes the block rule); 116 is 0.2014 (fails).
        let a = solid(1000, 1000, [255, 255, 255]);
        let mut b = a.clone();
        paint(&mut b, 0, 0, 24, 4, [0, 0, 0]); // 96
        paint(&mut b, 0, 4, 19, 5, [0, 0, 0]); // 19 -> 115
        assert!(compare(&a, &b).block <= TOL_BLOCK);
        paint(&mut b, 19, 4, 20, 5, [0, 0, 0]); // 116
        assert!(compare(&a, &b).block > TOL_BLOCK);
    }

    #[test]
    fn diff_image_marks_pixels_red_and_grays_the_rest() {
        let a = solid(30, 30, [90, 90, 90]);
        let mut b = a.clone();
        paint(&mut b, 5, 5, 6, 6, [0, 0, 0]);
        let d = compare(&a, &b).diff.unwrap();
        let at = |x: usize, y: usize| d.px[(y * 30 + x) * 4..(y * 30 + x) * 4 + 4].to_vec();
        assert_eq!(at(5, 5), vec![255, 0, 60, 255]);
        // round(90 * 0.3 + 178) = 205
        assert_eq!(at(0, 0), vec![205, 205, 205, 255]);
    }

    #[test]
    fn encode_decode_round_trip_with_and_without_quantizing() {
        let mut img = solid(64, 48, [10, 20, 30]);
        paint(&mut img, 8, 8, 40, 30, [250, 3, 129]);
        let exact = decode(&encode(&img, 1).unwrap()).unwrap();
        assert_eq!(exact, img);
        let q = decode(&encode(&img, REF_STEP).unwrap()).unwrap();
        assert!(
            compare(&img, &q).passes(),
            "quantized refs stay within tolerance of the original"
        );
        // 10 -> 8, 20 -> 24, 30 -> 32; 250 -> 248, 3 -> 0, 129 -> 128; 255 would clamp to 255.
        assert_eq!(&q.px[0..4], &[8, 24, 32, 255]);
        let o = ((8 * 64 + 8) * 4) as usize;
        assert_eq!(&q.px[o..o + 3], &[248, 0, 128]);
        assert_eq!(
            encode(&img, REF_STEP).unwrap(),
            encode(&img, REF_STEP).unwrap(),
            "deterministic bytes"
        );
    }

    #[test]
    fn quantize_clamps_at_255() {
        let q = decode(&encode(&solid(2, 2, [255, 252, 253]), 8).unwrap()).unwrap();
        assert_eq!(&q.px[0..3], &[255, 255, 255]);
    }

    #[test]
    fn decodes_rgba_and_rejects_other_formats() {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, 2, 1);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        }
        assert_eq!(decode(&out).unwrap().px, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let mut gray = Vec::new();
        {
            let mut e = png::Encoder::new(&mut gray, 1, 1);
            e.set_color(png::ColorType::Grayscale);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&[7]).unwrap();
        }
        assert!(decode(&gray).is_err());
        assert!(decode(b"not a png").is_err());
    }
}
