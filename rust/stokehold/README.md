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
same program and first argument there (`make ui-check`, `cargo test`); else the whole machine's cores and `--mem 4G` (so cold jobs, Docker commands included, start at least 10 s apart; after one run, history gives the real peak). A request larger than
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

All of these must hold:

- VRAM reservations: the running jobs' VRAM plus this request are at most 90% of capacity. RAM has no reservation sum
  (see Memory headroom). Since 0.4.9 each running job counts its live VRAM plus the unused part of its reservation: a
  declared (`--vram`) size holds that part for the whole run, a learned one for 2 minutes (the longest CPU hold, not
  memory's time to peak, so a GPU job that allocates late keeps it), then it decays over 1 minute. Before, the plain sum held for the whole run, and on the rig
  waiters waited 14 minutes on "vram" at 37% CPU with the GPU at 1.8 of 10G.
- CPU headroom: `95% of all cores - max(instant use, sustained use)` leaves one free core (90% before 0.4.5; Todd wants
  the machine about 95% hot). Instant use is non-heavy
  usage plus the live CPU (or ramp hold) of running jobs; sustained use is the machine's CPU over the last 20 s plus
  the ramp holds not yet in use. So jobs start while the CPU is at most about 89% busy (88% on 16 cores) and, while
  jobs wait, the CPU settles in an 88-95% band, never pinned at 100%. The sustained CPU is max(latest, mean of the
  readings in `~/.local/state/stokehold/busy`), which every stokehold process adds to (waiters as they re-evaluate, and while any
  job exists one reading at least every 10 s): a single 250 ms window that falls between build steps can read 80% on a
  pinned machine, and is not free CPU. A job with a learned (or `--cpu`) size holds its peak cores until that peak shows
  in its live use (3/4 of it) or its learned time to peak passes (history records when the CPU first reached 3/4 of its
  peak; the slowest recent ramp plus 25%, at least 10 s, at most 2 min, 30 s when unknown), the same way a learned memory size is held (a cold job holds the whole machine for 30 s; since 0.4.9 only half of it after the first 10 s while the machine reads
  under 50% busy, so a mid-size learned waiter need not sit out the whole 30 s behind one that shows next to nothing; a
  cold waiter always sees the whole hold, so two cold jobs never start within 30 s of each other). The unrealized part of a hold reserves the CPU band except a backfill share (2 cores, or 1/8 of the machine): a waiter of that size or smaller still starts beside a held peak while the machine reads idle. So a
  burst of builds starts one at a time as their peaks show (0.4.3: in 0.4.2 holds ended after 10 s, or after 3 s on a
  machine reading under 50%, so several 16-core cargo builds started before their CPU showed and the Mac sat at 95-100%
  CPU with load1 up to 184). Docker/VM jobs (their CPU never shows in the tree) hold their whole peak for their
  learned hold time (30 s when cold), never released by the tree's use or a plateau; after the first 10 s the part of
  other programs' CPU (where the VM's shows) is netted against the hold, so it is not counted twice (0.4.6: a 10 s
  hold let a second `docker build` in before the first one's VM CPU showed, and the Mac hit 100% with load1 130-330). A hold is also released once the job's CPU has
  plateaued (0.4.4): from 10 s after its slowest recent time to peak (at least 20 s after the start), the job's own tree
  sampler sees the last 10 s flat within itself (highest and lowest differ by at most 1 core, or 1/8 of its level) and marks its job file settled, which
  wakes the waiters. A learned peak is often one 500 ms spike (`make check` learned 15 cores and ran on 1.5), and in
  0.4.3 its 2-minute hold kept waiters out at 10-50% CPU. A lull before the slowest ramp never releases, and a settled job whose CPU rises again takes its hold back. Cold,
  `--cpu` declared and Docker/VM jobs are never released by a plateau. A `make` target that runs containers inside is
  not detected as Docker/VM work (only `docker` in the command is): its tree CPU reads low, so it may settle while its
  real work runs in the VM; the measured machine CPU still counts it.
  A whole-machine (cold) waiter that has waited 60 s (it holds the queue) skips the projected-peak test and starts under the band as in
  0.4.3, so one long job cannot leave the machine at 50-60% with jobs queued.
- Projected peak (0.4.4): a waiter bigger than the backfill share starts only while 2/5 of its learned peak fits within
  every core on top of the projected CPU (the measured use plus the unrealized holds of the jobs still ramping). A
  13-core build starts up to about 67% busy on 16 cores, a whole-machine one up to 60%; smaller waiters still fill the
  band up to one free core. 0.4.5 cut it from half: over 575 runs peaking at 6+ cores, average / peak cores is p50 0.18,
  p75 0.32, p90 0.51, and half kept 13-core builds off a Mac under 59% busy. A third or a quarter overran the machine
  43-47% of the time in the bursty-builds test; 2/5 keeps it at 0%. In 0.4.3 any waiter started with one free core under the band, so 13-core builds started
  at 83% busy and took the Mac to 100% with load1 34. With a learned sustained level (0.4.8, below) the projection is
  that level when it is lower than 2/5 of the peak. A waiter held back by a hold sleeps until the first
  moment the holds leave room, and re-samples at least every 10 s, so a peak that shows sooner releases it. A job's recorded peak is the larger of its process tree's peak and the
  machine-wide CPU rise of other programs over the pre-start baseline, so work hosted in Docker's VM (outside the
  tree) also teaches a real peak. On Linux a process's threads are not counted as children (sysinfo lists each thread
  with its whole process's RSS and CPU; before 0.3.2 a 16-thread rustc counted 17 times, so learned memory peaks were
  inflated about 17x and their reservations blocked idle machines on "mem").
- Sustained level (0.4.8): each run also records its tree's best 10 s mean CPU (`sustained_centi_cores` in history),
  and a learned size carries the median sustained/peak ratio of the runs that recorded one (at least 3 of them, or no
  sustained level), applied to the picked peak: a fresh cached 1-core run cannot shrink a 14-core command's level. A
  running job holds its full learned peak until its learned time to peak plus margin (at least 10 s), then at most
  that sustained level, and never less than it for the whole run: a bursty job's later spikes stay reserved. Since
  0.4.9 the largest running floor above its live use is reserved in full and half of each other one, not every one
  whole: with 0.4.8 three `make lint` runs (floors 6.5-8 cores, 1.3-2.2 cores used each) reserved 20 cores on the
  16-core Mac at 41-73% busy, and the rig (floors 10-14 cores, 1.3 used) idled 82% of the time jobs waited. Three
  running reserve two whole spikes, so the Mac runs three lint-shaped jobs and a fourth waits (reserving only the
  largest let four admit a fifth, and two coinciding spikes then took 17.5 of 16 cores). Learned peaks are the highest 500 ms sample: `make lint
  test-changed` learned 10-11 cores (a spike about 15 s in) and averaged 1.2 cores over a 23-minute run, so its hold
  and its projected peak kept 6-core waiters out on a machine 20-40% busy. Cold, `--cpu` declared and Docker/VM sizes
  (whose CPU shows outside the tree) have no sustained level and keep the full peak.
- Memory headroom: `95% of RAM - non-heavy usage - live memory of running jobs - the unused part of each job's reservation` fits the
  request. The unused part (reservation minus live use) of a declared (`--mem`) size is held for the whole run. A learned
  size's is held until the job's learned time to peak (history records when memory peaked; 60 s when unknown, at least 10 s),
  then shrinks to nothing over 60 s; those learned holds together take at most 3/4 of the memory still free. Reservations
  that merely sum high never block (before 0.4.2 reservations summing over 85% blocked jobs at 43% memory used), but a
  burst of late-peaking jobs cannot all start either (12 jobs of a 12G peak on a 64G Mac: 3 start, not 12). Docker and
  WSL VM memory is outside the job's process tree, so for docker jobs the learned peak also counts the machine-wide
  memory rise over the pre-start baseline, as CPU does. Non-heavy usage is machine-wide use minus the process trees of stokehold's own jobs (Chrome, Discord, other
  CLIs), smoothed as max(latest sample, mean of the last 3, 10 s apart): a rise blocks at once, a fall is released slowly. On
  Linux CPU includes steal time (`/proc/stat` field 8), which WSL2 and VMs otherwise hide.
- Known limit: the memory and pressure guards only veto new starts. A gate cannot stop jobs already running, and
  stokehold does not kill them: if running jobs all grow past their learned peaks, memory can still run out. The unused
  reservation holds above are what protect against this, not the guards.
- Pinned CPU (0.4.8): nothing starts, fast lane and a job alone included, while any shared CPU reading in the last
  10 s is 98% busy or more. On 0.4.7 the Mac read over 95% CPU or 3 x cores of load in 80% of samples over 6 hours,
  and 190 of 272 starts came at 95-100% busy, most in the dip between two spikes of bursty jobs.
- Hard guards (veto only, they block even a job that would run alone): memory used at 95% or more (90% before 0.4.5, on Linux too), 1-minute load at or over 4 x cores
  once the CPU confirms it: at least 50% busy, or Linux `/proc/pressure/cpu` `some avg10` at least 25, and always where
  PSI cannot be read (macOS, so the Mac keeps the 0.4.7 rule) (runaway load:
  120 on 16 cores, CPU pinned, killed Docker on 2026-11-19; before 0.4.8 it blocked whatever the CPU said, and a load
  of I/O-waiting threads at 8-15% CPU is no overload), 6 x cores whatever the CPU says, and critical memory
  pressure: macOS `kern.memorystatus_vm_pressure_level` 4, Linux `/proc/pressure/memory` (`some avg10` 25 or more, or
  `full avg10` 5 or more; the Windows rig's WSL Ubuntu). An unreadable pressure source never blocks.
- Soft guards (veto only): from 2 x cores up, load blocks while it is still rising (load1 > 1.1 x load5)
  and the CPU is at least 75% busy, or while the CPU is at least 90% busy (the CPU here is the larger of the latest
  and the sustained reading), or while Linux PSI cpu `some avg10` is at least 25 (tasks queue for a CPU; 0.4.8). On
  the rig on 2026-11-21, load1 10-35 on 16 cores came with 8-15% CPU and PSI cpu `some` under 1: threads in I/O
  wait. Load without CPU is not CPU overload: on
  macOS load also counts threads blocked in I/O (network, disk), so it can read 20-60 on an idle CPU (load1 58 at 26%
  CPU during a network outage on 2026-11-20). macOS
  pressure level 2 ("warn") blocks only with memory at least 85% used. Level 2 is common and sticky: on 2026-11-21 it
  held for hours at 49-58% used (1.8 of 3 GB swap), and 0.4.4, which treated it as pressure, kept three jobs queued for
  40+ minutes with nothing running at 27% CPU.
- Disk: free space on the cwd's filesystem must be at least `--disk` plus 10% of the disk. If it is not, waiting
  cannot help, so `stokehold` exits 75 at once and says the disk is full (clean up, or ask Todd).
- A job running alone always passes the CPU and memory headroom checks (but not the guards above: 95% memory used still blocks it). There is no cap on the number of jobs.

Queue: first in, first out. A job behind the head starts when the head and it fit together (it never stops the
head from fitting). When the head does not fit now, a job no bigger than the head may backfill past it while the
head has waited under 60 s. A tiny job (at most the backfill share of cores and 1/32 of RAM) may pass a head that is blocked
by memory, VRAM or a guard while the head has waited under 5 minutes, or by a ramp hold it does not make end later (0.4.5: in 24 h on the Mac, 393
job-samples sat queued behind a head under 60% CPU). Behind a head blocked on CPU room (or a ramp hold), a job of at
most the backfill share passes while the machine reads under 75% busy and the head has waited under 5 minutes (0.4.6:
strict FIFO past 60 s kept 1-2 core jobs queued up to 30 minutes behind an 11-16 core head at 40-60% CPU). 0.4.8
lets any job no bigger than the head (cores, memory and VRAM) backfill there, not only small ones: with 0.4.7 on the
Mac, 1-2 core jobs waited p50 42 s but 3-8 core jobs (`cargo clippy`, `cargo test` at 6 cores) p50 21 minutes, up to
74, behind 11-core `make lint` heads, with the Mac idle 23% of the time jobs waited. Such a backfiller (bigger than
tiny) passes only if its own learned p90 run time ends within 30 s of the head's 5 minutes; with no learned run time
it waits, so a 70-minute job arriving at 4:59 cannot keep the head's room for 70 minutes. Once the head has waited 5
minutes it holds the queue (only tiny jobs pass a head blocked by memory, VRAM or a guard, and only before that), so
no stream of backfills can starve it: only the jobs already running stand before it. (Aging its projected peak away
instead overran the machine 16% of the time in the bursty-builds test.)

Short small jobs (0.4.9): a job of at most the backfill share of cores, 1/32 of RAM, no VRAM, not Docker/VM work and
with a learned p90 run time of at most 5 minutes passes a head blocked on CPU room or a ramp hold past those 5
minutes too, up to 30 minutes of the head's wait, if it fits by its own checks. The short small jobs running and it
together take at most the backfill share of cores, so they keep at most 2 cores (1/8 of the machine) from the head at
any time, each for minutes. It passes a head on CPU room only while the head would still wait with the short small jobs
running gone, and a head on a ramp hold only if it does not move the head's start later, so a stream of them never
keeps the head waiting on themselves. On 2026-11-22 the Mac idled 4h40m on "queue" in 10h30m of waiting: 1-2 core jobs waited
behind 11-core `make lint` heads that had waited over 5 minutes.

Fast lane (0.4.7): a job whose own recent runs (at least 2 of this very command, never the pooled program tier) all
finished within 10 s, and that reserves at most 2 cores, 1/32 of RAM and no VRAM (and is not Docker/VM work), skips
the CPU band, ramp holds and the queue. At most 2 fast-lane jobs run at once, so the lane adds at most 4 cores to a
busy machine; a fast-lane job that runs long keeps its slot until it ends, and its long run takes the command out of
the lane from then on. Memory headroom, VRAM reservations and every guard (load, memory pressure, memory used) still
apply. On 2026-11-21 a 1-core `make lint` that ran 1.2 s waited 204 s on the rig at 95-98% busy.

`stokehold status` evaluates every waiter by these rules and prints why it waits, with the numbers (`cpu: needs 4.40
cores, 4.20 free under the 95% band (machine 68% busy)`, `mem: needs 6.5G, 2.1G free under 95% of RAM after running
jobs' holds`, a guard's reading), and the machine's CPU, sustained CPU and pressure level.

## State and waiting

`~/.local/state/stokehold/jobs/` holds one JSON file per job (pid, process start time, reservation, cmd, cwd, state).
Decisions are made under a `flock`, which the kernel releases if a process dies. Files of dead jobs are dropped.
Each job's `stokehold` also holds a `flock` on its own `<id>.lock` for its whole life (opened close-on-exec, so a server
the job leaves behind never inherits it). A job whose lock can be taken is dead, however it died (SIGKILL included), and
whatever process now has its pid: its file is deleted and it never keeps a reservation (0.4.3: in 0.4.2 `status` could
list a `running` job whose pid no longer existed).
Waiters watch the directory (kqueue/inotify) and re-evaluate when a job file changes or disappears. Only while live
data blocks a job is there a 10 s re-sample (monitoring; a waiter held back by a ramp sleeps until the first moment it fits instead, not until the ramp ends); a 30 s safety sweep catches jobs killed with SIGKILL.

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

`STOKEHOLD_STATE_DIR`, `STOKEHOLD_TIMEOUT` (default 60m). Test overrides: `STOKEHOLD_CAPACITY`
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
