# stokehold

`stokehold` runs heavy commands (builds, tests, GPU work) one admission at a time per machine. Several agents share one
laptop; each runs `stokehold <cmd>` as a background command and is notified when it exits. `stokehold` decides by itself
whether to start at once or wait in the queue. Nobody polls or checks a status.

```
stokehold [--cpu N] [--mem SIZE] [--vram SIZE] [--disk SIZE] [--timeout DURATION] [--] cmd args...
stokehold status        # for humans: running and queued jobs, reservations, live usage (`--status` works too)
stokehold history [--since 2h|1d|7d] [--html PATH]   # how well stokehold used the machine (default: 1d)
```

The exit code is the child's. Exit 75: the disk is full. A job over its timeout gets SIGTERM to its process group, 10 s of grace, then SIGKILL,
and `stokehold` exits 124. When the child exits (or is killed), `stokehold` sends SIGKILL to its whole process group, so
nothing it left behind keeps running or holds a pipe open.

Terminal: when stdin is a terminal and `stokehold` is in the foreground, the child's group gets the terminal while it
runs (for prompts and pagers) and `stokehold` takes it back afterwards. If the child stops (Ctrl-Z), `stokehold` stops too;
`fg` resumes both.

## Weights

Cores, RAM, GPU memory and free disk the job needs. Without flags: the median measured peak (whole process tree,
sampled every 0.5 s while the job runs: peak cores at once and peak RSS) from
`~/.local/state/stokehold/history.jsonl` for the same command and cwd; else for the same command in the same place of
the same repository (any worktree or clone sharing its git dir, so a fresh agent worktree is not cold); else for the
same program and first argument there (`make ui-check`, `cargo test`); else the whole machine's cores and `--mem 4G` (so cold jobs, Docker commands included, start at least 10 s apart; after one run, history gives the real peak). A request larger than
the machine is clamped, so that job runs alone. On macOS VRAM counts as RAM.
History entries carry a version (`v`); on Linux, entries from before 0.3.2 (no `v`) are ignored for defaults because they counted threads as processes.

## Admission

All of these must hold:

- Reservations: running RAM and VRAM reservations plus this request are at most 90% of capacity.
- CPU headroom: `90% of all cores - max(instant use, sustained use)` leaves one free core. Instant use is non-heavy
  usage plus the live CPU (or ramp hold) of running jobs; sustained use is the machine's CPU over the last 20 s plus
  the ramp holds not yet in use. So jobs start while the CPU is at most about 84% busy (83% on 16 cores) and, while
  jobs wait, the CPU settles in an 80-90% band, never pinned at 100%. The sustained CPU is max(latest, mean of the
  readings in `~/.local/state/stokehold/busy`), which every stokehold process adds to (waiters as they re-evaluate, and while any
  job exists one reading at least every 10 s): a single 250 ms window that falls between build steps can read 80% on a
  pinned machine, and is not free CPU. A job admitted in the last 10 s holds its peak cores (from history) while it
  ramps up, so a burst of waiters is staggered. The full 10 s hold covers cold jobs, Docker/VM jobs and learned peaks of 75% of
  the machine or more, so those start at least 10 s apart. Any smaller learned peak is full for 3 s; after that, if the machine reads
  under 50% CPU the job has shown its real use and the hold is released, otherwise it shrinks linearly to nothing at
  10 s. A waiter held back by a ramp
  sleeps until the first moment the shrinking holds leave room. A job's recorded peak is the larger of its process tree's peak and the
  machine-wide CPU rise of other programs over the pre-start baseline, so work hosted in Docker's VM (outside the
  tree) also teaches a real peak. On Linux a process's threads are not counted as children (sysinfo lists each thread
  with its whole process's RSS and CPU; before 0.3.2 a 16-thread rustc counted 17 times, so learned memory peaks were
  inflated about 17x and their reservations blocked idle machines on "mem").
- Memory headroom: `90% of RAM - non-heavy usage - sum over running jobs of max(reservation, live usage)` fits the
  request. Non-heavy usage is machine-wide use minus the process trees of stokehold's own jobs (Chrome, Discord, other
  CLIs), smoothed as max(latest sample, mean of the last 3, 10 s apart): a rise blocks at once, a fall is released slowly. On
  Linux CPU includes steal time (`/proc/stat` field 8), which WSL2 and VMs otherwise hide.
- Guards (veto only): 1-minute load at or over 4 x cores blocks whatever the CPU says (runaway load: 120 on 16
  cores killed Docker on 2026-11-19). From 2 x cores up, load blocks while it is still rising (load1 > 1.1 x load5)
  and the CPU is at least 75% busy, or while the CPU is at least 90% busy (the CPU here is the larger of the latest
  and the sustained reading). Load without CPU is not CPU overload: on
  macOS load also counts threads blocked in I/O (network, disk), so it can read 20-60 on an idle CPU (load1 58 at 26%
  CPU during a network outage on 2026-11-20). Memory used below 90%, and no memory
  pressure: macOS `kern.memorystatus_vm_pressure_level` of 2 or more, Linux `/proc/pressure/memory` (`some avg10`
  25 or more, or `full avg10` 5 or more; the Windows rig's WSL Ubuntu). An unreadable pressure source never blocks.
- Disk: free space on the cwd's filesystem must be at least `--disk` plus 10% of the disk. If it is not, waiting
  cannot help, so `stokehold` exits 75 at once and says the disk is full (clean up, or ask Todd).
- A job running alone always passes the resource checks. There is no cap on the number of jobs.

Queue: first in, first out. A job behind the head starts when the head and it fit together (it never stops the
head from fitting). When the head does not fit now, a job no bigger than the head may backfill past it while the
head has waited under 60 s.

## State and waiting

`~/.local/state/stokehold/jobs/` holds one JSON file per job (pid, process start time, reservation, cmd, cwd, state).
Decisions are made under a `flock`, which the kernel releases if a process dies. Files of dead jobs are dropped.
Waiters watch the directory (kqueue/inotify) and re-evaluate when a job file changes or disappears. Only while live
data blocks a job is there a 10 s re-sample (monitoring; a waiter held back by a ramp sleeps until the first moment it fits instead, not until the ramp ends); a 30 s safety sweep catches jobs killed with SIGKILL.

A job's file is never deleted while its pid exists: if the process start time cannot be read, a marker is stored and
liveness checks the pid only. Leftover `.tmp` files are swept with the dead jobs.

If `STOKEHOLD_JOB` names a live running job, `stokehold` execs the command directly.

## Stats (stderr)

Queued time (when it matters), run time, CPU user+sys and average cores, peak cores and peak RSS of the whole process tree plus the largest single process, disk blocks,
machine-wide network bytes, peak VRAM and CPU steal where measurable, a TIMED OUT line when it applies, and a
prompt to review the numbers. One JSON line per run is appended to `history.jsonl`.

## History: did stokehold leave the machine idle?

Besides `history.jsonl` (finished runs, used for default weights), stokehold appends to `history-YYYY-MM.jsonl` (UTC
months, newest 5 kept) in the state dir. One JSON line per event, one `write()` on an `O_APPEND` file, under 4 KB:

- `queued`, `started`, `finished`: ts, job id, pid, cwd, repo (git toplevel basename), command (`argv[0..2]`, 80
  chars), requested cpu and mem. `started` adds `wait_ms` and `reasons` (time blocked by each of
  cpu/mem/vram/load1/queue/ramp; older lines may say load5). `finished` adds exit code, wall time, cpu time, peak RSS.
- `sample`: ts, cpu %, mem %, load1, load5, cores, jobs running, jobs waiting, and `blocked` (what each waiting job is
  blocked by right now). While any job waits or runs, every stokehold process checks every 10 s whether a sample is due,
  and the one that takes the lock writes it, so samples are at most 30 s apart. With no jobs nothing is written
  (monitoring, exempt from the no-polling rule).

`stokehold history` prints totals (jobs, total, average and p95 wait), **avg cpu while jobs waited** (time-weighted;
it should be near full), **idle while waiting**, a per-repo table (jobs, wait, cpu-hours) and a 5-minute timeline of
cpu, running and waiting. Idle while waiting is the time a job waited although cpu < 75% and memory < 85% (load is
ignored): the time that can be stokehold's fault. It prints
its share of all waiting time and the blockers recorded in those minutes (`startable` means nothing blocked and the
waiter had not yet woken). `--html PATH` writes one self-contained page (inline SVG, no scripts) and prints its path.

## Configuration

`STOKEHOLD_STATE_DIR`, `STOKEHOLD_TIMEOUT` (default 60m). Test overrides: `STOKEHOLD_CAPACITY`
(`cpu=8,mem=16G,vram=0,disk=100G`), `STOKEHOLD_LIVE` (`load=,load5=,mem_pct=,mem_pressure=,disk_free=,nonheavy_cpu=,nonheavy_mem=,nonheavy_vram=`),
`STOKEHOLD_KILL_GRACE_MS`.

## Renamed from `heavy` (0.4.0)

The crate and binary were `toddstack-heavy` / `heavy` up to 0.3.2. What carries over:

- **argv[0] alias**: run through a symlink named `heavy` (`ln -sfn <path>/stokehold ~/.local/bin/heavy`) and it behaves
  identically, so existing sessions and scripts keep working.
- **State dir**: `~/.local/state/stokehold`. On the first run an existing `~/.local/state/heavy` is renamed to it and a
  symlink is left at the old path, so history is kept and a still-running old `heavy` binary follows the link to the
  same `jobs/` files and flock locks (one shared queue during the transition). Delete the symlink once no old binary runs.
- **Env vars**: `STOKEHOLD_*` are read first, then the legacy `HEAVY_*` names. Jobs get both `STOKEHOLD_JOB` and `HEAVY_JOB`,
  so nesting works across old and new binaries.
