# stokehold

`stokehold` runs heavy commands (builds, tests, GPU work) one admission at a time per machine. Several agents share one
laptop; each runs `stokehold <cmd>` as a background command and is notified when it exits. `stokehold` decides by itself
whether to start at once or wait in the queue. Nobody polls or checks a status.

```
stokehold [--cpu N] [--mem SIZE] [--vram SIZE] [--disk SIZE] [--timeout DURATION] [--] cmd args...
stokehold status        # for humans: running and queued jobs, why each waiter waits (with numbers), live usage (`--status` works too)
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
same program and first argument there (`make ui-check`, `cargo test`); else the whole machine's cores and `--mem 4G` (so cold jobs, Docker commands included, start at least 30 s apart; after one run, history gives the real peak). A request larger than
the machine is clamped, so that job runs alone. On macOS VRAM counts as RAM.
VRAM is learned only from the job's own use (0.4.7): the machine-wide VRAM in use at its peak minus what was in use
just before it started (`vram_rise` in history), 0 under 256M. Before 0.4.7 the machine-wide peak itself was learned,
which is mostly the desktop's: on the rig every job learned about 1.9G, and a 1 s `make lint` waited 204 s on "vram".
History entries without `vram_rise` teach no VRAM, so a job reserves VRAM only after it has used some, or with `--vram`.
Successful runs count, and so do failed or killed runs that ran at least 30 s (0.4.6: `make lint test-changed`
usually exits 2 after 5-25 minutes on 8-12 cores, so it learned 1 core and 0.1G from instant `make lint` no-ops, and
several of these "tiny" jobs ran at once on a pinned Mac). A run that fails sooner teaches nothing.
History entries carry a version (`v`); on Linux, entries from before 0.3.2 (no `v`) are ignored for defaults because they counted threads as processes.

## Admission

Closed loop since 0.4.10: a job starts on what the machine measures, not on what running jobs might still use. Nine
rounds of open-loop rules (ramp holds, plateaus, projected peaks, sustained floors, backfill, short-pass and fast-lane
rules) reserved each job's learned peak, but a learned peak is one 500 ms sample: `make lint` learned 13 cores and
averaged 1.4, `cargo test` learned 16 and averaged 2.4. So 0.4.9 idled while jobs waited (the Mac was under 80% CPU
86% of the time jobs waited, the rig 76%) and still pinned the CPU when bursts lined up (11.6% on the Mac).

All of these must hold (a job with nothing running needs only the guards and the disk check):

- VRAM reservations: the running jobs' VRAM plus this request are at most 90% of capacity. Each running job counts its
  live VRAM plus the unused part of its reservation: a declared (`--vram`) size holds that part for the whole run, a
  learned one for 2 minutes, then it decays over 1 minute (0.4.9: the plain sum held for the whole run, and the rig
  waited 14 minutes on "vram" with the GPU at 1.8 of 10G).
- Memory headroom: `95% of RAM - non-heavy usage - live memory of running jobs - the unused part of each job's
  reservation` fits the request. The unused part of a declared (`--mem`) size is held for the whole run. A learned
  size's is held until the job's learned time to peak (history records when memory peaked; 60 s when unknown, at
  least 10 s), then shrinks to nothing over 60 s; those learned holds together take at most 3/4 of the memory still
  free. Reservations that merely sum high never block, but a burst of late-peaking jobs cannot all start either (12
  jobs of a 12G peak on a 64G Mac: 3 start, not 12). Docker and WSL VM memory is outside the job's process tree, so for
  docker jobs the learned peak also counts the machine-wide memory rise over the pre-start baseline. Non-heavy usage
  is machine-wide use minus the process trees of stokehold's own jobs, smoothed as max(latest sample, mean of the last
  3, 10 s apart): a rise blocks at once, a fall is released slowly.
- CPU line: the machine's CPU, max(latest reading, sustained reading), is under 80%. The sustained reading is
  max(latest, mean of the last 20 s of readings in `~/.local/state/stokehold/busy`), which every stokehold process
  adds to, so a single window between build steps cannot admit onto a busy machine. Jobs start under 80% and their
  bursts fill the rest: the target is 80-90% busy while jobs wait. On Linux CPU includes steal time.
- Size: the measured busy cores plus the new job's typical cores (`--cpu`, else the median of its past runs' mean
  cores, else a quarter of the machine when cold) stay within 95% of the cores, settling or not, so a job that averages 16
  cores does not start at 79% busy. A waiter blocked by this for 5 minutes holds the queue, like a memory wait.
- Peak cap (a backstop that rarely binds): the expected peaks (learned peak, `--cpu`, or the whole machine when cold)
  of the jobs started under 60 s ago, plus the new one's, stay within 2.5 x cores. It stops cold builds that idle on
  downloads or a build-dir lock past their settle and then compile together: on 16 cores at most two cold jobs start
  in any minute. Older jobs do not count: their CPU shows in the measurement and their peaks are short bursts at
  different times (counting every running job's peak replayed the Mac's 2026-11-21 day at 12% in band).
- Settle: a job that started less than its settle window ago (or has not yet stamped its start) is settling: its CPU
  has not shown yet. The window is 10 s, or the learned time to its CPU peak when later, at most 60 s; cold jobs
  (no history) and Docker/VM jobs settle at least 30 s (cold `cargo build`s fetch crates and `docker build`s pull
  images before they compile). A job past 10 s that already uses its typical cores shows in the measurement and
  stops settling early.
  While any job settles, a new job starts only if the measured CPU, plus each settling job's size (learned peak,
  `--cpu`, or the whole machine when cold) minus its live use, plus the new job's own size fit under the 80% line.
  So small jobs still start beside a fresh build, two cold jobs start at least 30 s apart, and a burst of big builds
  starts one per settle as each one's CPU shows in the measurement. A waiter held by the settle sleeps until the first
  settle ends. Once a job has settled, only the measurement, the size check and the peak cap count it.
- Pinned CPU: nothing starts, a job alone included, while any shared CPU reading in the last 10 s is 98% busy or more.
- Hard guards (veto only, they block even a job that would run alone): memory used at 95% or more; 1-minute load at
  or over 4 x cores once the CPU confirms it (at least 50% busy, or Linux `/proc/pressure/cpu` `some avg10` at least
  25, and always where PSI cannot be read, so on macOS) (runaway load: 120 on 16 cores, CPU pinned, killed Docker on
  2026-11-19); 6 x cores whatever the CPU says; and critical memory pressure: macOS
  `kern.memorystatus_vm_pressure_level` 4, Linux `/proc/pressure/memory` (`some avg10` 25 or more, or `full avg10` 5 or
  more). An unreadable pressure source never blocks.
- Soft guards (veto only): from 2 x cores up, load blocks while it is still rising (load1 > 1.1 x load5) and the CPU is
  at least 75% busy, or while the CPU is at least 90% busy, or while Linux PSI cpu `some avg10` is at least 25. Load
  without CPU is I/O wait, not CPU overload (load1 10-35 at 8-15% CPU on the rig; 58 at 26% on the Mac during a
  network outage). macOS pressure level 2 ("warn") blocks only with memory at least 85% used.
- Disk: free space on the cwd's filesystem must be at least `--disk` plus 10% of the disk. If it is not, waiting
  cannot help, so `stokehold` exits 75 at once and says the disk is full (clean up, or ask Todd).
- Known limit: the guards only veto new starts. stokehold does not kill running jobs: if they all grow past their
  learned peaks, memory can still run out. The memory holds above are what protect against this.

There is no cap on the number of jobs.

Queue: oldest first, without head-of-line blocking. A waiter checks each older waiter in turn: if one can start now,
it goes first; if one waits only for the settle, the younger one waits for it too (it would take that room); if one
has waited 5 minutes or more on memory or VRAM, the queue holds for it, so small jobs cannot starve a big one. An older
waiter blocked by the CPU line or a guard does not hold anyone: the same checks block the younger one too, and when the
CPU drops below the line the oldest one that fits starts first.

Nice: jobs run at `nice 10` (through `/usr/bin/nice`, which execs the command, so the pid and process group stay the
same), so the desktop, shells and stokehold itself stay responsive while builds fill the CPU. `STOKEHOLD_NICE` sets the
level (0 turns it off). Nice is inherited: daemons a job starts (the Gradle daemon, an sccache server) keep running at
nice 10 after the job ends, and serve later un-niced builds at that level. Nice does not reach Docker's VM: the work
of `docker build` or `docker compose` runs in the VM at normal priority, so Docker/VM jobs (any command word
containing `docker`) run un-niced.

Learned CPU sizes: a job's recorded peak is the larger of its process tree's peak and the machine-wide CPU rise of
other programs over the pre-start baseline, so work hosted in Docker's VM also teaches a real peak. On Linux a
process's threads are not counted as children (before 0.3.2 a 16-thread rustc counted 17 times). History still records
each run's time to CPU peak, which sets its settle window, and its best 10 s mean (`sustained_centi_cores`) for the stats.

`stokehold status` evaluates every waiter by these rules and prints why it waits, with the numbers (`cpu: machine 85%
busy, jobs start under 80%`, `cpu: jobs that just started are settling; 24.60 cores with theirs, 12.80 under the 80%
line`, `mem: needs 6.5G, 2.1G free under 95% of RAM after running jobs' holds`, a guard's reading), and the machine's
CPU, sustained CPU and pressure level.

## State and waiting

`~/.local/state/stokehold/jobs/` holds one JSON file per job (pid, process start time, reservation, cmd, cwd, state).
Decisions are made under a `flock`, which the kernel releases if a process dies. Files of dead jobs are dropped.
Each job's `stokehold` also holds a `flock` on its own `<id>.lock` for its whole life (opened close-on-exec, so a server
the job leaves behind never inherits it). A job whose lock can be taken is dead, however it died (SIGKILL included), and
whatever process now has its pid: its file is deleted and it never keeps a reservation (0.4.3: in 0.4.2 `status` could
list a `running` job whose pid no longer existed).
Waiters watch the directory (kqueue/inotify) and re-evaluate when a job file changes or disappears. Only while live
data blocks a job is there a 10 s re-sample (monitoring; a waiter held back by the settle sleeps until the first settle ends); a 30 s safety sweep catches jobs killed with SIGKILL.

Job files without a lock file (written by 0.4.2 and older) fall back to the pid: dead when the pid is gone, a zombie,
or started at another time than the file says (pid reuse); if the process start time cannot be read, a marker is
stored and liveness checks the pid only. Leftover `.tmp` files are swept with the dead jobs.

If `STOKEHOLD_JOB` names a live running job, `stokehold` execs the command directly.

## Stats (stderr)

Queued time (when it matters), run time, CPU user+sys and average cores, peak cores and peak RSS of the whole process tree plus the largest single process, disk blocks,
machine-wide network bytes, peak VRAM (machine-wide, and the rise above its start) and CPU steal where measurable, a TIMED OUT line when it applies, and a
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

`stokehold history` prints totals (jobs, total, average and p95 wait), CPU and RAM p50 and p95 over every sample in
the window (0.4.9), **avg cpu while jobs waited** (time-weighted;
it should be near full), **idle while waiting**, a per-repo table (jobs, wait, cpu-hours) and a 5-minute timeline of
cpu, running and waiting. Idle while waiting is the time a job waited although cpu < 75% and memory < 85% (load is
ignored): the time that can be stokehold's fault. It prints
its share of all waiting time and the blockers recorded in those minutes (`startable` means nothing blocked and the
waiter had not yet woken). `--html PATH` writes one self-contained page (inline SVG, no scripts) and prints its path.

## Configuration

`STOKEHOLD_STATE_DIR`, `STOKEHOLD_TIMEOUT` (default 60m), `STOKEHOLD_NICE` (default 10, 0 off). Test overrides: `STOKEHOLD_CAPACITY`
(`cpu=8,mem=16G,vram=0,disk=100G`), `STOKEHOLD_LIVE` (`load=,load5=,mem_pct=,mem_pressure=,disk_free=,nonheavy_cpu=,nonheavy_mem=,nonheavy_vram=`; `mem_pressure` is a macOS level: 2 warn, 4 critical),
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
