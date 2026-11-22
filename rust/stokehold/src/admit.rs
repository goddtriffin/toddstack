//! Admission: pure decisions.
//!
//! CPU is closed-loop (0.4.10): a job starts while the machine's measured CPU (`Live::cpu_sustained_pct`, the larger
//! of the latest reading and the mean of the last `BUSY_WINDOW_MS`) is under `CPU_ADMIT_PCT`. A job's learned or
//! declared CPU size counts only while it settles: for `settle_window_ms` after a start (10 s, or the learned time to
//! its CPU peak, at most 60 s; 30 s for cold and Docker/VM jobs) the machine has not shown that job's CPU yet, so
//! another job starts then only if the measured CPU, the settling jobs' sizes and its own size together stay under
//! the admit line. After that only what is measured counts, with two backstops: the measured busy cores plus the new
//! job's typical cores stay within `SIZE_PCT` of the machine, and the running jobs' expected peaks stay within
//! `PEAK_SUM_CAP` x cores.
//!
//! Before 0.4.10 admission reserved each job's learned peak (and a sustained floor) for as long as the peak might
//! come. Jobs here are very bursty: `make lint` learned a 13-core peak and averaged 1.4 cores over 15 minutes,
//! `cargo test` 16 and 2.4. Reserving peaks left the Mac underused 86% of the time jobs waited (49% CPU on average),
//! and the bursts still pinned it now and then, because nothing admitted before a burst can be taken back. Nine
//! rounds of special cases (ramp holds, plateaus, floors, a fast lane, backfill and short-pass rules) moved the
//! numbers without changing that.
//!
//! Memory is judged by measured headroom: 95% of RAM minus non-heavy use, minus each running job's live use, minus
//! the part of each running job's reservation it has not yet used (its unmet part). A declared (`--mem`) size is held
//! for the whole run. A learned size is held until the job's learned time to peak (`mem_peak_at_ms`; at least
//! `SETTLE_MS`, and `MEM_FALLBACK_HOLD_MS` when unknown), then decays linearly to nothing over `MEM_DECAY_MS`. Only
//! the decaying tails are capped, together at `MEM_HELD_CAP` of the free memory. VRAM is reservation-based, netted
//! against live use (`vram_reserved`), within 90% of capacity.
//!
//! The queue: a waiter never starts while an older waiter could start now or is settling-blocked (it goes first), nor
//! while an older waiter has waited `QUEUE_PASS_MS` for memory, VRAM or the size check (younger jobs would keep taking
//! it). A waiter from a release before 0.4.10 (`Job::v` 0) never holds the queue. Behind an
//! older waiter blocked by the CPU line or a guard it may go: the same line and guards apply to it.
//!
//! Live guards (`guard_block`) veto: a CPU pinned within `PINNED_WINDOW_MS`, a 1-minute load of 2-6 x cores as
//! described there, memory pressure and memory used at 95%. With nothing running a job always passes the resource
//! checks, so a busy desktop cannot starve it; the guards still apply.

use std::collections::VecDeque;

use crate::job::{Capacity, JOB_VERSION, Job, Live, State, Usage, Weights};

/// Memory target: jobs start while measured use plus holds stays within this % of RAM, and memory used at this % blocks.
const MEM_PCT: u64 = 95;
/// VRAM reservations stay within this % of capacity.
const VRAM_PCT: u64 = 90;
/// macOS memory pressure level 2 ("warn") blocks only while memory used is at least this %. Level 2 is common and sticky:
/// on 2026-11-21 it held for hours at 49-58% used with 1.8 of 3 GB swap, and vetoed every start (40+ minutes with
/// nothing running at 27% CPU). Level 4 ("critical") always blocks.
const WARN_MEM_PCT: u64 = 85;
/// How far back the shared CPU readings count towards the sustained CPU.
pub const BUSY_WINDOW_MS: i64 = 20_000;
/// The CPU counts as pinned at or over this % busy: nothing new starts (0.4.8). Bursty jobs swing the CPU between 35%
/// and 99% within seconds, and starts landed in the dips.
pub const CPU_PINNED_PCT: u64 = 98;
/// How far back a pinned CPU reading blocks starts (`CPU_PINNED_PCT`): a start in a dip between two spikes still waits.
pub const PINNED_WINDOW_MS: i64 = 10_000;
/// Load at or over this many times the core count blocks only while the CPU is also this busy.
const LOAD_SOFT: f64 = 2.0;
const LOAD_BUSY_CPU_PCT: u64 = 90;
/// A rising load at or over `LOAD_SOFT` x cores blocks only while the CPU is at least this busy; below it the load is
/// threads waiting on I/O, not CPU work.
const LOAD_RISING_CPU_PCT: u64 = 75;
/// Load at or over this many times the core count blocks unless Linux PSI shows it is not CPU work: without a PSI
/// reading (macOS) it always blocks; with one, it blocks once the CPU is at least `LOAD_HARD_CPU_PCT` busy or tasks
/// stalled waiting for a CPU (PSI cpu `some avg10`) reach `CPU_PSI_PCT`. The 2026-11-19 Mac load of 120 on 16 cores
/// came with the CPU pinned.
const LOAD_HARD: f64 = 4.0;
/// The CPU % that confirms a load of `LOAD_HARD` x cores.
const LOAD_HARD_CPU_PCT: u64 = 50;
/// Load at or over this many times the core count blocks whatever the CPU says.
const LOAD_EXTREME: f64 = 6.0;
/// Linux PSI cpu `some avg10` at or over this confirms any load of `LOAD_SOFT` x cores or more.
const CPU_PSI_PCT: u64 = 25;
/// A job starts while the machine's measured CPU (`Live::cpu_sustained_pct` or the latest reading, whichever is
/// higher) is under this %. Jobs here average a fraction of their peak and burst for seconds, so the machine then
/// runs mostly in the 80-90% band while jobs wait, with the bursts of the running jobs on top.
pub const CPU_ADMIT_PCT: u64 = 80;
/// For at least this long after a start the machine has not shown the new job's CPU yet: it is settling
/// (`Block::Ramp`). A job settles for `settle_window_ms`.
pub const SETTLE_MS: i64 = 10_000;
/// A cold job (no history) or a Docker/VM job settles at least this long. Cold `cargo build`s spend 30-60 s fetching
/// crates or waiting on a build-dir lock, and `docker build`s pull images, before they compile: with a 10 s settle a
/// herd of them started 10 s apart and then all compiled at once (load about 120 on 16 cores, 2026-11-19).
pub const SETTLE_COLD_MS: i64 = 30_000;
/// No job settles longer than this, however late its learned peak.
pub const SETTLE_MAX_MS: i64 = 60_000;
/// The expected peaks (`Job::weights.cpu`: learned peak, `--cpu`, or the whole machine when cold) of the running jobs
/// still young (started under `SETTLE_MAX_MS` ago) plus the new one's add up to at most this many times the cores
/// (5/2). A backstop behind the measured CPU line, not the main gate. It stops the herd the measurement cannot see yet:
/// whole-machine jobs that idle on downloads or a build-dir lock for up to a minute and then compile together. On 16
/// cores at most two cold jobs and 8 cores of learned peaks are young at once, so at worst 40 cores of work line up
/// (load about 2.5 x cores, near the 2 x soft guard), not the 7.5 x of 2026-11-19. Older jobs do not count: their CPU
/// shows in the measurement, and their peaks are seconds-long bursts at different times (`make lint test-changed`
/// peaks at 10 cores and averages 1.2 for 30 minutes). Counting every running job's peak, as the review first asked,
/// replayed the Mac's 2026-11-21 day at 12% in band and waits of hours: the 0.4.9 failure again.
const PEAK_SUM_CAP: (u64, u64) = (5, 2);
/// The size check: measured busy cores plus the new job's typical cores (`typical_centi`) stay within this % of the
/// cores, whether or not a job is settling.
const SIZE_PCT: u64 = 95;
/// An older waiter that has waited this long for memory or VRAM holds back the younger ones, which would otherwise keep
/// taking what it waits for.
pub const QUEUE_PASS_MS: i64 = 5 * 60_000;
/// A learned VRAM size's unused part is held this long after admission, then decays over `MEM_DECAY_MS`.
pub const VRAM_HOLD_MS: i64 = 120_000;
/// A learned memory size with no known time to peak is held this long after admission.
pub const MEM_FALLBACK_HOLD_MS: i64 = 60_000;
/// After its hold, a learned size's unused part shrinks linearly to nothing over this long.
pub const MEM_DECAY_MS: i64 = 60_000;
/// The learned unused parts together may take at most this fraction (3/4) of the memory still free (ceiling minus
/// machine use), so a burst cannot all peak at once while measured use still admits jobs.
const MEM_HELD_CAP: (u64, u64) = (3, 4);
/// How many recent non-heavy samples the smoother averages (waiters re-sample every 10 s).
pub const SMOOTH_WINDOW: usize = 3;
/// The window (ms) of the sustained level: the highest mean of the process tree's CPU over any window this long.
pub const SUSTAINED_WINDOW_MS: u64 = 10_000;

/// Which check blocked a waiter (recorded in the history; a disk that is too full fails instead of waiting).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Block {
    Cpu,
    Mem,
    Vram,
    Load1,
    /// No longer a block of its own (the 5-minute load only tells rising from falling); kept so older history parses.
    Load5,
    Queue,
    Ramp,
}

impl Block {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Mem => "mem",
            Self::Vram => "vram",
            Self::Load1 => "load1",
            Self::Load5 => "load5",
            Self::Queue => "queue",
            Self::Ramp => "ramp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Start,
    /// The job can never run here (the disk is too full even with nothing else running): exit at once.
    Fail(String),
    /// `live` is true when live data is the blocker, so waiting re-samples on a timer; otherwise a job
    /// finishing (a file event) is what unblocks it.
    /// `block` is the check that blocked. `ramp_end_ms` is set when a ramp hold is the blocker: the moment the
    /// earliest ramp ends, so the waiter sleeps exactly until then.
    Wait {
        reason: String,
        live: bool,
        block: Block,
        ramp_end_ms: Option<i64>,
    },
}

struct Blocked {
    reason: String,
    live: bool,
    block: Block,
    ramp_end_ms: Option<i64>,
    /// Blocked by the size check: like a memory wait, the queue holds for it once it has waited `QUEUE_PASS_MS`, or a
    /// stream of small jobs would keep the machine too busy for it forever.
    by_size: bool,
}

fn blocked(reason: impl Into<String>, live: bool, block: Block) -> Blocked {
    Blocked {
        reason: reason.into(),
        live,
        block,
        ramp_end_ms: None,
        by_size: false,
    }
}

impl Blocked {
    fn sized(self) -> Self {
        Self {
            by_size: true,
            ..self
        }
    }
}

fn wait(reason: impl Into<String>, block: Block) -> Decision {
    Decision::Wait {
        reason: reason.into(),
        live: false,
        block,
        ramp_end_ms: None,
    }
}

/// max(latest sample, mean of the last few): fast to block when usage jumps (a fresh waiter has one sample only, and a
/// mean alone would take about 20 s to follow a jump), slow to release after a spike or a dip.
#[derive(Debug, Default)]
pub struct Smoother {
    window: VecDeque<Usage>,
}

impl Smoother {
    pub fn push(&mut self, sample: Usage) -> Usage {
        self.window.push_back(sample);
        while self.window.len() > SMOOTH_WINDOW {
            self.window.pop_front();
        }
        let n = u64::try_from(self.window.len().max(1)).unwrap_or(1);
        let sum = self.window.iter().fold(Usage::default(), |a, b| Usage {
            cpu: a.cpu + b.cpu,
            mem: a.mem + b.mem,
            vram: a.vram + b.vram,
        });
        Usage {
            cpu: (sum.cpu / n).max(sample.cpu),
            mem: (sum.mem / n).max(sample.mem),
            vram: (sum.vram / n).max(sample.vram),
        }
    }
}

/// Non-heavy usage: what the machine uses minus what heavy's own process trees use.
#[must_use]
pub fn non_heavy(machine: Usage, heavy_trees: Usage) -> Usage {
    machine.saturating_sub(heavy_trees)
}

fn centi(cores: u32) -> u64 {
    u64::from(cores) * 100
}

/// Centi-cores as `1.25`.
fn cores(centi: u64) -> String {
    format!("{}.{:02}", centi / 100, centi % 100)
}

fn ceiling(cap: &Capacity) -> Usage {
    Usage {
        cpu: centi(cap.cpu),
        mem: cap.mem / 100 * MEM_PCT,
        vram: cap.vram / 100 * VRAM_PCT,
    }
}

/// VRAM reservations only: the running jobs' VRAM (`vram_reserved`) plus the request within 90% of capacity. Memory
/// and CPU are live.
#[must_use]
pub fn fits_reservations(
    req: Weights,
    running: &[&Job],
    cap: &Capacity,
    live: &Live,
    now_ms: i64,
) -> bool {
    req.vram
        <= ceiling(cap)
            .vram
            .saturating_sub(vram_reserved(running, live, now_ms))
}

/// The VRAM the running jobs hold, netted against live use (0.4.9): each job's live VRAM plus the unused part of its
/// reservation. A declared (`--vram`) size's unused part is held for the whole run; a learned one's until
/// `VRAM_HOLD_MS`, then it decays over `MEM_DECAY_MS` (not on memory's learned time to peak: a GPU job that
/// allocates after its memory peaked would lose its reservation before it used it). Before 0.4.9 the plain sum of
/// reservations counted for the whole run: on the rig, jobs that had learned the desktop's 1.9G kept it reserved
/// whatever the GPU used, and waiters waited 14 minutes on "vram" at 37% CPU with the GPU at 1.8 of 10G.
#[must_use]
pub fn vram_reserved(running: &[&Job], live: &Live, now_ms: i64) -> u64 {
    running
        .iter()
        .map(|j| {
            let l = live
                .per_job
                .iter()
                .find(|(id, _)| *id == j.id)
                .map_or(0, |(_, u)| u.vram);
            let open = j.started_ms.map_or_else(
                || j.weights.vram.saturating_sub(l),
                |t| {
                    mem_unmet(
                        j.weights.vram,
                        l,
                        now_ms.saturating_sub(t).max(0),
                        j.vram_declared,
                        Some(VRAM_HOLD_MS),
                    )
                },
            );
            l + open
        })
        .sum()
}

/// The part of a reservation a job `elapsed_ms` after admission has not yet used. A `declared` size is held for the
/// whole run. A learned one is held until `peak_at_ms` (at least `SETTLE_MS`; `MEM_FALLBACK_HOLD_MS` when unknown), then
/// shrinks linearly to nothing over `MEM_DECAY_MS`.
#[must_use]
pub fn mem_unmet(
    reserved: u64,
    live: u64,
    elapsed_ms: i64,
    declared: bool,
    peak_at_ms: Option<i64>,
) -> u64 {
    let (full, tail) = mem_unmet_split(reserved, live, elapsed_ms, declared, peak_at_ms);
    full + tail
}

/// `mem_unmet` split into `(full, tail)`: `full` is held in full (a declared size, or a learned one before its
/// time-to-peak); `tail` is the decaying part after the peak time.
#[must_use]
pub fn mem_unmet_split(
    reserved: u64,
    live: u64,
    elapsed_ms: i64,
    declared: bool,
    peak_at_ms: Option<i64>,
) -> (u64, u64) {
    let unmet = reserved.saturating_sub(live);
    if declared {
        return (unmet, 0);
    }
    let hold = peak_at_ms.unwrap_or(MEM_FALLBACK_HOLD_MS).max(SETTLE_MS);
    if elapsed_ms <= hold {
        (unmet, 0)
    } else if elapsed_ms >= hold + MEM_DECAY_MS {
        (0, 0)
    } else {
        (
            0,
            unmet * u64::try_from(hold + MEM_DECAY_MS - elapsed_ms).unwrap_or(0)
                / u64::try_from(MEM_DECAY_MS).unwrap_or(1),
        )
    }
}

/// The machine's sustained CPU %: max(latest, mean of the readings from the last `BUSY_WINDOW_MS`). Fast to block when
/// the CPU rises, slow to release after a dip: a 250 ms window between build steps is not free CPU.
#[must_use]
pub fn sustained_pct(latest: u64, readings: &[(i64, u64)], now_ms: i64) -> u64 {
    let recent: Vec<u64> = readings
        .iter()
        .filter(|(ts, _)| *ts <= now_ms && now_ms - *ts <= BUSY_WINDOW_MS)
        .map(|(_, pct)| *pct)
        .collect();
    let n = u64::try_from(recent.len()).unwrap_or(u64::MAX);
    recent
        .iter()
        .sum::<u64>()
        .checked_div(n)
        .unwrap_or(0)
        .max(latest)
}

/// The highest of the shared CPU readings `(ms, busy %)` in the last `PINNED_WINDOW_MS`; 0 with none.
#[must_use]
pub fn recent_peak_pct(readings: &[(i64, u64)], now_ms: i64) -> u64 {
    readings
        .iter()
        .filter(|(ts, _)| *ts <= now_ms && now_ms - *ts <= PINNED_WINDOW_MS)
        .map(|(_, pct)| *pct)
        .max()
        .unwrap_or(0)
}

/// The disk cannot hold the request even with no heavy job running: waiting would never help.
#[must_use]
pub fn disk_full(req: Weights, cap: &Capacity, live: &Live) -> Option<String> {
    let need = u128::from(req.disk) + u128::from(cap.disk_total) / 10;
    (u128::from(live.disk_free) < need).then(|| {
        format!(
            "the disk is full: {} free, need {} (the request plus 10% of the disk). Clean up, or ask Todd",
            crate::sizes::fmt_size(live.disk_free),
            crate::sizes::fmt_size(u64::try_from(need).unwrap_or(u64::MAX))
        )
    })
}

/// Load counts as rising only when load1 exceeds load5 by this factor, so jitter at steady load does not flip it.
const RISING_MARGIN: f64 = 1.1;

/// Live guards, a veto only. The 1-minute load blocks at 4 x cores always without Linux PSI (macOS), and with it once the
/// CPU is at least 50% busy or PSI cpu `some` reads at least 25% (runaway load), 6 x cores always, and critical memory pressure (macOS level 4, Linux PSI) blocks. From 2 x
/// cores up a PSI cpu `some` of 25% blocks too, and the load blocks while it is still rising (load1 > 1.1 x load5)
/// with the CPU at least 75% busy, and while the CPU is at least 90% busy (the CPU is the larger of the latest and the
/// sustained reading); a high load with a calm CPU is I/O wait (macOS counts blocked threads), so it admits. Memory used
/// at 95% blocks too, even a lone job (no exception: a lone 8 GB job at 97% used thrashes), and so does macOS warn
/// pressure with memory used at 85%. Warn alone never blocks (2026-11-21: three jobs queued 40+ minutes with nothing
/// running at 27% CPU, on warn pressure at 49% used).
#[must_use]
pub fn guard_block(cap: &Capacity, live: &Live) -> Option<(String, Block)> {
    let cores = f64::from(cap.cpu);
    let (soft, hard) = (LOAD_SOFT * cores, LOAD_HARD * cores);
    let cpu = live.cpu_pct.max(live.cpu_sustained_pct);
    let peak = live.cpu_pct.max(live.cpu_recent_peak_pct);
    if peak >= CPU_PINNED_PCT {
        return Some((
            format!(
                "cpu pinned: {peak}% busy in the last {} s",
                PINNED_WINDOW_MS / 1_000
            ),
            Block::Cpu,
        ));
    }
    let psi = live.cpu_pressure.filter(|p| *p >= CPU_PSI_PCT);
    let psi_note = psi.map_or_else(String::new, |p| format!(", cpu pressure {p}%"));
    if live.load1 >= LOAD_EXTREME * cores {
        return Some((
            format!(
                "1-minute load {:.1} >= {:.0} (6 x {} cores)",
                live.load1,
                LOAD_EXTREME * cores,
                cap.cpu
            ),
            Block::Load1,
        ));
    }
    // Without PSI (macOS, or an unreadable file) nothing can tell CPU overload from I/O wait, so 4 x cores blocks
    // unconditionally there, as before 0.4.8; only a PSI reading under `CPU_PSI_PCT` with a calm CPU relaxes it.
    let unconfirmed = live.cpu_pressure.is_none();
    if live.load1 >= hard && (unconfirmed || cpu >= LOAD_HARD_CPU_PCT || psi.is_some()) {
        return Some((
            format!(
                "1-minute load {:.1} >= {hard:.0} (4 x {} cores), cpu {cpu}% busy{psi_note}",
                live.load1, cap.cpu
            ),
            Block::Load1,
        ));
    }
    if live.mem_pressure {
        return Some(("critical memory pressure".into(), Block::Mem));
    }
    if live.load1 >= soft {
        if live.load1 > live.load5 * RISING_MARGIN && cpu >= LOAD_RISING_CPU_PCT {
            return Some((
                format!(
                    "1-minute load {:.1} >= {soft:.0} (2 x {} cores) and still rising (5-minute {:.1}), cpu {cpu}% busy",
                    live.load1, cap.cpu, live.load5
                ),
                Block::Load1,
            ));
        }
        if cpu >= LOAD_BUSY_CPU_PCT || psi.is_some() {
            return Some((
                format!(
                    "1-minute load {:.1} >= {soft:.0} (2 x {} cores) and cpu {cpu}% busy{psi_note}",
                    live.load1, cap.cpu
                ),
                Block::Load1,
            ));
        }
    }
    if live.mem_used_pct >= MEM_PCT {
        return Some((
            format!("memory {}% used >= {MEM_PCT}%", live.mem_used_pct),
            Block::Mem,
        ));
    }
    if live.mem_pressure_warn && live.mem_used_pct >= WARN_MEM_PCT {
        return Some((
            format!(
                "memory pressure warn with memory {}% used >= {WARN_MEM_PCT}%",
                live.mem_used_pct
            ),
            Block::Mem,
        ));
    }
    None
}

/// The sustained level of CPU samples `(ms after the start, centi-cores)`, oldest first: the highest mean over any
/// `SUSTAINED_WINDOW_MS` window (the mean of the whole run when it is shorter). 0 with no samples.
#[must_use]
pub fn sustained_level(samples: &[(u64, u64)]) -> u64 {
    let Some(&(last, _)) = samples.last() else {
        return 0;
    };
    let mean = |w: &[(u64, u64)]| {
        w.iter().map(|s| s.1).sum::<u64>() / u64::try_from(w.len().max(1)).unwrap_or(1)
    };
    if last < SUSTAINED_WINDOW_MS {
        return mean(samples);
    }
    let mut best = 0;
    let mut from = 0;
    for (i, &(at, _)) in samples.iter().enumerate() {
        while samples[from].0 + SUSTAINED_WINDOW_MS <= at {
            from += 1;
        }
        if at >= SUSTAINED_WINDOW_MS {
            best = best.max(mean(&samples[from..=i]));
        }
    }
    best
}

/// The memory left for a new job: 95% of RAM minus non-heavy use, minus each running job's live use, minus the unmet
/// part of each running job's reservation (`mem_unmet_split`). A job not yet stamped, or one with a declared size, holds
/// its whole unused part; a learned size holds it in full until its time to peak, and only the decaying tails after
/// that are capped together, at `MEM_HELD_CAP` of the memory still free.
#[must_use]
pub fn mem_room(running: &[&Job], cap: &Capacity, live: &Live, now_ms: i64) -> u64 {
    let (held, pre_peak, tail) = running
        .iter()
        .fold((0u64, 0u64, 0u64), |(held, pre, tail), j| {
            let l = live_use(j, live).mem;
            match j.started_ms {
                Some(t) if !j.mem_declared => {
                    let (full, decaying) = mem_unmet_split(
                        j.weights.mem,
                        l,
                        now_ms.saturating_sub(t).max(0),
                        false,
                        j.mem_peak_at_ms,
                    );
                    (held + l, pre + full, tail + decaying)
                }
                _ => (held + l.max(j.weights.mem), pre, tail),
            }
        });
    let free = ceiling(cap)
        .mem
        .saturating_sub(live.non_heavy.mem)
        .saturating_sub(held)
        .saturating_sub(pre_peak);
    free.saturating_sub(tail.min(free / MEM_HELD_CAP.1 * MEM_HELD_CAP.0))
}

fn live_use(j: &Job, live: &Live) -> Usage {
    live.per_job
        .iter()
        .find(|(id, _)| *id == j.id)
        .map_or_else(Usage::default, |(_, u)| *u)
}

/// The machine's measured CPU %: the larger of the latest reading and the sustained one.
fn measured_pct(live: &Live) -> u64 {
    live.cpu_pct.max(live.cpu_sustained_pct)
}

/// How long a job settles: `SETTLE_MS`, or `SETTLE_COLD_MS` for a cold (`peak_at_ms` unknown) or Docker/VM job, or
/// the learned time to its CPU peak when that is later; at most `SETTLE_MAX_MS`.
#[must_use]
pub fn settle_window_ms(vm: bool, peak_at_ms: Option<u64>) -> i64 {
    let base = if vm || peak_at_ms.is_none() {
        SETTLE_COLD_MS
    } else {
        SETTLE_MS
    };
    let learned = peak_at_ms.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX));
    base.max(learned).min(SETTLE_MAX_MS)
}

fn settle_of(j: &Job) -> i64 {
    j.settle_ms.unwrap_or(SETTLE_MS)
}

/// The cores (centi) `j` typically uses: its learned typical or `--cpu` (`Job::typical_cpu`), else a quarter of the
/// machine (a cold job: 4 of 16 cores is above the 90th percentile of the Mac's recorded mean cores, 3.0; half the
/// machine replayed at 12% in band, because a cold job then waited for the machine to fall to 45%). Never more than
/// its size.
#[must_use]
pub fn typical_centi(j: &Job, cap: &Capacity) -> u64 {
    centi(
        j.typical_cpu
            .unwrap_or_else(|| cap.cpu.div_ceil(4))
            .min(j.weights.cpu)
            .min(cap.cpu),
    )
}

/// The young running jobs' (started under `SETTLE_MAX_MS` ago, or not yet stamped) expected peak cores (centi): each
/// one's size, at most the machine.
fn peak_sum(running: &[&Job], cap: &Capacity, now_ms: i64) -> u64 {
    running
        .iter()
        .filter(|j| j.started_ms.is_none_or(|t| now_ms - t < SETTLE_MAX_MS))
        .map(|j| centi(j.weights.cpu).min(centi(cap.cpu)))
        .sum()
}

/// Whether `j` is settling: not yet stamped, or started under `SETTLE_MS` ago, or under its `settle_window_ms` ago
/// while its process tree still uses less than its typical cores (at least 1): it is fetching, waiting on a lock, or
/// its work runs in Docker's VM, so the measurement cannot see what is coming. A job that already works at its typical
/// level shows in the measurement and settles at `SETTLE_MS`.
fn is_settling(j: &Job, cap: &Capacity, live: &Live, now_ms: i64) -> bool {
    j.started_ms.is_none_or(|t| {
        let age = now_ms - t;
        age < SETTLE_MS
            || (age < settle_of(j) && live_use(j, live).cpu < typical_centi(j, cap).max(100))
    })
}

/// The settling jobs (`is_settling`): the CPU (centi-cores) their sizes may still add beyond their live use, and the
/// moment the first of them has settled.
fn settling(running: &[&Job], cap: &Capacity, live: &Live, now_ms: i64) -> (u64, Option<i64>) {
    running
        .iter()
        .filter(|j| is_settling(j, cap, live, now_ms))
        .fold((0, None), |(cpu, first), j| {
            let size = centi(j.weights.cpu).min(centi(cap.cpu));
            let end = j
                .started_ms
                .map_or(now_ms + settle_of(j), |t| t + settle_of(j));
            (
                cpu + size.saturating_sub(live_use(j, live).cpu),
                Some(first.map_or(end, |f: i64| f.min(end))),
            )
        })
}

/// Why `want` cannot start now, or `None`. Alone (nothing running) a clamped request always passes the resource
/// checks, so a busy desktop cannot starve it; the guards still apply.
fn blocker(
    want: &Job,
    running: &[&Job],
    cap: &Capacity,
    live: &Live,
    now_ms: i64,
) -> Option<Blocked> {
    let w = want.weights;
    if !running.is_empty() {
        if !fits_reservations(w, running, cap, live, now_ms) {
            return Some(blocked("vram reservations full", false, Block::Vram));
        }
        let room = mem_room(running, cap, live, now_ms);
        if w.mem > room {
            return Some(blocked(
                format!(
                    "mem: needs {}, {} free under {MEM_PCT}% of RAM after running jobs' holds",
                    crate::sizes::fmt_size(w.mem),
                    crate::sizes::fmt_size(room)
                ),
                true,
                Block::Mem,
            ));
        }
        let total = centi(cap.cpu);
        let peaks = peak_sum(running, cap, now_ms) + centi(w.cpu).min(total);
        let peak_cap = total * PEAK_SUM_CAP.0 / PEAK_SUM_CAP.1;
        if peaks > peak_cap {
            return Some(blocked(
                format!(
                    "cpu: running jobs' peaks with this one's are {} cores, over {} (2.5 x cores)",
                    cores(peaks),
                    cores(peak_cap)
                ),
                true,
                Block::Cpu,
            ));
        }
        let pct = measured_pct(live);
        if pct >= CPU_ADMIT_PCT {
            return Some(blocked(
                format!("cpu: machine {pct}% busy, jobs start under {CPU_ADMIT_PCT}%"),
                true,
                Block::Cpu,
            ));
        }
        let busy = pct * total / 100;
        let typical = typical_centi(want, cap);
        let size_line = total * SIZE_PCT / 100;
        if busy + typical > size_line {
            return Some(blocked(
                format!(
                    "cpu: machine {} cores busy plus this job's typical {} is over {} ({SIZE_PCT}%)",
                    cores(busy),
                    cores(typical),
                    cores(size_line)
                ),
                true,
                Block::Cpu,
            )
            .sized());
        }
        let (unshown, settled_at) = settling(running, cap, live, now_ms);
        if let Some(at) = settled_at {
            let cpu_line = centi(cap.cpu) * CPU_ADMIT_PCT / 100;
            let need = pct * centi(cap.cpu) / 100 + unshown + centi(w.cpu).min(centi(cap.cpu));
            if need > cpu_line {
                return Some(Blocked {
                    reason: format!(
                        "cpu: jobs that just started are settling; {} cores with theirs, {} under the {CPU_ADMIT_PCT}% line",
                        cores(need),
                        cores(cpu_line)
                    ),
                    live: true,
                    block: Block::Ramp,
                    ramp_end_ms: Some(at),
                    by_size: false,
                });
            }
        }
    }
    guard_block(cap, live).map(|(why, block)| blocked(why, true, block))
}

/// Can `job` start now, given every job file (already pruned of dead ones)?
#[must_use]
pub fn evaluate(job: &Job, jobs: &[Job], cap: &Capacity, live: &Live, now_ms: i64) -> Decision {
    if let Some(why) = disk_full(job.weights, cap, live) {
        return Decision::Fail(why);
    }
    let running: Vec<&Job> = jobs.iter().filter(|j| j.state == State::Running).collect();
    // A waiter that is stopped (Ctrl-Z) cannot start, so it never holds back the ones behind it. Nor does one from a
    // release before 0.4.10 (`Job::v`): it decides by the old rules, which may block it while these would start it, so
    // waiting behind it would idle the machine until it starts by its own rules.
    let mut older: Vec<&Job> = jobs
        .iter()
        .filter(|j| {
            j.state == State::Queued
                && j.v >= JOB_VERSION
                && j.id != job.id
                && !live.stopped.contains(&j.id)
                && (j.enqueued_ms, &j.id) < (job.enqueued_ms, &job.id)
        })
        .collect();
    older.sort_by(|a, b| (a.enqueued_ms, &a.id).cmp(&(b.enqueued_ms, &b.id)));
    for o in older {
        match blocker(o, &running, cap, live, now_ms) {
            // It goes first; this job then checks again with it running.
            None => {
                return Decision::Wait {
                    reason: format!("queued behind {}, which can start now", o.id),
                    live: true,
                    block: Block::Queue,
                    ramp_end_ms: None,
                };
            }
            // Its turn comes when the jobs that just started have settled.
            Some(b) if b.block == Block::Ramp => {
                return Decision::Wait {
                    reason: format!(
                        "queued behind {}, which starts once the new jobs settle",
                        o.id
                    ),
                    live: true,
                    block: Block::Queue,
                    ramp_end_ms: b.ramp_end_ms,
                };
            }
            Some(b)
                if (b.by_size || matches!(b.block, Block::Mem | Block::Vram))
                    && now_ms - o.enqueued_ms >= QUEUE_PASS_MS =>
            {
                return wait(
                    format!(
                        "queued behind {} (waited over {} min for {})",
                        o.id,
                        QUEUE_PASS_MS / 60_000,
                        if b.by_size {
                            "room for its size"
                        } else {
                            b.block.name()
                        }
                    ),
                    Block::Queue,
                );
            }
            // Blocked by the CPU line or a guard: the same checks apply to this job.
            Some(_) => {}
        }
    }
    match blocker(job, &running, cap, live, now_ms) {
        Some(b) => Decision::Wait {
            reason: b.reason,
            live: b.live,
            block: b.block,
            ramp_end_ms: b.ramp_end_ms,
        },
        None => Decision::Start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    fn cap() -> Capacity {
        Capacity {
            cpu: 10,
            mem: 100 * G,
            vram: 0,
            disk_total: 1000 * G,
            vram_is_ram: false,
        }
    }
    fn calm() -> Live {
        Live {
            load1: 0.5,
            mem_used_pct: 10,
            disk_free: 900 * G,
            ..Live::default()
        }
    }
    fn job(id: &str, cpu: u32, mem_g: u64, state: State, enq: i64) -> Job {
        Job {
            id: id.into(),
            pid: 1,
            start_time: 0,
            weights: Weights {
                cpu,
                mem: mem_g * G,
                vram: 0,
                disk: 0,
            },
            cmd: vec!["x".into()],
            cwd: "/".into(),
            state,
            enqueued_ms: enq,
            started_ms: None,
            blocked_by: None,
            mem_declared: false,
            mem_peak_at_ms: None,
            vram_declared: false,
            v: JOB_VERSION,
            settle_ms: None,
            typical_cpu: Some(cpu),
        }
    }
    fn waits(d: &Decision) -> bool {
        matches!(d, Decision::Wait { .. })
    }

    #[test]
    fn alone_always_runs() {
        let me = job("a", 10, 100, State::Queued, 0);
        let busy = Live {
            non_heavy: Usage {
                cpu: 900,
                mem: 90 * G,
                vram: 0,
            },
            ..calm()
        };
        assert_eq!(
            evaluate(&me, std::slice::from_ref(&me), &cap(), &busy, 0),
            Decision::Start
        );
    }

    #[test]
    fn ninety_percent_vram_reservation_ceiling() {
        let c = Capacity {
            vram: 100 * G,
            ..cap()
        };
        let with_vram = |id: &str, g: u64, state| {
            let j = job(id, 1, 1, state, 0);
            Job {
                weights: Weights {
                    vram: g * G,
                    ..j.weights
                },
                ..j
            }
        };
        let run = with_vram("r", 50, State::Running);
        let fits = with_vram("a", 40, State::Queued);
        let over = with_vram("b", 41, State::Queued);
        assert_eq!(
            evaluate(&fits, &[run.clone(), fits.clone()], &c, &calm(), 2),
            Decision::Start
        );
        let d = evaluate(&over, &[run, over.clone()], &c, &calm(), 2);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    live: false,
                    block: Block::Vram,
                    ..
                }
            ),
            "{d:?}"
        );
    }

    #[test]
    fn declared_reservations_over_the_ceiling_admit_when_measured_use_is_low() {
        // The wtlayer case: running reservations (learned/declared) total 88G of 100G, measured memory 43% used.
        let c = cap();
        let live = Live {
            mem_used_pct: 43,
            non_heavy: Usage {
                cpu: 0,
                mem: 20 * G,
                vram: 0,
            },
            per_job: vec![
                (
                    "r1".into(),
                    Usage {
                        cpu: 100,
                        mem: 12 * G,
                        vram: 0,
                    },
                ),
                (
                    "r2".into(),
                    Usage {
                        cpu: 100,
                        mem: 11 * G,
                        vram: 0,
                    },
                ),
            ],
            ..calm()
        };
        let old = |id: &str| Job {
            started_ms: Some(0),
            ..job(id, 2, 44, State::Running, 0)
        };
        let me = job("w", 2, 20, State::Queued, 1);
        let jobs = [old("r1"), old("r2"), me.clone()];
        assert_eq!(
            evaluate(&me, &jobs, &c, &live, 100_000),
            Decision::Start,
            "reservations 88G + 20G > 90G, measured 43G"
        );
        // Right after admission the unused reservation still counts: the same pair blocks on memory, then releases.
        let d = evaluate(&me, &jobs, &c, &live, 1_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Mem,
                    live: true,
                    ..
                }
            ),
            "{d:?}"
        );
        // Real pressure still blocks: measured use leaves no room for the request.
        let full = Live {
            non_heavy: Usage {
                cpu: 0,
                mem: 60 * G,
                vram: 0,
            },
            ..live.clone()
        };
        let d = evaluate(&me, &jobs, &c, &full, 100_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Mem,
                    live: true,
                    ..
                }
            ),
            "{d:?}"
        );
    }

    #[test]
    fn mem_unmet_is_held_to_the_learned_peak_then_decays() {
        let unmet = |at: i64, declared, peak| mem_unmet(10 * G, 3 * G, at, declared, peak);
        assert_eq!(
            mem_unmet(10 * G, 12 * G, 0, false, None),
            0,
            "live use over the reservation leaves nothing unmet"
        );
        // Learned peak at 90 s: held in full until then, half left 30 s later, nothing after the decay.
        assert_eq!(unmet(SETTLE_MS, false, Some(90_000)), 7 * G);
        assert_eq!(unmet(90_000, false, Some(90_000)), 7 * G);
        assert_eq!(
            unmet(90_000 + MEM_DECAY_MS / 2, false, Some(90_000)),
            7 * G / 2
        );
        assert_eq!(unmet(90_000 + MEM_DECAY_MS, false, Some(90_000)), 0);
        // A peak earlier than the ramp still holds through the ramp; an unknown peak uses the fallback hold.
        assert_eq!(unmet(SETTLE_MS, false, Some(2_000)), 7 * G);
        assert_eq!(unmet(MEM_FALLBACK_HOLD_MS, false, None), 7 * G);
        assert_eq!(unmet(MEM_FALLBACK_HOLD_MS + MEM_DECAY_MS, false, None), 0);
        // A declared size is held for the whole run.
        assert_eq!(unmet(10_000_000, true, None), 7 * G);
    }

    fn big_mac() -> Capacity {
        Capacity {
            cpu: 16,
            mem: 64 * G,
            vram: 0,
            disk_total: 1000 * G,
            vram_is_ram: false,
        }
    }

    /// How many of 12 waiters, each with a learned 12G peak that use 1G for their first minutes, start in turn on a
    /// 64G Mac with 10G of other programs.
    fn admitted_of_twelve(declared: bool, now: impl Fn(usize) -> i64) -> usize {
        let cap = big_mac();
        let mut running: Vec<Job> = vec![];
        for i in 0..12_i64 {
            let w = Job {
                mem_declared: declared,
                mem_peak_at_ms: Some(120_000),
                ..job(&format!("w{i:02}"), 1, 12, State::Queued, i)
            };
            let live = Live {
                non_heavy: Usage {
                    cpu: 0,
                    mem: 10 * G,
                    vram: 0,
                },
                per_job: running
                    .iter()
                    .map(|j: &Job| {
                        (
                            j.id.clone(),
                            Usage {
                                cpu: 50,
                                mem: G,
                                vram: 0,
                            },
                        )
                    })
                    .collect(),
                ..calm()
            };
            let mut all = running.clone();
            all.push(w.clone());
            if evaluate(&w, &all, &cap, &live, now(usize::try_from(i).unwrap_or(0)))
                == Decision::Start
            {
                running.push(Job {
                    state: State::Running,
                    started_ms: Some(0),
                    ..w
                });
            }
        }
        running.len()
    }

    #[test]
    fn twelve_late_peaking_jobs_do_not_all_start_on_a_64g_mac() {
        // Reviewer's case: 144G of peaks on 64G. 0.4.1 admitted 4; releasing every hold at 10 s admitted all 12.
        // Held until the learned peak (120 s), 30 s in: a few start, never all (4 x 12G + 10G within 95% of 64G).
        assert_eq!(admitted_of_twelve(false, |_| 30_000), 4);
        // Past peak and decay (3 minutes) the holds are gone and measured use (1G each) admits them all.
        assert_eq!(admitted_of_twelve(false, |_| 180_000), 12);
        // Declared sizes are held whatever the time.
        assert_eq!(admitted_of_twelve(true, |_| 3_600_000), 4);
    }

    #[test]
    fn the_free_memory_cap_binds_only_the_decaying_tail_not_the_hold_before_the_peak() {
        // 64G Mac (57.6G usable), 10G other programs, one learned 40G job (peak at 120 s) using nothing yet.
        // Old cap: min(40G, 3/4 of 47.6G free = 35.7G) left 4.3G of its reservation unprotected before the peak.
        let cap = big_mac();
        let live = Live {
            non_heavy: Usage {
                cpu: 0,
                mem: 10 * G,
                vram: 0,
            },
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 50,
                    mem: 0,
                    vram: 0,
                },
            )],
            ..calm()
        };
        let r = Job {
            started_ms: Some(0),
            mem_peak_at_ms: Some(120_000),
            ..job("r", 1, 40, State::Running, 0)
        };
        let usable = ceiling(&cap).mem;
        let at = |ms| mem_room(&[&r], &cap, &live, ms);
        // Before the learned peak the hold is full: free = usable - 10G - 40G exactly.
        assert_eq!(at(30_000), usable - 50 * G);
        assert_eq!(at(120_000), usable - 50 * G);
        // After the peak, the decaying tail (here 20G at the midpoint) is still under the cap, so it is held as is.
        assert_eq!(at(150_000), usable - 30 * G);
        // A tail larger than 3/4 of what is free is capped: 3/4 of 47.6G, not the whole hold.
        let big = Job {
            weights: Weights {
                mem: 80 * G,
                ..r.weights
            },
            ..r.clone()
        };
        let free = usable - 10 * G;
        let tail = 80 * G / 2;
        assert!(tail > free / 4 * 3);
        assert_eq!(mem_room(&[&big], &cap, &live, 150_000), free - free / 4 * 3);
    }

    #[test]
    fn a_declared_mem_is_held_for_the_whole_run() {
        let cap = big_mac();
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 100,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            ..calm()
        };
        let me = job("w", 1, 30, State::Queued, 1);
        let held = |declared: bool, now: i64| {
            let r = Job {
                started_ms: Some(0),
                mem_declared: declared,
                mem_peak_at_ms: Some(5_000),
                ..job("r", 1, 40, State::Running, 0)
            };
            evaluate(&me, &[r, me.clone()], &cap, &live, now)
        };
        // Declared 40G, using 2G, an hour in: 38G still held, so 30G does not fit under the 57.6G ceiling.
        assert!(
            matches!(
                held(true, 3_600_000),
                Decision::Wait {
                    block: Block::Mem,
                    ..
                }
            ),
            "{:?}",
            held(true, 3_600_000)
        );
        // The same learned size is gone after its peak time and decay.
        assert_eq!(held(false, 3_600_000), Decision::Start);
    }

    #[test]
    fn headroom_math_with_fake_non_heavy_usage() {
        // 10 cores, 100G. Memory ceiling 95G. Non-heavy: 2 cores, 30G.
        // Running job reserves 10G but really uses 3 cores / 20G: memory counts max(reservation, live).
        let run = job("r", 2, 10, State::Running, 0);
        let live = Live {
            non_heavy: Usage {
                cpu: 200,
                mem: 30 * G,
                vram: 0,
            },
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 300,
                    mem: 20 * G,
                    vram: 0,
                },
            )],
            ..calm()
        };
        let room = mem_room(&[&run], &cap(), &live, 100_000);
        assert_eq!(room, 95 * G - 30 * G - 20 * G);
        // Memory fits at exactly the edge, blocked one GiB over; both pass the reservation-only check.
        let edge = job("e", 4, 45, State::Queued, 1);
        assert_eq!(
            evaluate(&edge, &[run.clone(), edge.clone()], &cap(), &live, 2),
            Decision::Start
        );
        let over = job("o", 5, 46, State::Queued, 1);
        let d = evaluate(&over, &[run.clone(), over.clone()], &cap(), &live, 2);
        assert!(matches!(d, Decision::Wait { live: true, .. }), "{d:?}");
        // Past its hold and decay an unused reservation holds nothing; memory is just live.
        let idle = Live {
            per_job: vec![("r".into(), Usage::default())],
            non_heavy: Usage::default(),
            ..calm()
        };
        let run = Job {
            started_ms: Some(0),
            ..run
        };
        assert_eq!(mem_room(&[&run], &cap(), &idle, 200_000), 95 * G);
        // Inside the settle time the unused part of the reservation still counts.
        assert_eq!(mem_room(&[&run], &cap(), &idle, 1_000), 85 * G);
    }

    #[test]
    fn non_heavy_is_machine_minus_heavy_trees() {
        let machine = Usage {
            cpu: 800,
            mem: 50 * G,
            vram: 2 * G,
        };
        let trees = Usage {
            cpu: 600,
            mem: 20 * G,
            vram: 5 * G,
        };
        assert_eq!(
            non_heavy(machine, trees),
            Usage {
                cpu: 200,
                mem: 30 * G,
                vram: 0
            }
        );
    }

    #[test]
    fn smoother_is_fast_to_rise_and_slow_to_fall() {
        let mut s = Smoother::default();
        let low = Usage {
            cpu: 100,
            mem: 2 * G,
            vram: 0,
        };
        let hi = Usage {
            cpu: 900,
            mem: 12 * G,
            vram: 0,
        };
        assert_eq!(s.push(low), low);
        assert_eq!(s.push(low), low);
        // A jump counts in full at once (a mean of the window would say 366).
        assert_eq!(s.push(hi), hi, "a rise is not averaged away");
        // After it, the mean (the window holds low, hi, low) releases slowly.
        assert_eq!(
            s.push(low),
            Usage {
                cpu: 366,
                mem: 16 * G / 3,
                vram: 0
            },
            "a fall is released slowly"
        );
        assert_eq!(
            s.push(low),
            Usage {
                cpu: 366,
                mem: 16 * G / 3,
                vram: 0
            }
        );
        for _ in 0..SMOOTH_WINDOW {
            s.push(Usage::default());
        }
        assert_eq!(
            s.push(Usage::default()),
            Usage::default(),
            "old peaks age out"
        );
    }

    #[test]
    fn a_fresh_waiter_sees_a_jump_at_once() {
        // A new waiter's smoother is empty: its first sample is the latest, so a loaded machine blocks at once.
        let mut s = Smoother::default();
        let hot = Usage {
            cpu: 1500,
            mem: 30 * G,
            vram: 0,
        };
        assert_eq!(s.push(hot), hot);
    }

    #[test]
    fn many_tiny_jobs_run_together() {
        let mut all: Vec<Job> = (0..50)
            .map(|i| job(&format!("r{i:02}"), 0, 0, State::Running, 0))
            .collect();
        let me = job("a", 0, 0, State::Queued, 1);
        all.push(me.clone());
        assert_eq!(
            evaluate(&me, &all, &cap(), &calm(), 2),
            Decision::Start,
            "no cap on the number of jobs"
        );
    }

    #[test]
    fn guards_veto() {
        let me = job("a", 1, 1, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        // 10 cores: load 20 blocks only with a busy CPU, load 40 always.
        let busy = Live {
            load1: 21.0,
            load5: 22.0,
            cpu_pct: 95,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, one, &cap(), &busy, 0),
            Decision::Wait {
                live: true,
                block: Block::Load1,
                ..
            }
        ));
        let calm_1m = Live {
            load1: 19.0,
            load5: 5.0,
            cpu_pct: 95,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, one, &cap(), &calm_1m, 0),
            Decision::Start,
            "macOS load reads high: 2 x cores"
        );
        let io_bound = Live {
            load1: 35.0,
            load5: 36.0,
            cpu_pct: 40,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, one, &cap(), &io_bound, 0),
            Decision::Start,
            "high, falling load with a calm CPU is threads waiting on I/O"
        );
        let runaway = Live {
            load1: 40.0,
            cpu_pct: 50,
            ..calm()
        };
        assert!(
            matches!(
                evaluate(&me, one, &cap(), &runaway, 0),
                Decision::Wait {
                    block: Block::Load1,
                    ..
                }
            ),
            "4 x cores blocks with the CPU half busy"
        );
        let io_storm = Live {
            cpu_pct: 10,
            cpu_pressure: Some(0),
            ..runaway.clone()
        };
        assert_eq!(
            evaluate(&me, one, &cap(), &io_storm, 0),
            Decision::Start,
            "0.4.8: 4 x cores at 10% CPU with PSI showing no CPU pressure is I/O wait"
        );
        let no_psi = Live {
            cpu_pressure: None,
            ..io_storm
        };
        assert!(
            waits(&evaluate(&me, one, &cap(), &no_psi, 0)),
            "without PSI (macOS) 4 x cores blocks whatever the CPU says"
        );
        // Memory used at 95% blocks, a lone job too.
        let mem = Live {
            mem_used_pct: 95,
            ..calm()
        };
        let run = job("r", 1, 1, State::Running, 0);
        assert!(waits(&evaluate(&me, &[run, me.clone()], &cap(), &mem, 0)));
        assert!(waits(&evaluate(&me, one, &cap(), &mem, 0)), "even alone");
        // 2026-11-21 review: 97% used, level 2 pressure, nothing running: an 8 GB job must not start.
        let thrash = Live {
            mem_used_pct: 97,
            mem_pressure_warn: true,
            ..calm()
        };
        let big = job("b", 1, 8, State::Queued, 0);
        assert!(waits(&evaluate(
            &big,
            std::slice::from_ref(&big),
            &cap(),
            &thrash,
            0
        )));
    }

    #[test]
    fn rising_load_blocks_at_two_times_cores_but_falling_load_admits() {
        let me = job("a", 1, 1, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        // 10 cores, soft limit 20. A rising load with a CPU at least 75% busy is builds piling up: blocked.
        let rising = Live {
            load1: 30.0,
            load5: 22.0,
            cpu_pct: 80,
            ..calm()
        };
        let d = evaluate(&me, one, &cap(), &rising, 0);
        assert!(
            matches!(&d, Decision::Wait { block: Block::Load1, live: true, reason, .. } if reason.contains("rising")),
            "{d:?}"
        );
        // The same rising load with a calm CPU is threads waiting on I/O, not CPU overload: admitted.
        let rising_io = Live {
            cpu_pct: 30,
            ..rising.clone()
        };
        assert_eq!(evaluate(&me, one, &cap(), &rising_io, 0), Decision::Start);
        let falling = Live {
            load1: 30.0,
            load5: 38.0,
            cpu_pct: 30,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, one, &cap(), &falling, 0),
            Decision::Start,
            "falling and CPU under 90%"
        );
        let level = Live {
            load1: 30.0,
            load5: 30.0,
            cpu_pct: 30,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, one, &cap(), &level, 0),
            Decision::Start,
            "load1 <= load5 is not rising"
        );
        let jitter = Live {
            load1: 31.5,
            load5: 30.0,
            cpu_pct: 30,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, one, &cap(), &jitter, 0),
            Decision::Start,
            "load1 = load5 x 1.05 is jitter, not rising"
        );
        // Falling but the CPU is busy: still blocked.
        let falling_busy = Live {
            cpu_pct: 92,
            ..falling.clone()
        };
        assert!(waits(&evaluate(&me, one, &cap(), &falling_busy, 0)));
        // Below 2 x cores a rising load is fine; at 4 x cores nothing helps.
        let ramping_up = Live {
            load1: 15.0,
            load5: 5.0,
            cpu_pct: 30,
            ..calm()
        };
        assert_eq!(evaluate(&me, one, &cap(), &ramping_up, 0), Decision::Start);
        let hard = Live {
            load1: 41.0,
            load5: 80.0,
            cpu_pct: 55,
            ..calm()
        };
        assert!(waits(&evaluate(&me, one, &cap(), &hard, 0)));
    }

    #[test]
    fn memory_pressure_blocks() {
        let me = job("a", 1, 1, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        let pressured = Live {
            mem_pressure: true,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, one, &cap(), &pressured, 0),
            Decision::Wait {
                block: Block::Mem,
                live: true,
                ..
            }
        ));
        assert_eq!(evaluate(&me, one, &cap(), &calm(), 0), Decision::Start);
    }

    #[test]
    fn disk_guard() {
        let mut me = job("a", 1, 1, State::Queued, 0);
        me.weights.disk = 500 * G;
        let low = Live {
            disk_free: 550 * G,
            ..calm()
        };
        assert!(
            matches!(evaluate(&me, std::slice::from_ref(&me), &cap(), &low, 0), Decision::Fail(m) if m.contains("disk is full"))
        );
        let ok = Live {
            disk_free: 600 * G,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, std::slice::from_ref(&me), &cap(), &ok, 0),
            Decision::Start
        );
    }

    #[test]
    fn stopped_waiter_file_does_not_block_the_next_waiter() {
        let run = job("r", 1, 50, State::Running, 0);
        let head = job("h", 1, 30, State::Queued, 1);
        let next = job("n", 1, 40, State::Queued, 2);
        let t = 3 + QUEUE_PASS_MS;
        assert!(waits(&evaluate(
            &next,
            &[run.clone(), head.clone(), next.clone()],
            &cap(),
            &calm(),
            t
        )));
        let stopped = Job {
            state: State::Stopped,
            ..head
        };
        assert_eq!(
            evaluate(&next, &[run, stopped, next.clone()], &cap(), &calm(), t),
            Decision::Start
        );
    }

    #[test]
    fn stopped_head_does_not_block_the_next_waiter() {
        let run = job("r", 1, 50, State::Running, 0);
        let head = job("h", 1, 30, State::Queued, 1);
        let next = job("n", 1, 40, State::Queued, 2);
        let all = [run, head, next.clone()];
        assert!(
            waits(&evaluate(&next, &all, &cap(), &calm(), 3 + QUEUE_PASS_MS)),
            "a live head that fits goes first"
        );
        let live = Live {
            stopped: vec!["h".into()],
            ..calm()
        };
        assert_eq!(
            evaluate(&next, &all, &cap(), &live, 3 + QUEUE_PASS_MS),
            Decision::Start
        );
    }

    fn mac() -> Capacity {
        Capacity {
            cpu: 16,
            mem: 64 * G,
            vram: 0,
            disk_total: 1000 * G,
            vram_is_ram: true,
        }
    }

    #[test]
    fn a_lagging_five_minute_load_never_blocks_an_idle_machine() {
        // 2026-11-20 03:08: cpu 12%, load1 22.6, load5 34.2, nothing running, one waiter blocked by load5.
        let me = job("a", 4, 4, State::Queued, 0);
        let live = Live {
            load1: 22.6,
            load5: 34.2,
            cpu_pct: 12,
            mem_used_pct: 46,
            non_heavy: Usage {
                cpu: 192,
                mem: 29 * G,
                vram: 0,
            },
            ..calm()
        };
        assert_eq!(
            evaluate(&me, std::slice::from_ref(&me), &mac(), &live, 0),
            Decision::Start
        );
        // 03:22: cpu 16%, load1 21.3, load5 32.9, one job running, three waiting.
        let run = Job {
            started_ms: Some(0),
            ..job("r", 4, 4, State::Running, 0)
        };
        let live = Live {
            load1: 21.3,
            load5: 32.9,
            cpu_pct: 16,
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 100,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            non_heavy: Usage {
                cpu: 156,
                mem: 24 * G,
                vram: 0,
            },
            ..live
        };
        assert_eq!(
            evaluate(&me, &[run, me.clone()], &mac(), &live, 100_000),
            Decision::Start
        );
    }

    #[test]
    fn rising_load_with_idle_cpu_is_io_wait_not_overload() {
        // 2026-11-20 06:59-07:01, a power and network outage: load1 46-58 against load5 24-26 (rising), cpu 16-28%,
        // nothing heavy running. 0.3.0 blocked on the rising load; it is threads stuck in I/O, not CPU work.
        let me = job("a", 4, 4, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        for (load1, load5, cpu_pct) in [(57.85, 24.18, 26), (53.44, 24.32, 28), (46.16, 25.83, 16)]
        {
            let live = Live {
                load1,
                load5,
                cpu_pct,
                mem_used_pct: 36,
                non_heavy: Usage {
                    cpu: cpu_pct * 16,
                    mem: 23 * G,
                    vram: 0,
                },
                ..calm()
            };
            assert_eq!(
                evaluate(&me, one, &mac(), &live, 0),
                Decision::Start,
                "load1 {load1} cpu {cpu_pct}%"
            );
            // The same load with a CPU 80% busy is builds piling up: still blocked.
            let busy = Live {
                cpu_pct: 80,
                ..live.clone()
            };
            assert!(
                matches!(
                    evaluate(&me, one, &mac(), &busy, 0),
                    Decision::Wait {
                        block: Block::Load1,
                        ..
                    }
                ),
                "load1 {load1} busy"
            );
        }
        // 4 x cores (64 on 16) at 16% CPU: the Mac has no PSI, so it blocks whatever the CPU says. Where PSI reads no
        // CPU pressure it is I/O wait (0.4.8); CPU pressure confirms it, and memory pressure blocks.
        let hard = Live {
            load1: 64.0,
            load5: 24.0,
            cpu_pct: 16,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, one, &mac(), &hard, 0),
            Decision::Wait {
                block: Block::Load1,
                ..
            }
        ));
        let hard = Live {
            cpu_pressure: Some(0),
            ..hard
        };
        assert_eq!(evaluate(&me, one, &mac(), &hard, 0), Decision::Start);
        let hard = Live {
            cpu_pressure: Some(30),
            ..hard
        };
        assert!(matches!(
            evaluate(&me, one, &mac(), &hard, 0),
            Decision::Wait {
                block: Block::Load1,
                ..
            }
        ));
        let pressured = Live {
            load1: 50.0,
            load5: 24.0,
            cpu_pct: 16,
            mem_pressure: true,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, one, &mac(), &pressured, 0),
            Decision::Wait {
                block: Block::Mem,
                ..
            }
        ));
    }

    #[test]
    fn nothing_starts_while_the_cpu_reads_pinned() {
        // 2026-11-21 on 0.4.7: 190 of 272 Mac starts in 6 hours came at 95-100% CPU, most in a dip between spikes of
        // bursty jobs. A pinned reading in the last 10 s blocks every start, a job alone included.
        let me = job("a", 1, 0, State::Queued, 0);
        let dip = Live {
            cpu_pct: 40,
            cpu_sustained_pct: 70,
            cpu_recent_peak_pct: CPU_PINNED_PCT,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, std::slice::from_ref(&me), &mac(), &dip, 0),
            Decision::Wait {
                block: Block::Cpu,
                ..
            }
        ));
        let calm_again = Live {
            cpu_recent_peak_pct: CPU_PINNED_PCT - 1,
            ..dip
        };
        assert_eq!(
            evaluate(&me, std::slice::from_ref(&me), &mac(), &calm_again, 0),
            Decision::Start
        );
        assert_eq!(
            recent_peak_pct(&[(0, 100), (5_000, 60), (12_000, 50)], 12_000),
            60,
            "only the last 10 s count"
        );
        assert_eq!(recent_peak_pct(&[], 0), 0);
    }

    #[test]
    fn a_saturated_machine_still_blocks() {
        // 2026-11-20 03:19: cpu 100%, load1 55: the guards hold even for a job that would run alone. A pinned CPU is
        // named first; at 90% busy the load guard holds.
        let me = job("a", 4, 4, State::Queued, 0);
        let live = Live {
            load1: 55.3,
            load5: 39.6,
            cpu_pct: 100,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, std::slice::from_ref(&me), &mac(), &live, 0),
            Decision::Wait {
                block: Block::Cpu,
                ..
            }
        ));
        let loaded = Live {
            cpu_pct: 90,
            ..live
        };
        assert!(matches!(
            evaluate(&me, std::slice::from_ref(&me), &mac(), &loaded, 0),
            Decision::Wait {
                block: Block::Load1,
                ..
            }
        ));
        // Load 120 on 16 cores (2026-11-19) blocks even if the CPU reads idle: 6 x cores always blocks.
        let storm = Live {
            load1: 120.0,
            cpu_pct: 5,
            ..calm()
        };
        assert!(matches!(
            evaluate(&me, std::slice::from_ref(&me), &mac(), &storm, 0),
            Decision::Wait {
                block: Block::Load1,
                ..
            }
        ));
    }

    #[test]
    fn clamp_to_machine_and_fold_vram() {
        let c = cap();
        let w = c.clamp(Weights {
            cpu: 64,
            mem: 999 * G,
            vram: 5 * G,
            disk: 9999 * G,
        });
        assert_eq!(
            w,
            Weights {
                cpu: 10,
                mem: 100 * G,
                vram: 0,
                disk: 1000 * G
            }
        );
        let mac = Capacity {
            vram_is_ram: true,
            ..c
        };
        assert_eq!(
            mac.clamp(Weights {
                cpu: 1,
                mem: 4 * G,
                vram: 2 * G,
                disk: 0
            })
            .mem,
            6 * G
        );
    }

    #[test]
    fn sustained_cpu_rises_at_once_and_falls_slowly() {
        assert_eq!(sustained_pct(70, &[], 0), 70, "no readings: the latest");
        assert_eq!(
            sustained_pct(10, &[(0, 10), (90_000, 100)], 5_000),
            10,
            "a reading from the future (clock skew) is dropped"
        );
        assert_eq!(
            sustained_pct(100, &[(0, 40), (5_000, 40)], 5_000),
            100,
            "a rise counts at once"
        );
        assert_eq!(
            sustained_pct(40, &[(0, 100), (10_000, 100), (20_000, 40)], 20_000),
            80,
            "a fall is averaged"
        );
        assert_eq!(
            sustained_pct(40, &[(0, 100), (25_000, 40)], 25_000),
            40,
            "readings over 20 s old are dropped"
        );
    }

    #[test]
    fn sustained_overload_is_blocked_by_the_two_times_cores_rule() {
        let c = mac();
        let over = Live {
            load1: 49.0,
            load5: 60.0,
            cpu_pct: 96,
            cpu_sustained_pct: 97,
            ..calm()
        };
        let (why, block) = guard_block(&c, &over).unwrap();
        assert_eq!(block, Block::Load1);
        assert!(
            why.contains("2 x 16 cores") && why.contains("cpu 97% busy"),
            "{why}"
        );
        // Alone on the machine it still waits: overload protection has no exception.
        let me = job("a", 1, 1, State::Queued, 0);
        assert!(waits(&evaluate(
            &me,
            std::slice::from_ref(&me),
            &c,
            &over,
            0
        )));
        // The same load with a calm CPU is I/O wait, not overload.
        let io = Live {
            cpu_pct: 30,
            cpu_sustained_pct: 30,
            ..over
        };
        assert_eq!(guard_block(&c, &io), None);
    }

    #[test]
    fn warn_pressure_blocks_only_with_memory_nearly_used_up() {
        let me = job("a", 1, 1, State::Queued, 1);
        let run = Job {
            started_ms: Some(0),
            ..job("r", 1, 1, State::Running, 0)
        };
        let both = [run, me.clone()];
        let warn = |mem_used_pct| Live {
            mem_pressure_warn: true,
            mem_used_pct,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, &both, &cap(), &warn(84), 100_000),
            Decision::Start
        );
        assert!(matches!(
            evaluate(&me, &both, &cap(), &warn(85), 100_000),
            Decision::Wait {
                block: Block::Mem,
                ..
            }
        ));
        // Without warn, memory used blocks from 95% (90% in 0.4.4).
        let used = |mem_used_pct| Live {
            mem_used_pct,
            ..calm()
        };
        assert_eq!(
            evaluate(&me, &both, &cap(), &used(94), 100_000),
            Decision::Start
        );
        assert!(waits(&evaluate(&me, &both, &cap(), &used(95), 100_000)));
    }

    #[test]
    fn a_lone_job_is_held_by_the_memory_and_runaway_guards_but_not_by_warn_alone() {
        let me = job("a", 16, 8, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        let c = mac();
        // 2026-11-21: warn pressure at 49% used with nothing running starts the job.
        let warn = Live {
            mem_used_pct: 49,
            mem_pressure_warn: true,
            cpu_pct: 27,
            ..calm()
        };
        assert_eq!(evaluate(&me, one, &c, &warn, 0), Decision::Start);
        // 95% used blocks a lone job, with or without warn.
        for warn_on in [false, true] {
            let full = Live {
                mem_used_pct: 97,
                mem_pressure_warn: warn_on,
                ..calm()
            };
            assert!(matches!(
                evaluate(&me, one, &c, &full, 0),
                Decision::Wait {
                    block: Block::Mem,
                    ..
                }
            ));
        }
        let critical = Live {
            mem_pressure: true,
            ..warn.clone()
        };
        assert!(waits(&evaluate(&me, one, &c, &critical, 0)));
        let runaway = Live {
            load1: 64.0,
            load5: 30.0,
            cpu_pct: 55,
            ..warn
        };
        assert!(matches!(
            evaluate(&me, one, &c, &runaway, 0),
            Decision::Wait {
                block: Block::Load1,
                ..
            }
        ));
    }

    #[test]
    fn a_blocked_waiter_says_why_with_the_numbers() {
        let run = Job {
            started_ms: Some(0),
            ..job("r", 1, 1, State::Running, 0)
        };
        let me = job("w", 2, 1, State::Queued, 1);
        let busy = Live {
            cpu_pct: 85,
            ..calm()
        };
        let Decision::Wait { reason, .. } =
            evaluate(&me, &[run, me.clone()], &cap(), &busy, 60_000)
        else {
            panic!()
        };
        assert_eq!(reason, "cpu: machine 85% busy, jobs start under 80%");
    }

    #[test]
    fn a_high_load_blocks_only_when_the_cpu_or_cpu_pressure_confirms_it() {
        // 2026-11-21 on the rig (16 cores): load1 35 with the CPU 8-15% busy and PSI cpu some 0.19: threads in I/O wait.
        let c = Capacity { cpu: 16, ..cap() };
        let me = job("a", 1, 1, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        let rig = Live {
            load1: 35.4,
            load5: 30.0,
            cpu_pct: 15,
            cpu_pressure: Some(0),
            ..calm()
        };
        assert_eq!(evaluate(&me, one, &c, &rig, 0), Decision::Start);
        // Even at 4 x cores (64), I/O wait alone admits; PSI cpu or a half-busy CPU confirms it.
        let storm = Live {
            load1: 70.0,
            load5: 60.0,
            ..rig.clone()
        };
        assert_eq!(evaluate(&me, one, &c, &storm, 0), Decision::Start);
        for confirmed in [
            Live {
                cpu_pressure: Some(CPU_PSI_PCT),
                ..storm.clone()
            },
            Live {
                cpu_pct: LOAD_HARD_CPU_PCT,
                ..storm.clone()
            },
        ] {
            let (why, block) = guard_block(&c, &confirmed).expect("blocked");
            assert_eq!(block, Block::Load1);
            assert!(why.contains("4 x 16 cores"), "{why}");
        }
        // From 2 x cores, tasks queueing for the CPU (PSI) block at a calm CPU reading too.
        let queued = Live {
            cpu_pressure: Some(40),
            ..rig.clone()
        };
        let (why, _) = guard_block(&c, &queued).expect("blocked");
        assert!(why.contains("cpu pressure 40%"), "{why}");
        assert_eq!(
            guard_block(
                &c,
                &Live {
                    load1: 20.0,
                    ..queued
                }
            ),
            None,
            "under 2 x cores PSI alone never blocks"
        );
        // The 2026-11-19 Mac (no PSI): load 120 on 16 cores with the CPU pinned still blocks.
        let mac_meltdown = Live {
            load1: 120.0,
            load5: 90.0,
            cpu_pct: 100,
            ..calm()
        };
        assert!(guard_block(&c, &mac_meltdown).is_some());
    }

    #[test]
    fn a_learned_vram_reservation_is_netted_against_live_use() {
        // 2026-11-21 on the rig: learned 1.9G reservations held for the whole run kept waiters on "vram" for 14 minutes
        // with the GPU at 1.8 of 10G.
        let c = Capacity {
            vram: 10 * G,
            ..mac()
        };
        let run = |declared: bool| {
            let j = job("r", 1, 1, State::Running, 0);
            Job {
                weights: Weights {
                    vram: 5 * G,
                    ..j.weights
                },
                started_ms: Some(0),
                mem_peak_at_ms: Some(10_000),
                vram_declared: declared,
                ..j
            }
        };
        let w = job("w", 1, 1, State::Queued, 1);
        let want = Job {
            weights: Weights {
                vram: 5 * G,
                ..w.weights
            },
            ..w
        };
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 0,
                    mem: G,
                    vram: G,
                },
            )],
            ..calm()
        };
        let late = VRAM_HOLD_MS + MEM_DECAY_MS;
        assert_eq!(vram_reserved(&[&run(false)], &live, late), G);
        assert_eq!(vram_reserved(&[&run(true)], &live, late), 5 * G);
        assert_eq!(vram_reserved(&[&run(false)], &live, 5_000), 5 * G);
        // Memory peaked at 10 s and its hold has decayed: a GPU job that allocates later keeps its VRAM reservation.
        assert_eq!(
            vram_reserved(&[&run(false)], &live, 10_000 + MEM_DECAY_MS),
            5 * G
        );
        assert_eq!(
            evaluate(&want, &[run(false), want.clone()], &c, &live, late),
            Decision::Start
        );
        assert!(matches!(
            evaluate(&want, &[run(true), want.clone()], &c, &live, late),
            Decision::Wait {
                block: Block::Vram,
                ..
            }
        ));
    }

    #[test]
    fn the_sustained_level_is_the_best_ten_second_mean() {
        assert_eq!(sustained_level(&[]), 0);
        // A 500 ms spike to 10 cores in a run on 1 core: the level stays near 1 core.
        let mut spike: Vec<(u64, u64)> = (0..60).map(|i| (i * 500, 100)).collect();
        spike[30].1 = 1_000;
        assert_eq!(sustained_level(&spike), 145);
        // 15 s at 8 cores, then 1 core: the 10 s window inside the burst.
        let burst: Vec<(u64, u64)> = (0..80)
            .map(|i| (i * 500, if i < 30 { 800 } else { 100 }))
            .collect();
        assert_eq!(sustained_level(&burst), 800);
        // Shorter than the window: the mean of the run.
        assert_eq!(sustained_level(&[(0, 100), (500, 300)]), 200);
    }

    fn started(id: &str, cpu: u32, at: i64) -> Job {
        Job {
            started_ms: Some(at),
            ..job(id, cpu, 1, State::Running, at)
        }
    }

    fn on_mac(pct: u64) -> Live {
        Live {
            cpu_pct: pct,
            cpu_sustained_pct: pct,
            ..calm()
        }
    }

    #[test]
    fn jobs_start_while_the_machine_reads_under_the_cpu_line() {
        // A long-running job whatever its learned peak: only the measured CPU counts once it has settled. The waiter
        // learned a 16-core peak but typically uses 2, so the size check passes too.
        let run = started("r", 16, 0);
        let me = Job {
            typical_cpu: Some(2),
            ..job("w", 16, 1, State::Queued, 1)
        };
        let jobs = [run, me.clone()];
        let at = |pct| evaluate(&me, &jobs, &mac(), &on_mac(pct), 60_000);
        assert_eq!(at(CPU_ADMIT_PCT - 1), Decision::Start);
        assert!(matches!(
            at(CPU_ADMIT_PCT),
            Decision::Wait {
                block: Block::Cpu,
                live: true,
                ..
            }
        ));
        // The sustained reading counts too: a dip between spikes does not admit.
        let dip = Live {
            cpu_pct: 20,
            cpu_sustained_pct: 85,
            ..calm()
        };
        assert!(waits(&evaluate(&me, &jobs, &mac(), &dip, 60_000)));
    }

    #[test]
    fn a_new_job_settles_before_its_size_is_trusted_to_the_measured_cpu() {
        // A 16-core job that started 2 s ago reads 1 core: the 80% line (12.8 cores) has no room for another big one
        // until it settles, and the waiter sleeps until exactly then.
        let run = started("r", 16, 0);
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 100,
                    mem: G,
                    vram: 0,
                },
            )],
            ..on_mac(10)
        };
        let me = job("w", 8, 1, State::Queued, 1);
        let jobs = [run, me.clone()];
        assert_eq!(
            evaluate(&me, &jobs, &mac(), &live, 2_000),
            Decision::Wait {
                reason: "cpu: jobs that just started are settling; 24.60 cores with theirs, 12.80 under the 80% line"
                    .into(),
                live: true,
                block: Block::Ramp,
                ramp_end_ms: Some(SETTLE_MS),
            }
        );
        assert_eq!(
            evaluate(&me, &jobs, &mac(), &live, SETTLE_MS),
            Decision::Start
        );
    }

    #[test]
    fn small_jobs_fit_beside_settling_ones() {
        // Two 2-core jobs just started on an idle Mac: 1.6 + 2 + 2 + 2 cores is under the 12.8-core line.
        let jobs = [
            started("a", 2, 0),
            started("b", 2, 0),
            job("w", 2, 1, State::Queued, 1),
        ];
        assert_eq!(
            evaluate(&jobs[2], &jobs, &mac(), &on_mac(10), 1_000),
            Decision::Start
        );
        // A not-yet-stamped job settles too (it holds its full size).
        let unstamped = Job {
            started_ms: None,
            ..started("c", 12, 0)
        };
        let jobs = [unstamped, job("w", 2, 1, State::Queued, 1)];
        let d = evaluate(&jobs[1], &jobs, &mac(), &on_mac(10), 1_000);
        assert!(
            matches!(d, Decision::Wait { block: Block::Ramp, ramp_end_ms: Some(t), .. } if t == 1_000 + SETTLE_MS),
            "{d:?}"
        );
    }

    #[test]
    fn the_oldest_waiter_that_can_start_goes_first() {
        let run = started("r", 1, 0);
        let head = job("h", 4, 1, State::Queued, 1);
        let next = job("n", 1, 1, State::Queued, 2);
        let jobs = [run, head.clone(), next.clone()];
        let d = evaluate(&next, &jobs, &mac(), &on_mac(10), 60_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Queue,
                    ..
                }
            ),
            "{d:?}"
        );
        assert_eq!(
            evaluate(&head, &jobs, &mac(), &on_mac(10), 60_000),
            Decision::Start
        );
    }

    #[test]
    fn a_settle_blocked_head_holds_the_ones_behind_it_until_the_settle_ends() {
        // A big head waits for a fresh job to settle; a small follower that would fit now must not take its room.
        let run = started("r", 8, 50_000);
        let head = job("h", 8, 1, State::Queued, 1);
        let next = job("n", 1, 1, State::Queued, 2);
        let jobs = [run, head, next.clone()];
        let d = evaluate(&next, &jobs, &mac(), &on_mac(10), 52_000);
        assert!(
            matches!(d, Decision::Wait { block: Block::Queue, ramp_end_ms: Some(t), .. } if t == 50_000 + SETTLE_MS),
            "{d:?}"
        );
    }

    #[test]
    fn a_cpu_blocked_head_does_not_hold_the_queue_but_a_long_memory_wait_does() {
        let run = Job {
            started_ms: Some(0),
            ..job("r", 1, 40, State::Running, 0)
        };
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 100,
                    mem: 40 * G,
                    vram: 0,
                },
            )],
            ..on_mac(85)
        };
        // At 85% both wait on the CPU line; the follower names its own reason, not the queue.
        let head = job("h", 1, 1, State::Queued, 1);
        let next = job("n", 1, 1, State::Queued, 2);
        let d = evaluate(
            &next,
            &[run.clone(), head, next.clone()],
            &mac(),
            &live,
            60_000,
        );
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Cpu,
                    ..
                }
            ),
            "{d:?}"
        );
        // A head blocked on memory: small followers pass it for 5 minutes, then the queue holds for it.
        let calm_cpu = Live {
            cpu_pct: 10,
            cpu_sustained_pct: 10,
            ..live
        };
        let big = job("h", 1, 30, State::Queued, 1);
        let jobs = [run, big, next.clone()];
        assert_eq!(
            evaluate(&next, &jobs, &mac(), &calm_cpu, 60_000),
            Decision::Start
        );
        assert!(matches!(
            evaluate(&next, &jobs, &mac(), &calm_cpu, 1 + QUEUE_PASS_MS),
            Decision::Wait {
                block: Block::Queue,
                ..
            }
        ));
    }

    #[test]
    fn the_settle_window_follows_the_learned_peak_and_covers_cold_and_docker_jobs() {
        assert_eq!(settle_window_ms(false, Some(0)), SETTLE_MS);
        assert_eq!(settle_window_ms(false, Some(45_000)), 45_000);
        assert_eq!(settle_window_ms(false, Some(600_000)), SETTLE_MAX_MS);
        assert_eq!(settle_window_ms(false, None), SETTLE_COLD_MS);
        assert_eq!(settle_window_ms(true, Some(0)), SETTLE_COLD_MS);
        assert_eq!(settle_window_ms(true, Some(50_000)), 50_000);
    }

    fn cold(id: &str, enq: i64) -> Job {
        Job {
            typical_cpu: None,
            settle_ms: Some(settle_window_ms(false, None)),
            ..job(id, 16, 1, State::Queued, enq)
        }
    }

    #[test]
    fn a_herd_of_cold_builds_starts_one_per_settle_window_within_the_peak_cap() {
        // Six cold whole-machine `cargo build`s queue at once and spend minutes fetching crates: the machine reads 20%
        // the whole time, so only the settle window and the peak cap stand between them and a herd.
        let mut jobs: Vec<Job> = (0..6).map(|i| cold(&format!("c{i}"), i)).collect();
        let mut starts = Vec::new();
        for now in (0..600_000).step_by(1_000) {
            for i in 0..jobs.len() {
                if jobs[i].state != State::Queued {
                    continue;
                }
                if evaluate(&jobs[i], &jobs, &mac(), &on_mac(20), now) == Decision::Start {
                    jobs[i].state = State::Running;
                    jobs[i].started_ms = Some(now);
                    starts.push(now);
                }
            }
            let running: Vec<&Job> = jobs.iter().filter(|j| j.state == State::Running).collect();
            assert!(
                peak_sum(&running, &mac(), now) * PEAK_SUM_CAP.1 <= centi(16) * PEAK_SUM_CAP.0,
                "peak cap broken at {now}"
            );
        }
        // One start per settle window, and never more than two young whole-machine jobs.
        assert!(
            starts.windows(2).all(|w| w[1] - w[0] >= SETTLE_COLD_MS),
            "{starts:?}"
        );
        assert_eq!(&starts[..3], &[0, SETTLE_COLD_MS, 2 * SETTLE_COLD_MS]);
        // The cap alone: two cold builds 40 s and 10 s old already compile at 4 cores each (settled), so a third
        // cold one waits on the 40-core cap until the older one is a minute old; a 1-core job still fits.
        let busy = |id: &str, at: i64| Job {
            state: State::Running,
            ..started(id, 16, at)
        };
        let shown = Live {
            per_job: ["a", "b"]
                .iter()
                .map(|id| {
                    (
                        (*id).to_string(),
                        Usage {
                            cpu: 400,
                            mem: G,
                            vram: 0,
                        },
                    )
                })
                .collect(),
            ..on_mac(50)
        };
        let third = cold("t", 11);
        let small = job("s", 1, 1, State::Queued, 12);
        let jobs = [
            busy("a", 0),
            busy("b", 30_000),
            third.clone(),
            small.clone(),
        ];
        let d = evaluate(&third, &jobs, &mac(), &shown, 40_000);
        assert!(
            matches!(d, Decision::Wait { block: Block::Cpu, live: true, ref reason, .. } if reason.contains("peaks")),
            "{d:?}"
        );
        assert_eq!(
            evaluate(&small, &jobs, &mac(), &shown, 40_000),
            Decision::Start
        );
        assert_eq!(
            evaluate(&third, &jobs, &mac(), &shown, SETTLE_MAX_MS),
            Decision::Start
        );
    }

    #[test]
    fn a_docker_build_keeps_the_machine_settled_until_its_window_ends() {
        // A `docker build` pulls images in the VM: its process tree reads 0 cores, the machine 10%. Another build
        // waits for the 30 s window, not 10 s.
        let docker = Job {
            cmd: vec!["docker".into(), "build".into(), ".".into()],
            settle_ms: Some(settle_window_ms(true, Some(0))),
            ..started("d", 16, 0)
        };
        let me = job("w", 4, 1, State::Queued, 1);
        let jobs = [docker, me.clone()];
        let live = Live {
            per_job: vec![("d".into(), Usage::default())],
            ..on_mac(10)
        };
        for now in [SETTLE_MS, 20_000, SETTLE_COLD_MS - 1] {
            let d = evaluate(&me, &jobs, &mac(), &live, now);
            assert!(
                matches!(d, Decision::Wait { block: Block::Ramp, ramp_end_ms: Some(t), .. } if t == SETTLE_COLD_MS),
                "{now}: {d:?}"
            );
        }
        assert_eq!(
            evaluate(&me, &jobs, &mac(), &live, SETTLE_COLD_MS),
            Decision::Start
        );
    }

    #[test]
    fn the_size_check_holds_with_no_job_settling() {
        // A settled job and the machine at 79%: under the CPU line, but a job that typically uses 16 cores would take
        // it to 28.6 cores. It waits; a small one fits (12.64 + 2 <= 15.2).
        let run = started("r", 16, 0);
        let at =
            |w: &Job, pct| evaluate(w, &[run.clone(), w.clone()], &mac(), &on_mac(pct), 60_000);
        let big = job("b", 12, 1, State::Queued, 1);
        assert!(matches!(
            at(&big, 79),
            Decision::Wait {
                block: Block::Cpu,
                live: true,
                ..
            }
        ));
        assert_eq!(at(&big, 10), Decision::Start);
        // Small jobs pass it while it waits, but not for ever: after 5 minutes the queue holds for it.
        let small = job("s", 1, 1, State::Queued, 2);
        let jobs = [run.clone(), big, small.clone()];
        assert_eq!(
            evaluate(&small, &jobs, &mac(), &on_mac(79), 60_000),
            Decision::Start
        );
        assert!(matches!(
            evaluate(&small, &jobs, &mac(), &on_mac(79), 1 + QUEUE_PASS_MS),
            Decision::Wait {
                block: Block::Queue,
                ..
            }
        ));
        assert_eq!(at(&job("s", 2, 1, State::Queued, 1), 79), Decision::Start);
        // It weighs the typical cores, not the peak: `cargo test` learned a 16-core peak and averages 2.4.
        let bursty = Job {
            typical_cpu: Some(2),
            ..job("t", 16, 1, State::Queued, 1)
        };
        assert_eq!(at(&bursty, 79), Decision::Start);
        // A cold job counts as a quarter of the machine.
        let c = cold("c", 1);
        assert!(waits(&at(&c, 79)));
        assert_eq!(at(&c, 40), Decision::Start);
    }

    #[test]
    fn a_waiter_from_an_older_release_never_holds_the_queue() {
        // A 0.4.9 waiter decides by the old rules, which may hold it while these would start it: a 0.4.10 waiter that
        // queued behind it would idle the machine.
        let run = started("r", 1, 0);
        let old = Job {
            v: 0,
            ..job("h", 4, 1, State::Queued, 1)
        };
        let next = job("n", 1, 1, State::Queued, 2);
        assert_eq!(
            evaluate(
                &next,
                &[run, old, next.clone()],
                &mac(),
                &on_mac(10),
                60_000
            ),
            Decision::Start
        );
    }
}
