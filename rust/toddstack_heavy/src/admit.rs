//! Admission: pure decisions.
//!
//! Memory and VRAM are reservation-based (real safety): running reservations plus the request must fit
//! within 90% of capacity. CPU is judged by live headroom only, never by peak-core reservations (a build
//! that peaks near all cores would otherwise block everything on an idle machine):
//!
//! cpu headroom = 90% of cores - non-heavy usage - live use of running jobs
//!
//! Jobs admitted in the last `RAMP_MS` reserve at least min(their cores, `RAMP_CPU`) while their CPU ramps up. Memory
//! headroom uses max(reservation, live usage) per running job.
//!
//! Non-heavy usage (Chrome, Discord, other CLIs) is smoothed by the caller (max of recent samples) so a
//! dip does not over-admit. Live guards (load, memory, disk) can only veto.

use std::collections::VecDeque;

use crate::job::{Capacity, Job, Live, State, Usage, Weights};

/// A backfilling job may pass the head only while the head has waited less than this.
pub const BACKFILL_WINDOW_MS: i64 = 60_000;
const PCT: u64 = 90;
/// A job admitted this recently has not ramped its CPU yet.
pub const RAMP_MS: i64 = 10_000;
/// CPU (centi-cores) a just-admitted job holds until it ramps, and what a new job needs free to start.
pub const RAMP_CPU: u64 = 200;
const CPU_ASK: u64 = 100;
/// How many recent non-heavy samples the smoother keeps.
pub const SMOOTH_WINDOW: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Start,
    /// The job can never run here (the disk is too full even with nothing else running): exit at once.
    Fail(String),
    /// `live` is true when live data is the blocker, so waiting re-samples on a timer; otherwise a job
    /// finishing (a file event) is what unblocks it.
    Wait { reason: String, live: bool },
}

fn wait(reason: impl Into<String>, live: bool) -> Decision {
    Decision::Wait { reason: reason.into(), live }
}

/// Max of the last few non-heavy samples.
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
        self.window.iter().fold(Usage::default(), |a, b| a.max(*b))
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
    Usage { cpu: centi(cap.cpu) * PCT / 100, mem: cap.mem / 100 * PCT, vram: cap.vram / 100 * PCT }
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
        let cpu = if ramping { l.cpu.max(centi(j.weights.cpu).min(RAMP_CPU)) } else { l.cpu };
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

/// Live guards, a veto only. Load is judged over 1 and 5 minutes, each under 2 x cores (macOS load reads high).
#[must_use]
pub fn guard_block(cap: &Capacity, live: &Live) -> Option<String> {
    let limit = 2.0 * f64::from(cap.cpu);
    if live.load1 >= limit {
        return Some(format!("1-minute load {:.1} >= {limit:.0} (2 x {} cores)", live.load1, cap.cpu));
    }
    if live.load5 >= limit {
        return Some(format!("5-minute load {:.1} >= {limit:.0} (2 x {} cores)", live.load5, cap.cpu));
    }
    if live.mem_used_pct >= PCT {
        return Some(format!("memory {}% used", live.mem_used_pct));
    }
    None
}

/// Why a request cannot start now, or `None`. Alone (nothing running) a clamped request always passes
/// the resource checks, so a busy desktop cannot starve it forever; guards still apply.
fn blocker(w: Weights, running: &[&Job], cap: &Capacity, live: &Live, now_ms: i64) -> Option<(String, bool)> {
    if !running.is_empty() {
        if !fits_reservations(w, running, cap) {
            return Some(("memory reservations full".to_string(), false));
        }
        if !fits(request(w), headroom(running, cap, live, now_ms)) {
            return Some(("no headroom (other programs and running jobs use the machine)".to_string(), true));
        }
    }
    guard_block(cap, live).map(|why| (why, true))
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
            let me = Job { state: State::Running, ..job.clone() };
            let mut with_me = running.clone();
            with_me.push(&me);
            if blocker(head.weights, &with_me, cap, live, now_ms).is_some() {
                return wait("queued behind a job that fits now", false);
            }
        } else {
            if now_ms - head.enqueued_ms >= BACKFILL_WINDOW_MS {
                return wait(format!("queued behind {} (waited over 60 s)", head.id), false);
            }
            let (a, h) = (job.weights, head.weights);
            if a.cpu > h.cpu || a.mem > h.mem || a.vram > h.vram {
                return wait("queued behind a smaller-or-equal head", false);
            }
        }
    }
    match blocker(job.weights, &running, cap, live, now_ms) {
        Some((why, is_live)) => wait(why, is_live),
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
        // 10 cores, 100G. Ceiling 900 centi-cores, 90G. Non-heavy: 2 cores, 30G.
        // Running job reserves 10G but really uses 3 cores / 20G: CPU is live, memory max(reservation, live).
        let run = job("r", 2, 10, State::Running, 0);
        let live = Live {
            non_heavy: Usage { cpu: 200, mem: 30 * G, vram: 0 },
            per_job: vec![("r".into(), Usage { cpu: 300, mem: 20 * G, vram: 0 })],
            ..calm()
        };
        let room = headroom(&[&run], &cap(), &live, 100_000);
        assert_eq!(room, Usage { cpu: 900 - 200 - 300, mem: 90 * G - 30 * G - 20 * G, vram: 0 });
        // Memory fits at exactly the edge, blocked one GiB over; both pass the reservation-only check.
        let edge = job("e", 4, 40, State::Queued, 1);
        assert_eq!(evaluate(&edge, &[run.clone(), edge.clone()], &cap(), &live, 2), Decision::Start);
        let over = job("o", 5, 41, State::Queued, 1);
        let d = evaluate(&over, &[run.clone(), over.clone()], &cap(), &live, 2);
        assert!(matches!(d, Decision::Wait { live: true, .. }), "{d:?}");
        // The memory reservation is the floor when live usage is lower; CPU is just live.
        let idle = Live { per_job: vec![("r".into(), Usage::default())], non_heavy: Usage::default(), ..calm() };
        let room = headroom(&[&run], &cap(), &idle, 100_000);
        assert_eq!((room.cpu, room.mem), (900, 80 * G));
    }

    #[test]
    fn non_heavy_is_machine_minus_heavy_trees() {
        let machine = Usage { cpu: 800, mem: 50 * G, vram: 2 * G };
        let trees = Usage { cpu: 600, mem: 20 * G, vram: 5 * G };
        assert_eq!(non_heavy(machine, trees), Usage { cpu: 200, mem: 30 * G, vram: 0 });
    }

    #[test]
    fn smoother_keeps_the_recent_max() {
        let mut s = Smoother::default();
        let hi = Usage { cpu: 500, mem: 10 * G, vram: 0 };
        assert_eq!(s.push(hi), hi);
        assert_eq!(s.push(Usage::default()), hi, "a dip does not lower it");
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
        let busy = Live { load1: 21.0, ..calm() };
        assert!(matches!(evaluate(&me, std::slice::from_ref(&me), &cap(), &busy, 0), Decision::Wait { live: true, .. }));
        let calm_1m = Live { load1: 19.0, ..calm() };
        assert_eq!(evaluate(&me, std::slice::from_ref(&me), &cap(), &calm_1m, 0), Decision::Start, "macOS load reads high: 2 x cores");
        let busy5 = Live { load5: 20.5, ..calm() };
        assert!(matches!(evaluate(&me, std::slice::from_ref(&me), &cap(), &busy5, 0), Decision::Wait { live: true, .. }));
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
    fn peak_core_waiters_start_on_a_mostly_idle_machine() {
        // Both builds peaked near all cores in history (weights.cpu = ncpu); live CPU is 20%.
        let c = Capacity { cpu: 16, ..cap() };
        let live = Live { non_heavy: Usage { cpu: 320, mem: 8 * G, vram: 0 }, ..calm() };
        let a = job("a", 16, 8, State::Queued, 1);
        let b = job("b", 16, 8, State::Queued, 2);
        assert_eq!(evaluate(&a, &[a.clone(), b.clone()], &c, &live, 10), Decision::Start);
        // a was admitted just now: b is judged next to a running, ramping a.
        let ra = Job { state: State::Running, started_ms: Some(10), ..a };
        assert_eq!(evaluate(&b, &[ra.clone(), b.clone()], &c, &live, 11), Decision::Start);
        // A third one right behind still fits (live 320 + 2 x ramp 200 < 1440).
        let ca = job("c", 16, 8, State::Queued, 3);
        let rb = Job { state: State::Running, started_ms: Some(11), ..b };
        assert_eq!(evaluate(&ca, &[ra, rb, ca.clone()], &c, &live, 12), Decision::Start);
        // A genuinely busy CPU still blocks.
        let busy = Live { non_heavy: Usage { cpu: 1400, mem: 8 * G, vram: 0 }, ..calm() };
        let run = Job { state: State::Running, started_ms: Some(0), ..job("r", 16, 8, State::Running, 0) };
        assert!(waits(&evaluate(&ca, &[run, ca.clone()], &c, &busy, 100_000)));
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

    #[test]
    fn clamp_to_machine_and_fold_vram() {
        let c = cap();
        let w = c.clamp(Weights { cpu: 64, mem: 999 * G, vram: 5 * G, disk: 9999 * G });
        assert_eq!(w, Weights { cpu: 10, mem: 100 * G, vram: 0, disk: 1000 * G });
        let mac = Capacity { vram_is_ram: true, ..c };
        assert_eq!(mac.clamp(Weights { cpu: 1, mem: 4 * G, vram: 2 * G, disk: 0 }).mem, 6 * G);
    }
}
