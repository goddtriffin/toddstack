//! Replay (test only, ignored by default): re-runs recorded jobs from a history file through `admit::evaluate` on a
//! simulated machine and prints the utilization figures the 6-hourly check uses (in band, underused, overloaded).
//!
//! `REPLAY_HISTORY=<history.jsonl> REPLAY_FROM=<RFC 3339> REPLAY_TO=<RFC 3339>
//! cargo test -p stokehold replay -- --ignored --nocapture`. Optional: `REPLAY_CORES` (16),
//! `REPLAY_MEM_G` (64), `REPLAY_BG_PCT` (other programs' CPU, 29), `REPLAY_BG_MEM_PCT` (50).
//!
//! The model: each job arrives when it was queued and needs the CPU work it recorded (mean cores x run time). Its demand
//! follows its recorded shape: 10 s blocks at its sustained level (its best 10 s mean) for the share of the run that
//! keeps its mean, the rest at a third of its mean, and one second at its recorded peak. When demand exceeds the cores
//! every job slows in proportion (work-conserving). Other programs' CPU is constant. Load is the 1- and 5-minute
//! average of the demand. Every waiter is evaluated each simulated second, oldest first.

use std::collections::HashMap;

use jiff::Timestamp;

use crate::admit::{BUSY_WINDOW_MS, Decision, evaluate, recent_peak_pct, sustained_pct};
use crate::history::{Defaults, Entry, defaults_from, repo_key};
use crate::job::{Capacity, Job, Live, State, Usage, Weights};

const G: u64 = 1 << 30;

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn hash(a: u64, b: u64) -> u64 {
    let mut z = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_add(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Sim {
    job: Job,
    seed: u64,
    run_s: u64,
    mean: u64,
    sustained: u64,
    peak: u64,
    peak_at_s: u64,
    mem: u64,
    mem_at_s: u64,
    progress: f64,
    samples: Vec<(u64, u64)>,
    done_at: Option<i64>,
}

impl Sim {
    /// Demand (centi-cores) at nominal second `s` of the run.
    fn demand(&self, s: u64, cores: u64) -> u64 {
        let d = if s == self.peak_at_s {
            self.peak
        } else if self.sustained <= self.mean || self.run_s < 10 {
            self.mean
        } else {
            let base = self.mean / 3;
            let share = (self.mean - base) * 1_000 / (self.sustained - base).max(1);
            let block = s / 10;
            if block == self.peak_at_s / 10 || hash(self.seed, block) % 1_000 < share {
                self.sustained
            } else {
                base
            }
        };
        d.min(cores)
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "progress is a non-negative count of seconds"
    )]
    fn mem_now(&self) -> u64 {
        let s = self.progress as u64;
        // Up to the peak by its recorded time, then half of it: a peak RSS is a moment, not a level.
        if s > self.mem_at_s + 10 {
            self.mem / 2
        } else if s >= self.mem_at_s {
            self.mem
        } else {
            self.mem * (s + 1) / (self.mem_at_s + 1)
        }
    }
}

/// Builds the job file the waiter would have written, from what history knew when it was queued.
fn make_job(id: String, e: &Entry, d: Option<Defaults>, cap: &Capacity, q_ms: i64) -> Job {
    let weights = cap.clamp(Weights {
        cpu: d.map_or(cap.cpu, |d| d.cpu),
        mem: d.map_or(4 << 30, |d| d.mem),
        vram: d.and_then(|d| d.vram).unwrap_or(0),
        disk: 0,
    });
    let (settle_ms, typical_cpu) = crate::run::admit_shape(&e.cmd, d, None, cap);
    Job {
        id,
        pid: 0,
        start_time: 0,
        weights,
        cmd: e.cmd.clone(),
        cwd: e.cwd.clone(),
        state: State::Queued,
        enqueued_ms: q_ms,
        started_ms: None,
        blocked_by: None,
        mem_declared: false,
        mem_peak_at_ms: d
            .and_then(|d| d.mem_peak_at_ms)
            .and_then(|m| i64::try_from(m).ok()),
        vram_declared: false,
        v: crate::job::JOB_VERSION,
        settle_ms: Some(settle_ms),
        typical_cpu,
    }
}

fn ms(t: Timestamp) -> i64 {
    t.as_millisecond()
}

#[test]
#[ignore = "replays a real history file; run by hand with REPLAY_HISTORY"]
#[expect(clippy::too_many_lines, reason = "one simulation loop")]
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "simulation arithmetic"
)]
fn replay() {
    let Ok(path) = std::env::var("REPLAY_HISTORY") else {
        return;
    };
    let from: Timestamp = std::env::var("REPLAY_FROM").unwrap().parse().unwrap();
    let to: Timestamp = std::env::var("REPLAY_TO").unwrap().parse().unwrap();
    let cores: u32 = env("REPLAY_CORES", 16);
    let cap = Capacity {
        cpu: cores,
        mem: env("REPLAY_MEM_G", 64) * G,
        vram: 0,
        disk_total: 1000 * G,
        vram_is_ram: true,
    };
    let bg_cpu = u64::from(cores) * env("REPLAY_BG_PCT", 29u64);
    let bg_mem = cap.mem / 100 * env("REPLAY_BG_MEM_PCT", 50u64);
    let centi = u64::from(cores) * 100;

    let text = std::fs::read_to_string(path).unwrap();
    let mut entries: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    entries.sort_by_key(|e| e.ts);
    let mut sims: Vec<Sim> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let m = &e.measured;
        let q = ms(e.ts) - i64::try_from(m.run_ms + m.queued_ms).unwrap_or(0);
        if q < ms(from) || q >= ms(to) {
            continue;
        }
        let before: Vec<Entry> = entries.iter().filter(|x| ms(x.ts) < q).cloned().collect();
        let d = defaults_from(
            &before,
            &e.cmd,
            &e.cwd,
            repo_key(std::path::Path::new(&e.cwd)).as_deref(),
        );
        let run_s = (m.run_ms / 1_000).max(1);
        let vm = crate::run::hosts_vm_work(&e.cmd);
        let mean = if vm {
            m.centi_cores.max(m.peak_other_centi_cores / 2)
        } else {
            m.centi_cores
        };
        let peak = m
            .peak_centi_cores
            .max(if vm { m.peak_other_centi_cores } else { 0 })
            .max(mean);
        sims.push(Sim {
            job: make_job(format!("{q:013}-{i}"), e, d, &cap, q),
            seed: i as u64,
            run_s,
            mean,
            sustained: m.sustained_centi_cores.clamp(mean, peak),
            peak,
            peak_at_s: (m.peak_cpu_at_ms / 1_000).min(run_s - 1),
            mem: m.peak_tree_rss.max(m.peak_rss),
            mem_at_s: m.peak_mem_at_ms / 1_000,
            progress: 0.0,
            samples: Vec::new(),
            done_at: None,
        });
    }
    sims.sort_by_key(|s| s.job.enqueued_ms);
    println!("replay: {} jobs from {from} to {to}", sims.len());
    let mut now = ms(from);
    let end_by = ms(to) + 6 * 3_600_000;
    let (mut load1, mut load5) = (bg_cpu as f64 / 100.0, bg_cpu as f64 / 100.0);
    let mut busy: Vec<(i64, u64)> = Vec::new();
    let (mut demand_t, mut inband, mut under, mut over, mut all_t, mut cpu_sum) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut cpu_band, mut cpu_under, mut cpu_over) = (0u64, 0u64, 0u64);
    let mut last_pct = 0u64;
    let mut waits: HashMap<String, i64> = HashMap::new();
    while now < end_by && sims.iter().any(|s| s.done_at.is_none()) {
        // Run one second.
        let running: Vec<usize> = (0..sims.len())
            .filter(|&i| sims[i].job.state == State::Running && sims[i].done_at.is_none())
            .collect();
        let demands: Vec<u64> = running
            .iter()
            .map(|&i| sims[i].demand(sims[i].progress as u64, centi))
            .collect();
        let total = bg_cpu + demands.iter().sum::<u64>();
        let rate = if total > centi {
            centi as f64 / total as f64
        } else {
            1.0
        };
        let pct = total.min(centi) * 100 / centi;
        let mut per_job = Vec::new();
        for (k, &i) in running.iter().enumerate() {
            let s = &mut sims[i];
            let used = (demands[k] as f64 * rate) as u64;
            let el = u64::try_from(now - s.job.started_ms.unwrap_or(now)).unwrap_or(0);
            s.samples.push((el, used));
            s.progress += rate;
            per_job.push((
                s.job.id.clone(),
                Usage {
                    cpu: used,
                    mem: s.mem_now(),
                    vram: 0,
                },
            ));
            if s.progress >= s.run_s as f64 {
                s.done_at = Some(now);
            }
        }
        load1 += (total as f64 / 100.0 - load1) * (1.0 - (-1.0f64 / 60.0).exp());
        load5 += (total as f64 / 100.0 - load5) * (1.0 - (-1.0f64 / 300.0).exp());
        let mem_used = bg_mem + per_job.iter().map(|(_, u)| u.mem).sum::<u64>();
        let mem_pct = mem_used * 100 / cap.mem;
        // Metrics, as the 6-hourly check counts them.
        let waiting = sims
            .iter()
            .filter(|s| s.job.state == State::Queued && s.job.enqueued_ms <= now)
            .count();
        if (ms(from)..ms(to)).contains(&now) {
            all_t += 1;
            if pct > 95 || mem_pct > 92 || load1 > 3.0 * f64::from(cores) {
                over += 1;
            }
            if pct > 95 || load1 > 3.0 * f64::from(cores) {
                cpu_over += 1;
            }
            if waiting > 0 {
                demand_t += 1;
                cpu_sum += pct;
                if (80..=90).contains(&pct) || (80..=90).contains(&mem_pct) {
                    inband += 1;
                }
                if pct < 75 && mem_pct < 75 {
                    under += 1;
                }
                cpu_band += u64::from((80..=90).contains(&pct));
                cpu_under += u64::from(pct < 75);
            }
        }
        now += 1_000;
        // Admission, with the machine as it read this second.
        busy.push((now, pct));
        busy.retain(|(t, _)| now - t <= BUSY_WINDOW_MS);
        last_pct = pct;
        let live = Live {
            load1,
            load5,
            mem_used_pct: mem_pct,
            cpu_pct: pct,
            cpu_sustained_pct: sustained_pct(pct, &busy, now),
            cpu_recent_peak_pct: recent_peak_pct(&busy, now),
            disk_free: 900 * G,
            non_heavy: Usage {
                cpu: bg_cpu,
                mem: bg_mem,
                vram: 0,
            },
            per_job,
            ..Live::default()
        };
        for s in &mut sims {
            if s.done_at.is_some_and(|d| d < now) && s.job.state == State::Running {
                s.job.state = State::Stopped; // finished: dropped from the job files below
            }
        }
        let mut order: Vec<usize> = (0..sims.len())
            .filter(|&i| sims[i].job.state == State::Queued && sims[i].job.enqueued_ms <= now)
            .collect();
        order.sort_by_key(|&i| sims[i].job.enqueued_ms);
        for i in order {
            let jobs: Vec<Job> = sims
                .iter()
                .filter(|s| {
                    s.job.enqueued_ms <= now
                        && matches!(s.job.state, State::Queued | State::Running)
                        && s.done_at.is_none()
                })
                .map(|s| s.job.clone())
                .collect();
            if evaluate(&sims[i].job, &jobs, &cap, &live, now) == Decision::Start {
                let s = &mut sims[i];
                s.job.state = State::Running;
                s.job.started_ms = Some(now);
                waits.insert(s.job.id.clone(), now - s.job.enqueued_ms);
            }
        }
    }
    let _ = last_pct;
    let n = demand_t.max(1) as f64;
    let mut w: Vec<i64> = waits.values().copied().collect();
    w.sort_unstable();
    let p = |q: f64| {
        w.get(((w.len() as f64 * q) as usize).min(w.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
            / 1_000
    };
    let mean_wait = w.iter().sum::<i64>() as f64 / w.len().max(1) as f64 / 1_000.0;
    let done = sims.iter().filter_map(|s| s.done_at).max().unwrap_or(now);
    println!(
        "replay: demand {:.0}% of the time; in band {:.1}%, underused {:.1}%, overloaded {:.1}% (of all time); demand-time cpu {:.0}%",
        100.0 * demand_t as f64 / all_t.max(1) as f64,
        100.0 * inband as f64 / n,
        100.0 * under as f64 / n,
        100.0 * over as f64 / all_t.max(1) as f64,
        cpu_sum as f64 / n
    );
    println!(
        "replay: cpu only: in band {:.1}%, under 75% {:.1}%, overloaded (cpu > 95% or load1 > 3 x cores) {:.1}%",
        100.0 * cpu_band as f64 / n,
        100.0 * cpu_under as f64 / n,
        100.0 * cpu_over as f64 / all_t.max(1) as f64
    );
    println!(
        "replay: waits mean {mean_wait:.0} s, p50 {} s, p90 {} s, max {} s; {} of {} jobs started; last job done {} min after the window",
        p(0.5),
        p(0.9),
        p(1.0),
        w.len(),
        sims.len(),
        (done - ms(to)) / 60_000
    );
}
