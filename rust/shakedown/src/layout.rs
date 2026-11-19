//! The layout tree and every layout rule. Every platform produces the same JSON (design section 6); the rules run
//! here, in Rust, on that tree. The web collector (`collector.js`) is the first producer.

use serde::{Deserialize, Serialize};

/// Rules, by name. Config ignores and `data-shakedown-ignore` use these names.
pub const RULES: [&str; 7] = [
    "sideways-scroll",
    "off-screen",
    "clipped-text",
    "text-overlap",
    "text-min-size",
    "tap-min-size",
    "layout-shift",
];

/// Text smaller than this (CSS px, points, dp) fails.
pub const MIN_TEXT: f64 = 11.0;
/// A tap target smaller than this either way fails (WCAG 2.2 target size).
pub const MIN_TAP: f64 = 24.0;
/// Two text lines overlapping by more than this both ways fail.
pub const OVERLAP: f64 = 2.0;
/// A shift below this (font-swap noise, a 1-3 px nudge) rounds to 0.000 and passes.
pub const SHIFT_EPS: f64 = 0.0005;
/// Slack for sub-pixel rounding.
const SLACK: f64 = 1.0;

/// x, y, width, height.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub struct Rect {
    /// Left.
    pub x: f64,
    /// Top.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    /// Right edge.
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    /// Bottom edge.
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    /// How far this and `o` overlap, horizontally and vertically (negative: apart).
    pub fn overlap(&self, o: &Self) -> (f64, f64) {
        (
            self.right().min(o.right()) - self.x.max(o.x),
            self.bottom().min(o.bottom()) - self.y.max(o.y),
        )
    }
}

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Draws text (may also be a tap target: `tap` wins).
    Text,
    /// Something the user presses.
    Tap,
    /// An image, canvas or svg.
    Image,
    /// A progress bar or spinner.
    Progress,
    /// Anything else visible.
    Container,
}

/// One visible element.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the collector's JSON node, one flag per rule input"
)]
pub struct Node {
    /// Stable across captures: the child-index path from the body (`0.3.1`).
    pub id: String,
    /// A short description for messages: `button.primary`.
    pub label: String,
    /// The first characters of its text.
    #[serde(default)]
    pub text: String,
    /// Kind.
    pub kind: Kind,
    /// Border box, viewport coordinates.
    pub rect: Rect,
    /// One rect per drawn text line (all of them).
    #[serde(default)]
    pub lines: Vec<Rect>,
    /// The text lines that are not covered by something else (a dialog).
    #[serde(default)]
    pub top_lines: Vec<Rect>,
    /// Font size in the platform's points.
    #[serde(default)]
    pub font_size: f64,
    /// The text is cut off.
    #[serde(default)]
    pub clipped: bool,
    /// Inside a marked horizontal scroller.
    #[serde(default)]
    pub scroll_x: bool,
    /// A link inside running text (exempt from the tap size rule).
    #[serde(default)]
    pub inline: bool,
    /// Inside an `aria-hidden="true"` subtree (decoration): two such nodes never count as overlapping.
    #[serde(default)]
    pub decor: bool,
    /// Rules switched off for this node, with the reason given.
    #[serde(default)]
    pub ignore: Vec<Ignored>,
}

/// An opt-out for one rule.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ignored {
    /// The rule.
    pub rule: String,
    /// Why (required).
    pub why: String,
}

/// One layout shift the browser reported.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Shift {
    /// The shift score.
    pub value: f64,
    /// Which elements moved, as `name before -> after`.
    #[serde(default)]
    pub sources: Vec<String>,
}

/// What the page reports about itself.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tree {
    /// Viewport width.
    pub viewport_w: f64,
    /// Viewport height.
    pub viewport_h: f64,
    /// Width of the content (`scrollWidth`).
    pub content_w: f64,
    /// Every visible element.
    pub nodes: Vec<Node>,
    /// Layout shifts since the first frame.
    #[serde(default)]
    pub shifts: Vec<Shift>,
    /// Opt-outs that gave no reason.
    #[serde(default)]
    pub bad_ignores: Vec<String>,
}

/// One thing wrong.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// The rule (or the kind of problem: `console-error`, `unfaked-call`, ...).
    pub rule: String,
    /// What is wrong, without anything that varies between runs.
    pub message: String,
    /// Where, for the report's overlay.
    pub boxes: Vec<[f64; 4]>,
}

impl Finding {
    /// A finding with no boxes.
    pub fn new(rule: &str, message: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            message: message.into(),
            boxes: Vec::new(),
        }
    }
    fn at(mut self, rects: &[Rect]) -> Self {
        self.boxes = rects.iter().map(|r| [r.x, r.y, r.w, r.h]).collect();
        self
    }
    /// The one-line form.
    pub fn line(&self) -> String {
        format!("{}: {}", self.rule, self.message)
    }
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn show(n: &Node) -> String {
    if n.text.is_empty() {
        n.label.clone()
    } else {
        format!("{} {:?}", n.label, n.text)
    }
}

fn skip(n: &Node, rule: &str) -> bool {
    n.ignore.iter().any(|i| i.rule == rule)
}

/// A is an ancestor of B, or the other way round (ids are child-index paths).
fn related(a: &str, b: &str) -> bool {
    let below =
        |p: &str, c: &str| c.len() > p.len() && c.starts_with(p) && c.as_bytes()[p.len()] == b'.';
    a == b || below(a, b) || below(b, a)
}

/// Runs every rule on the tree.
pub fn check(tree: &Tree) -> Vec<Finding> {
    let mut out = Vec::new();
    for bad in &tree.bad_ignores {
        out.push(Finding::new(
            "ignore-without-why",
            format!("{bad} opts out of a rule but gives no data-shakedown-why"),
        ));
    }
    if tree.content_w > tree.viewport_w + SLACK {
        out.push(Finding::new(
            "sideways-scroll",
            format!(
                "the page scrolls sideways: content {}px wide, viewport {}px",
                r1(tree.content_w),
                r1(tree.viewport_w)
            ),
        ));
    }
    off_screen(tree, &mut out);
    for n in &tree.nodes {
        if n.clipped && !skip(n, "clipped-text") {
            out.push(
                Finding::new("clipped-text", format!("text is cut off: {}", show(n))).at(&[n.rect]),
            );
        }
        if !n.lines.is_empty()
            && n.font_size > 0.0
            && n.font_size < MIN_TEXT
            && !skip(n, "text-min-size")
        {
            out.push(
                Finding::new(
                    "text-min-size",
                    format!(
                        "text is {}px, under {MIN_TEXT}px: {}",
                        r1(n.font_size),
                        show(n)
                    ),
                )
                .at(&[n.rect]),
            );
        }
        if n.kind == Kind::Tap
            && !n.inline
            && n.rect.w > 0.0
            && n.rect.h > 0.0
            && (n.rect.w < MIN_TAP || n.rect.h < MIN_TAP)
            && !skip(n, "tap-min-size")
        {
            out.push(
                Finding::new(
                    "tap-min-size",
                    format!(
                        "tap target is {}x{}px, under {MIN_TAP}px: {}",
                        r1(n.rect.w),
                        r1(n.rect.h),
                        show(n)
                    ),
                )
                .at(&[n.rect]),
            );
        }
    }
    overlaps(tree, &mut out);
    for s in tree.shifts.iter().filter(|s| s.value >= SHIFT_EPS) {
        out.push(Finding::new(
            "layout-shift",
            format!("layout shift {:.4}: {}", s.value, s.sources.join("; ")),
        ));
    }
    out
}

fn off_screen(tree: &Tree, out: &mut Vec<Finding>) {
    let mut flagged: Vec<&str> = Vec::new();
    for n in &tree.nodes {
        let outside =
            n.rect.w > 0.0 && (n.rect.right() > tree.viewport_w + SLACK || n.rect.x < -SLACK);
        if !outside || n.scroll_x || skip(n, "off-screen") {
            continue;
        }
        // Report the outermost element only: its children are outside because it is.
        if flagged.iter().any(|f| related(f, &n.id)) {
            continue;
        }
        flagged.push(&n.id);
        out.push(
            Finding::new(
                "off-screen",
                format!(
                    "{} is outside the screen: left {}, right {}, viewport {}px",
                    show(n),
                    r1(n.rect.x),
                    r1(n.rect.right()),
                    r1(tree.viewport_w)
                ),
            )
            .at(&[n.rect]),
        );
    }
}

fn overlaps(tree: &Tree, out: &mut Vec<Finding>) {
    let texts: Vec<&Node> = tree
        .nodes
        .iter()
        .filter(|n| !n.top_lines.is_empty() && !skip(n, "text-overlap"))
        .collect();
    for (i, a) in texts.iter().enumerate() {
        for b in &texts[i + 1..] {
            if related(&a.id, &b.id) || (a.decor && b.decor) {
                continue;
            }
            let hit = a.top_lines.iter().find_map(|r| {
                b.top_lines.iter().find_map(|q| {
                    let (ox, oy) = r.overlap(q);
                    (ox > OVERLAP && oy > OVERLAP).then_some([*r, *q])
                })
            });
            if let Some(pair) = hit {
                out.push(
                    Finding::new(
                        "text-overlap",
                        format!("text overlaps: {} / {}", show(a), show(b)),
                    )
                    .at(&pair),
                );
            }
        }
    }
}

/// Rule names in `findings`, sorted and de-duplicated.
#[cfg(test)]
pub fn rules_of(findings: &[Finding]) -> Vec<String> {
    let mut v: Vec<String> = findings.iter().map(|f| f.rule.clone()).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn node(id: &str, kind: Kind, r: Rect) -> Node {
        Node {
            id: id.into(),
            label: format!("<{id}>"),
            text: String::new(),
            kind,
            rect: r,
            lines: Vec::new(),
            top_lines: Vec::new(),
            font_size: 16.0,
            clipped: false,
            scroll_x: false,
            inline: false,
            decor: false,
            ignore: Vec::new(),
        }
    }

    fn text(id: &str, r: Rect) -> Node {
        let mut n = node(id, Kind::Text, r);
        n.text = "hello".into();
        n.lines = vec![r];
        n.top_lines = vec![r];
        n
    }

    fn tree(nodes: Vec<Node>) -> Tree {
        Tree {
            viewport_w: 390.0,
            viewport_h: 844.0,
            content_w: 390.0,
            nodes,
            shifts: Vec::new(),
            bad_ignores: Vec::new(),
        }
    }

    const NONE: [&str; 0] = [];

    fn rules(t: &Tree) -> Vec<String> {
        rules_of(&check(t))
    }

    #[test]
    fn a_clean_tree_passes() {
        let mut btn = node("0.1", Kind::Tap, rect(10.0, 100.0, 120.0, 44.0));
        btn.text = "Go".into();
        let t = tree(vec![text("0.0", rect(10.0, 10.0, 200.0, 20.0)), btn]);
        assert_eq!(rules(&t), NONE);
    }

    #[test]
    fn sideways_scroll() {
        let mut t = tree(vec![]);
        t.content_w = 391.5;
        assert_eq!(rules(&t), ["sideways-scroll"]);
        t.content_w = 390.5;
        assert!(rules(&t).is_empty(), "one pixel of rounding is fine");
    }

    #[test]
    fn off_screen_reports_the_outermost_and_honours_scrollers() {
        let mut t = tree(vec![
            node("0", Kind::Container, rect(0.0, 0.0, 500.0, 50.0)),
            node("0.1", Kind::Container, rect(400.0, 0.0, 50.0, 50.0)),
        ]);
        assert_eq!(check(&t).len(), 1);
        t.nodes[0].scroll_x = true;
        t.nodes[1].scroll_x = true;
        assert_eq!(rules(&t), NONE);
        let left = tree(vec![node(
            "1",
            Kind::Container,
            rect(-20.0, 0.0, 50.0, 50.0),
        )]);
        assert_eq!(rules(&left), ["off-screen"]);
    }

    #[test]
    fn clipped_text() {
        let mut n = text("0", rect(0.0, 0.0, 50.0, 20.0));
        n.clipped = true;
        assert_eq!(rules(&tree(vec![n])), ["clipped-text"]);
    }

    #[test]
    fn text_overlap_needs_more_than_two_pixels_both_ways_and_skips_containment() {
        let a = text("0.0", rect(10.0, 10.0, 100.0, 20.0));
        let mut b = text("0.1", rect(10.0, 25.0, 100.0, 20.0));
        assert_eq!(rules(&tree(vec![a.clone(), b.clone()])), ["text-overlap"]);
        b.lines = vec![rect(10.0, 29.0, 100.0, 20.0)];
        b.top_lines = b.lines.clone();
        assert_eq!(rules(&tree(vec![a.clone(), b])), NONE, "1px overlap passes");
        let inner = text("0.0.4", rect(10.0, 10.0, 50.0, 20.0));
        assert_eq!(rules(&tree(vec![a, inner])), NONE, "a parent and its child");
        let covered = {
            let mut c = text("0.2", rect(10.0, 10.0, 100.0, 20.0));
            c.top_lines.clear();
            c
        };
        let a = text("0.0", rect(10.0, 10.0, 100.0, 20.0));
        assert_eq!(rules(&tree(vec![a, covered])), NONE, "behind a dialog");
    }

    #[test]
    fn ids_are_paths_not_prefixes() {
        assert!(related("0.1", "0.1.2"));
        assert!(!related("0.1", "0.12"));
    }

    #[test]
    fn text_min_size() {
        let mut n = text("0", rect(0.0, 0.0, 50.0, 20.0));
        n.font_size = 10.0;
        assert_eq!(rules(&tree(vec![n.clone()])), ["text-min-size"]);
        n.font_size = 11.0;
        assert_eq!(rules(&tree(vec![n])), NONE);
    }

    #[test]
    fn tap_min_size_and_inline_links() {
        let mut n = node("0", Kind::Tap, rect(0.0, 0.0, 23.0, 40.0));
        assert_eq!(rules(&tree(vec![n.clone()])), ["tap-min-size"]);
        n.rect = rect(0.0, 0.0, 24.0, 24.0);
        assert_eq!(rules(&tree(vec![n.clone()])), NONE);
        n.rect = rect(0.0, 0.0, 10.0, 10.0);
        n.inline = true;
        assert_eq!(rules(&tree(vec![n])), NONE);
    }

    #[test]
    fn layout_shift_ignores_font_swap_noise() {
        let mut t = tree(vec![]);
        t.shifts = vec![Shift {
            value: 0.0004,
            sources: vec![],
        }];
        assert_eq!(rules(&t), NONE);
        t.shifts = vec![Shift {
            value: 0.0005,
            sources: vec!["div 0,0 1x1 -> 0,5 1x1".into()],
        }];
        assert_eq!(rules(&t), ["layout-shift"]);
    }

    #[test]
    fn an_ignore_switches_one_rule_off_for_one_node() {
        let mut n = text("0", rect(0.0, 0.0, 50.0, 20.0));
        n.clipped = true;
        n.font_size = 9.0;
        n.ignore = vec![Ignored {
            rule: "clipped-text".into(),
            why: "scrolls".into(),
        }];
        assert_eq!(rules(&tree(vec![n])), ["text-min-size"]);
    }

    #[test]
    fn an_ignore_without_a_reason_is_a_finding() {
        let mut t = tree(vec![]);
        t.bad_ignores = vec!["<div.x>".into()];
        assert_eq!(rules(&t), ["ignore-without-why"]);
    }

    #[test]
    fn every_rule_has_a_name() {
        assert_eq!(RULES.len(), 7);
    }
}
