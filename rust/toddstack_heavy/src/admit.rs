//! Admission: pure decisions.
//!
//! Memory and VRAM are reservation-based (real safety): running reservations plus the request must fit
//! within 90% of capacity. CPU is judged by live headroom only, never by peak-core reservations (a build
//! that peaks near all cores would otherwise block everything on an idle machine):
//!
//! cpu headroom = all cores - non-heavy usage - live use of running jobs
//!
//! A job asks for one free core, so on 16 cores jobs start until the CPU is about 94% busy.
//!
//! Jobs admitted in the last `RAMP_MS` hold their full peak cores (from history, clamped to the machine) while their
//! CPU ramps up, so a burst of waiters is staggered about `RAMP_MS` apart instead of all starting at once. After the
//! ramp only live use counts. Memory headroom uses max(reservation, live usage) per running job.
//!
//! Non-heavy usage (Chrome, Discord, other CLIs) is smoothed by the caller (mean of recent samples) so neither a
//! dip nor a spike decides alone. Live guards (load, memory, disk) can only veto.
//!
//! Load is an overload signal only together with a busy CPU: on macOS load also counts threads blocked in I/O, so
//! load can read 20-30 with the CPU nearly idle. The 5-minute load lags and is not used. A 1-minute load of 4 x cores
//! blocks whatever the CPU says (an I/O storm; on 2026-11-19 load 120 on 16 cores killed Docker).

use std::collections::VecDeque;

use crate::job::{Capacity, Job, Live, State, Usage, Weights};

/// A backfilling job may pass the head only while the head has waited less than this.
pub const BACKFILL_WINDOW_MS: i64 = 60_000;
const PCT: u64 = 90;
/// CPU headroom is measured against every core: a job asks for one free core.
const CPU_PCT: u64 = 100;
/// Load at or over this many times the core count blocks only while the CPU is also this busy.
const LOAD_SOFT: f64 = 2.0;
const LOAD_BUSY_CPU_PCT: u64 = 90;
/// Load at or over this many times the core count blocks whatever the CPU says.
const LOAD_HARD: f64 = 4.0;
/// A job admitted this recently has not ramped its CPU yet.
pub const RAMP_MS: i64 = 10_000;
const CPU_ASK: u64 = 100;
/// How many recent non-heavy samples the smoother averages (waiters re-sample every 10 s).
pub const SMOOTH_WINDOW: usize = 3;

/// Which check blocked a waiter (recorded in the history; a disk that is too full fails instead of waiting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Block {
    Cpu,
    Mem,
    Vram,
    Load1,
    /// No longer decided (the 5-minute load lags); kept so older history still parses.
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
    Blocked { reason: reason.into(), live, block, ramp_end_ms: None }
}

fn wait(reason: impl Into<String>, block: Block) -> Decision {
    Decision::Wait { reason: reason.into(), live: false, block, ramp_end_ms: None }
}

/// Mean of the last few non-heavy samples. A max would let one spike (or a machine-vs-tree sampling mismatch) block
/// for the whole window while the CPU sits idle; a mean still damps a single dip.
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
        let sum = self.window.iter().fold(Usage::default(), |a, b| Usage { cpu: a.cpu + b.cpu, mem: a.mem + b.mem, vram: a.vram + b.vram });
        Usage { cpu: sum.cpu / n, mem: sum.mem / n, vram: sum.vram / n }
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
    Usage { cpu: centi(cap.cpu) * CPU_PCT / 100, mem: cap.mem / 100 * PCT, vram: cap.vram / 100 * PCT }
}

fn request(w: Weights) -> Usage {
    Usage { cpu: if w.cpu > 0 { CPU_ASK } else { 0 }, mem: w.mem, vram: w.vram }
}

fn fits(ask: Usage, room: Usage) -> bool {
    ask.cpu <= room.cpu && ask.mem <= room.mem && ask.vram <= room.vram
}

/// Memory and VRAM reservations only: running sums plus the request within 90% of capacity. CPU is live.
#[must_use]
pub fn fits_reservations(req: Weights, running: &[&Job], cap: &Capacity) -> bool {
    let used = running.iter().fold(Usage::default(), |a, j| Usage { cpu: 0, mem: a.mem + j.weights.mem, vram: a.vram + j.weights.vram });
    let room = ceiling(cap).saturating_sub(used);
    fits(Usage { cpu: 0, ..request(req) }, room)
}

/// What is left for a new job once the other programs and the running jobs' real use are counted.
#[must_use]
pub fn headroom(running: &[&Job], cap: &Capacity, live: &Live, now_ms: i64) -> Usage {
    let held = running.iter().fold(Usage::default(), |a, j| {
        let l = live.per_job.iter().find(|(id, _)| *id == j.id).map_or_else(Usage::default, |(_, u)| *u);
        let ramping = j.started_ms.is_some_and(|t| now_ms - t < RAMP_MS);
        let cpu = if ramping { l.cpu.max(centi(j.weights.cpu).min(centi(cap.cpu))) } else { l.cpu };
        Usage { cpu: a.cpu + cpu, mem: a.mem + l.mem.max(j.weights.mem), vram: a.vram + l.vram.max(j.weights.vram) }
    });
    ceiling(cap).saturating_sub(live.non_heavy).saturating_sub(held)
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

/// Live guards, a veto only. The 1-minute load blocks at 2 x cores only while the CPU is also at least 90% busy
/// (macOS load counts threads blocked in I/O), and at 4 x cores always (runaway load). The 5-minute load is not used:
/// it lags, and blocked a nearly idle machine for minutes after a burst.
#[must_use]
pub fn guard_block(cap: &Capacity, live: &Live) -> Option<(String, Block)> {
    let cores = f64::from(cap.cpu);
    let (soft, hard) = (LOAD_SOFT * cores, LOAD_HARD * cores);
    if live.load1 >= hard {
        return Some((format!("1-minute load {:.1} >= {hard:.0} (4 x {} cores)", live.load1, cap.cpu), Block::Load1));
    }
    if live.load1 >= soft && live.cpu_pct >= LOAD_BUSY_CPU_PCT {
        return Some((format!("1-minute load {:.1} >= {soft:.0} (2 x {} cores) and cpu {}% busy", live.load1, cap.cpu, live.cpu_pct), Block::Load1));
    }
    if live.mem_used_pct >= PCT {
        return Some((format!("memory {}% used", live.mem_used_pct), Block::Mem));
    }
    None
}

/// Why a request cannot start now, or `None`. Alone (nothing running) a clamped request always passes
/// the resource checks, so a busy desktop cannot starve it forever; guards still apply.
fn blocker(w: Weights, running: &[&Job], cap: &Capacity, live: &Live, now_ms: i64) -> Option<Blocked> {
    if !running.is_empty() {
        if !fits_reservations(w, running, cap) {
            let vram = Usage { cpu: 0, mem: 0, vram: w.vram };
            let used_vram: u64 = running.iter().map(|j| j.weights.vram).sum();
            let block = if fits(vram, ceiling(cap).saturating_sub(Usage { cpu: 0, mem: 0, vram: used_vram })) { Block::Mem } else { Block::Vram };
            return Some(blocked("memory reservations full", false, block));
        }
        let ask = request(w);
        if !fits(ask, headroom(running, cap, live, now_ms)) {
            // Would it fit without the ramp holds? Then the ramp is the blocker, and it ends at a known time.
            if fits(ask, headroom(running, cap, live, i64::MAX)) {
                let end = running.iter().filter_map(|j| j.started_ms).map(|t| t + RAMP_MS).filter(|e| *e > now_ms).min();
                return Some(Blocked { reason: "no headroom (jobs admitted a moment ago are still ramping up)".into(), live: true, block: Block::Ramp, ramp_end_ms: end });
            }
            let room = headroom(running, cap, live, now_ms);
            let block = if ask.cpu > room.cpu {
                Block::Cpu
            } else if ask.mem > room.mem {
                Block::Mem
            } else {
                Block::Vram
            };
            return Some(blocked("no headroom (other programs and running jobs use the machine)", true, block));
        }
    }
    guard_block(cap, live).map(|(why, block)| blocked(why, true, block))
}

/// Can `job` start now, given every job file (already pruned of dead ones)?
#[must_use]
pub fn evaluate(job: &Job, jobs: &[Job], cap: &Capacity, live: &Live, now_ms: i64) -> Decision {
    let running: Vec<&Job> = jobs.iter().filter(|j| j.state == State::Running).collect();
    // A waiter that is stopped (Ctrl-Z) cannot start, so it never blocks the queue behind it.
    let mut queued: Vec<&Job> = jobs.iter().filter(|j| j.state == State::Queued && (j.id == job.id || !live.stopped.contains(&j.id))).collect();
    queued.sort_by(|a, b| (a.enqueued_ms, &a.id).cmp(&(b.enqueued_ms, &b.id)));

    if let Some(why) = disk_full(job.weights, cap, live) {
        return Decision::Fail(why);
    }
    if let Some(head) = queued.first().filter(|h| h.id != job.id) {
        if blocker(head.weights, &running, cap, live, now_ms).is_none() {
            // The head could start now: this job may go first only if the head still fits next to it.
            let me = Job { state: State::Running, started_ms: Some(now_ms), ..job.clone() };
            let mut with_me = running.clone();
            with_me.push(&me);
            if blocker(head.weights, &with_me, cap, live, now_ms).is_some() {
                return wait("queued behind a job that fits now", Block::Queue);
            }
        } else {
            if now_ms - head.enqueued_ms >= BACKFILL_WINDOW_MS {
                return wait(format!("queued behind {} (waited over 60 s)", head.id), Block::Queue);
            }
            let (a, h) = (job.weights, head.weights);
            if a.cpu > h.cpu || a.mem > h.mem || a.vram > h.vram {
                return wait("queued behind a smaller-or-equal head", Block::Queue);
            }
        }
    }
    match blocker(job.weights, &running, cap, live, now_ms) {
        Some(b) => Decision::Wait { reason: b.reason, live: b.live, block: b.block, ramp_end_ms: b.ramp_end_ms },
        None => Decision::Start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    fn cap() -> Capacity {
        Capacity { cpu: 10, mem: 100 * G, vram: 0, disk_total: 1000 * G, vram_is_ram: false }
    }
    fn calm() -> Live {
        Live { load1: 0.5, mem_used_pct: 10, disk_free: 900 * G, ..Live::default() }
    }
    fn job(id: &str, cpu: u32, mem_g: u64, state: State, enq: i64) -> Job {
        Job {
            id: id.into(),
            pid: 1,
            start_time: 0,
            weights: Weights { cpu, mem: mem_g * G, vram: 0, disk: 0 },
            cmd: vec!["x".into()],
            cwd: "/".into(),
            state,
            enqueued_ms: enq,
            started_ms: None,
            blocked_by: None,
        }
    }
    fn waits(d: &Decision) -> bool {
        matches!(d, Decision::Wait { .. })
    }

    #[test]
    fn alone_always_runs() {
        let me = job("a", 10, 100, State::Queued, 0);
        let busy = Live { non_heavy: Usage { cpu: 900, mem: 90 * G, vram: 0 }, ..calm() };
        assert_eq!(evaluate(&me, std::slice::from_ref(&me), &cap(), &busy, 0), Decision::Start);
    }

    #[test]
    fn ninety_percent_reservation_ceiling() {
        let run = job("r", 5, 50, State::Running, 0);
        let fits = job("a", 4, 40, State::Queued, 1);
        let over = job("b", 5, 41, State::Queued, 1);
        assert_eq!(evaluate(&fits, &[run.clone(), fits.clone()], &cap(), &calm(), 2), Decision::Start);
        let d = evaluate(&over, &[run, over.clone()], &cap(), &calm(), 2);
        assert!(matches!(d, Decision::Wait { live: false, .. }), "{d:?}");
    }

    #[test]
    fn headroom_math_with_fake_non_heavy_usage() {
        // 10 cores, 100G. Ceiling 1000 centi-cores (every core), 90G. Non-heavy: 2 cores, 30G.
        // Running job reserves 10G but really uses 3 cores / 20G: CPU is live, memory max(reservation, live).
        let run = job("r", 2, 10, State::Running, 0);
        let live = Live {
            non_heavy: Usage { cpu: 200, mem: 30 * G, vram: 0 },
            per_job: vec![("r".into(), Usage { cpu: 300, mem: 20 * G, vram: 0 })],
            ..calm()
        };
        let room = headroom(&[&run], &cap(), &live, 100_000);
        assert_eq!(room, Usage { cpu: 1000 - 200 - 300, mem: 90 * G - 30 * G - 20 * G, vram: 0 });
        // Memory fits at exactly the edge, blocked one GiB over; both pass the reservation-only check.
        let edge = job("e", 4, 40, State::Queued, 1);
        assert_eq!(evaluate(&edge, &[run.clone(), edge.clone()], &cap(), &live, 2), Decision::Start);
        let over = job("o", 5, 41, State::Queued, 1);
        let d = evaluate(&over, &[run.clone(), over.clone()], &cap(), &live, 2);
        assert!(matches!(d, Decision::Wait { live: true, .. }), "{d:?}");
        // The memory reservation is the floor when live usage is lower; CPU is just live.
        let idle = Live { per_job: vec![("r".into(), Usage::default())], non_heavy: Usage::default(), ..calm() };
        let room = headroom(&[&run], &cap(), &idle, 100_000);
        assert_eq!((room.cpu, room.mem), (1000, 80 * G));
    }

    #[test]
    fn non_heavy_is_machine_minus_heavy_trees() {
        let machine = Usage { cpu: 800, mem: 50 * G, vram: 2 * G };
        let trees = Usage { cpu: 600, mem: 20 * G, vram: 5 * G };
        assert_eq!(non_heavy(machine, trees), Usage { cpu: 200, mem: 30 * G, vram: 0 });
    }

    #[test]
    fn smoother_averages_the_recent_samples() {
        let mut s = Smoother::default();
        let hi = Usage { cpu: 600, mem: 12 * G, vram: 0 };
        assert_eq!(s.push(hi), hi);
        assert_eq!(s.push(Usage::default()), Usage { cpu: 300, mem: 6 * G, vram: 0 }, "a dip only halves it");
        assert_eq!(s.push(Usage::default()), Usage { cpu: 200, mem: 4 * G, vram: 0 }, "one spike does not hold the window");
        for _ in 0..SMOOTH_WINDOW {
            s.push(Usage::default());
        }
        assert_eq!(s.push(Usage::default()), Usage::default(), "old peaks age out");
    }

    #[test]
    fn many_tiny_jobs_run_together() {
        let mut all: Vec<Job> = (0..50).map(|i| job(&format!("r{i:02}"), 0, 0, State::Running, 0)).collect();
        let me = job("a", 0, 0, State::Queued, 1);
        all.push(me.clone());
        assert_eq!(evaluate(&me, &all, &cap(), &calm(), 2), Decision::Start, "no cap on the number of jobs");
    }

    #[test]
    fn guards_veto() {
        let me = job("a", 1, 1, State::Queued, 0);
        let one = std::slice::from_ref(&me);
        // 10 cores: load 20 blocks only with a busy CPU, load 40 always.
        let busy = Live { load1: 21.0, cpu_pct: 95, ..calm() };
        assert!(matches!(evaluate(&me, one, &cap(), &busy, 0), Decision::Wait { live: true, block: Block::Load1, .. }));
        let calm_1m = Live { load1: 19.0, cpu_pct: 95, ..calm() };
        assert_eq!(evaluate(&me, one, &cap(), &calm_1m, 0), Decision::Start, "macOS load reads high: 2 x cores");
        let io_bound = Live { load1: 35.0, cpu_pct: 40, ..calm() };
        assert_eq!(evaluate(&me, one, &cap(), &io_bound, 0), Decision::Start, "high load with an idle CPU is threads waiting on I/O");
        let runaway = Live { load1: 40.0, cpu_pct: 10, ..calm() };
        assert!(matches!(evaluate(&me, one, &cap(), &runaway, 0), Decision::Wait { block: Block::Load1, .. }), "4 x cores blocks whatever the CPU says");
        let mem = Live { mem_used_pct: 95, ..calm() };
        assert!(waits(&evaluate(&me, std::slice::from_ref(&me), &cap(), &mem, 0)));
    }

    #[test]
    fn disk_guard() {
        let mut me = job("a", 1, 1, State::Queued, 0);
        me.weights.disk = 500 * G;
        let low = Live { disk_free: 550 * G, ..calm() };
        assert!(matches!(evaluate(&me, std::slice::from_ref(&me), &cap(), &low, 0), Decision::Fail(m) if m.contains("disk is full")));
        let ok = Live { disk_free: 600 * G, ..calm() };
        assert_eq!(evaluate(&me, std::slice::from_ref(&me), &cap(), &ok, 0), Decision::Start);
    }

    #[test]
    fn fifo_head_blocks_non_fitting_follower() {
        let run = job("r", 8, 50, State::Running, 0);
        let head = job("h", 5, 30, State::Queued, 1);
        let big = job("z", 6, 40, State::Queued, 2);
        assert!(waits(&evaluate(&big, &[run, head, big.clone()], &cap(), &calm(), 3)));
    }

    #[test]
    fn backfill_only_while_head_is_young_and_follower_smaller() {
        let run = job("r", 8, 70, State::Running, 0);
        let head = job("h", 5, 30, State::Queued, 1);
        let small = job("s", 1, 5, State::Queued, 2);
        let all = [run, head, small.clone()];
        assert_eq!(evaluate(&small, &all, &cap(), &calm(), 1000), Decision::Start);
        assert!(waits(&evaluate(&small, &all, &cap(), &calm(), BACKFILL_WINDOW_MS + 1)));
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
        let live = Live { non_heavy: Usage { cpu: 320, mem: 8 * G, vram: 0 }, ..calm() };
        let ra = Job { state: State::Running, started_ms: Some(10), ..job("a", 16, 8, State::Running, 0) };
        let b = job("b", 16, 8, State::Queued, 2);
        let d = evaluate(&b, &[ra, b.clone()], &c, &live, 500);
        assert!(matches!(d, Decision::Wait { block: Block::Ramp, ramp_end_ms: Some(e), live: true, .. } if e == 10 + RAMP_MS), "{d:?}");
        // A genuinely busy CPU is a cpu block, with no ramp time.
        let busy = Live { non_heavy: Usage { cpu: 1550, mem: 8 * G, vram: 0 }, ..calm() };
        let run = Job { state: State::Running, started_ms: Some(0), ..job("r", 1, 8, State::Running, 0) };
        let d = evaluate(&b, &[run, b.clone()], &c, &busy, 100_000);
        assert!(matches!(d, Decision::Wait { block: Block::Cpu, ramp_end_ms: None, .. }), "{d:?}");
    }

    #[test]
    fn peak_core_waiters_start_on_a_mostly_idle_machine() {
        // Both builds peaked near all cores in history (weights.cpu = ncpu); live CPU is 20%.
        let c = Capacity { cpu: 16, ..cap() };
        let live = Live { non_heavy: Usage { cpu: 320, mem: 8 * G, vram: 0 }, ..calm() };
        let a = job("a", 16, 8, State::Queued, 1);
        let b = job("b", 16, 8, State::Queued, 2);
        assert_eq!(evaluate(&a, &[a.clone(), b.clone()], &c, &live, 10), Decision::Start);
        // a was admitted just now and holds its full 16 cores while ramping: b waits for the ramp, then starts.
        let ra = Job { state: State::Running, started_ms: Some(10), ..a };
        assert!(waits(&evaluate(&b, &[ra.clone(), b.clone()], &c, &live, 11)), "blocked by a's ramp hold");
        assert_eq!(evaluate(&b, &[ra.clone(), b.clone()], &c, &live, 10 + RAMP_MS), Decision::Start, "the second starts once the first one's ramp has ended");
        let ca = job("c", 16, 8, State::Queued, 3);
        // A genuinely busy CPU still blocks.
        let busy = Live { non_heavy: Usage { cpu: 1550, mem: 8 * G, vram: 0 }, ..calm() };
        let run = Job { state: State::Running, started_ms: Some(0), ..job("r", 16, 8, State::Running, 0) };
        assert!(waits(&evaluate(&ca, &[run, ca.clone()], &c, &busy, 100_000)));
    }

    #[test]
    fn burst_is_held_back_by_ramp_holds() {
        // 16 cores, ceiling 1600; every build peaked at 4 cores but is at 0% live while ramping.
        let c = Capacity { cpu: 16, ..cap() };
        let mut jobs: Vec<Job> = (0..8).map(|i| job(&format!("j{i}"), 4, 1, State::Queued, i)).collect();
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
        assert_eq!(started, 4, "holds of 400 each leave 400 for the 4th and 0 for the 5th: not a burst of 8");
        // Once the ramp window passes and live CPU is low, the rest follow.
        let next = jobs.iter().find(|j| j.state == State::Queued).unwrap().clone();
        assert_eq!(evaluate(&next, &jobs, &c, &calm(), 100 + RAMP_MS), Decision::Start);
        // Not blocked on peaks long-term: after the ramp the hold is gone entirely.
        let ramped = Job { state: State::Running, started_ms: Some(0), ..job("r", 16, 1, State::Running, 0) };
        let room = headroom(&[&ramped], &c, &calm(), RAMP_MS + 1);
        assert_eq!(room.cpu, 1600);
    }

    #[test]
    fn stopped_waiter_file_does_not_block_the_next_waiter() {
        let run = job("r", 1, 50, State::Running, 0);
        let head = job("h", 1, 30, State::Queued, 1);
        let next = job("n", 1, 40, State::Queued, 2);
        let t = 3 + BACKFILL_WINDOW_MS;
        assert!(waits(&evaluate(&next, &[run.clone(), head.clone(), next.clone()], &cap(), &calm(), t)));
        let stopped = Job { state: State::Stopped, ..head };
        assert_eq!(evaluate(&next, &[run, stopped, next.clone()], &cap(), &calm(), t), Decision::Start);
    }

    #[test]
    fn stopped_head_does_not_block_the_next_waiter() {
        let run = job("r", 1, 50, State::Running, 0);
        let head = job("h", 1, 30, State::Queued, 1);
        let next = job("n", 1, 40, State::Queued, 2);
        let all = [run, head, next.clone()];
        assert!(waits(&evaluate(&next, &all, &cap(), &calm(), 3 + BACKFILL_WINDOW_MS)), "a live head blocks the bigger follower");
        let live = Live { stopped: vec!["h".into()], ..calm() };
        assert_eq!(evaluate(&next, &all, &cap(), &live, 3 + BACKFILL_WINDOW_MS), Decision::Start);
    }

    fn mac() -> Capacity {
        Capacity { cpu: 16, mem: 64 * G, vram: 0, disk_total: 1000 * G, vram_is_ram: true }
    }

    #[test]
    fn a_lagging_five_minute_load_never_blocks_an_idle_machine() {
        // 2026-11-20 03:08: cpu 12%, load1 22.6, load5 34.2, nothing running, one waiter blocked by load5.
        let me = job("a", 4, 4, State::Queued, 0);
        let live = Live { load1: 22.6, load5: 34.2, cpu_pct: 12, mem_used_pct: 46, non_heavy: Usage { cpu: 192, mem: 29 * G, vram: 0 }, ..calm() };
        assert_eq!(evaluate(&me, std::slice::from_ref(&me), &mac(), &live, 0), Decision::Start);
        // 03:22: cpu 16%, load1 21.3, load5 32.9, one job running, three waiting.
        let run = Job { started_ms: Some(0), ..job("r", 4, 4, State::Running, 0) };
        let live = Live { load1: 21.3, load5: 32.9, cpu_pct: 16, per_job: vec![("r".into(), Usage { cpu: 100, mem: 2 * G, vram: 0 })], non_heavy: Usage { cpu: 156, mem: 24 * G, vram: 0 }, ..live };
        assert_eq!(evaluate(&me, &[run, me.clone()], &mac(), &live, 100_000), Decision::Start);
    }

    #[test]
    fn a_small_job_starts_at_seventy_percent_cpu() {
        // 2026-11-20 03:31-03:36: cpu 67-73%, one or two jobs running, a waiter blocked by cpu.
        let run = Job { started_ms: Some(0), ..job("r", 8, 4, State::Running, 0) };
        let me = job("a", 4, 4, State::Queued, 1);
        let live = Live {
            load1: 26.0,
            load5: 31.0,
            cpu_pct: 72,
            per_job: vec![("r".into(), Usage { cpu: 800, mem: 4 * G, vram: 0 })],
            non_heavy: Usage { cpu: 352, mem: 22 * G, vram: 0 },
            ..calm()
        };
        assert_eq!(evaluate(&me, &[run.clone(), me.clone()], &mac(), &live, 100_000), Decision::Start);
        // Jobs keep starting until about one core is left (15 of 16 busy), then wait on cpu.
        let full = Live { per_job: vec![("r".into(), Usage { cpu: 1100, mem: 4 * G, vram: 0 })], non_heavy: Usage { cpu: 420, mem: 22 * G, vram: 0 }, cpu_pct: 95, ..live };
        assert!(matches!(evaluate(&me, &[run, me.clone()], &mac(), &full, 100_000), Decision::Wait { block: Block::Cpu, .. }));
    }

    #[test]
    fn a_saturated_machine_still_blocks() {
        // 2026-11-20 03:19: cpu 100%, load1 55: the load guard holds even for a job that would run alone.
        let me = job("a", 4, 4, State::Queued, 0);
        let live = Live { load1: 55.3, load5: 39.6, cpu_pct: 100, ..calm() };
        assert!(matches!(evaluate(&me, std::slice::from_ref(&me), &mac(), &live, 0), Decision::Wait { block: Block::Load1, .. }));
        // Load 120 on 16 cores (2026-11-19) blocks even if the CPU reads idle.
        let storm = Live { load1: 120.0, cpu_pct: 5, ..calm() };
        assert!(matches!(evaluate(&me, std::slice::from_ref(&me), &mac(), &storm, 0), Decision::Wait { block: Block::Load1, .. }));
    }

    #[test]
    fn clamp_to_machine_and_fold_vram() {
        let c = cap();
        let w = c.clamp(Weights { cpu: 64, mem: 999 * G, vram: 5 * G, disk: 9999 * G });
        assert_eq!(w, Weights { cpu: 10, mem: 100 * G, vram: 0, disk: 1000 * G });
        let mac = Capacity { vram_is_ram: true, ..c };
        assert_eq!(mac.clamp(Weights { cpu: 1, mem: 4 * G, vram: 2 * G, disk: 0 }).mem, 6 * G);
    }
}
