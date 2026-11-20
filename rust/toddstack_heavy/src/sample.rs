//! Machine samples for `heavy history`. While any heavy job waits or runs, each heavy process checks every 10 s
//! whether a sample is due; the one that finds it due and takes the lock writes it. So samples are at most 30 s apart
//! in the log, whoever writes them, and none are written when no job exists. This is monitoring (exempt from the
//! no-polling rule).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;

use crate::admit::{Decision, evaluate};
use crate::error::Error;
use crate::job::{Capacity, State};
use crate::log::{self, Event};
use crate::machine;
use crate::state::StateDir;

/// Samples are at least this far apart.
pub const SAMPLE_EVERY_MS: i64 = 30_000;
/// How often each process checks whether a sample is due.
const CHECK: Duration = Duration::from_secs(10);

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// Writes a sample if one is due and some job exists.
///
/// # Errors
/// Fails when the machine cannot be probed or the state dir cannot be locked.
pub fn sample_once(state: &StateDir, cwd: &std::path::Path, cap: &Capacity) -> Result<bool, Error> {
    if now_ms() - state.last_sample_ms() < SAMPLE_EVERY_MS {
        return Ok(false);
    }
    // Measure before taking the lock (it takes a quarter of a second); recheck under it.
    let running: Vec<_> = state.read_jobs().into_iter().filter(|j| j.state == State::Running).collect();
    let mut live = machine::live(cwd, cap, &running)?;
    let _lock = state.lock()?;
    let now = now_ms();
    if now - state.last_sample_ms() < SAMPLE_EVERY_MS {
        return Ok(false);
    }
    let jobs = state.live_jobs();
    if jobs.is_empty() {
        return Ok(false);
    }
    live.stopped = machine::stopped_ids(&jobs);
    let mut blocked: BTreeMap<String, u32> = BTreeMap::new();
    let mut waiting = 0;
    for j in jobs.iter().filter(|j| j.state != State::Running) {
        waiting += 1;
        if j.state == State::Queued
            && !live.stopped.contains(&j.id)
            && let Decision::Wait { block, .. } = evaluate(j, &jobs, cap, &live, now)
        {
            *blocked.entry(block.name().to_string()).or_default() += 1;
        }
    }
    let running = u32::try_from(jobs.iter().filter(|j| j.state == State::Running).count()).unwrap_or(u32::MAX);
    log::record(
        state.base(),
        &Event::Sample {
            ts: Timestamp::from_millisecond(now).unwrap_or_default(),
            cpu_pct: live.cpu_pct,
            mem_pct: live.mem_used_pct,
            load1: live.load1,
            load5: live.load5,
            ncpu: cap.cpu,
            running,
            waiting,
            blocked,
        },
    );
    state.set_last_sample_ms(now);
    Ok(true)
}

/// Starts the background check for this heavy process (it ends with the process).
pub fn spawn(state: &StateDir, cwd: PathBuf, cap: Capacity) {
    let state = state.clone();
    std::thread::spawn(move || {
        loop {
            let _ = sample_once(&state, &cwd, &cap);
            std::thread::sleep(CHECK);
        }
    });
}
