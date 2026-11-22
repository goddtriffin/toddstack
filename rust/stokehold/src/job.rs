//! The shapes shared by every module: weights, the machine, one job file.

use serde::{Deserialize, Serialize};

/// Marker in `start_time` when the process start could not be probed: liveness then checks the pid only.
pub const START_UNKNOWN: u64 = u64::MAX;

/// What a job reserves. Disk is free space it needs left over, not a sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Weights {
    pub cpu: u32,
    pub mem: u64,
    pub vram: u64,
    pub disk: u64,
}

/// What the machine has. `disk_total` is the size of the filesystem holding the cwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub cpu: u32,
    pub mem: u64,
    pub vram: u64,
    pub disk_total: u64,
    /// macOS: GPU memory is system memory, so a VRAM request is folded into RAM.
    pub vram_is_ram: bool,
}

impl Capacity {
    /// Folds VRAM into RAM where they are the same memory, then clamps every number to the machine.
    #[must_use]
    pub fn clamp(&self, w: Weights) -> Weights {
        let (mem, vram) = if self.vram_is_ram {
            (w.mem.saturating_add(w.vram), 0)
        } else {
            (w.mem, w.vram)
        };
        Weights {
            cpu: w.cpu.min(self.cpu),
            mem: mem.min(self.mem),
            vram: vram.min(self.vram),
            disk: w.disk.min(self.disk_total),
        }
    }
}

/// Live usage: CPU in hundredths of a core (100 = one busy core), RAM and VRAM in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub cpu: u64,
    pub mem: u64,
    pub vram: u64,
}

impl Usage {
    /// Per-dimension maximum.
    #[must_use]
    pub fn max(self, o: Self) -> Self {
        Self {
            cpu: self.cpu.max(o.cpu),
            mem: self.mem.max(o.mem),
            vram: self.vram.max(o.vram),
        }
    }

    #[must_use]
    pub fn saturating_sub(self, o: Self) -> Self {
        Self {
            cpu: self.cpu.saturating_sub(o.cpu),
            mem: self.mem.saturating_sub(o.mem),
            vram: self.vram.saturating_sub(o.vram),
        }
    }
}

/// A live sample. Reservations decide admission; live data only tightens it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Live {
    pub load1: f64,
    pub load5: f64,
    pub mem_used_pct: u64,
    /// Machine-wide CPU busy, 0 to 100, over this sample's short window.
    pub cpu_pct: u64,
    /// The machine's sustained CPU busy, 0 to 100: max(latest, mean of every heavy process's readings in the last
    /// `admit::BUSY_WINDOW_MS`). 0 when unknown (then only the latest sample counts).
    pub cpu_sustained_pct: u64,
    /// The system reports critical memory pressure (macOS level 4, Linux PSI); false when it cannot be read.
    pub mem_pressure: bool,
    /// macOS level 2 ("warn"): common and sticky, so it blocks only with memory nearly used up (`admit::guard_block`).
    pub mem_pressure_warn: bool,
    pub disk_free: u64,
    /// Machine-wide usage minus the process trees of running heavy jobs (smoothed by the caller).
    pub non_heavy: Usage,
    /// Live usage of each running job's process tree, by job id.
    pub per_job: Vec<(String, Usage)>,
    /// Ids of jobs whose process is stopped (Ctrl-Z): they cannot start, so they never block the queue.
    pub stopped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Running,
    /// A waiter stopped with Ctrl-Z: it cannot start, so it never blocks the queue. Written before it stops.
    Stopped,
}

/// One JSON file per job in the state dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub pid: u32,
    /// Process start time (seconds since the epoch): defeats pid reuse.
    pub start_time: u64,
    pub weights: Weights,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub state: State,
    pub enqueued_ms: i64,
    pub started_ms: Option<i64>,
    /// The check that blocks this waiter right now (as the waiter itself last decided, with its smoothed data).
    /// The sampler reads it instead of re-evaluating.
    #[serde(default)]
    pub blocked_by: Option<String>,
    /// The memory size came from `--mem`: its unused part is held for the whole run (a human knows this size).
    #[serde(default)]
    pub mem_declared: bool,
    /// A learned size only: milliseconds after the start at which past runs peaked in memory (history median).
    /// `None` when unknown (cold, or history from before 0.4.2).
    #[serde(default)]
    pub mem_peak_at_ms: Option<i64>,
    /// A learned or declared CPU size: how long (ms after the start) its peak cores are held while they have not shown
    /// up in the job's live use (`admit::cpu_hold_ms`). `None` only for job files from before 0.4.3 (cold jobs hold the whole machine for the fallback time): the
    /// short `admit::RAMP_MS` ramp hold.
    #[serde(default)]
    pub cpu_hold_ms: Option<i64>,
    /// Set by the running job itself once its CPU has plateaued past its learned slowest time to peak (`admit::cpu_plateaued`):
    /// it has shown its real use, so its CPU hold is released early.
    #[serde(default)]
    pub cpu_settled: bool,
    /// Centi-cores of other programs' CPU (not heavy jobs) when this job started: a Docker/VM job's hold is netted only
    /// against the rise above it, so unrelated apps and always-on containers do not cancel the hold.
    #[serde(default)]
    pub non_heavy_cpu_at_start: u64,
    /// Learned short and small (`admit::fast_lane_eligible`): it may start in the bounded fast lane past CPU room and
    /// the queue (`admit::fast_lane`).
    #[serde(default)]
    pub fast: bool,
}
