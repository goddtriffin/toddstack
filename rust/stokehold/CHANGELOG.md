# Changelog

Earlier releases are described in the git history and the README.

## 0.4.12

- Memory: a job's tree memory is capped at what the machine really uses, live and when learned (Linux: total minus
  available; macOS: app + wired + compressor from `vm_stat`, because sysinfo's available counts active anonymous
  memory and left 20 GB out, so total minus available was 31.8 GB against 44.2 GB really used). Summed RSS counts shared file mappings once per process: on the rig a `make lint` learned 17-19 GB while the
  machine never used over 8 of 32 GB, and `c9-timeouts` waited 4m37s on "mem" at 9-22% RAM used (2026-11-25).
- The machine's memory is read at the same moment as the trees (it was read before the 250 ms sleep).
- CPU: waiters blocked by the CPU (line, load, pinned, settle) re-sample every 2 s instead of 10 s. Waits of 23-30 s at
  47-54% busy came from a 10 s timer and one reading per 10 s in the sustained mean.

## 0.4.11

- Size check uses the learned typical cores, at most `--cpu`. A declared `--cpu 12` is a ceiling: a lint that averaged
  1 core was held at 39% busy because 6 busy cores plus the declared 12 passed 95% (2026-11-24: 9m07s of waiting with
  the CPU idle). `--cpu` alone (a cold job) still counts as the typical size.
- A Docker/VM job keeps `--cpu` as its typical size: its learned mean is only the docker client's, not the VM's work.
- History lookups can fall back to a sibling command (same program and first 2 words), so a flagged job may inherit a
  smaller learned typical. Overload stays bounded by the peak cap, the 80% line and the pinned guard.

## 0.4.10

- Closed-loop admission: a job starts while the measured CPU is under 80%. Learned peak and floor reservations are gone.
- A job settles for 10 s, or its learned time to CPU peak (at most 60 s); cold jobs for at least 15 s, Docker/VM jobs 30 s.
  A job that already uses its typical cores stops settling after 10 s.
- Size check: measured busy cores plus the new job's typical cores stay within 95% of the cores.
- Peak cap: the expected peaks of jobs started in the last 60 s stay within 2.5 x cores.
- Jobs run at nice 10 (`STOKEHOLD_NICE`); Docker/VM jobs run un-niced.
- Job files carry a version (`v`). A waiter from 0.4.9 or earlier never holds the queue for a 0.4.10 waiter.
- Upgrade: while 0.4.9 waiters remain (until each one starts or exits), they judge 0.4.10 jobs by the old rules and
  0.4.10 waiters let them pass, so the machine may briefly over-admit. Upgrade when the queue is quiet.
