# toddstack-heavy

`heavy` runs heavy commands (builds, tests, GPU work) one admission at a time per machine. Several agents share one
laptop; each runs `heavy <cmd>` as a background command and is notified when it exits. `heavy` decides by itself
whether to start at once or wait in the queue. Nobody polls or checks a status.

```
heavy [--cpu N] [--mem SIZE] [--vram SIZE] [--disk SIZE] [--timeout DURATION] [--] cmd args...
heavy status        # for humans: running and queued jobs, reservations, live usage (`--status` works too)
heavy history [--since 2h|1d|7d] [--html PATH]   # how well heavy used the machine (default: 1d)
```

The exit code is the child's. Exit 75: the disk is full. A job over its timeout gets SIGTERM to its process group, 10 s of grace, then SIGKILL,
and `heavy` exits 124. When the child exits (or is killed), `heavy` sends SIGKILL to its whole process group, so
nothing it left behind keeps running or holds a pipe open.

Terminal: when stdin is a terminal and `heavy` is in the foreground, the child's group gets the terminal while it
runs (for prompts and pagers) and `heavy` takes it back afterwards. If the child stops (Ctrl-Z), `heavy` stops too;
`fg` resumes both.

## Weights

Cores, RAM, GPU memory and free disk the job needs. Without flags: the median measured peak (whole process tree,
sampled every 0.5 s while the job runs: peak cores at once and peak RSS) from
`~/.local/state/heavy/history.jsonl` for the same command and cwd, else `--cpu 4 --mem 4G`. A request larger than
the machine is clamped, so that job runs alone. On macOS VRAM counts as RAM.

## Admission

All of these must hold:

- Reservations: running reservations plus this request are at most 90% of capacity (cores, RAM, VRAM).
- Headroom: `90% of capacity - non-heavy usage - sum over running jobs of max(reservation, live usage)` fits the
  request. Non-heavy usage is machine-wide use minus the process trees of heavy's own jobs (Chrome, Discord, other
  CLIs), smoothed as the max of the last 4 samples. On Linux CPU includes steal time (`/proc/stat` field 8), which
  WSL2 and VMs otherwise hide.
- Guards (veto only): 1-minute and 5-minute load each below 2 x the core count (macOS load reads high), memory used
  below 90%.
- Disk: free space on the cwd's filesystem must be at least `--disk` plus 10% of the disk. If it is not, waiting
  cannot help, so `heavy` exits 75 at once and says the disk is full (clean up, or ask Todd).
- A job running alone always passes the resource checks. There is no cap on the number of jobs.

Queue: first in, first out. A job behind the head starts when the head and it fit together (it never stops the
head from fitting). When the head does not fit now, a job no bigger than the head may backfill past it while the
head has waited under 60 s.

## State and waiting

`~/.local/state/heavy/jobs/` holds one JSON file per job (pid, process start time, reservation, cmd, cwd, state).
Decisions are made under a `flock`, which the kernel releases if a process dies. Files of dead jobs are dropped.
Waiters watch the directory (kqueue/inotify) and re-evaluate when a job file changes or disappears. Only while live
data blocks a job is there a 10 s re-sample (monitoring; a waiter held back by a ramp sleeps until the ramp ends instead); a 30 s safety sweep catches jobs killed with SIGKILL.

A job's file is never deleted while its pid exists: if the process start time cannot be read, a marker is stored and
liveness checks the pid only. Leftover `.tmp` files are swept with the dead jobs.

If `HEAVY_JOB` names a live running job, `heavy` execs the command directly.

## Stats (stderr)

Queued time (when it matters), run time, CPU user+sys and average cores, peak cores and peak RSS of the whole process tree plus the largest single process, disk blocks,
machine-wide network bytes, peak VRAM and CPU steal where measurable, a TIMED OUT line when it applies, and a
prompt to review the numbers. One JSON line per run is appended to `history.jsonl`.

## History: did heavy leave the machine idle?

Besides `history.jsonl` (finished runs, used for default weights), heavy appends to `history-YYYY-MM.jsonl` (UTC
months, newest 5 kept) in the state dir. One JSON line per event, one `write()` on an `O_APPEND` file, under 4 KB:

- `queued`, `started`, `finished`: ts, job id, pid, cwd, repo (git toplevel basename), command (`argv[0..2]`, 80
  chars), requested cpu and mem. `started` adds `wait_ms` and `reasons` (time blocked by each of
  cpu/mem/vram/load1/load5/queue/ramp). `finished` adds exit code, wall time, cpu time, peak RSS.
- `sample`: ts, cpu %, mem %, load1, load5, cores, jobs running, jobs waiting, and `blocked` (what each waiting job is
  blocked by right now). While any job waits or runs, every heavy process checks every 10 s whether a sample is due,
  and the one that takes the lock writes it, so samples are at most 30 s apart. With no jobs nothing is written
  (monitoring, exempt from the no-polling rule).

`heavy history` prints totals (jobs, total, average and p95 wait), **idle while waiting**, a per-repo table (jobs,
wait, cpu-hours) and a 5-minute timeline of cpu, running and waiting. Idle while waiting is the time a job waited
although cpu < 60%, memory < 85% and 1-minute load < 2 x cores: the only time that can be heavy's fault. It prints
its share of all waiting time and the blockers recorded in those minutes (`startable` means nothing blocked and the
waiter had not yet woken). `--html PATH` writes one self-contained page (inline SVG, no scripts) and prints its path.

## Configuration

`HEAVY_STATE_DIR`, `HEAVY_TIMEOUT` (default 60m). Test overrides: `HEAVY_CAPACITY`
(`cpu=8,mem=16G,vram=0,disk=100G`), `HEAVY_LIVE` (`load=,load5=,mem_pct=,disk_free=,nonheavy_cpu=,nonheavy_mem=,nonheavy_vram=`),
`HEAVY_KILL_GRACE_MS`.
