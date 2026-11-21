//! Admission: pure decisions.
//!
//! Memory is judged by measured headroom, like CPU: 90% of RAM minus non-heavy use, minus each running job's live use,
//! minus the part of each running job's reservation it has not yet used (its unmet part). A declared (`--mem`) size is
//! held for the whole run. A learned size is held until the job's learned time to peak (`mem_peak_at_ms`; at least
//! `RAMP_MS`, and `MEM_FALLBACK_HOLD_MS` when unknown), then decays linearly to nothing over `MEM_DECAY_MS`: builds
//! peak late, and a hold cut at 10 s let 12 jobs of a 12G peak start on a 64G Mac (reviewer's simulation). Only the decaying tails after
//! the peak time are capped, together at `MEM_HELD_CAP` of the free memory, so measured use still admits jobs. Reservations that
//! only sum high never block (2026-11-20: reservations summing over 85% blocked jobs at 43% memory used and left the
//! Mac idle). VRAM stays reservation-based: running reservations plus the request within 90% of capacity.
//! CPU is judged by live headroom only, never by peak-core reservations (a build
//! that peaks near all cores would otherwise block everything on an idle machine):
//!
//! cpu headroom = 90% of all cores - max(instant use, sustained use)
//!
//! A job asks for one free core under that 90% target, so jobs start while the machine is at most about 84% busy (83%
//! on 16 cores) and the CPU settles in the 80-90% band while jobs wait. Instant use is non-heavy usage plus each running
//! job's live use (or its ramp hold); sustained use is the machine's CPU over the last `BUSY_WINDOW_MS` (max(latest,
//! mean of the readings every heavy process shares)) plus the part of the ramp holds not yet in use. Without the
//! sustained term a single 250 ms window that caught builds between steps read 80% on a machine pinned at 100%, and
//! admitted another build into it (2026-11-20: 20% of admissions on the Mac and the rig came while the previous sample
//! read 90-100%).
//!
//! A job with a learned (or declared) CPU size holds its peak cores (clamped to the machine) until that peak shows in its
//! live use (3/4 of it) or its learned time to peak passes (`cpu_hold_ms`: history's slowest recent time to peak plus 25%, 10 s to 2 min, 30 s when
//! unknown), the same way a learned memory size is held. A burst of builds then starts one at a time as their peaks
//! show: after 0.4.2, holds that ended after 10 s (or at once on an idle-looking machine) let several 16-core cargo builds
//! in before their CPU showed, and 42% of the samples while jobs waited read CPU over 95% with load1 over 3 x cores.
//!
//! Cold jobs hold the whole machine like that, for the 30 s fallback. Docker/VM work and job files from before 0.4.3 keep the ramp schedule: a job admitted in the last
//! `RAMP_MS` holds its peak cores while its CPU ramps up, so a burst of waiters cannot all start at once. A full 10 s hold (`RAMP_MS`) covers cold jobs, Docker/VM jobs and
//! learned peaks of 75% of the machine or more (`FULL_HOLD_FRACTION`), so those start at least 10 s apart: their real use
//! is unknown or large and may show up late. Any smaller learned peak is full for the first `RAMP_FULL_MS` (the stagger floor); after that the measured machine decides:
//! under `RAMP_IDLE_CPU_PCT` busy the job has shown its real use and the hold is released, otherwise it shrinks linearly
//! to nothing at `RAMP_MS`. After the ramp only live use counts. Memory headroom likewise holds a job's unused
//! reservation only until `RAMP_MS` after admission.
//!
//! Non-heavy usage (Chrome, Discord, other CLIs) is smoothed by the caller: max(latest sample, mean of the last 3), so
//! a rise blocks at once and a fall is released slowly. Live guards (load, memory, disk) can only veto.
//!
//! Load: a 1-minute load of 4 x cores blocks whatever the CPU says (an I/O storm; on 2026-11-19 load 120 on 16 cores
//! killed Docker). From 2 x cores up, a load that is still rising (load1 > 1.1 x load5) blocks while the CPU is at
//! least 75% busy, and any load there blocks while the CPU is at least 90% busy. Load without CPU is not CPU overload:
//! on macOS load also counts threads blocked in I/O (network, disk), so it can read 20-60 with the CPU nearly idle
//! (a power or network outage on 2026-11-20 read load1 58 at 26% CPU with nothing heavy running). Memory pressure
//! (macOS level, Linux PSI) also blocks.

use std::collections::VecDeque;

use crate::job::{Capacity, Job, Live, State, Usage, Weights};

/// A backfilling job may pass the head only while the head has waited less than this.
pub const BACKFILL_WINDOW_MS: i64 = 60_000;
const PCT: u64 = 90;
/// The CPU target: jobs start while the machine's CPU plus one core stays within this % of every core.
const CPU_PCT: u64 = 90;
/// How far back the shared CPU readings count towards the sustained CPU.
pub const BUSY_WINDOW_MS: i64 = 20_000;
/// Load at or over this many times the core count blocks only while the CPU is also this busy.
const LOAD_SOFT: f64 = 2.0;
const LOAD_BUSY_CPU_PCT: u64 = 90;
/// A rising load at or over `LOAD_SOFT` x cores blocks only while the CPU is at least this busy; below it the load is
/// threads waiting on I/O, not CPU work (and blocking there would leave the CPU under the 75% that counts as underused).
const LOAD_RISING_CPU_PCT: u64 = 75;
/// Load at or over this many times the core count blocks whatever the CPU says.
const LOAD_HARD: f64 = 4.0;
/// A job admitted this recently has not ramped its CPU yet.
pub const RAMP_MS: i64 = 10_000;
/// A learned or declared CPU size with no known time to peak is held this long, unless its peak shows sooner.
pub const CPU_FALLBACK_HOLD_MS: i64 = 30_000;
/// A learned CPU peak is never held longer than this, whatever history says: a run that never reaches its learned peak
/// (an incremental build) must not keep the machine reserved.
pub const CPU_MAX_HOLD_MS: i64 = 120_000;
/// A held CPU peak has shown up once the job's live use reaches this fraction (3/4) of it.
const CPU_SHOWN: (u64, u64) = (3, 4);
/// For this long after admission a ramping job holds its full peak, whatever the machine reads: the stagger floor.
pub const RAMP_FULL_MS: i64 = 3_000;
/// A learned peak of at least this fraction (3/4) of the machine keeps the full `RAMP_MS` hold, like a whole-machine hold: two
/// such jobs together would overload it, and an idle reading right after the floor proves nothing yet.
const FULL_HOLD_FRACTION: (u64, u64) = (3, 4);
/// A learned memory size with no known time to peak is held this long after admission.
pub const MEM_FALLBACK_HOLD_MS: i64 = 60_000;
/// After its hold, a learned size's unused part shrinks linearly to nothing over this long.
pub const MEM_DECAY_MS: i64 = 60_000;
/// The learned unused parts together may take at most this fraction (3/4) of the memory still free (ceiling minus
/// machine use), so a burst cannot all peak at once while measured use still admits jobs.
const MEM_HELD_CAP: (u64, u64) = (3, 4);
/// After `RAMP_FULL_MS`, a machine measured under this CPU % has room the ramping job is not using: its hold is released.
const RAMP_IDLE_CPU_PCT: u64 = 50;
/// Granularity of the search for the moment a ramp-blocked waiter will fit.
const RAMP_STEP_MS: i64 = 250;
const CPU_ASK: u64 = 100;
/// The least CPU (2 cores) a held peak leaves free in the band for small waiters (`backfill`).
const BACKFILL_CPU: u64 = 200;
/// How many recent non-heavy samples the smoother averages (waiters re-sample every 10 s).
pub const SMOOTH_WINDOW: usize = 3;

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
}

fn blocked(reason: impl Into<String>, live: bool, block: Block) -> Blocked {
    Blocked {
        reason: reason.into(),
        live,
        block,
        ramp_end_ms: None,
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

fn ceiling(cap: &Capacity) -> Usage {
    Usage {
        cpu: centi(cap.cpu) * CPU_PCT / 100,
        mem: cap.mem / 100 * PCT,
        vram: cap.vram / 100 * PCT,
    }
}

/// The CPU (centi-cores) kept out of the hold reservation for small waiters: 2 cores, or 1/8 of the machine if more.
fn backfill(cap: &Capacity) -> u64 {
    (centi(cap.cpu) / 8).max(BACKFILL_CPU)
}

fn request(w: Weights) -> Usage {
    Usage {
        cpu: if w.cpu > 0 { CPU_ASK } else { 0 },
        mem: w.mem,
        vram: w.vram,
    }
}

fn fits(ask: Usage, room: Usage) -> bool {
    ask.cpu <= room.cpu && ask.mem <= room.mem && ask.vram <= room.vram
}

/// VRAM reservations only: running sums plus the request within 90% of capacity. Memory and CPU are live.
#[must_use]
pub fn fits_reservations(req: Weights, running: &[&Job], cap: &Capacity) -> bool {
    let used: u64 = running.iter().map(|j| j.weights.vram).sum();
    req.vram <= ceiling(cap).vram.saturating_sub(used)
}

/// The part of a reservation a job `elapsed_ms` after admission has not yet used. A `declared` size is held for the
/// whole run. A learned one is held until `peak_at_ms` (at least `RAMP_MS`; `MEM_FALLBACK_HOLD_MS` when unknown), then
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
    let hold = peak_at_ms.unwrap_or(MEM_FALLBACK_HOLD_MS).max(RAMP_MS);
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

/// The CPU a job `elapsed_ms` into its ramp still holds beyond its live use (`peak` already clamped to `machine`). A
/// hold of Docker/VM work (`vm`), a cold whole-machine hold or a peak of `FULL_HOLD_FRACTION` of the machine is full for
/// all of `RAMP_MS`. A smaller learned peak is full for `RAMP_FULL_MS`, then nothing if the machine is measured under `RAMP_IDLE_CPU_PCT` busy, else it shrinks linearly to 0
/// at `RAMP_MS`.
#[must_use]
pub fn ramp_hold(peak: u64, machine: u64, elapsed_ms: i64, cpu_pct: u64, vm: bool) -> u64 {
    if elapsed_ms >= RAMP_MS {
        return 0;
    }
    if elapsed_ms < RAMP_FULL_MS
        || vm
        || peak * FULL_HOLD_FRACTION.1 >= machine * FULL_HOLD_FRACTION.0
    {
        return peak;
    }
    if cpu_pct < RAMP_IDLE_CPU_PCT {
        return 0;
    }
    let left = u64::try_from(RAMP_MS - elapsed_ms).unwrap_or(0);
    let span = u64::try_from(RAMP_MS - RAMP_FULL_MS).unwrap_or(1);
    peak * left / span
}

/// How long a learned or declared CPU peak is held, from history's slowest recent time to peak (`None`: unknown) plus
/// a 25% margin: at least `RAMP_MS`, at most `CPU_MAX_HOLD_MS`, and `CPU_FALLBACK_HOLD_MS` when unknown.
#[must_use]
pub fn cpu_hold_ms(peak_at_ms: Option<u64>) -> i64 {
    peak_at_ms
        .and_then(|ms| i64::try_from(ms.saturating_add(ms / 4)).ok())
        .unwrap_or(CPU_FALLBACK_HOLD_MS)
        .clamp(RAMP_MS, CPU_MAX_HOLD_MS)
}

/// The CPU a running job holds beyond its live use. A job with a learned or declared size (`hold_ms`) holds its whole
/// peak until the peak shows in its live use (3/4 of it) or `hold_ms` has passed, like a learned memory size, never
/// less than the `RAMP_FULL_MS` floor: a cargo build spends its first 10-30 s on a few cores, and a hold that ended
/// sooner (or was released because the machine read idle) let several 16-core builds in before any of their CPU showed
/// (2026-11-20). Docker/VM work (its CPU never shows in the tree) and job files without `hold_ms` keep the
/// `RAMP_MS` schedule of `ramp_hold`.
#[must_use]
pub fn cpu_hold(
    peak: u64,
    machine: u64,
    elapsed_ms: i64,
    cpu_pct: u64,
    vm: bool,
    live_cpu: u64,
    hold_ms: Option<i64>,
) -> u64 {
    match hold_ms {
        Some(hold) if !vm => {
            if elapsed_ms < RAMP_FULL_MS {
                peak
            } else if elapsed_ms >= hold || live_cpu * CPU_SHOWN.1 >= peak * CPU_SHOWN.0 {
                0
            } else {
                peak
            }
        }
        _ => ramp_hold(peak, machine, elapsed_ms, cpu_pct, vm),
    }
}

/// When the CPU hold of a job started at `started_ms` ends at the latest.
fn cpu_hold_end(j: &Job, started_ms: i64) -> i64 {
    started_ms
        + match j.cpu_hold_ms {
            Some(h) if !crate::run::hosts_vm_work(&j.cmd) => h.max(RAMP_FULL_MS),
            _ => RAMP_MS,
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

/// What is left for a new job once the other programs and the running jobs' real use are counted. CPU counts the
/// larger of the instant use (non-heavy plus each job's live use or ramp hold) and the sustained machine CPU plus the
/// ramp holds not yet in use.
#[cfg(test)]
#[must_use]
pub fn headroom(running: &[&Job], cap: &Capacity, live: &Live, now_ms: i64) -> Usage {
    headroom_for(running, cap, live, now_ms, false)
}

/// `headroom` for a request that is `small` (at most `backfill` cores): the unrealized part of the held peaks leaves
/// that backfill share of the CPU band free, so a 1-core waiter is not blocked by a 16-core hold at 13% CPU. A big
/// request sees the whole hold.
#[must_use]
pub fn headroom_for(
    running: &[&Job],
    cap: &Capacity,
    live: &Live,
    now_ms: i64,
    small: bool,
) -> Usage {
    let (mut held, (unmet, unmet_small), (pre_peak, tail)) = running.iter().fold(
        (Usage::default(), (0, 0), (0u64, 0u64)),
        |(a, unmet, learned), j| {
            let l = live
                .per_job
                .iter()
                .find(|(id, _)| *id == j.id)
                .map_or_else(Usage::default, |(_, u)| *u);
            let hold = j.started_ms.map_or(0, |t| {
                cpu_hold(
                    centi(j.weights.cpu).min(centi(cap.cpu)),
                    centi(cap.cpu),
                    now_ms.saturating_sub(t).max(0),
                    live.cpu_pct,
                    crate::run::hosts_vm_work(&j.cmd),
                    l.cpu,
                    j.cpu_hold_ms,
                )
            });
            let cpu = l.cpu.max(hold);
            // A running job not yet stamped, or one with a declared size, holds its whole unused part; a learned size holds
            // it by schedule, and those holds are capped together below.
            let (mem, learned_unmet) = match j.started_ms {
                Some(t) if !j.mem_declared => (
                    l.mem,
                    mem_unmet_split(
                        j.weights.mem,
                        l.mem,
                        now_ms.saturating_sub(t).max(0),
                        false,
                        j.mem_peak_at_ms,
                    ),
                ),
                Some(t) => (
                    l.mem
                        + mem_unmet(
                            j.weights.mem,
                            l.mem,
                            now_ms.saturating_sub(t).max(0),
                            true,
                            None,
                        ),
                    (0, 0),
                ),
                None => (l.mem + j.weights.mem.saturating_sub(l.mem), (0, 0)),
            };
            (
                Usage {
                    cpu: a.cpu + cpu,
                    mem: a.mem + mem,
                    vram: a.vram + l.vram.max(j.weights.vram),
                },
                {
                    let own = hold.saturating_sub(l.cpu);
                    // A small job's own unrealized hold is not part of the capped big reservation: it stacks.
                    let is_small =
                        small && centi(j.weights.cpu).min(centi(cap.cpu)) <= backfill(cap);
                    (unmet.0 + own, unmet.1 + if is_small { own } else { 0 })
                },
                (learned.0 + learned_unmet.0, learned.1 + learned_unmet.1),
            )
        },
    );
    let free = ceiling(cap)
        .mem
        .saturating_sub(live.non_heavy.mem)
        .saturating_sub(held.mem);
    // Before a job's learned time-to-peak its hold is full (the cap would leave free memory unprotected while peaks are
    // still coming); only the decaying tail after the peak is capped to a fraction of what is still free.
    held.mem += pre_peak;
    let free = free.saturating_sub(pre_peak);
    held.mem += tail.min(free / MEM_HELD_CAP.1 * MEM_HELD_CAP.0);
    // The unrealized part of a held peak reserves the CPU band, except a backfill share: a big hold (16 cores on 16)
    // would otherwise block even a 1-core waiter for its whole hold while the machine reads 13%. Real use still counts in
    // full, so the band is never exceeded by what is measured, only by what is merely expected.
    let limit = if small {
        ceiling(cap).cpu.saturating_sub(backfill(cap))
    } else {
        u64::MAX
    };
    // The backfill share is one shared budget: the big holds are capped to leave it free, and every admitted small
    // job's own unrealized hold (`unmet_small`) is then spent from it, on top of the cap.
    let live_jobs = held.cpu.saturating_sub(unmet);
    let held_cpu = held
        .cpu
        .saturating_sub(unmet_small)
        .min(limit.saturating_sub(live.non_heavy.cpu))
        .max(live_jobs)
        + unmet_small;
    let measured = live.cpu_sustained_pct * centi(cap.cpu) / 100;
    let instant = live.non_heavy.cpu + held_cpu;
    let sustained = (measured + unmet - unmet_small).min(limit).max(measured) + unmet_small;
    let room = ceiling(cap)
        .saturating_sub(live.non_heavy)
        .saturating_sub(held);
    Usage {
        cpu: ceiling(cap).cpu.saturating_sub(instant.max(sustained)),
        ..room
    }
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

/// Live guards, a veto only. The 1-minute load blocks at 4 x cores always (runaway load). From 2 x cores up it blocks
/// while it is still rising (load1 > 1.1 x load5) with the CPU at least 75% busy, and while the CPU is at least 90%
/// busy (the CPU is the larger of the latest and the sustained reading); a high load with a calm CPU is I/O wait (macOS
/// counts blocked threads), so it admits. Memory used at 90% or memory pressure blocks.
#[must_use]
pub fn guard_block(cap: &Capacity, live: &Live) -> Option<(String, Block)> {
    let cores = f64::from(cap.cpu);
    let (soft, hard) = (LOAD_SOFT * cores, LOAD_HARD * cores);
    let cpu = live.cpu_pct.max(live.cpu_sustained_pct);
    if live.load1 >= hard {
        return Some((
            format!(
                "1-minute load {:.1} >= {hard:.0} (4 x {} cores)",
                live.load1, cap.cpu
            ),
            Block::Load1,
        ));
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
        if cpu >= LOAD_BUSY_CPU_PCT {
            return Some((
                format!(
                    "1-minute load {:.1} >= {soft:.0} (2 x {} cores) and cpu {cpu}% busy",
                    live.load1, cap.cpu
                ),
                Block::Load1,
            ));
        }
    }
    if live.mem_used_pct >= PCT {
        return Some((format!("memory {}% used", live.mem_used_pct), Block::Mem));
    }
    if live.mem_pressure {
        return Some(("the system is under memory pressure".into(), Block::Mem));
    }
    None
}

/// Why a request cannot start now, or `None`. Alone (nothing running) a clamped request always passes
/// the resource checks, so a busy desktop cannot starve it forever; guards still apply.
fn blocker(
    w: Weights,
    running: &[&Job],
    cap: &Capacity,
    live: &Live,
    now_ms: i64,
) -> Option<Blocked> {
    if !running.is_empty() {
        if !fits_reservations(w, running, cap) {
            return Some(blocked("vram reservations full", false, Block::Vram));
        }
        let small = centi(w.cpu) <= backfill(cap);
        let mut ask = request(w);
        if small {
            // A small waiter's learned peak must fit in what the band has left after every outstanding hold.
            ask.cpu = ask.cpu.max(centi(w.cpu));
        }
        let room_at = |t: i64| headroom_for(running, cap, live, t, small);
        if !fits(ask, room_at(now_ms)) {
            // Would it fit without the ramp holds? Then the ramp is the blocker, and the holds shrink on a known schedule:
            // name the first moment it fits with the machine as measured now (at the latest, when the last ramp ends).
            if fits(ask, room_at(i64::MAX)) {
                let last = running
                    .iter()
                    .filter_map(|j| j.started_ms.map(|t| cpu_hold_end(j, t)))
                    .filter(|e| *e > now_ms)
                    .max();
                let end = last.map(|last| {
                    // Every step, plus the exact moments the stagger floors end (where an idle machine releases a hold).
                    let floors = running
                        .iter()
                        .filter_map(|j| j.started_ms)
                        .map(|t| t + RAMP_FULL_MS);
                    let mut at: Vec<i64> = (1..=(last - now_ms) / RAMP_STEP_MS)
                        .map(|k| now_ms + k * RAMP_STEP_MS)
                        .chain(floors)
                        .filter(|t| *t > now_ms && *t < last)
                        .collect();
                    at.sort_unstable();
                    at.into_iter()
                        .find(|t| fits(ask, room_at(*t)))
                        .unwrap_or(last)
                });
                return Some(Blocked {
                    reason: "no headroom (jobs admitted a moment ago are still ramping up)".into(),
                    live: true,
                    block: Block::Ramp,
                    ramp_end_ms: end,
                });
            }
            let room = room_at(now_ms);
            let block = if ask.cpu > room.cpu {
                Block::Cpu
            } else if ask.mem > room.mem {
                Block::Mem
            } else {
                Block::Vram
            };
            return Some(blocked(
                "no headroom (other programs and running jobs use the machine)",
                true,
                block,
            ));
        }
    }
    guard_block(cap, live).map(|(why, block)| blocked(why, true, block))
}

/// Can `job` start now, given every job file (already pruned of dead ones)?
#[must_use]
pub fn evaluate(job: &Job, jobs: &[Job], cap: &Capacity, live: &Live, now_ms: i64) -> Decision {
    let running: Vec<&Job> = jobs.iter().filter(|j| j.state == State::Running).collect();
    // A waiter that is stopped (Ctrl-Z) cannot start, so it never blocks the queue behind it.
    let mut queued: Vec<&Job> = jobs
        .iter()
        .filter(|j| j.state == State::Queued && (j.id == job.id || !live.stopped.contains(&j.id)))
        .collect();
    queued.sort_by(|a, b| (a.enqueued_ms, &a.id).cmp(&(b.enqueued_ms, &b.id)));

    if let Some(why) = disk_full(job.weights, cap, live) {
        return Decision::Fail(why);
    }
    if let Some(head) = queued.first().filter(|h| h.id != job.id) {
        if blocker(head.weights, &running, cap, live, now_ms).is_none() {
            // The head could start now: this job may go first only if the head still fits next to it.
            let me = Job {
                state: State::Running,
                started_ms: Some(now_ms),
                ..job.clone()
            };
            let mut with_me = running.clone();
            with_me.push(&me);
            if blocker(head.weights, &with_me, cap, live, now_ms).is_some() {
                return wait("queued behind a job that fits now", Block::Queue);
            }
        } else {
            if now_ms - head.enqueued_ms >= BACKFILL_WINDOW_MS {
                return wait(
                    format!("queued behind {} (waited over 60 s)", head.id),
                    Block::Queue,
                );
            }
            let (a, h) = (job.weights, head.weights);
            if a.cpu > h.cpu || a.mem > h.mem || a.vram > h.vram {
                return wait("queued behind a smaller-or-equal head", Block::Queue);
            }
        }
    }
    match blocker(job.weights, &running, cap, live, now_ms) {
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
            cpu_hold_ms: None,
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
        // Right after admission the unused reservation still counts: the same pair blocks as a ramp, then releases.
        let d = evaluate(&me, &jobs, &c, &live, 1_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Ramp,
                    ramp_end_ms: Some(_),
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
        assert_eq!(unmet(RAMP_MS, false, Some(90_000)), 7 * G);
        assert_eq!(unmet(90_000, false, Some(90_000)), 7 * G);
        assert_eq!(
            unmet(90_000 + MEM_DECAY_MS / 2, false, Some(90_000)),
            7 * G / 2
        );
        assert_eq!(unmet(90_000 + MEM_DECAY_MS, false, Some(90_000)), 0);
        // A peak earlier than the ramp still holds through the ramp; an unknown peak uses the fallback hold.
        assert_eq!(unmet(RAMP_MS, false, Some(2_000)), 7 * G);
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
        // Held until the learned peak (120 s), 30 s in: a few start, never all.
        assert_eq!(admitted_of_twelve(false, |_| 30_000), 3);
        // Past peak and decay (3 minutes) the holds are gone and measured use (1G each) admits them all.
        assert_eq!(admitted_of_twelve(false, |_| 180_000), 12);
        // Declared sizes are held whatever the time.
        assert_eq!(admitted_of_twelve(true, |_| 3_600_000), 3);
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
        let at = |ms| headroom(&[&r], &cap, &live, ms).mem;
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
        assert_eq!(
            headroom(&[&big], &cap, &live, 150_000).mem,
            free - free / 4 * 3
        );
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
        // 10 cores, 100G. Ceiling 900 centi-cores (90% of every core), 90G. Non-heavy: 2 cores, 30G.
        // Running job reserves 10G but really uses 3 cores / 20G: CPU is live, memory max(reservation, live).
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
        let room = headroom(&[&run], &cap(), &live, 100_000);
        assert_eq!(
            room,
            Usage {
                cpu: 900 - 200 - 300,
                mem: 90 * G - 30 * G - 20 * G,
                vram: 0
            }
        );
        // Memory fits at exactly the edge, blocked one GiB over; both pass the reservation-only check.
        let edge = job("e", 4, 40, State::Queued, 1);
        assert_eq!(
            evaluate(&edge, &[run.clone(), edge.clone()], &cap(), &live, 2),
            Decision::Start
        );
        let over = job("o", 5, 41, State::Queued, 1);
        let d = evaluate(&over, &[run.clone(), over.clone()], &cap(), &live, 2);
        assert!(matches!(d, Decision::Wait { live: true, .. }), "{d:?}");
        // Past its hold and decay an unused reservation holds nothing; CPU and memory are both just live.
        let idle = Live {
            per_job: vec![("r".into(), Usage::default())],
            non_heavy: Usage::default(),
            ..calm()
        };
        let run = Job {
            started_ms: Some(0),
            ..run
        };
        let room = headroom(&[&run], &cap(), &idle, 200_000);
        assert_eq!((room.cpu, room.mem), (900, 90 * G));
        // Inside the ramp the unused part of the reservation still counts.
        let room = headroom(&[&run], &cap(), &idle, 1_000);
        assert_eq!(room.mem, 80 * G);
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
            cpu_pct: 10,
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
            "4 x cores blocks whatever the CPU says"
        );
        let mem = Live {
            mem_used_pct: 95,
            ..calm()
        };
        assert!(waits(&evaluate(
            &me,
            std::slice::from_ref(&me),
            &cap(),
            &mem,
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
            cpu_pct: 5,
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
    fn eight_cold_builds_start_one_ramp_apart() {
        // 16 cores, 8 jobs with no history: each asks for the whole machine, so each holds all 16 cores while ramping.
        let c = Capacity { cpu: 16, ..cap() };
        let mut jobs: Vec<Job> = (0..8)
            .map(|i| job(&format!("j{i}"), 16, 1, State::Queued, i))
            .collect();
        let mut starts = Vec::new();
        let mut t = 100;
        // Every waiter looks every 100 ms; they admit in FIFO order, as the lock serializes them.
        while t < 100 + 8 * RAMP_MS && starts.len() < 8 {
            for i in 0..jobs.len() {
                if jobs[i].state == State::Queued
                    && evaluate(&jobs[i].clone(), &jobs, &c, &calm(), t) == Decision::Start
                {
                    jobs[i].state = State::Running;
                    jobs[i].started_ms = Some(t);
                    starts.push(t);
                }
            }
            t += 100;
        }
        assert_eq!(starts.len(), 8);
        // Even on a machine that reads idle, whole-machine holds are never released early: one cold build per ramp.
        assert!(
            starts.windows(2).all(|w| w[1] - w[0] >= RAMP_MS),
            "one cold build per ramp: {starts:?}"
        );
        // Same with the CPU measured 60% busy.
        let busy = Live {
            cpu_pct: 60,
            non_heavy: Usage {
                cpu: 960,
                mem: 0,
                vram: 0,
            },
            ..calm()
        };
        let mut jobs: Vec<Job> = (0..3)
            .map(|i| job(&format!("k{i}"), 16, 1, State::Queued, i))
            .collect();
        let mut starts = Vec::new();
        let mut t = 100;
        while t < 100 + 4 * RAMP_MS && starts.len() < 3 {
            for i in 0..jobs.len() {
                if jobs[i].state == State::Queued
                    && evaluate(&jobs[i].clone(), &jobs, &c, &busy, t) == Decision::Start
                {
                    jobs[i].state = State::Running;
                    jobs[i].started_ms = Some(t);
                    starts.push(t);
                }
            }
            t += 100;
        }
        assert_eq!(starts.len(), 3);
        assert!(
            starts.windows(2).all(|w| w[1] - w[0] >= RAMP_MS),
            "busy: one cold build per ramp: {starts:?}"
        );
        // At one instant only the first is admitted.
        let mut now = jobs.clone();
        for j in &mut now {
            j.state = State::Queued;
            j.started_ms = None;
        }
        let first = evaluate(&now[0], &now, &c, &calm(), 5000);
        assert_eq!(first, Decision::Start);
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
    fn fifo_head_blocks_non_fitting_follower() {
        let run = job("r", 8, 50, State::Running, 0);
        let head = job("h", 5, 30, State::Queued, 1);
        let big = job("z", 6, 40, State::Queued, 2);
        assert!(waits(&evaluate(
            &big,
            &[run, head, big.clone()],
            &cap(),
            &calm(),
            3
        )));
    }

    #[test]
    fn backfill_only_while_head_is_young_and_follower_smaller() {
        let run = job("r", 8, 70, State::Running, 0);
        let head = job("h", 5, 30, State::Queued, 1);
        let small = job("s", 1, 5, State::Queued, 2);
        let all = [run, head, small.clone()];
        assert_eq!(
            evaluate(&small, &all, &cap(), &calm(), 1000),
            Decision::Start
        );
        assert!(waits(&evaluate(
            &small,
            &all,
            &cap(),
            &calm(),
            BACKFILL_WINDOW_MS + 1
        )));
    }

    #[test]
    fn follower_starts_when_it_fits_together_with_the_head() {
        let head = job("h", 5, 10, State::Queued, 1);
        let small = job("s", 1, 5, State::Queued, 2);
        let all = [head.clone(), small.clone()];
        assert_eq!(evaluate(&small, &all, &cap(), &calm(), 3), Decision::Start);
        assert_eq!(evaluate(&head, &all, &cap(), &calm(), 3), Decision::Start);
    }

    #[test]
    fn follower_waits_when_it_would_stop_the_head_fitting() {
        // The memory ceiling is 90G: head 50 fits alone, but 50 + 50 does not.
        let head = job("h", 5, 50, State::Queued, 1);
        let other = job("o", 5, 50, State::Queued, 2);
        let all = [head.clone(), other.clone()];
        assert!(waits(&evaluate(&other, &all, &cap(), &calm(), 3)));
        assert_eq!(evaluate(&head, &all, &cap(), &calm(), 3), Decision::Start);
        // Same with a running job: head fits (50 + 30), a follower of 20 would push it over 90.
        let run = job("r", 3, 30, State::Running, 0);
        let follow = job("f", 2, 20, State::Queued, 3);
        let all = [run, head.clone(), follow.clone()];
        assert!(waits(&evaluate(&follow, &all, &cap(), &calm(), 4)));
    }

    #[test]
    fn a_ramp_blocker_names_the_moment_it_ends() {
        let c = Capacity { cpu: 16, ..cap() };
        let live = Live {
            non_heavy: Usage {
                cpu: 320,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let ra = Job {
            state: State::Running,
            started_ms: Some(10),
            ..job("a", 16, 8, State::Running, 0)
        };
        let b = job("b", 16, 8, State::Queued, 2);
        // A whole-machine hold (cold) lasts the full ramp, idle or not.
        let d = evaluate(&b, &[ra.clone(), b.clone()], &c, &live, 500);
        assert!(
            matches!(d, Decision::Wait { block: Block::Ramp, ramp_end_ms: Some(e), live: true, .. } if e == 10 + RAMP_MS),
            "{d:?}"
        );
        let warm = Live {
            cpu_pct: 60,
            ..live.clone()
        };
        let d = evaluate(&b, &[ra, b.clone()], &c, &warm, 500);
        assert!(
            matches!(d, Decision::Wait { block: Block::Ramp, ramp_end_ms: Some(e), .. } if e == 10 + RAMP_MS),
            "{d:?}"
        );
        // A learned 8-core peak (under 75%) on a machine that reads idle: released when the stagger floor ends.
        let live = Live {
            non_heavy: Usage {
                cpu: 800,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let warm = Live {
            cpu_pct: 60,
            ..live.clone()
        };
        let rl = Job {
            state: State::Running,
            started_ms: Some(10),
            ..job("l", 8, 8, State::Running, 0)
        };
        let d = evaluate(&b, &[rl.clone(), b.clone()], &c, &live, 500);
        assert!(
            matches!(d, Decision::Wait { block: Block::Ramp, ramp_end_ms: Some(e), .. } if e == 10 + RAMP_FULL_MS),
            "{d:?}"
        );
        // At 60% busy it shrinks linearly: 800 x (10 s - e) / 7 s, rounded down, fits the 540 left (1440 - 800 - the
        // 100 ask) from e = 5.28 s; the next 250 ms step is 5.5 s.
        let d = evaluate(&b, &[rl.clone(), b.clone()], &c, &warm, 500);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Ramp,
                    ramp_end_ms: Some(5500),
                    ..
                }
            ),
            "{d:?}"
        );
        assert_eq!(
            evaluate(&b, &[rl, b.clone()], &c, &warm, 5500),
            Decision::Start
        );
        // A genuinely busy CPU is a cpu block, with no ramp time.
        let busy = Live {
            non_heavy: Usage {
                cpu: 1550,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let run = Job {
            state: State::Running,
            started_ms: Some(0),
            ..job("r", 1, 8, State::Running, 0)
        };
        let d = evaluate(&b, &[run, b.clone()], &c, &busy, 100_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Cpu,
                    ramp_end_ms: None,
                    ..
                }
            ),
            "{d:?}"
        );
    }

    #[test]
    fn peak_core_waiters_start_on_a_mostly_idle_machine() {
        // Both builds peaked near all cores in history (weights.cpu = ncpu); live CPU is 20%.
        let c = Capacity { cpu: 16, ..cap() };
        let live = Live {
            non_heavy: Usage {
                cpu: 320,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let a = job("a", 16, 8, State::Queued, 1);
        let b = job("b", 16, 8, State::Queued, 2);
        assert_eq!(
            evaluate(&a, &[a.clone(), b.clone()], &c, &live, 10),
            Decision::Start
        );
        // a was admitted just now and holds its full 16 cores while ramping: b waits for the ramp, then starts.
        let ra = Job {
            state: State::Running,
            started_ms: Some(10),
            ..a
        };
        assert!(
            waits(&evaluate(&b, &[ra.clone(), b.clone()], &c, &live, 11)),
            "blocked by a's ramp hold"
        );
        assert!(
            waits(&evaluate(
                &b,
                &[ra.clone(), b.clone()],
                &c,
                &live,
                10 + RAMP_FULL_MS
            )),
            "a whole-machine hold is not released early"
        );
        assert_eq!(
            evaluate(&b, &[ra.clone(), b.clone()], &c, &live, 10 + RAMP_MS),
            Decision::Start,
            "the second starts once the first one's ramp has ended"
        );
        let ca = job("c", 16, 8, State::Queued, 3);
        // A genuinely busy CPU still blocks.
        let busy = Live {
            non_heavy: Usage {
                cpu: 1550,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let run = Job {
            state: State::Running,
            started_ms: Some(0),
            ..job("r", 16, 8, State::Running, 0)
        };
        assert!(waits(&evaluate(
            &ca,
            &[run, ca.clone()],
            &c,
            &busy,
            100_000
        )));
    }

    #[test]
    fn burst_is_held_back_by_ramp_holds() {
        // 16 cores, ceiling 1440 (90%); every build peaked at 4 cores but is at 0% live while ramping.
        let c = Capacity { cpu: 16, ..cap() };
        let mut jobs: Vec<Job> = (0..8)
            .map(|i| job(&format!("j{i}"), 4, 1, State::Queued, i))
            .collect();
        let mut started = 0;
        // All eight evaluate at the same instant; admit in FIFO order, as the lock serializes them.
        for i in 0..jobs.len() {
            let me = jobs[i].clone();
            if evaluate(&me, &jobs, &c, &calm(), 100) == Decision::Start {
                jobs[i].state = State::Running;
                jobs[i].started_ms = Some(100);
                started += 1;
            }
        }
        assert_eq!(
            started, 4,
            "holds of 400 each leave 240 for the 4th and none for the 5th: not a burst of 8"
        );
        // Once the ramp window passes and live CPU is low, the rest follow.
        let next = jobs
            .iter()
            .find(|j| j.state == State::Queued)
            .unwrap()
            .clone();
        assert_eq!(
            evaluate(&next, &jobs, &c, &calm(), 100 + RAMP_MS),
            Decision::Start
        );
        // Not blocked on peaks long-term: after the ramp the hold is gone entirely.
        let ramped = Job {
            state: State::Running,
            started_ms: Some(0),
            ..job("r", 16, 1, State::Running, 0)
        };
        let room = headroom(&[&ramped], &c, &calm(), RAMP_MS + 1);
        assert_eq!(room.cpu, 1440);
    }

    #[test]
    fn stopped_waiter_file_does_not_block_the_next_waiter() {
        let run = job("r", 1, 50, State::Running, 0);
        let head = job("h", 1, 30, State::Queued, 1);
        let next = job("n", 1, 40, State::Queued, 2);
        let t = 3 + BACKFILL_WINDOW_MS;
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
            waits(&evaluate(
                &next,
                &all,
                &cap(),
                &calm(),
                3 + BACKFILL_WINDOW_MS
            )),
            "a live head blocks the bigger follower"
        );
        let live = Live {
            stopped: vec!["h".into()],
            ..calm()
        };
        assert_eq!(
            evaluate(&next, &all, &cap(), &live, 3 + BACKFILL_WINDOW_MS),
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
    fn a_small_job_starts_at_seventy_percent_cpu() {
        // 2026-11-20 03:31-03:36: cpu 67-73%, one or two jobs running, a waiter blocked by cpu.
        let run = Job {
            started_ms: Some(0),
            ..job("r", 8, 4, State::Running, 0)
        };
        let me = job("a", 4, 4, State::Queued, 1);
        let live = Live {
            load1: 26.0,
            load5: 31.0,
            cpu_pct: 72,
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 800,
                    mem: 4 * G,
                    vram: 0,
                },
            )],
            non_heavy: Usage {
                cpu: 352,
                mem: 22 * G,
                vram: 0,
            },
            ..calm()
        };
        assert_eq!(
            evaluate(&me, &[run.clone(), me.clone()], &mac(), &live, 100_000),
            Decision::Start
        );
        // Jobs keep starting until about one core is left (15 of 16 busy), then wait on cpu.
        let full = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 1100,
                    mem: 4 * G,
                    vram: 0,
                },
            )],
            non_heavy: Usage {
                cpu: 420,
                mem: 22 * G,
                vram: 0,
            },
            cpu_pct: 95,
            ..live
        };
        assert!(matches!(
            evaluate(&me, &[run, me.clone()], &mac(), &full, 100_000),
            Decision::Wait {
                block: Block::Cpu,
                ..
            }
        ));
    }

    #[test]
    fn ramp_hold_is_released_while_measured_cpu_is_low() {
        // 2026-11-20 07:01:33: cpu 20%, one job running (ramping), one waiting, blocked by ramp.
        // 0.3.0 held the running job's peak for the whole 10 s ramp, so the waiter sat with 80% of the CPU idle.
        // Here the running job has a learned 8-core peak (under 75% of the machine).
        let run = Job {
            started_ms: Some(0),
            ..job("r", 8, 4, State::Running, 0)
        };
        let me = job("a", 16, 4, State::Queued, 1);
        let all = [run.clone(), me.clone()];
        let live = Live {
            load1: 29.4,
            load5: 23.7,
            cpu_pct: 20,
            mem_used_pct: 38,
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 150,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            non_heavy: Usage {
                cpu: 800,
                mem: 22 * G,
                vram: 0,
            },
            ..calm()
        };
        // Inside the stagger floor the hold is full: no simultaneous starts.
        let d = evaluate(&me, &all, &mac(), &live, 1_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Ramp,
                    ramp_end_ms: Some(RAMP_FULL_MS),
                    ..
                }
            ),
            "{d:?}"
        );
        // After it, the idle machine shows the ramping job's real use: admitted (0.3.0 waited until 10 s).
        assert_eq!(evaluate(&me, &all, &mac(), &live, 4_000), Decision::Start);
        assert_eq!(
            evaluate(&me, &all, &mac(), &live, RAMP_FULL_MS),
            Decision::Start
        );
        // The same moment with a cold running job (whole-machine hold): held for the full 10 s, as in 0.3.0.
        let cold = Job {
            started_ms: Some(0),
            ..job("r", 16, 4, State::Running, 0)
        };
        let all_cold = [cold, me.clone()];
        let d = evaluate(&me, &all_cold, &mac(), &live, 4_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Ramp,
                    ramp_end_ms: Some(RAMP_MS),
                    ..
                }
            ),
            "{d:?}"
        );
        assert!(waits(&evaluate(&me, &all_cold, &mac(), &live, RAMP_MS - 1)));
        assert_eq!(
            evaluate(&me, &all_cold, &mac(), &live, RAMP_MS),
            Decision::Start
        );
    }

    #[test]
    fn ramp_hold_shrinks_for_a_recent_job_with_a_learned_peak() {
        let me = job("a", 16, 4, State::Queued, 1);
        // 06:22:23: cpu 24%, four running (three ramped, one admitted 5 s ago with a learned 8-core peak), one waiting.
        let mut all: Vec<Job> = (0..3)
            .map(|i| Job {
                started_ms: Some(0),
                ..job(&format!("d{i}"), 4, 2, State::Running, 0)
            })
            .collect();
        all.push(Job {
            started_ms: Some(95_000),
            ..job("new", 8, 2, State::Running, 1)
        });
        all.push(me.clone());
        let live = Live {
            load1: 15.5,
            load5: 14.3,
            cpu_pct: 24,
            mem_used_pct: 46,
            per_job: vec![
                (
                    "d0".into(),
                    Usage {
                        cpu: 80,
                        mem: G,
                        vram: 0,
                    },
                ),
                (
                    "d1".into(),
                    Usage {
                        cpu: 60,
                        mem: G,
                        vram: 0,
                    },
                ),
                (
                    "d2".into(),
                    Usage {
                        cpu: 40,
                        mem: G,
                        vram: 0,
                    },
                ),
                (
                    "new".into(),
                    Usage {
                        cpu: 50,
                        mem: G,
                        vram: 0,
                    },
                ),
            ],
            non_heavy: Usage {
                cpu: 150,
                mem: 25 * G,
                vram: 0,
            },
            ..calm()
        };
        assert_eq!(evaluate(&me, &all, &mac(), &live, 100_000), Decision::Start);
        // The same moment with the CPU busy keeps a shrinking hold: 1180 busy plus the new job's 571 held (800 x 5/7) is
        // over the 1500 that leaves a core free.
        let busy = Live {
            cpu_pct: 70,
            non_heavy: Usage {
                cpu: 1180 - 230,
                mem: 25 * G,
                vram: 0,
            },
            ..live
        };
        assert!(matches!(
            evaluate(&me, &all, &mac(), &busy, 100_000),
            Decision::Wait {
                block: Block::Ramp,
                ..
            }
        ));
    }

    #[test]
    fn big_learned_peaks_and_docker_keep_the_full_hold() {
        let c = Capacity { cpu: 16, ..cap() };
        let idle = Live {
            non_heavy: Usage {
                cpu: 100,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let b = job("b", 15, 8, State::Queued, 2);
        // Two learned 15-core waiters arriving together on an idle machine start at least 10 s apart.
        let ra = Job {
            state: State::Running,
            started_ms: Some(10),
            ..job("a", 15, 8, State::Running, 0)
        };
        assert!(
            waits(&evaluate(
                &b,
                &[ra.clone(), b.clone()],
                &c,
                &idle,
                10 + RAMP_FULL_MS
            )),
            "no early release at 94%"
        );
        assert!(waits(&evaluate(
            &b,
            &[ra.clone(), b.clone()],
            &c,
            &idle,
            10 + RAMP_MS - 1
        )));
        assert_eq!(
            evaluate(&b, &[ra, b.clone()], &c, &idle, 10 + RAMP_MS),
            Decision::Start
        );
        // Under 75% (8 of 16 cores) is still released at 3 s on an idle machine.
        let busy_other = Live {
            non_heavy: Usage {
                cpu: 800,
                mem: 8 * G,
                vram: 0,
            },
            ..calm()
        };
        let rh = Job {
            state: State::Running,
            started_ms: Some(10),
            ..job("h", 8, 8, State::Running, 0)
        };
        assert!(waits(&evaluate(
            &b,
            &[rh.clone(), b.clone()],
            &c,
            &busy_other,
            11
        )));
        assert_eq!(
            evaluate(&b, &[rh, b.clone()], &c, &busy_other, 10 + RAMP_FULL_MS),
            Decision::Start
        );
        // A learned Docker job keeps the 10 s hold even at 8 cores.
        let rd = Job {
            state: State::Running,
            started_ms: Some(10),
            cmd: vec!["docker".into(), "build".into()],
            ..job("d", 8, 8, State::Running, 0)
        };
        assert!(waits(&evaluate(
            &b,
            &[rd.clone(), b.clone()],
            &c,
            &busy_other,
            10 + RAMP_FULL_MS
        )));
        assert_eq!(
            evaluate(&b, &[rd, b.clone()], &c, &busy_other, 10 + RAMP_MS),
            Decision::Start
        );
        // The 75% line itself: 12 of 16 cores is a full hold, 11 is not.
        assert_eq!(ramp_hold(1200, 1600, RAMP_FULL_MS, 0, false), 1200);
        assert_eq!(ramp_hold(1100, 1600, RAMP_FULL_MS, 0, false), 0);
        assert_eq!(ramp_hold(800, 1600, RAMP_FULL_MS, 0, true), 800);
    }

    #[test]
    fn ramp_hold_schedule() {
        let (peak, machine) = (800, 1600);
        assert_eq!(
            ramp_hold(peak, machine, 0, 0, false),
            peak,
            "full at admission"
        );
        assert_eq!(
            ramp_hold(peak, machine, RAMP_FULL_MS - 1, 0, false),
            peak,
            "full through the floor, however idle"
        );
        assert_eq!(
            ramp_hold(peak, machine, RAMP_FULL_MS, 49, false),
            0,
            "released on an idle machine"
        );
        assert_eq!(
            ramp_hold(peak, machine, RAMP_FULL_MS, 50, false),
            peak,
            "shrinks from the full peak on a busy one"
        );
        assert_eq!(
            ramp_hold(peak, machine, 6_500, 80, false),
            400,
            "halfway down at the middle of the shrink"
        );
        assert_eq!(
            ramp_hold(peak, machine, RAMP_MS, 100, false),
            0,
            "gone when the ramp ends"
        );
        // A whole-machine hold (cold or Docker) is full for the whole ramp, however idle the machine reads.
        assert_eq!(ramp_hold(machine, machine, RAMP_FULL_MS, 0, false), machine);
        assert_eq!(ramp_hold(machine, machine, RAMP_MS - 1, 0, false), machine);
        assert_eq!(ramp_hold(machine, machine, RAMP_MS, 0, false), 0);
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
        // 4 x cores (64 on 16) still blocks whatever the CPU says, and memory pressure still blocks.
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
    fn a_saturated_machine_still_blocks() {
        // 2026-11-20 03:19: cpu 100%, load1 55: the load guard holds even for a job that would run alone.
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
                block: Block::Load1,
                ..
            }
        ));
        // Load 120 on 16 cores (2026-11-19) blocks even if the CPU reads idle.
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

    /// Two builds that started long ago (no ramp holds) using `jobs` centi-cores between them, the rest non-heavy.
    fn pinned(jobs: u64, non_heavy: u64) -> (Vec<Job>, Live) {
        let a = Job {
            started_ms: Some(0),
            ..job("a", 8, 2, State::Running, 0)
        };
        let b = Job {
            started_ms: Some(0),
            ..job("b", 8, 2, State::Running, 0)
        };
        let live = Live {
            cpu_pct: (jobs + non_heavy) / 16,
            per_job: vec![
                (
                    "a".into(),
                    Usage {
                        cpu: jobs / 2,
                        mem: 2 * G,
                        vram: 0,
                    },
                ),
                (
                    "b".into(),
                    Usage {
                        cpu: jobs / 2,
                        mem: 2 * G,
                        vram: 0,
                    },
                ),
            ],
            non_heavy: Usage {
                cpu: non_heavy,
                mem: 20 * G,
                vram: 0,
            },
            ..calm()
        };
        (vec![a, b], live)
    }

    #[test]
    fn a_dip_between_build_steps_does_not_admit_onto_a_pinned_machine() {
        // 2026-11-20 on the Mac and the rig: samples read 98-100% for minutes, yet jobs were admitted between them. A
        // waiter's single 250 ms window that fell between two builds' steps read 80% (1280 of 1600). 0.3.1 measured
        // headroom against every core (1600 - 1280 = 320 >= one core) and started another build into a pinned machine.
        let (mut jobs, dip) = pinned(1_100, 180);
        let me = job("c", 4, 2, State::Queued, 50_000);
        jobs.push(me.clone());
        assert_eq!(dip.cpu_pct, 80);
        // With the readings every heavy process shared over the last 20 s (100, 100, 98, then this 80): mean 94%.
        let readings = [(40_000, 100), (45_000, 100), (52_000, 98), (60_000, 80)];
        let live = Live {
            cpu_sustained_pct: sustained_pct(dip.cpu_pct, &readings, 60_000),
            ..dip.clone()
        };
        assert_eq!(live.cpu_sustained_pct, 94);
        let d = evaluate(&me, &jobs, &mac(), &live, 60_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Cpu,
                    ramp_end_ms: None,
                    ..
                }
            ),
            "{d:?}"
        );
        // Once the machine really has settled at 80% (the readings agree), the job starts: the 80-90% band.
        let settled = [(45_000, 81), (52_000, 79), (60_000, 80)];
        let live = Live {
            cpu_sustained_pct: sustained_pct(80, &settled, 60_000),
            ..dip
        };
        assert_eq!(evaluate(&me, &jobs, &mac(), &live, 60_000), Decision::Start);
    }

    #[test]
    fn jobs_start_up_to_the_band_and_wait_inside_it() {
        // 16 cores: the ceiling is 1440 (90%) and a job asks for one core (100), so it starts up to 83% busy.
        let me = job("c", 4, 2, State::Queued, 1);
        for (busy, starts) in [
            (60, true),
            (75, true),
            (83, true),
            (84, false),
            (88, false),
            (100, false),
        ] {
            let (mut jobs, live) = pinned(busy * 16 - 160, 160);
            jobs.push(me.clone());
            let live = Live {
                cpu_sustained_pct: busy,
                ..live
            };
            assert_eq!(
                evaluate(&me, &jobs, &mac(), &live, 100_000) == Decision::Start,
                starts,
                "{busy}% busy"
            );
        }
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
    fn a_rising_load_in_the_underused_zone_admits() {
        // 2026-11-20 on the Mac: waiters blocked on a rising load1 over 32 with the CPU at 60-74%, under the band.
        let me = job("c", 4, 2, State::Queued, 1);
        let (mut jobs, live) = pinned(1_000, 120);
        jobs.push(me.clone());
        let rising = Live {
            load1: 40.0,
            load5: 30.0,
            cpu_sustained_pct: 70,
            ..live
        };
        assert_eq!(rising.cpu_pct, 70);
        assert_eq!(
            evaluate(&me, &jobs, &mac(), &rising, 100_000),
            Decision::Start
        );
        // The same rising load with the sustained CPU at 80% is builds piling up: blocked.
        let busy = Live {
            cpu_sustained_pct: 80,
            ..rising
        };
        assert!(matches!(
            evaluate(&me, &jobs, &mac(), &busy, 100_000),
            Decision::Wait {
                block: Block::Load1,
                ..
            }
        ));
    }

    #[test]
    fn thread_inflated_memory_reservations_no_longer_starve_an_idle_rig() {
        // 2026-11-20 09:00 on the rig: cpu 21%, memory 6% used, 3 running, a waiter blocked on "mem". Each `cargo
        // install` had learned a 46 GB peak by summing every rustc thread's copy of its process RSS (2.6 GB real).
        let rig = Capacity {
            cpu: 32,
            mem: 64 * G,
            vram: 0,
            disk_total: 1000 * G,
            vram_is_ram: false,
        };
        let live = Live {
            cpu_pct: 21,
            cpu_sustained_pct: 21,
            mem_used_pct: 6,
            ..calm()
        };
        let me = job("w", 4, 3, State::Queued, 1);
        let inflated = |id: &str| Job {
            started_ms: Some(0),
            ..job(id, 32, 46, State::Running, 0)
        };
        let d = evaluate(
            &me,
            &[inflated("r"), inflated("s"), me.clone()],
            &rig,
            &live,
            100_000,
        );
        // Past the ramp the inflated reservations are only a number: measured memory is 6% used, so the job starts.
        assert_eq!(d, Decision::Start, "{d:?}");
        // Learned without threads (3 GB, 4 cores) the same job starts.
        let real = |id: &str| Job {
            started_ms: Some(0),
            ..job(id, 4, 3, State::Running, 0)
        };
        assert_eq!(
            evaluate(
                &me,
                &[real("r"), real("s"), me.clone()],
                &rig,
                &live,
                100_000
            ),
            Decision::Start
        );
    }

    /// Six cargo-like builds, each with a learned 16-core peak and time to peak `ttp`, queued at once on the 16-core
    /// Mac. Each runs on 2 cores until `ttp` after its start, then takes all 16 (scaled down when over-subscribed),
    /// and finishes `run` after its start. Returns the start times and the most builds ever running before their peak.
    fn burst_of_six(cpu_hold: Option<i64>, ttp: i64, run: i64) -> (Vec<i64>, usize) {
        let c = mac();
        let mut jobs: Vec<Job> = (0..6)
            .map(|i| Job {
                cpu_hold_ms: cpu_hold,
                ..job(&format!("b{i}"), 16, 2, State::Queued, i)
            })
            .collect();
        let (mut starts, mut most_ramping) = (Vec::new(), 0);
        let mut t = 0;
        while t <= 700_000 && starts.len() < 6 {
            jobs.retain(|j| j.started_ms.is_none_or(|s| t - s < run));
            let demand: Vec<(String, u64)> = jobs
                .iter()
                .filter_map(|j| {
                    j.started_ms
                        .map(|s| (j.id.clone(), if t - s < ttp { 200 } else { 1600 }))
                })
                .collect();
            let total: u64 = demand.iter().map(|d| d.1).sum();
            let pct = total.min(1600) * 100 / 1600;
            let live = Live {
                cpu_pct: pct,
                cpu_sustained_pct: pct,
                per_job: demand
                    .iter()
                    .map(|(id, d)| {
                        let cpu = if total > 1600 { d * 1600 / total } else { *d };
                        (
                            id.clone(),
                            Usage {
                                cpu,
                                mem: 2 * G,
                                vram: 0,
                            },
                        )
                    })
                    .collect(),
                ..calm()
            };
            // Every waiter evaluates; the state lock serializes them in FIFO order.
            for i in 0..jobs.len() {
                if jobs[i].state == State::Queued
                    && evaluate(&jobs[i].clone(), &jobs, &c, &live, t) == Decision::Start
                {
                    jobs[i].state = State::Running;
                    jobs[i].started_ms = Some(t);
                    starts.push(t);
                }
            }
            let ramping = jobs
                .iter()
                .filter(|j| j.started_ms.is_some_and(|s| t - s < ttp))
                .count();
            most_ramping = most_ramping.max(ramping);
            t += 500;
        }
        (starts, most_ramping)
    }

    #[test]
    fn a_burst_of_six_16_core_cargo_builds_starts_one_at_a_time_as_their_peaks_show() {
        // 2026-11-20 after 0.4.2: holds that ended 10 s after admission let builds in before their CPU showed; CPU sat at
        // 95-100% with load1 over 48 (3 x cores). Before the fix (no learned hold): three builds in the first 30 s.
        let (old, old_ramping) = burst_of_six(None, 30_000, 90_000);
        assert!(
            old.iter().filter(|t| **t < 30_000).count() >= 3 && old_ramping >= 3,
            "the 0.4.2 schedule admits a burst: {old:?}"
        );
        // Held until its peak shows (or its learned 30 s time to peak passes): one build at a time.
        let (starts, ramping) = burst_of_six(Some(cpu_hold_ms(Some(30_000))), 30_000, 90_000);
        assert_eq!(ramping, 1, "never two builds ramping at once: {starts:?}");
        assert_eq!(
            starts.len(),
            6,
            "all six still run (no starvation): {starts:?}"
        );
        assert!(
            starts.windows(2).all(|w| w[1] - w[0] >= 30_000),
            "each start waits for the previous peak: {starts:?}"
        );
        // A history from before 0.4.3 (time to peak unknown) holds for the 30 s fallback: still one at a time.
        let (starts, ramping) = burst_of_six(Some(cpu_hold_ms(None)), 20_000, 60_000);
        assert_eq!((ramping, starts.len()), (1, 6), "{starts:?}");
        // Cold jobs (whole machine, nothing learned) get the same hold: never admitted 10 s apart.
        let (starts, ramping) = burst_of_six(Some(CPU_FALLBACK_HOLD_MS), 25_000, 60_000);
        assert_eq!((ramping, starts.len()), (1, 6), "{starts:?}");
        assert!(
            starts.windows(2).all(|w| w[1] - w[0] >= 25_000),
            "cold jobs wait for each other's peak: {starts:?}"
        );
    }

    #[test]
    fn a_hold_from_the_slowest_ramp_outlasts_a_median_length_ramp() {
        // Runs peaked at 20, 30 and 40 s: the median is 30 s. A build that ramps for 38 s is still ramping when a
        // median-length hold ends, but not when the hold is the slowest ramp plus a margin.
        let median_hold = 30_000;
        let hold = cpu_hold_ms(Some(40_000));
        assert!(hold > 40_000, "{hold}");
        let (peak, machine) = (1_600, 1_600);
        let still_ramping = |at, h| cpu_hold(peak, machine, at, 10, false, 200, Some(h));
        assert_eq!(still_ramping(38_000, median_hold), 0);
        assert_eq!(still_ramping(38_000, hold), peak);
        assert_eq!(still_ramping(hold, hold), 0, "still bounded");
    }

    #[test]
    fn a_learned_cpu_peak_is_held_until_it_shows_or_its_time_passes() {
        let (peak, machine) = (1_600, 1_600);
        let hold = Some(cpu_hold_ms(Some(32_000)));
        assert_eq!(hold, Some(40_000), "the slowest ramp plus 25%");
        // Before the floor: full, whatever the job shows.
        assert_eq!(cpu_hold(peak, machine, 1_000, 10, false, 1_600, hold), peak);
        // After the 0.4.2 ramp end and on an idle-looking machine, a peak that has not shown is still held.
        assert_eq!(cpu_hold(peak, machine, 20_000, 10, false, 200, hold), peak);
        // Shown (3/4 of it in live use): live use counts instead.
        assert_eq!(cpu_hold(peak, machine, 20_000, 90, false, 1_200, hold), 0);
        // Its time to peak passed without it showing (an incremental build): released, so the machine is not kept idle.
        assert_eq!(cpu_hold(peak, machine, 40_000, 10, false, 200, hold), 0);
        // Docker work and old job files keep the 10 s ramp schedule.
        assert_eq!(cpu_hold(peak, machine, 9_000, 10, true, 0, hold), peak);
        assert_eq!(cpu_hold(peak, machine, RAMP_MS, 10, true, 0, hold), 0);
        assert_eq!(cpu_hold(peak, machine, RAMP_MS, 10, false, 0, None), 0);
        // The hold length: history's slowest ramp plus 25%, clamped to 10 s .. 2 min, 30 s when unknown.
        assert_eq!(cpu_hold_ms(None), CPU_FALLBACK_HOLD_MS);
        assert_eq!(cpu_hold_ms(Some(500)), RAMP_MS);
        assert_eq!(cpu_hold_ms(Some(3_600_000)), CPU_MAX_HOLD_MS);
    }

    #[test]
    fn a_small_waiter_starts_under_a_held_16_core_peak_when_the_band_has_room() {
        let c = mac();
        let run = Job {
            state: State::Running,
            started_ms: Some(0),
            cpu_hold_ms: Some(40_000),
            ..job("r", 16, 2, State::Running, 0)
        };
        // The 16-core build ramps on 2 cores; the machine reads 13%.
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 200,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            cpu_pct: 13,
            cpu_sustained_pct: 13,
            ..calm()
        };
        let one = job("one", 1, 1, State::Queued, 1);
        assert_eq!(
            evaluate(&one, &[run.clone(), one.clone()], &c, &live, 20_000),
            Decision::Start,
            "a 1-core waiter fits in the band minus the held reservation"
        );
        // A big waiter still waits for the hold: it sees the whole 16-core reservation.
        let big = job("big", 16, 1, State::Queued, 1);
        assert!(waits(&evaluate(
            &big,
            &[run.clone(), big.clone()],
            &c,
            &live,
            20_000
        )));
        // Real use counts in full: with the machine measured at 85% the small waiter waits too.
        let busy = Live {
            cpu_pct: 85,
            cpu_sustained_pct: 85,
            ..live
        };
        assert!(waits(&evaluate(
            &one,
            &[run, one.clone()],
            &c,
            &busy,
            20_000
        )));
    }

    fn six_small_waiters(live: &Live, run: &Job, now: i64) -> usize {
        let c = mac();
        let mut running = vec![run.clone()];
        for i in 0..6 {
            let id = format!("s{i}");
            let w = job(&id, 2, 1, State::Queued, 1);
            let mut jobs = running.clone();
            jobs.push(w.clone());
            if evaluate(&w, &jobs, &c, live, now) == Decision::Start {
                running.push(Job {
                    state: State::Running,
                    started_ms: Some(now),
                    cpu_hold_ms: Some(40_000),
                    ..w
                });
            }
        }
        running.len() - 1
    }

    fn held_big() -> (Job, Live) {
        let run = Job {
            state: State::Running,
            started_ms: Some(0),
            cpu_hold_ms: Some(40_000),
            ..job("r", 16, 2, State::Running, 0)
        };
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 200,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            cpu_pct: 13,
            cpu_sustained_pct: 13,
            ..calm()
        };
        (run, live)
    }

    #[test]
    fn small_waiters_share_one_backfill_budget_under_a_held_peak() {
        let (run, live) = held_big();
        // Band 14.4 cores, big hold capped to leave 2 free: only one 2-core waiter fits, not all six.
        assert_eq!(six_small_waiters(&live, &run, 20_000), 1);
    }

    #[test]
    fn the_rest_start_once_the_big_peak_shows_or_its_hold_expires() {
        let (run, live) = held_big();
        // Peak shows (1200+ cores-centi in live use at 75% of 16): live use counts, holds are gone; 6 x 2 cores fit
        // only as far as the measured 75% leaves room.
        let shown = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 1_200,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            cpu_pct: 75,
            cpu_sustained_pct: 75,
            ..live.clone()
        };
        assert_eq!(six_small_waiters(&shown, &run, 20_000), 1);
        // Hold expired with the machine reading low (an incremental build): all six are admitted.
        assert_eq!(six_small_waiters(&live, &run, 41_000), 6);
    }

    #[test]
    fn a_ramp_blocker_on_a_learned_hold_names_the_hold_end() {
        let c = mac();
        let run = Job {
            state: State::Running,
            started_ms: Some(0),
            cpu_hold_ms: Some(40_000),
            ..job("r", 16, 2, State::Running, 0)
        };
        let me = job("w", 8, 2, State::Queued, 1);
        let live = Live {
            per_job: vec![(
                "r".into(),
                Usage {
                    cpu: 200,
                    mem: 2 * G,
                    vram: 0,
                },
            )],
            cpu_pct: 13,
            cpu_sustained_pct: 13,
            ..calm()
        };
        let d = evaluate(&me, &[run.clone(), me.clone()], &c, &live, 20_000);
        assert!(
            matches!(
                d,
                Decision::Wait {
                    block: Block::Ramp,
                    ramp_end_ms: Some(40_000),
                    ..
                }
            ),
            "{d:?}"
        );
        assert_eq!(
            evaluate(&me, &[run, me.clone()], &c, &live, 40_000),
            Decision::Start
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
}
