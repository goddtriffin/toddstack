//! Probing the machine: capacity, live samples, process liveness, network and VRAM counters.
//!
//! `STOKEHOLD_CAPACITY` (`cpu=8,mem=16G,vram=0,disk=100G`) and `STOKEHOLD_LIVE` (`load=0.5,load5=0.5,mem_pct=10,disk_free=50G,nonheavy_cpu=200,nonheavy_mem=8G,nonheavy_vram=0`)
//! override probed numbers; the tests use them (`mem_pressure=2` fakes warn, `mem_pressure=4` critical pressure; with any override set, the real
//! pressure is not read).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nix::sys::statvfs::statvfs;
use sysinfo::{Networks, Pid, ProcessesToUpdate, System};

use crate::admit::non_heavy;
use crate::error::Error;
use crate::job::{Capacity, Job, Live, Usage};
use crate::sizes::parse_size;

pub fn to_u64<T: TryInto<u64>>(x: T) -> u64 {
    x.try_into().unwrap_or(0)
}

fn overrides(var: &str) -> Result<Vec<(String, String)>, Error> {
    let Some(text) = crate::state::env_var(var).and_then(|v| v.into_string().ok()) else {
        return Ok(Vec::new());
    };
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
    Ok((
        to_u64(s.blocks()).saturating_mul(unit),
        to_u64(s.blocks_available()).saturating_mul(unit),
    ))
}

/// Total GPU memory and currently used, in bytes, via `nvidia-smi`; `None` without an NVIDIA GPU.
#[must_use]
pub fn vram() -> Option<(u64, u64)> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
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
    let o = overrides("STOKEHOLD_CAPACITY")?;
    let mut sys = System::new()?;
    sys.refresh_memory();
    let (disk_total, _) = disk(cwd)?;
    let on_mac = cfg!(target_os = "macos");
    let probed_cpu = std::thread::available_parallelism()
        .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    let cpu = match get(&o, "cpu") {
        Some(v) => v
            .parse()
            .map_err(|_| Error::Usage(format!("STOKEHOLD_CAPACITY cpu: {v:?}")))?,
        None => probed_cpu,
    };
    let mem = get(&o, "mem")
        .map(parse_size)
        .transpose()?
        .unwrap_or_else(|| sys.total_memory());
    let vram = match get(&o, "vram") {
        Some(v) => parse_size(v)?,
        None if on_mac => 0,
        None => vram().map_or(0, |(t, _)| t),
    };
    let disk_total = get(&o, "disk")
        .map(parse_size)
        .transpose()?
        .unwrap_or(disk_total);
    Ok(Capacity {
        cpu,
        mem,
        vram,
        disk_total,
        vram_is_ram: on_mac,
    })
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
    Some(CpuTimes {
        busy,
        total: busy + idle,
        steal,
    })
}

#[must_use]
pub fn busy_pct(a: CpuTimes, b: CpuTimes) -> u64 {
    let total = b.total.saturating_sub(a.total);
    (b.busy.saturating_sub(a.busy) * 100)
        .checked_div(total)
        .unwrap_or(0)
}

/// Reads `/proc/stat` (Linux only).
#[must_use]
pub fn cpu_times() -> Option<CpuTimes> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_line(text.lines().next()?)
}

/// Per-process GPU memory (pid, bytes) from `nvidia-smi`; empty where it is not reported.
fn vram_by_pid() -> Vec<(u32, u64)> {
    let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
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
    children_of(
        sys.processes()
            .iter()
            .map(|(pid, p)| (*pid, p.parent(), p.thread_kind().is_some())),
    )
}

/// Parent to child processes, threads left out. On Linux sysinfo also lists every thread (task) as a "process" whose
/// parent is its own process, and a thread reports its whole process's RSS and CPU: summed as children, a rustc with 16
/// threads counted 17 times its memory (a 46 GB "peak" for a `cargo install` on the rig while the machine used 3 GB).
/// The process's own entry already covers all its threads.
fn children_of(procs: impl Iterator<Item = (Pid, Option<Pid>, bool)>) -> HashMap<Pid, Vec<Pid>> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, parent, thread) in procs {
        if let Some(parent) = parent.filter(|_| !thread) {
            children.entry(parent).or_default().push(pid);
        }
    }
    children
}

/// Live usage of the process tree rooted at `root` (the one accounting used for running jobs and for the
/// peaks recorded in history).
fn tree_usage(
    sys: &System,
    children: &HashMap<Pid, Vec<Pid>>,
    root: u32,
    gpu: &[(u32, u64)],
) -> Usage {
    let mut total = Usage::default();
    for pid in tree_pids(children, Pid::from_u32(root)) {
        if let Some(p) = sys.process(pid) {
            total.cpu += whole(p.cpu_usage());
            total.mem += p.memory();
            total.vram += gpu
                .iter()
                .filter(|(g, _)| *g == pid.as_u32())
                .map(|(_, m)| m)
                .sum::<u64>();
        }
    }
    total
}

/// Memory the machine really uses: total minus available (page cache and shared file mappings count as available).
fn machine_used(sys: &System) -> u64 {
    sys.total_memory().saturating_sub(sys.available_memory())
}

/// A tree's memory is the sum of its processes' RSS, which counts shared pages once per process: file-backed pages
/// (rustc and clippy map the same crate metadata in every process), shared libraries and copy-on-write pages. On the
/// rig a `make lint` learned a 17-19 GB peak while the machine never used more than 8 GB of its 32 GB (2026-11-25),
/// and that reservation held waiters on "mem" for 4m37s at 9-22% RAM used; `cargo install` did the same on
/// 2026-11-20. Nothing the tree uses can exceed what the whole machine uses, so the sum is capped there.
#[must_use]
pub fn cap_tree_mem(tree_rss: u64, machine_used: u64) -> u64 {
    tree_rss.min(machine_used)
}

/// `cap_tree_mem` for several trees at once: when their RSS together exceeds what the machine uses, each is scaled
/// down in proportion so that they add up to it.
#[must_use]
pub fn cap_trees_mem(trees: &[u64], machine_used: u64) -> Vec<u64> {
    let sum: u128 = trees.iter().map(|t| u128::from(*t)).sum();
    if sum <= u128::from(machine_used) {
        return trees.to_vec();
    }
    trees
        .iter()
        .map(|t| u64::try_from(u128::from(*t) * u128::from(machine_used) / sum).unwrap_or(0))
        .collect()
}

/// The root and every descendant process (what `tree_usage` sums).
fn tree_pids(children: &HashMap<Pid, Vec<Pid>>, root: Pid) -> Vec<Pid> {
    let mut seen = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        seen.push(pid);
        stack.extend(children.get(&pid).into_iter().flatten().copied());
    }
    seen
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
        Ok(Self {
            sys: System::new()?,
            root,
        })
    }

    /// Current CPU (centi-cores) and RSS of the whole process tree, the RSS at most the memory the machine uses.
    pub fn sample(&mut self) -> Usage {
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let children = child_map(&self.sys);
        let mut u = tree_usage(&self.sys, &children, self.root, &[]);
        u.mem = cap_tree_mem(u.mem, machine_used(&self.sys));
        u
    }
}

/// macOS: `kern.memorystatus_vm_pressure_level` is 1 normal, 2 warn, 4 critical. Returns `(warn, critical)`. Level 2 is
/// common and sticky (2026-11-21: level 2 for hours at 49% memory used and 1.8 of 3 GB swap), so only level 4 is
/// pressure on its own; `admit::guard_block` counts warn only with memory nearly used up.
#[must_use]
pub fn mac_pressure(sysctl_out: &str) -> (bool, bool) {
    sysctl_out
        .trim()
        .parse::<u32>()
        .map_or((false, false), |l| (l >= 2, l >= 4))
}

/// Linux PSI (`/proc/pressure/memory`): tasks stalled on memory. Pressure is `some avg10` of 25 or more, or
/// `full avg10` of 5 or more.
#[must_use]
pub fn psi_pressure(text: &str) -> bool {
    let avg10 = |line: &str| {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    text.lines().any(|l| {
        (l.starts_with("some ") && avg10(l) >= 25.0) || (l.starts_with("full ") && avg10(l) >= 5.0)
    })
}

/// Linux PSI cpu (`/proc/pressure/cpu`): the `some avg10` %, rounded (the share of the last 10 s some task waited for a
/// CPU). `None` when the line is missing or malformed.
#[must_use]
pub fn psi_cpu_some(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|w| w.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a finite, non-negative percentage"
            )]
            let pct = v.round() as u64;
            pct
        })
}

/// Linux PSI cpu `some avg10` (`psi_cpu_some`); `None` on macOS and when it cannot be read.
fn cpu_pressure() -> Option<u64> {
    if cfg!(target_os = "macos") {
        return None;
    }
    std::fs::read_to_string("/proc/pressure/cpu")
        .ok()
        .and_then(|t| psi_cpu_some(&t))
}

/// Whether the system is under memory pressure, as `(warn, critical)`. Cheap, and fails open (false) when it cannot be
/// read. Linux PSI counts stalled time, a real signal: it is critical.
fn memory_pressure() -> (bool, bool) {
    if cfg!(target_os = "macos") {
        return Command::new("sysctl")
            .args(["-n", "kern.memorystatus_vm_pressure_level"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map_or((false, false), |o| {
                mac_pressure(&String::from_utf8_lossy(&o.stdout))
            });
    }
    let p = std::fs::read_to_string("/proc/pressure/memory").is_ok_and(|t| psi_pressure(&t));
    (p, p)
}

/// Samples what the other programs (not heavy's own job trees) use: machine CPU and memory minus every heavy job's
/// tree. Work hosted outside the process tree (Docker's or WSL's VM) shows up here, so a job's peak can be learned from
/// the machine's CPU and memory rise over a baseline.
pub struct OtherSampler {
    sys: System,
    cores: u64,
}

impl OtherSampler {
    /// # Errors
    /// Fails when the system cannot be probed.
    pub fn new(cores: u32) -> Result<Self, Error> {
        Ok(Self {
            sys: System::new()?,
            cores: u64::from(cores),
        })
    }

    /// CPU (centi-cores) and memory (bytes) of the machine minus the process trees rooted at `roots`.
    pub fn sample(&mut self, roots: &[u32]) -> Usage {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let machine = Usage {
            cpu: whole(self.sys.global_cpu_usage()) * self.cores,
            mem: self
                .sys
                .total_memory()
                .saturating_sub(self.sys.available_memory()),
            vram: 0,
        };
        let children = child_map(&self.sys);
        let trees = roots.iter().fold(Usage::default(), |a, r| {
            let t = tree_usage(&self.sys, &children, *r, &[]);
            Usage {
                cpu: a.cpu + t.cpu,
                mem: a.mem + t.mem,
                vram: 0,
            }
        });
        machine.saturating_sub(trees)
    }
}

/// A job's memory peak for the history: the process tree's peak, or the machine-wide rise over the baseline when
/// that is larger (Docker's or WSL's VM memory is outside the tree), capped at the machine's memory.
#[must_use]
pub fn effective_mem_peak(tree_peak: u64, other_rise: u64, machine_mem: u64) -> u64 {
    tree_peak.max(other_rise).min(machine_mem)
}

/// A job's peak for the history: the process tree's peak, or the machine-wide rise over the baseline when that is
/// larger (work in a VM, such as Docker builds, is outside the tree), capped at the whole machine.
#[must_use]
pub fn effective_peak(tree_peak: u64, other_peak: u64, baseline: u64, cores: u32) -> u64 {
    tree_peak
        .max(other_peak.saturating_sub(baseline))
        .min(u64::from(cores) * 100)
}

/// Machine-wide usage and the live usage of each running job's process tree, over a short window.
fn measure(
    jobs: &[Job],
    cpu_count: u32,
    want_vram: bool,
) -> Result<(Usage, Vec<(String, Usage)>), Error> {
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
        vram: if want_vram {
            vram().map_or(0, |(_, used)| used)
        } else {
            0
        },
    };
    let children = child_map(&sys);
    let mut per_job: Vec<(String, Usage)> = jobs
        .iter()
        .map(|j| (j.id.clone(), tree_usage(&sys, &children, j.pid, &gpu)))
        .collect();
    let capped = cap_trees_mem(
        &per_job.iter().map(|(_, u)| u.mem).collect::<Vec<_>>(),
        machine.mem,
    );
    for ((_, u), mem) in per_job.iter_mut().zip(capped) {
        u.mem = mem;
    }
    Ok((machine, per_job))
}

/// Takes one live sample. `running` are the running jobs whose trees are measured and subtracted.
/// `non_heavy` is raw here; the waiter smooths it.
///
/// # Errors
/// Fails when the filesystem cannot be read or an override is malformed.
pub fn live(cwd: &Path, cap: &Capacity, running: &[Job]) -> Result<Live, Error> {
    let o = overrides("STOKEHOLD_LIVE")?;
    let bad = |k: &str, v: &str| Error::Usage(format!("STOKEHOLD_LIVE {k}: {v:?}"));
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
    let fake = ["nonheavy_cpu", "nonheavy_mem", "nonheavy_vram"]
        .iter()
        .any(|k| get(&o, k).is_some());
    // The override is a macOS level: 2 fakes warn, 4 critical.
    let (mem_pressure_warn, mem_pressure) = match get(&o, "mem_pressure") {
        Some(v) => {
            let l = v.parse::<u32>().map_err(|_| bad("mem_pressure", v))?;
            (l >= 2, l >= 4)
        }
        None if o.is_empty() => memory_pressure(),
        None => (false, false),
    };
    // The override is the PSI cpu `some avg10` %.
    let cpu_pressure = match get(&o, "cpu_psi") {
        Some(v) => Some(v.parse::<u64>().map_err(|_| bad("cpu_psi", v))?),
        None if o.is_empty() => cpu_pressure(),
        None => None,
    };
    let (non_heavy, per_job, cpu_pct) = if fake {
        let num = |k: &str| -> Result<u64, Error> {
            get(&o, k).map_or(Ok(0), |v| {
                if k == "nonheavy_cpu" {
                    v.parse().map_err(|_| bad(k, v))
                } else {
                    parse_size(v)
                }
            })
        };
        let nh = Usage {
            cpu: num("nonheavy_cpu")?,
            mem: num("nonheavy_mem")?,
            vram: num("nonheavy_vram")?,
        };
        (
            nh,
            Vec::new(),
            (nh.cpu / u64::from(cap.cpu.max(1))).min(100),
        )
    } else {
        let (machine, per_job) = measure(running, cap.cpu, cap.vram > 0)?;
        let trees = per_job.iter().fold(Usage::default(), |a, (_, u)| Usage {
            cpu: a.cpu + u.cpu,
            mem: a.mem + u.mem,
            vram: a.vram + u.vram,
        });
        (
            non_heavy(machine, trees),
            per_job,
            (machine.cpu / u64::from(cap.cpu.max(1))).min(100),
        )
    };
    Ok(Live {
        load1,
        load5,
        mem_used_pct,
        cpu_pct,
        cpu_sustained_pct: 0,
        cpu_recent_peak_pct: 0,
        mem_pressure,
        mem_pressure_warn,
        cpu_pressure,
        disk_free,
        non_heavy,
        per_job,
        stopped: Vec::new(),
    })
}

/// Ids of the jobs whose process is stopped (state T).
#[must_use]
pub fn stopped_ids(jobs: &[Job]) -> Vec<String> {
    let pids: Vec<Pid> = jobs.iter().map(|j| Pid::from_u32(j.pid)).collect();
    let Ok(mut sys) = System::new() else {
        return Vec::new();
    };
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    jobs.iter()
        .filter(|j| {
            sys.process(Pid::from_u32(j.pid))
                .is_some_and(|p| matches!(p.status(), sysinfo::ProcessStatus::Stop))
        })
        .map(|j| j.id.clone())
        .collect()
}

/// The start time of a live process (seconds since the epoch), or `None` when it does not exist.
#[must_use]
pub fn process_start(pid: u32) -> Option<u64> {
    process_probe(pid).map(|p| p.start)
}

/// What the process table says about one pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessProbe {
    /// Start time, seconds since the epoch.
    pub start: u64,
    /// The process has exited and waits for its parent to reap it.
    pub zombie: bool,
}

/// The start time and zombie state of `pid`, or `None` when the process table has no such process (or cannot be read).
#[must_use]
pub fn process_probe(pid: u32) -> Option<ProcessProbe> {
    let mut sys = System::new().ok()?;
    let p = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).map(|proc| ProcessProbe {
        start: proc.start_time(),
        zombie: proc.status() == sysinfo::ProcessStatus::Zombie,
    })
}

/// Machine-wide network bytes (in, out), loopback excluded.
#[must_use]
pub fn net_bytes() -> (u64, u64) {
    let Ok(nets) = Networks::new_with_refreshed_list() else {
        return (0, 0);
    };
    nets.iter()
        .filter(|(name, _)| !name.to_string_lossy().starts_with("lo"))
        .fold((0, 0), |(i, o), (_, d)| {
            (i + d.total_received(), o + d.total_transmitted())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_pressure_levels() {
        assert_eq!(mac_pressure("1\n"), (false, false));
        assert_eq!(
            mac_pressure("2\n"),
            (true, false),
            "warn is not pressure on its own"
        );
        assert_eq!(mac_pressure("4"), (true, true));
        assert_eq!(mac_pressure("garbage"), (false, false), "fails open");
        assert!(!psi_pressure(
            "some avg10=3.00 avg60=1.00 avg300=0.50 total=1\nfull avg10=0.50 avg60=0.0 avg300=0.0 total=0\n"
        ));
        assert!(psi_pressure(
            "some avg10=30.00 avg60=1.00 avg300=0.50 total=1\nfull avg10=0.50 avg60=0.0 avg300=0.0 total=0\n"
        ));
        assert!(psi_pressure(
            "some avg10=3.00 avg60=1.00 avg300=0.50 total=1\nfull avg10=6.00 avg60=0.0 avg300=0.0 total=0\n"
        ));
        assert!(!psi_pressure(""));
    }

    #[test]
    fn cpu_psi_some_avg10() {
        // The rig on 2026-11-21: load 10-18 on 16 cores, CPU about 8%.
        let rig = "some avg10=0.19 avg60=2.40 avg300=13.20 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(psi_cpu_some(rig), Some(0));
        assert_eq!(
            psi_cpu_some("some avg10=37.62 avg60=20.00 avg300=5.00 total=9"),
            Some(38)
        );
        assert_eq!(psi_cpu_some(""), None, "unreadable is unknown, not zero");
        assert_eq!(psi_cpu_some("some avg10=nan"), None);
    }

    #[test]
    fn a_vm_hosted_job_learns_the_machine_rise() {
        // The tree (docker client) peaked at 1 core, the machine rose 9 cores over the baseline: the peak is 9.
        assert_eq!(effective_peak(100, 1100, 200, 16), 900);
        // A native job: the tree peak wins.
        assert_eq!(effective_peak(800, 300, 200, 16), 800);
        // Never below the tree, never above the machine, and a lower-than-baseline machine is no rise.
        assert_eq!(effective_peak(100, 50, 200, 16), 100);
        assert_eq!(effective_peak(100, 5000, 0, 16), 1600);
    }

    #[test]
    fn a_vm_hosted_job_learns_the_machine_memory_rise() {
        const G: u64 = 1 << 30;
        // The docker client's tree peaked at 100M, the machine's memory rose 9G over the baseline: the peak is 9G.
        assert_eq!(effective_mem_peak(100 << 20, 9 * G, 64 * G), 9 * G);
        // A native job: the tree peak wins. Never above the machine.
        assert_eq!(effective_mem_peak(8 * G, 3 * G, 64 * G), 8 * G);
        assert_eq!(effective_mem_peak(G, 99 * G, 64 * G), 64 * G);
    }

    #[test]
    fn shared_pages_do_not_count_once_per_process() {
        const G: u64 = 1 << 30;
        // 2026-11-25 on the rig: a lint tree summed 18.7 GB of RSS while the whole machine used 7 GB of 32.
        assert_eq!(cap_tree_mem(18 * G, 7 * G), 7 * G);
        // An honest tree is untouched.
        assert_eq!(cap_tree_mem(3 * G, 7 * G), 3 * G);
        // Several trees that together claim more than the machine uses are scaled to what it uses.
        let capped = cap_trees_mem(&[12 * G, 6 * G], 9 * G);
        assert_eq!(capped, vec![6 * G, 3 * G]);
        assert_eq!(cap_trees_mem(&[G, 2 * G], 9 * G), vec![G, 2 * G]);
        assert_eq!(cap_trees_mem(&[], 9 * G), Vec::<u64>::new());
        assert_eq!(cap_trees_mem(&[G], 0), vec![0]);
    }

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
    fn threads_are_not_counted_as_child_processes() {
        // 2026-11-20 on the rig: `cargo install` learned a 46 GB tree peak (largest process 2.6 GB) while the machine
        // used 3 GB, so its reservation blocked every other job on "mem" at 6-8% memory used. A rustc (pid 10) with 16
        // threads (pids 11-26, each reporting the process's 2.6 GB) and one child process (pid 30, 0.5 GB).
        let p = Pid::from_u32;
        let threads = (11..27).map(|t| (p(t), Some(p(10)), true));
        let procs = std::iter::once((p(10), Some(p(1)), false))
            .chain(threads)
            .chain(std::iter::once((p(30), Some(p(10)), false)));
        let children = children_of(procs);
        assert_eq!(
            children.get(&p(10)),
            Some(&vec![p(30)]),
            "threads are not children"
        );
        let rss = |pid: Pid| if pid == p(30) { 512 } else { 2_600 };
        let total: u64 = tree_pids(&children, p(10)).into_iter().map(rss).sum();
        assert_eq!(
            total, 3_112,
            "MB: the process plus its child, not 17 x 2.6 GB"
        );
    }

    #[test]
    fn own_process_start_time_is_found() {
        assert!(process_start(std::process::id()).is_some());
    }
}
