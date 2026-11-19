# toddstack-heavy

`heavy` runs heavy commands (builds, tests, GPU work) one admission at a time per machine. Several agents share one
laptop; each runs `heavy <cmd>` as a background command and is notified when it exits. `heavy` decides by itself
whether to start at once or wait in the queue. Nobody polls or checks a status.

```
heavy [--cpu N] [--mem SIZE] [--vram SIZE] [--disk SIZE] [--timeout DURATION] [--] cmd args...
heavy status        # for humans: running and queued jobs, reservations, live usage
```

The exit code is the child's. A job over its timeout gets SIGTERM to its process group, 10 s of grace, then SIGKILL,
and `heavy` exits 124.

## Weights

Cores, RAM, GPU memory and free disk the job needs. Without flags: the median measured usage from
`~/.local/state/heavy/history.jsonl` for the same command and cwd, else `--cpu 4 --mem 4G`. A request larger than
the machine is clamped, so that job runs alone. On macOS VRAM counts as RAM.

## Admission

All of these must hold:

- Reservations: running reservations plus this request are at most 90% of capacity (cores, RAM, VRAM).
- Headroom: `90% of capacity - non-heavy usage - sum over running jobs of max(reservation, live usage)` fits the
  request. Non-heavy usage is machine-wide use minus the process trees of heavy's own jobs (Chrome, Discord, other
  CLIs), smoothed as the max of the last 4 samples. On Linux CPU includes steal time (`/proc/stat` field 8), which
  WSL2 and VMs otherwise hide.
- Guards (veto only): 1-minute load below the core count, memory used below 90%, free disk on the cwd's filesystem at
  least `--disk` plus 10% of the disk.
- A job running alone always passes the resource checks. There is no cap on the number of jobs.

Queue: first in, first out. A smaller job may backfill past the head only while the head has waited under 60 s.

## State and waiting

`~/.local/state/heavy/jobs/` holds one JSON file per job (pid, process start time, reservation, cmd, cwd, state).
Decisions are made under a `flock`, which the kernel releases if a process dies. Files of dead jobs are dropped.
Waiters watch the directory (kqueue/inotify) and re-evaluate when a job file changes or disappears. Only while live
data blocks a job is there a 10 s re-sample (monitoring); a 30 s safety sweep catches jobs killed with SIGKILL.

If `HEAVY_JOB` names a live running job, `heavy` execs the command directly.

## Stats (stderr)

Queued time (when it matters), run time, CPU user+sys and average cores, peak RSS (largest process), disk blocks,
machine-wide network bytes, peak VRAM and CPU steal where measurable, a TIMED OUT line when it applies, and a
prompt to review the numbers. One JSON line per run is appended to `history.jsonl`.

## Configuration

`HEAVY_STATE_DIR`, `HEAVY_TIMEOUT` (default 60m). Test overrides: `HEAVY_CAPACITY`
(`cpu=8,mem=16G,vram=0,disk=100G`), `HEAVY_LIVE` (`load=,mem_pct=,disk_free=,nonheavy_cpu=,nonheavy_mem=,nonheavy_vram=`),
`HEAVY_KILL_GRACE_MS`.
