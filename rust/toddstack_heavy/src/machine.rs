//! Probing the machine: capacity, live samples, process liveness, network and VRAM counters.
//!
//! `HEAVY_CAPACITY` (`cpu=8,mem=16G,vram=0,disk=100G`) and `HEAVY_LIVE` (`load=0.5,load5=0.5,mem_pct=10,disk_free=50G,nonheavy_cpu=200,nonheavy_mem=8G,nonheavy_vram=0`)
//! override probed numbers; the tests use them.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nix::sys::statvfs::statvfs;
use sysinfo::{Networks, Pid, ProcessesToUpdate, System};

use crate::error::Error;
use crate::admit::non_heavy;
use crate::job::{Capacity, Job, Live, Usage};
use crate::sizes::parse_size;

pub fn to_u64<T: TryInto<u64>>(x: T) -> u64 {
    x.try_into().unwrap_or(0)
}

fn overrides(var: &str) -> Result<Vec<(String, String)>, Error> {
    let Ok(text) = std::env::var(var) else { return Ok(Vec::new()) };
    text.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| Error::Usage(format!("{var}: expected key=value, got {kv:?}")))
        })
        .collect()
}

fn get<'a>(list: &'a [(String, String)], key: &str) -> Option<&'a str> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Filesystem (total, free) bytes for the filesystem holding `path`.
fn disk(path: &Path) -> Result<(u64, u64), Error> {
    let s = statvfs(path)?;
    let unit = to_u64(s.fragment_size());
    Ok((to_u64(s.blocks()).saturating_mul(unit), to_u64(s.blocks_available()).saturating_mul(unit)))
}

/// Total GPU memory and currently used, in bytes, via `nvidia-smi`; `None` without an NVIDIA GPU.
#[must_use]
pub fn vram() -> Option<(u64, u64)> {
    let out = Command::new("nvidia-smi").args(["--query-gpu=memory.total,memory.used", "--format=csv,noheader,nounits"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut total, mut used) = (0_u64, 0_u64);
    for line in text.lines() {
        let (t, u) = line.split_once(',')?;
        total += t.trim().parse::<u64>().ok()? << 20;
        used += u.trim().parse::<u64>().ok()? << 20;
    }
    (total > 0).then_some((total, used))
}

/// # Errors
/// Fails when the cwd's filesystem cannot be read or an override is malformed.
pub fn capacity(cwd: &Path) -> Result<Capacity, Error> {
    let o = overrides("HEAVY_CAPACITY")?;
    let mut sys = System::new()?;
    sys.refresh_memory();
    let (disk_total, _) = disk(cwd)?;
    let on_mac = cfg!(target_os = "macos");
    let probed_cpu = std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    let cpu = match get(&o, "cpu") {
        Some(v) => v.parse().map_err(|_| Error::Usage(format!("HEAVY_CAPACITY cpu: {v:?}")))?,
        None => probed_cpu,
    };
    let mem = get(&o, "mem").map(parse_size).transpose()?.unwrap_or_else(|| sys.total_memory());
    let vram = match get(&o, "vram") {
        Some(v) => parse_size(v)?,
        None if on_mac => 0,
        None => vram().map_or(0, |(t, _)| t),
    };
    let disk_total = get(&o, "disk").map(parse_size).transpose()?.unwrap_or(disk_total);
    Ok(Capacity { cpu, mem, vram, disk_total, vram_is_ram: on_mac })
}

/// Cumulative CPU ticks from the first line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
    pub steal: u64,
}

/// Parses `cpu  user nice system idle iowait irq softirq steal ...`. Steal is field 8: WSL2 and VMs hide
/// stolen time from the other fields, so it counts as busy.
#[must_use]
pub fn parse_cpu_line(line: &str) -> Option<CpuTimes> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }
    let f: Vec<u64> = parts.map(|p| p.parse().unwrap_or(0)).collect();
    let at = |i: usize| f.get(i).copied().unwrap_or(0);
    if f.len() < 4 {
        return None;
    }
    let idle = at(3) + at(4);
    let steal = at(7);
    let busy = at(0) + at(1) + at(2) + at(5) + at(6) + steal;
    Some(CpuTimes { busy, total: busy + idle, steal })
}

#[must_use]
pub fn busy_pct(a: CpuTimes, b: CpuTimes) -> u64 {
    let total = b.total.saturating_sub(a.total);
    (b.busy.saturating_sub(a.busy) * 100).checked_div(total).unwrap_or(0)
}

/// Reads `/proc/stat` (Linux only).
#[must_use]
pub fn cpu_times() -> Option<CpuTimes> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_line(text.lines().next()?)
}

/// Per-process GPU memory (pid, bytes) from `nvidia-smi`; empty where it is not reported.
fn vram_by_pid() -> Vec<(u32, u64)> {
    let Ok(out) = Command::new("nvidia-smi").args(["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (p, m) = l.split_once(',')?;
            Some((p.trim().parse().ok()?, m.trim().parse::<u64>().ok()? << 20))
        })
        .collect()
}

/// A float percentage or centi-core count as a whole number (no casts).
fn whole(x: f32) -> u64 {
    format!("{:.0}", x.max(0.0)).parse().unwrap_or(0)
}

fn child_map(sys: &System) -> HashMap<Pid, Vec<Pid>> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    children
}

/// Live usage of the process tree rooted at `root` (the one accounting used for running jobs and for the
/// peaks recorded in history).
fn tree_usage(sys: &System, children: &HashMap<Pid, Vec<Pid>>, root: u32, gpu: &[(u32, u64)]) -> Usage {
    let mut total = Usage::default();
    let mut stack = vec![Pid::from_u32(root)];
    while let Some(pid) = stack.pop() {
        if let Some(p) = sys.process(pid) {
            total.cpu += whole(p.cpu_usage());
            total.mem += p.memory();
            total.vram += gpu.iter().filter(|(g, _)| *g == pid.as_u32()).map(|(_, m)| m).sum::<u64>();
        }
        stack.extend(children.get(&pid).into_iter().flatten().copied());
    }
    total
}

/// Keeps one `System` between samples so per-process CPU percentages are valid from the second sample on.
pub struct TreeSampler {
    sys: System,
    root: u32,
}

impl TreeSampler {
    /// # Errors
    /// Fails when the system cannot be probed.
    pub fn new(root: u32) -> Result<Self, Error> {
        Ok(Self { sys: System::new()?, root })
    }

    /// Current CPU (centi-cores) and RSS of the whole process tree.
    pub fn sample(&mut self) -> Usage {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let children = child_map(&self.sys);
        tree_usage(&self.sys, &children, self.root, &[])
    }
}

/// Machine-wide usage and the live usage of each running job's process tree, over a short window.
fn measure(jobs: &[Job], cpu_count: u32, want_vram: bool) -> Result<(Usage, Vec<(String, Usage)>), Error> {
    let mut sys = System::new()?;
    sys.refresh_memory();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.refresh_cpu_usage();
    let before = cpu_times();
    std::thread::sleep(Duration::from_millis(250));
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.refresh_cpu_usage();
    // Linux: /proc/stat, which includes steal time (WSL2 and VMs hide it from the usual fields).
    let pct = match (before, cpu_times()) {
        (Some(a), Some(b)) => busy_pct(a, b),
        _ => whole(sys.global_cpu_usage()).min(100),
    };
    let gpu = if want_vram { vram_by_pid() } else { Vec::new() };
    let machine = Usage {
        cpu: pct * u64::from(cpu_count),
        mem: sys.total_memory().saturating_sub(sys.available_memory()),
        vram: if want_vram { vram().map_or(0, |(_, used)| used) } else { 0 },
    };
    let children = child_map(&sys);
    let per_job = jobs.iter().map(|j| (j.id.clone(), tree_usage(&sys, &children, j.pid, &gpu))).collect();
    Ok((machine, per_job))
}

/// Takes one live sample. `running` are the running jobs whose trees are measured and subtracted.
/// `non_heavy` is raw here; the waiter smooths it.
///
/// # Errors
/// Fails when the filesystem cannot be read or an override is malformed.
pub fn live(cwd: &Path, cap: &Capacity, running: &[Job]) -> Result<Live, Error> {
    let o = overrides("HEAVY_LIVE")?;
    let bad = |k: &str, v: &str| Error::Usage(format!("HEAVY_LIVE {k}: {v:?}"));
    let load1 = match get(&o, "load") {
        Some(v) => v.parse().map_err(|_| bad("load", v))?,
        None => System::load_average()?.one,
    };
    let load5 = match get(&o, "load5") {
        Some(v) => v.parse().map_err(|_| bad("load5", v))?,
        None if get(&o, "load").is_some() => load1,
        None => System::load_average()?.five,
    };
    let mem_used_pct = if let Some(v) = get(&o, "mem_pct") {
        v.parse().map_err(|_| bad("mem_pct", v))?
    } else {
        let mut sys = System::new()?;
        sys.refresh_memory();
        let total = sys.total_memory().max(1);
        total.saturating_sub(sys.available_memory()) * 100 / total
    };
    let disk_free = match get(&o, "disk_free") {
        Some(v) => parse_size(v)?,
        None => disk(cwd)?.1,
    };
    // Tests fake the other programs' usage: nonheavy_cpu (centi-cores), nonheavy_mem, nonheavy_vram.
    let fake = ["nonheavy_cpu", "nonheavy_mem", "nonheavy_vram"].iter().any(|k| get(&o, k).is_some());
    let (non_heavy, per_job) = if fake {
        let num = |k: &str| -> Result<u64, Error> { get(&o, k).map_or(Ok(0), |v| if k == "nonheavy_cpu" { v.parse().map_err(|_| bad(k, v)) } else { parse_size(v) }) };
        (Usage { cpu: num("nonheavy_cpu")?, mem: num("nonheavy_mem")?, vram: num("nonheavy_vram")? }, Vec::new())
    } else {
        let (machine, per_job) = measure(running, cap.cpu, cap.vram > 0)?;
        let trees = per_job.iter().fold(Usage::default(), |a, (_, u)| Usage { cpu: a.cpu + u.cpu, mem: a.mem + u.mem, vram: a.vram + u.vram });
        (non_heavy(machine, trees), per_job)
    };
    Ok(Live { load1, load5, mem_used_pct, disk_free, non_heavy, per_job, stopped: Vec::new() })
}

/// Ids of the jobs whose process is stopped (state T).
#[must_use]
pub fn stopped_ids(jobs: &[Job]) -> Vec<String> {
    let pids: Vec<Pid> = jobs.iter().map(|j| Pid::from_u32(j.pid)).collect();
    let Ok(mut sys) = System::new() else { return Vec::new() };
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    jobs.iter().filter(|j| sys.process(Pid::from_u32(j.pid)).is_some_and(|p| matches!(p.status(), sysinfo::ProcessStatus::Stop))).map(|j| j.id.clone()).collect()
}

/// The start time of a live process (seconds since the epoch), or `None` when it does not exist.
#[must_use]
pub fn process_start(pid: u32) -> Option<u64> {
    let mut sys = System::new().ok()?;
    let p = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).map(sysinfo::Process::start_time)
}

/// Machine-wide network bytes (in, out), loopback excluded.
#[must_use]
pub fn net_bytes() -> (u64, u64) {
    let Ok(nets) = Networks::new_with_refreshed_list() else { return (0, 0) };
    nets.iter().filter(|(name, _)| !name.to_string_lossy().starts_with("lo")).fold((0, 0), |(i, o), (_, d)| (i + d.total_received(), o + d.total_transmitted()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steal_counts_as_busy() {
        let a = parse_cpu_line("cpu  100 0 100 700 100 0 0 0 0 0").unwrap();
        let b = parse_cpu_line("cpu  100 0 100 700 100 0 0 800 0 0").unwrap();
        assert_eq!(a.steal, 0);
        assert_eq!(b.steal, 800);
        assert_eq!(busy_pct(a, b), 100);
        assert!(parse_cpu_line("intr 1 2 3").is_none());
    }

    #[test]
    fn idle_machine_is_not_busy() {
        let a = parse_cpu_line("cpu  10 0 10 100 0 0 0 0").unwrap();
        let b = parse_cpu_line("cpu  10 0 10 200 0 0 0 0").unwrap();
        assert_eq!(busy_pct(a, b), 0);
    }

    #[test]
    fn own_process_start_time_is_found() {
        assert!(process_start(std::process::id()).is_some());
    }
}
