//! Contact sheets for the review pack (design section 11): one route in one theme, every device tiled at one
//! scale with its name burned in, at most 1568 px on the long edge (the size Claude reads without downscaling).
//! The sheet is laid out here, written as HTML, and captured with the pinned Chrome, so no font or drawing crate
//! is needed.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::page::ScreenshotParams;

use crate::capture;
use crate::error::Error;
use crate::report::esc;

/// The longest edge of a sheet.
pub const MAX_EDGE: u32 = 1568;
/// Space between tiles and around the sheet.
pub const GAP: u32 = 8;
/// Height of a tile's name line.
pub const LABEL: u32 = 16;
/// Width of one character of the name line (12 px monospace).
const CHAR_W: u32 = 7;

/// One device's picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The device.
    pub name: String,
    /// The shot's width in pixels.
    pub w: u32,
    /// The shot's height in pixels.
    pub h: u32,
}

impl Frame {
    /// What is burned in: the device and its real size.
    pub fn caption(&self) -> String {
        format!("{} {}x{}", self.name, self.w, self.h)
    }
}

/// Where a frame goes on the sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    /// Index into the frames given to [`layout`].
    pub frame: usize,
    /// Left edge of the tile (the name line and the picture).
    pub x: u32,
    /// Top edge of the tile.
    pub y: u32,
    /// Picture width after scaling.
    pub w: u32,
    /// Picture height after scaling.
    pub h: u32,
}

/// A laid out sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    /// Sheet width.
    pub w: u32,
    /// Sheet height.
    pub h: u32,
    /// The one scale every picture has.
    pub scale: f64,
    /// Tiles, in frame order.
    pub tiles: Vec<Tile>,
}

fn caption_w(f: &Frame) -> u32 {
    u32::try_from(f.caption().len()).unwrap_or(u32::MAX) * CHAR_W + 4
}

fn scaled(v: u32, s: f64) -> u32 {
    // Rounded down, at least one pixel; sheets are small, so the cast cannot truncate.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a pixel size below 100000"
    )]
    let n = (f64::from(v) * s).floor() as u32;
    n.max(1)
}

/// Tries one scale: rows of tiles from the tallest down, wrapping at the width limit. `None` when it does not fit.
fn pack(frames: &[Frame], order: &[usize], s: f64, max_edge: u32) -> Option<Sheet> {
    let mut tiles = vec![
        Tile {
            frame: 0,
            x: 0,
            y: 0,
            w: 0,
            h: 0
        };
        frames.len()
    ];
    let (mut x, mut y, mut row_h, mut width) = (GAP, GAP, 0, 0);
    for &i in order {
        let f = &frames[i];
        let (tw, th) = (scaled(f.w, s), scaled(f.h, s));
        let cell = tw.max(caption_w(f));
        if cell + 2 * GAP > max_edge {
            return None;
        }
        if x + cell + GAP > max_edge {
            y += row_h + GAP;
            x = GAP;
            row_h = 0;
        }
        tiles[i] = Tile {
            frame: i,
            x,
            y,
            w: tw,
            h: th,
        };
        x += cell + GAP;
        width = width.max(x);
        row_h = row_h.max(LABEL + th);
    }
    let height = y + row_h + GAP;
    (height <= max_edge).then_some(Sheet {
        w: width,
        h: height,
        scale: s,
        tiles,
    })
}

/// Lays frames out at the largest single scale whose sheet stays within `max_edge` on both edges.
/// Deterministic: the same frames give the same sheet.
pub fn layout(frames: &[Frame], max_edge: u32) -> Sheet {
    let mut order: Vec<usize> = (0..frames.len()).collect();
    order.sort_by(|&a, &b| (frames[b].h, &frames[a].name).cmp(&(frames[a].h, &frames[b].name)));
    let mut s = 1.0_f64;
    for _ in 0..400 {
        if let Some(sheet) = pack(frames, &order, s, max_edge) {
            return sheet;
        }
        s *= 0.97;
    }
    // Unreachable for real device lists (a sheet of nothing but 1 px tiles fits); an empty sheet is the safe answer.
    Sheet {
        w: 2 * GAP,
        h: 2 * GAP,
        scale: 0.0,
        tiles: Vec::new(),
    }
}

/// One picture for the sheet: its frame, its PNG (empty when the page could not be drawn), and whether the
/// automated rules found anything.
pub struct Picture {
    /// Device and size.
    pub frame: Frame,
    /// The PNG bytes.
    pub png: Vec<u8>,
    /// The rules found something here (the tile is outlined).
    pub flagged: bool,
}

/// The sheet as HTML: absolutely placed tiles, pictures as data URIs.
pub fn html(sheet: &Sheet, pictures: &[Picture]) -> String {
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><style>html,body{{margin:0;background:#2b2b2b}}body{{width:{}px;height:{}px;position:relative;overflow:hidden;font:12px/{LABEL}px ui-monospace,Menlo,monospace}}\
.t{{position:absolute}}.n{{height:{LABEL}px;color:#eee;white-space:nowrap;overflow:hidden}}.t.f .n{{color:#ff6b6b;font-weight:700}}\
.t img,.t .x{{display:block;background:#fff}}.t.f img,.t.f .x{{outline:2px solid #ff3b3b;outline-offset:-2px}}.x{{background:#555;color:#fff}}</style>",
        sheet.w, sheet.h
    );
    for t in &sheet.tiles {
        let Some(p) = pictures.get(t.frame) else {
            continue;
        };
        let class = if p.flagged { "t f" } else { "t" };
        let mark = if p.flagged { "! " } else { "" };
        let _ = write!(
            h,
            "<div class=\"{class}\" style=\"left:{}px;top:{}px\"><div class=n>{mark}{}</div>",
            t.x,
            t.y,
            esc(&p.frame.caption())
        );
        if p.png.is_empty() {
            let _ = write!(
                h,
                "<div class=x style=\"width:{}px;height:{}px\">not drawn</div>",
                t.w, t.h
            );
        } else {
            let _ = write!(
                h,
                "<img style=\"width:{}px;height:{}px\" src=\"data:image/png;base64,{}\">",
                t.w,
                t.h,
                capture::base64(&p.png)
            );
        }
        h.push_str("</div>");
    }
    h
}

/// Captures sheets with the pinned Chrome: one browser for all of them. Each item is the sheet's HTML and its size;
/// the PNGs come back in the same order.
///
/// # Errors
///
/// Chrome does not start or a capture fails.
pub async fn capture_all(
    chrome: &Path,
    items: &[(String, u32, u32)],
) -> Result<Vec<Vec<u8>>, Error> {
    let profile = std::env::temp_dir().join(format!("shakedown-sheets-{}", std::process::id()));
    // Dropping the guard (an error, or the caller dropping this future on a caught signal) kills Chrome and removes
    // the profile.
    let mut chromes = capture::Chromes::default();
    let browser = chromes.launch(chrome, &profile).await?;
    let result = async {
        let env = |e: String| Error::Environment(format!("sheet: {e}"));
        // Several pages of the one browser draw at once; item `i` goes to page `i mod pages`, and the PNGs are put
        // back in item order, so the result is the same as drawing them one after another.
        let pages = capture::parallel().min(items.len()).max(1);
        let worker = |k: usize| {
            let browser = &browser;
            async move {
                // A context of its own: a page in a shared window is hidden behind its siblings and never draws a frame.
                let ctx = browser
                    .create_browser_context(CreateBrowserContextParams::default())
                    .await
                    .map_err(|e| Error::Environment(format!("sheet context: {e}")))?;
                let target = CreateTargetParams::builder()
                    .url("about:blank")
                    .browser_context_id(ctx)
                    .build()
                    .map_err(Error::Environment)?;
                let page = browser
                    .new_page(target)
                    .await
                    .map_err(|e| Error::Environment(format!("sheet page: {e}")))?;
                let mut out = Vec::new();
                for (i, (html, w, h)) in items.iter().enumerate().skip(k).step_by(pages) {
                    page.execute(
                        SetDeviceMetricsOverrideParams::builder()
                            .width(i64::from(*w))
                            .height(i64::from(*h))
                            .device_scale_factor(1.0)
                            .mobile(false)
                            .build()
                            .map_err(env)?,
                    )
                    .await
                    .map_err(|e| env(e.to_string()))?;
                    page.set_content(html.as_str())
                        .await
                        .map_err(|e| env(e.to_string()))?;
                    // Every picture decoded before the capture.
                    page.evaluate("Promise.all([...document.images].map((i) => i.decode().catch(() => {}))).then(() => true)")
                        .await
                        .map_err(|e| env(e.to_string()))?;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let png = page
                        .screenshot(ScreenshotParams::from(
                            CaptureScreenshotParams::builder()
                                .format(CaptureScreenshotFormat::Png)
                                .build(),
                        ))
                        .await
                        .map_err(|e| env(e.to_string()))?;
                    out.push((i, png));
                }
                Ok::<_, Error>(out)
            }
        };
        let done = futures::future::try_join_all((0..pages).map(worker)).await?;
        let mut all: Vec<(usize, Vec<u8>)> = done.into_iter().flatten().collect();
        all.sort_by_key(|(i, _)| *i);
        Ok::<_, Error>(all.into_iter().map(|(_, png)| png).collect::<Vec<_>>())
    }
    .await;
    for browser in chromes.take_browsers() {
        if let Ok(mut b) = Arc::try_unwrap(browser) {
            let _ = b.kill().await;
            let _ = b.wait().await;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> Vec<Frame> {
        [
            ("desktop", 1440, 900),
            ("laptop", 1280, 720),
            ("tablet", 820, 1180),
            ("tablet-land", 1180, 820),
            ("phone", 390, 844),
            ("phone-land", 844, 390),
            ("small-phone", 375, 667),
            ("fold-folded", 344, 882),
            ("duo-open-land", 951, 585),
        ]
        .iter()
        .map(|(n, w, h)| Frame {
            name: (*n).into(),
            w: *w,
            h: *h,
        })
        .collect()
    }

    #[test]
    fn the_long_edge_is_at_most_the_limit() {
        let s = layout(&frames(), MAX_EDGE);
        assert!(s.w <= MAX_EDGE && s.h <= MAX_EDGE, "{}x{}", s.w, s.h);
        assert!(s.w.max(s.h) > MAX_EDGE / 2, "sheet should use the space");
    }

    #[test]
    fn every_tile_has_the_same_scale_and_nothing_overlaps() {
        let f = frames();
        let s = layout(&f, MAX_EDGE);
        assert_eq!(s.tiles.len(), f.len());
        for t in &s.tiles {
            let fr = &f[t.frame];
            assert_eq!(t.w, scaled(fr.w, s.scale));
            assert_eq!(t.h, scaled(fr.h, s.scale));
            assert!(t.x + t.w <= s.w && t.y + LABEL + t.h <= s.h, "{t:?}");
        }
        for (i, a) in s.tiles.iter().enumerate() {
            for b in &s.tiles[i + 1..] {
                let apart = a.x + a.w.max(caption_w(&f[a.frame])) <= b.x
                    || b.x + b.w.max(caption_w(&f[b.frame])) <= a.x
                    || a.y + LABEL + a.h <= b.y
                    || b.y + LABEL + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn it_is_deterministic_and_does_not_depend_on_input_order() {
        let mut f = frames();
        let a = layout(&f, MAX_EDGE);
        assert_eq!(a, layout(&f, MAX_EDGE));
        f.reverse();
        let b = layout(&f, MAX_EDGE);
        assert!((a.scale - b.scale).abs() < f64::EPSILON);
        assert_eq!((a.w, a.h), (b.w, b.h));
    }

    #[test]
    fn a_single_small_frame_is_not_scaled_up_or_down() {
        let f = vec![Frame {
            name: "phone".into(),
            w: 390,
            h: 844,
        }];
        let s = layout(&f, MAX_EDGE);
        assert!((s.scale - 1.0).abs() < f64::EPSILON);
        assert_eq!((s.tiles[0].w, s.tiles[0].h), (390, 844));
    }

    #[test]
    fn a_smaller_limit_shrinks_everything() {
        let big = layout(&frames(), MAX_EDGE);
        let small = layout(&frames(), 800);
        assert!(small.scale < big.scale);
        assert!(small.w <= 800 && small.h <= 800);
    }

    #[test]
    fn the_html_burns_in_names_and_outlines_flagged_tiles() {
        let f = frames();
        let sheet = layout(&f, MAX_EDGE);
        let pictures: Vec<Picture> = f
            .iter()
            .enumerate()
            .map(|(i, fr)| Picture {
                frame: fr.clone(),
                png: if i == 0 {
                    Vec::new()
                } else {
                    vec![137, 80, 78, 71]
                },
                flagged: i == 1,
            })
            .collect();
        let h = html(&sheet, &pictures);
        assert!(h.contains("desktop 1440x900") && h.contains("not drawn"));
        assert!(h.contains("class=\"t f\"") && h.contains("! laptop 1280x720"));
        assert!(h.contains("data:image/png;base64,iVBORw=="));
        assert_eq!(h.matches("<img").count(), f.len() - 1);
    }
}
