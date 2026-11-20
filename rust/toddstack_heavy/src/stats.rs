//! What a run used: rusage of the waited child, machine-wide network, and peak VRAM.

use std::fmt::Write;

use nix::sys::resource::{UsageWho, getrusage};
use nix::sys::time::TimeValLike;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::machine::to_u64;
use crate::sizes::fmt_size;

/// Queued time is only worth printing past this.
pub const SIGNIFICANT_QUEUE_MS: u64 = 1000;

pub const REVIEW_PROMPT: &str = "Review these stats: do they match what you expected this run to do? If anything looks slow, oversized or wrong, file an issue to fix it.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Stats {
    pub queued_ms: u64,
    pub run_ms: u64,
    pub cpu_user_ms: u64,
    pub cpu_sys_ms: u64,
    /// Average cores used, times 100 (cpu time over wall time).
    pub centi_cores: u64,
    /// The largest single process (`ru_maxrss`).
    pub peak_rss: u64,
    /// Peak of the whole process tree's RSS, sampled while the job ran.
    #[serde(default)]
    pub peak_tree_rss: u64,
    /// Peak cores (times 100) the whole process tree used at once, sampled while the job ran.
    #[serde(default)]
    pub peak_centi_cores: u64,
    pub blocks_in: u64,
    pub blocks_out: u64,
    /// Machine-wide, not just this job.
    pub net_in: u64,
    pub net_out: u64,
    /// Machine-wide GPU memory in use, peak during the run.
    pub peak_vram: Option<u64>,
    /// Machine-wide CPU steal during the run (Linux).
    pub steal_ms: Option<u64>,
    pub timed_out: bool,
}

/// Cumulative usage of all waited-for children of this process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChildUsage {
    pub user_us: u64,
    pub sys_us: u64,
    pub max_rss: u64,
    pub blocks_in: u64,
    pub blocks_out: u64,
}

/// # Errors
/// Fails when `getrusage` does.
pub fn children_usage() -> Result<ChildUsage, Error> {
    let u = getrusage(UsageWho::RUSAGE_CHILDREN)?;
    let rss = to_u64(u.max_rss());
    // macOS reports bytes, Linux kilobytes.
    let max_rss = if cfg!(target_os = "macos") { rss } else { rss * 1024 };
    Ok(ChildUsage {
        user_us: to_u64(u.user_time().num_microseconds()),
        sys_us: to_u64(u.system_time().num_microseconds()),
        max_rss,
        blocks_in: to_u64(u.block_reads()),
        blocks_out: to_u64(u.block_writes()),
    })
}

/// Builds the stats from before/after samples.
#[must_use]
pub fn compute(queued_ms: u64, run_ms: u64, before: ChildUsage, after: ChildUsage) -> Stats {
    let user = after.user_us.saturating_sub(before.user_us);
    let sys = after.sys_us.saturating_sub(before.sys_us);
    let cpu_ms = (user + sys) / 1000;
    Stats {
        queued_ms,
        run_ms,
        cpu_user_ms: user / 1000,
        cpu_sys_ms: sys / 1000,
        centi_cores: (cpu_ms * 100).checked_div(run_ms).unwrap_or(0),
        peak_rss: after.max_rss,
        blocks_in: after.blocks_in.saturating_sub(before.blocks_in),
        blocks_out: after.blocks_out.saturating_sub(before.blocks_out),
        ..Stats::default()
    }
}

fn secs(ms: u64) -> String {
    format!("{}.{}s", ms / 1000, ms % 1000 / 100)
}

/// The stderr report.
#[must_use]
pub fn format_stats(s: &Stats) -> String {
    let timing = if s.queued_ms >= SIGNIFICANT_QUEUE_MS { format!("queued {}, ran {}", secs(s.queued_ms), secs(s.run_ms)) } else { format!("ran {}", secs(s.run_ms)) };
    let mut out = format!(
        "heavy: {timing}\nheavy: cpu {} user + {} sys, avg {}.{:02} cores, peak {}.{:02} cores\nheavy: peak rss {} (whole process tree) / {} (largest process), disk blocks read {} written {}\nheavy: network (machine-wide) in {} out {}",
        secs(s.cpu_user_ms),
        secs(s.cpu_sys_ms),
        s.centi_cores / 100,
        s.centi_cores % 100,
        s.peak_centi_cores / 100,
        s.peak_centi_cores % 100,
        fmt_size(s.peak_tree_rss),
        fmt_size(s.peak_rss),
        s.blocks_in,
        s.blocks_out,
        fmt_size(s.net_in),
        fmt_size(s.net_out),
    );
    if let Some(v) = s.peak_vram {
        let _ = write!(out, "\nheavy: peak vram (machine-wide) {}", fmt_size(v));
    }
    if let Some(v) = s.steal_ms {
        let _ = write!(out, "\nheavy: cpu steal (machine-wide) {}", secs(v));
    }
    if s.timed_out {
        let _ = write!(out, "\nheavy: TIMED OUT after {} (SIGTERM to the process group, SIGKILL after the grace period)", secs(s.run_ms));
    }
    out.push_str("\nheavy: ");
    out.push_str(REVIEW_PROMPT);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_averages() {
        let before = ChildUsage::default();
        let after = ChildUsage { user_us: 6_000_000, sys_us: 2_000_000, max_rss: 1 << 30, blocks_in: 5, blocks_out: 7 };
        let s = compute(1500, 4000, before, after);
        assert_eq!(s.centi_cores, 200);
        assert_eq!((s.cpu_user_ms, s.cpu_sys_ms, s.blocks_out), (6000, 2000, 7));
    }

    #[test]
    fn formats_report() {
        let s = Stats { queued_ms: 1500, run_ms: 4000, cpu_user_ms: 6000, cpu_sys_ms: 2000, centi_cores: 250, peak_rss: 1 << 30, peak_tree_rss: 3 << 30, peak_centi_cores: 450, blocks_in: 5, blocks_out: 7, net_in: 2048, net_out: 0, peak_vram: Some(1 << 30), steal_ms: Some(300), timed_out: true };
        let text = format_stats(&s);
        assert!(text.contains("queued 1.5s, ran 4.0s"), "{text}");
        assert!(text.contains("avg 2.50 cores"), "{text}");
        assert!(text.contains("peak 4.50 cores"), "{text}");
        assert!(text.contains("peak rss 3.0G (whole process tree) / 1.0G (largest process)"), "{text}");
        assert!(text.contains("network (machine-wide) in 2.0K out 0B"), "{text}");
        assert!(text.contains("peak vram (machine-wide) 1.0G"), "{text}");
        assert!(text.contains("steal (machine-wide) 0.3s"), "{text}");
        assert!(text.contains("TIMED OUT"), "{text}");
        assert!(text.ends_with(REVIEW_PROMPT), "{text}");
        let quick = format_stats(&Stats { queued_ms: 200, run_ms: 1000, ..Stats::default() });
        assert!(quick.contains("heavy: ran 1.0s") && !quick.contains("queued"), "{quick}");
    }
}
