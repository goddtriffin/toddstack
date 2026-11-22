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
    /// Peak machine-wide CPU (times 100) of other programs above the pre-start baseline: work hosted outside the
    /// process tree (Docker's VM). `peak_centi_cores` is the larger of this and the tree's peak.
    #[serde(default)]
    pub peak_other_centi_cores: u64,
    /// Peak machine-wide memory rise over the pre-start baseline: work hosted outside the process tree (Docker's
    /// or WSL's VM). `peak_tree_rss` is the larger of this and the tree's peak.
    #[serde(default)]
    pub peak_other_mem: u64,
    /// Milliseconds after the start at which the memory peak was seen (0 when unknown): how long a learned
    /// reservation's unused part is held.
    #[serde(default)]
    pub peak_mem_at_ms: u64,
    /// Milliseconds after the start at which the process tree's CPU first reached 3/4 of its peak (0 when unknown):
    /// how long the job settles (`admit::settle_window_ms`).
    #[serde(default)]
    pub peak_cpu_at_ms: u64,
    /// The process tree's sustained CPU (times 100): its highest mean over any 10 s window (`admit::sustained_level`).
    /// 0 when unknown (entries from before 0.4.8, and Docker/VM work whose CPU shows outside the tree).
    #[serde(default)]
    pub sustained_centi_cores: u64,
    pub blocks_in: u64,
    pub blocks_out: u64,
    /// Machine-wide, not just this job.
    pub net_in: u64,
    pub net_out: u64,
    /// Machine-wide GPU memory in use, peak during the run (monitoring only: it includes every other program's VRAM).
    pub peak_vram: Option<u64>,
    /// The job's own GPU memory: the machine-wide peak minus the VRAM in use just before the job started, 0 under
    /// `run::VRAM_NOISE`. The only VRAM that teaches a size (0.4.7). `None` without an NVIDIA GPU and in entries from
    /// before 0.4.7, whose `peak_vram` alone counted other programs' VRAM as the job's.
    #[serde(default)]
    pub vram_rise: Option<u64>,
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
    let max_rss = if cfg!(target_os = "macos") {
        rss
    } else {
        rss * 1024
    };
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
    let timing = if s.queued_ms >= SIGNIFICANT_QUEUE_MS {
        format!("queued {}, ran {}", secs(s.queued_ms), secs(s.run_ms))
    } else {
        format!("ran {}", secs(s.run_ms))
    };
    let mut out = format!(
        "stokehold: {timing}\nstokehold: cpu {} user + {} sys, avg {}.{:02} cores, peak {}.{:02} cores\nstokehold: peak rss {} (whole process tree) / {} (largest process), disk blocks read {} written {}\nstokehold: network (machine-wide) in {} out {}",
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
        let _ = write!(out, "\nstokehold: peak vram (machine-wide) {}", fmt_size(v));
        if let Some(r) = s.vram_rise {
            let _ = write!(out, ", {} above its start", fmt_size(r));
        }
    }
    if let Some(v) = s.steal_ms {
        let _ = write!(out, "\nstokehold: cpu steal (machine-wide) {}", secs(v));
    }
    if s.timed_out {
        let _ = write!(
            out,
            "\nstokehold: TIMED OUT after {} (SIGTERM to the process group, SIGKILL after the grace period)",
            secs(s.run_ms)
        );
    }
    out.push_str("\nstokehold: ");
    out.push_str(REVIEW_PROMPT);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_averages() {
        let before = ChildUsage::default();
        let after = ChildUsage {
            user_us: 6_000_000,
            sys_us: 2_000_000,
            max_rss: 1 << 30,
            blocks_in: 5,
            blocks_out: 7,
        };
        let s = compute(1500, 4000, before, after);
        assert_eq!(s.centi_cores, 200);
        assert_eq!((s.cpu_user_ms, s.cpu_sys_ms, s.blocks_out), (6000, 2000, 7));
    }

    #[test]
    fn formats_report() {
        let s = Stats {
            queued_ms: 1500,
            run_ms: 4000,
            cpu_user_ms: 6000,
            cpu_sys_ms: 2000,
            centi_cores: 250,
            peak_rss: 1 << 30,
            peak_other_centi_cores: 0,
            peak_other_mem: 0,
            peak_mem_at_ms: 0,
            peak_cpu_at_ms: 0,
            sustained_centi_cores: 0,
            peak_tree_rss: 3 << 30,
            peak_centi_cores: 450,
            blocks_in: 5,
            blocks_out: 7,
            net_in: 2048,
            net_out: 0,
            peak_vram: Some(1 << 30),
            vram_rise: Some(0),
            steal_ms: Some(300),
            timed_out: true,
        };
        let text = format_stats(&s);
        assert!(text.contains("queued 1.5s, ran 4.0s"), "{text}");
        assert!(text.contains("avg 2.50 cores"), "{text}");
        assert!(text.contains("peak 4.50 cores"), "{text}");
        assert!(
            text.contains("peak rss 3.0G (whole process tree) / 1.0G (largest process)"),
            "{text}"
        );
        assert!(
            text.contains("network (machine-wide) in 2.0K out 0B"),
            "{text}"
        );
        assert!(
            text.contains("peak vram (machine-wide) 1.0G, 0B above its start"),
            "{text}"
        );
        assert!(text.contains("steal (machine-wide) 0.3s"), "{text}");
        assert!(text.contains("TIMED OUT"), "{text}");
        assert!(text.ends_with(REVIEW_PROMPT), "{text}");
        let quick = format_stats(&Stats {
            queued_ms: 200,
            run_ms: 1000,
            ..Stats::default()
        });
        assert!(
            quick.contains("stokehold: ran 1.0s") && !quick.contains("queued"),
            "{quick}"
        );
    }
}
