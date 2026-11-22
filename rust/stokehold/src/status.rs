//! `stokehold status`: for humans only. Agents never need it; `heavy` decides by itself.

use crate::admit::{self, Decision};
use crate::error::Error;
use crate::job::State;
use crate::machine;
use crate::sizes::fmt_size;
use crate::state::StateDir;

/// # Errors
/// Fails when the machine cannot be probed.
pub fn print(state: &StateDir) -> Result<(), Error> {
    state.ensure()?;
    let cwd = std::env::current_dir()?;
    let cap = machine::capacity(&cwd)?;
    let (jobs, busy) = {
        let _lock = state.lock()?;
        (state.live_jobs(), state.busy_readings())
    };
    let running: Vec<_> = jobs
        .iter()
        .filter(|j| j.state == State::Running)
        .cloned()
        .collect();
    let mut live = machine::live(&cwd, &cap, &running)?;
    live.stopped = machine::stopped_ids(&jobs);
    let now = crate::run::now_ms();
    live.cpu_sustained_pct = admit::sustained_pct(live.cpu_pct, &busy, now);
    live.cpu_recent_peak_pct = admit::recent_peak_pct(&busy, now);
    println!(
        "capacity   {} cores, {} ram, {} vram, {} disk here",
        cap.cpu,
        fmt_size(cap.mem),
        fmt_size(cap.vram),
        fmt_size(cap.disk_total)
    );
    println!(
        "live       cpu {}% busy ({}% sustained), load {:.1}, memory {}% used{}, {} disk free, other programs use {}.{:02} cores / {} ram / {} vram",
        live.cpu_pct,
        live.cpu_sustained_pct,
        live.load1,
        live.mem_used_pct,
        if live.mem_pressure {
            ", critical memory pressure"
        } else if live.mem_pressure_warn {
            ", memory pressure warn"
        } else {
            ""
        },
        fmt_size(live.disk_free),
        live.non_heavy.cpu / 100,
        live.non_heavy.cpu % 100,
        fmt_size(live.non_heavy.mem),
        fmt_size(live.non_heavy.vram)
    );
    if jobs.is_empty() {
        println!("no jobs");
        return Ok(());
    }
    println!(
        "{:<8} {:>7} {:>5} {:>8} {:>8} {:>8}  command (live use)",
        "state", "pid", "cpu", "mem", "vram", "disk"
    );
    for j in &jobs {
        let u = live
            .per_job
            .iter()
            .find(|(id, _)| *id == j.id)
            .map(|(_, u)| *u);
        let state = match j.state {
            State::Running => "running",
            State::Stopped => "stopped",
            State::Queued => "queued",
        };
        let used = u.map_or(String::new(), |u| {
            format!(
                " ({}.{:02} cores, {})",
                u.cpu / 100,
                u.cpu % 100,
                fmt_size(u.mem)
            )
        });
        println!(
            "{:<8} {:>7} {:>5} {:>8} {:>8} {:>8}  {}{used}",
            state,
            j.pid,
            j.weights.cpu,
            fmt_size(j.weights.mem),
            fmt_size(j.weights.vram),
            fmt_size(j.weights.disk),
            j.cmd.join(" ")
        );
        // Why a waiter waits, judged now by the same rules it uses, with the numbers.
        if j.state == State::Queued {
            let why = why_waiting(&admit::evaluate(j, &jobs, &cap, &live, now));
            println!("{:<8} blocked: {why}", "");
        }
    }
    Ok(())
}

/// The `blocked:` text for a waiter's decision.
fn why_waiting(d: &Decision) -> String {
    match d {
        Decision::Start => "fits now (starts at its next look)".to_string(),
        Decision::Fail(why) => why.clone(),
        Decision::Wait { reason, block, .. } => format!("{}: {reason}", block.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::Block;

    #[test]
    fn why_waiting_names_the_block_and_the_numbers() {
        assert_eq!(
            why_waiting(&Decision::Wait {
                reason: "mem: needs 30 GB, 5 GB free".into(),
                live: true,
                block: Block::Mem,
                ramp_end_ms: None,
            }),
            "mem: mem: needs 30 GB, 5 GB free"
        );
        assert_eq!(
            why_waiting(&Decision::Start),
            "fits now (starts at its next look)"
        );
        assert_eq!(
            why_waiting(&Decision::Fail("disk full".into())),
            "disk full"
        );
    }
}
