//! `stokehold status`: for humans only. Agents never need it; `heavy` decides by itself.

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
    let jobs = {
        let _lock = state.lock()?;
        state.live_jobs()
    };
    let running: Vec<_> = jobs.iter().filter(|j| j.state == State::Running).cloned().collect();
    let live = machine::live(&cwd, &cap, &running)?;
    println!("capacity   {} cores, {} ram, {} vram, {} disk here", cap.cpu, fmt_size(cap.mem), fmt_size(cap.vram), fmt_size(cap.disk_total));
    println!(
        "live       load {:.1}, memory {}% used, {} disk free, other programs use {}.{:02} cores / {} ram / {} vram",
        live.load1,
        live.mem_used_pct,
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
    println!("{:<8} {:>7} {:>5} {:>8} {:>8} {:>8}  command (live use)", "state", "pid", "cpu", "mem", "vram", "disk");
    for j in &jobs {
        let u = live.per_job.iter().find(|(id, _)| *id == j.id).map(|(_, u)| *u);
        let state = match j.state {
            State::Running => "running",
            State::Stopped => "stopped",
            State::Queued => "queued",
        };
        let used = u.map_or(String::new(), |u| format!(" ({}.{:02} cores, {})", u.cpu / 100, u.cpu % 100, fmt_size(u.mem)));
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
    }
    Ok(())
}
