//! The pixel-diff budget the arena measured, and the comparison.

/// Pixel differences between two RGBA8 images: share of pixels whose worst colour channel differs
/// by more than 1, 8 and 32 levels (percent), and the largest difference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Diff {
    /// Percent of pixels over 1 level.
    pub over_1: f64,
    /// Percent of pixels over 8 levels.
    pub over_8: f64,
    /// Percent of pixels over 32 levels.
    pub over_32: f64,
    /// Largest per-channel difference.
    pub max: u8,
}

/// Limits a [`Diff`] must stay under.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    /// Percent over 1 level.
    pub over_1: f64,
    /// Percent over 8 levels.
    pub over_8: f64,
    /// Percent over 32 levels.
    pub over_32: f64,
    /// Largest difference.
    pub max: u8,
}

impl Budget {
    /// The arena's budget (about 2x the measured gap of 0.909% / 0.258% / 0% / 31).
    pub const ARENA: Self = Self {
        over_1: 2.0,
        over_8: 0.6,
        over_32: 0.05,
        max: 48,
    };

    /// For scenes full of path edges: the GPU fills with 16 samples a pixel and tiny-skia with its
    /// own 16, so edge pixels differ by a coverage step (measured 0.76% / 0.71% / 0.006% / 41).
    pub const PATHS: Self = Self {
        over_1: 2.0,
        over_8: 1.0,
        over_32: 0.05,
        max: 48,
    };

    /// For a kernel's output drawn in a scene: float rounding in the GPU kernel can flip a
    /// nearest-palette pick on a few pixels (measured 0.08% / 0.04% / 0.005% / 120).
    pub const KERNEL: Self = Self {
        over_1: 0.2,
        over_8: 0.1,
        over_32: 0.02,
        max: 255,
    };
}

impl Diff {
    /// Compares two RGBA8 images (alpha ignored). Images of different length compare as total failure.
    #[must_use]
    pub fn between(a: &[u8], b: &[u8]) -> Self {
        if a.len() != b.len() || a.is_empty() {
            return Self {
                over_1: 100.0,
                over_8: 100.0,
                over_32: 100.0,
                max: 255,
            };
        }
        let (mut n1, mut n8, mut n32, mut max): (u64, u64, u64, u8) = (0, 0, 0, 0);
        for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
            let d: u8 = (0..3).map(|i| p[i].abs_diff(q[i])).max().unwrap_or(0);
            n1 += u64::from(d > 1);
            n8 += u64::from(d > 8);
            n32 += u64::from(d > 32);
            max = max.max(d);
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "pixel counts are far below 2^52"
        )]
        let pct = |n: u64| 100.0 * n as f64 / (a.len() / 4) as f64;
        Self {
            over_1: pct(n1),
            over_8: pct(n8),
            over_32: pct(n32),
            max,
        }
    }

    /// True when every number is within the budget.
    #[must_use]
    pub fn within(&self, budget: &Budget) -> bool {
        self.over_1 <= budget.over_1
            && self.over_8 <= budget.over_8
            && self.over_32 <= budget.over_32
            && self.max <= budget.max
    }
}

impl core::fmt::Display for Diff {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            ">1 {:.3}%  >8 {:.3}%  >32 {:.3}%  max {}",
            self.over_1, self.over_8, self.over_32, self.max
        )
    }
}
