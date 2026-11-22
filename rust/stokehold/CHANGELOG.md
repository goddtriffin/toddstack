# Changelog

Earlier releases are described in the git history and the README.

## 0.4.10

- Closed-loop admission: a job starts while the measured CPU is under 80%. Learned peak and floor reservations are gone.
- A job settles for 10 s, or its learned time to CPU peak (at most 60 s); cold and Docker/VM jobs for at least 30 s.
  A job that already uses its typical cores stops settling after 10 s.
- Size check: measured busy cores plus the new job's typical cores stay within 95% of the cores.
- Peak cap: the expected peaks of jobs started in the last 60 s stay within 2.5 x cores.
- Jobs run at nice 10 (`STOKEHOLD_NICE`); Docker/VM jobs run un-niced.
- Job files carry a version (`v`). A waiter from 0.4.9 or earlier never holds the queue for a 0.4.10 waiter.
- Upgrade: while 0.4.9 waiters remain (until each one starts or exits), they judge 0.4.10 jobs by the old rules and
  0.4.10 waiters let them pass, so the machine may briefly over-admit. Upgrade when the queue is quiet.
