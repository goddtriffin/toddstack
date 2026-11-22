//! Machine samples for `stokehold history`. While any heavy job waits or runs, each heavy process checks every 10 s
//! whether a sample is due; the one that finds it due and takes the lock writes it. So samples are at most 30 s apart
//! in the log, whoever writes them, and none are written when no job exists. This is monitoring (exempt from the
//! no-polling rule). The same check keeps the shared CPU readings (`admit::sustained_pct`) at most 10 s old while any
//! job exists, so a waiter that arrives on a busy machine sees the last seconds, not only its own 250 ms window.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;

use crate::admit::BUSY_WINDOW_MS;
use crate::error::Error;
use crate::job::{Capacity, Job, State};
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

/// Writes a sample if one is due and some job exists, and adds a shared CPU reading if the newest is `CHECK` old.
///
/// # Errors
/// Fails when the machine cannot be probed or the state dir cannot be locked.
pub fn sample_once(state: &StateDir, cwd: &std::path::Path, cap: &Capacity) -> Result<bool, Error> {
    let due = |now: i64| {
        (
            now - state.last_sample_ms() >= SAMPLE_EVERY_MS,
            busy_due(state, now),
        )
    };
    if due(now_ms()) == (false, false) {
        return Ok(false);
    }
    // Measure before taking the lock (it takes a quarter of a second); recheck under it.
    let running: Vec<_> = state
        .read_jobs()
        .into_iter()
        .filter(|j| j.state == State::Running)
        .collect();
    let mut live = machine::live(cwd, cap, &running)?;
    let _lock = state.lock()?;
    let now = now_ms();
    let (sample_due, reading_due) = due(now);
    let jobs = state.live_jobs();
    if jobs.is_empty() {
        return Ok(false);
    }
    if reading_due {
        state.push_busy(now, live.cpu_pct, BUSY_WINDOW_MS);
    }
    if !sample_due {
        return Ok(false);
    }
    live.stopped = machine::stopped_ids(&jobs);
    let (waiting, blocked) = tally(&jobs, &live.stopped);
    let running = u32::try_from(jobs.iter().filter(|j| j.state == State::Running).count())
        .unwrap_or(u32::MAX);
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

/// Is the newest shared CPU reading at least `CHECK` old (or missing)?
fn busy_due(state: &StateDir, now: i64) -> bool {
    let every = i64::try_from(CHECK.as_millis()).unwrap_or(i64::MAX);
    state
        .busy_readings()
        .last()
        .is_none_or(|(ts, _)| now - ts >= every)
}

/// Counts the waiters and what blocks them. Only queued jobs that have evaluated (have a reason) wait: a stopped one (Ctrl-Z) cannot start, so it is
/// neither waiting nor blocked. The reasons are the ones the waiters wrote into their own job files (they decide with
/// smoothed data; the sampler never re-evaluates).
fn tally(jobs: &[Job], stopped: &[String]) -> (u32, BTreeMap<String, u32>) {
    let mut blocked: BTreeMap<String, u32> = BTreeMap::new();
    let mut waiting = 0;
    for j in jobs
        .iter()
        .filter(|j| j.state == State::Queued && !stopped.contains(&j.id))
    {
        // No reason yet: the waiter has not evaluated, so it is not known to wait.
        let Some(b) = &j.blocked_by else { continue };
        waiting += 1;
        *blocked.entry(b.clone()).or_default() += 1;
    }
    (waiting, blocked)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Weights;

    fn job(id: &str, state: State, by: Option<&str>) -> Job {
        Job {
            id: id.into(),
            pid: 1,
            start_time: 0,
            weights: Weights::default(),
            cmd: vec!["x".into()],
            cwd: "/".into(),
            state,
            enqueued_ms: 1,
            started_ms: None,
            mem_declared: false,
            mem_peak_at_ms: None,
            vram_declared: false,
            v: crate::job::JOB_VERSION,
            settle_ms: None,
            typical_cpu: None,
            blocked_by: by.map(Into::into),
        }
    }

    #[test]
    fn the_sampler_reads_the_reasons_the_waiters_wrote() {
        let jobs = [
            job("r", State::Running, None),
            job("a", State::Queued, Some("cpu")),
            job("b", State::Queued, Some("ramp")),
            job("c", State::Queued, Some("cpu")),
        ];
        let (waiting, blocked) = tally(&jobs, &[]);
        assert_eq!(waiting, 3);
        assert_eq!(
            blocked,
            BTreeMap::from([("cpu".to_string(), 2), ("ramp".to_string(), 1)])
        );
    }

    #[test]
    fn unevaluated_waiters_are_not_counted() {
        let jobs = [
            job("a", State::Queued, None),
            job("b", State::Queued, Some("cpu")),
        ];
        let (waiting, blocked) = tally(&jobs, &[]);
        assert_eq!(waiting, 1);
        assert_eq!(blocked, BTreeMap::from([("cpu".to_string(), 1)]));
    }

    #[test]
    fn stopped_jobs_do_not_wait() {
        let jobs = [
            job("a", State::Stopped, Some("cpu")),
            job("b", State::Queued, Some("cpu")),
            job("c", State::Queued, Some("ramp")),
        ];
        let (waiting, blocked) = tally(&jobs, &["c".to_string()]);
        assert_eq!(waiting, 1);
        assert_eq!(blocked, BTreeMap::from([("cpu".to_string(), 1)]));
    }
}
