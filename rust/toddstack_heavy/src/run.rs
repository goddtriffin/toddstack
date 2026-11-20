//! The run: enqueue, wait for admission (event-driven), run the child in its own process group, report.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGTSTP};
use signal_hook::iterator::Signals;

use crate::admit::{Decision, Smoother, evaluate};
use crate::cli::Flags;
use crate::error::Error;
use crate::history::{self, Entry};
use crate::job::{Capacity, Job, START_UNKNOWN, State, Usage, Weights};
use crate::machine::{self, TreeSampler, cpu_times, net_bytes, process_start, vram};
use crate::state::{StateDir, is_alive};
use crate::stats::{children_usage, compute, format_stats};
use crate::tty::{Terminal, TerminalGuard};

const DEFAULT_CPU: u32 = 4;
const DEFAULT_MEM: u64 = 4 << 30;
/// Re-sample live data this often while live data is what blocks a job (monitoring).
const LIVE_RESAMPLE: Duration = Duration::from_secs(10);
/// A safety sweep for jobs killed with SIGKILL, which leave a file but cause no event.
const SWEEP: Duration = Duration::from_secs(30);
const DEFAULT_GRACE_MS: u64 = 10_000;
const TIMEOUT_EXIT: u8 = 124;
/// `EX_TEMPFAIL`: the disk is full, retry after cleaning up.
const DISK_FULL_EXIT: u8 = 75;
/// How often the process tree's peak CPU and RSS are sampled while a job runs (monitoring).
const TREE_SAMPLE: Duration = Duration::from_millis(500);

enum Event {
    Fs,
    Signal(i32),
    /// Ctrl-Z while waiting in the queue.
    Stop,
}

/// Shared with the signal thread so a signal reaches the child's group at any moment.
#[derive(Default)]
struct Sig {
    pgid: AtomicI32,
    last: AtomicI32,
    /// True while this heavy waits in the queue: Ctrl-Z is then handled here (see `park`), not by the default stop.
    waiting: AtomicBool,
}

/// Sends `sig` to the group, then SIGCONT so a stopped group can act on it.
fn forward(pgid: i32, sig: i32) {
    if let Ok(s) = Signal::try_from(sig) {
        let _ = killpg(Pid::from_raw(pgid), s);
        if s != Signal::SIGKILL {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGCONT);
        }
    }
}

/// Because the child runs in its own process group (so Ctrl-C does not hit it twice), this thread
/// forwards INT, TERM and HUP to that group.
fn spawn_signal_thread(tx: Sender<Event>) -> Result<Arc<Sig>, Error> {
    let sig = Arc::new(Sig::default());
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGTSTP])?;
    let shared = sig.clone();
    std::thread::spawn(move || {
        for s in signals.forever() {
            if s == SIGTSTP {
                if shared.waiting.load(Ordering::SeqCst) {
                    let _ = tx.send(Event::Stop);
                } else if let Err(e) = signal_hook::low_level::emulate_default_handler(s) {
                    eprintln!("heavy: cannot stop: {e}");
                }
                continue;
            }
            shared.last.store(s, Ordering::SeqCst);
            let pg = shared.pgid.load(Ordering::SeqCst);
            if pg > 0 {
                forward(pg, s);
            }
            let _ = tx.send(Event::Signal(s));
        }
    });
    Ok(sig)
}

/// Removes the job file on every exit path, panics included.
struct JobGuard<'a> {
    state: &'a StateDir,
    id: String,
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        self.state.remove_job(&self.id);
    }
}

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

fn resolve_weights(state: &StateDir, flags: &Flags, cmd: &[String], cwd: &str, cap: &Capacity) -> Weights {
    let hist = history::defaults_for(&state.history_path(), cmd, cwd);
    cap.clamp(Weights {
        cpu: flags.cpu.or(hist.map(|d| d.cpu)).unwrap_or(DEFAULT_CPU),
        mem: flags.mem.or(hist.map(|d| d.mem)).unwrap_or(DEFAULT_MEM),
        vram: flags.vram.or(hist.and_then(|d| d.vram)).unwrap_or(0),
        disk: flags.disk.unwrap_or(0),
    })
}

fn exec_nested(state: &StateDir, cmd: &[String]) -> Option<Error> {
    let id = std::env::var("HEAVY_JOB").ok()?;
    let job = state.read_job(&id)?;
    if job.state != State::Running || !is_alive(&job) {
        return None;
    }
    let (first, rest) = cmd.split_first()?;
    Some(Error::Io(Command::new(first).args(rest).exec()))
}

/// # Errors
/// Fails on state-dir, probe or watcher errors. A child that cannot start exits 127.
pub fn run(state: &StateDir, flags: &Flags, cmd: &[String]) -> Result<u8, Error> {
    if let Some(e) = exec_nested(state, cmd) {
        eprintln!("heavy: cannot run {}: {e}", cmd.first().map_or("", String::as_str));
        return Ok(127);
    }
    state.ensure()?;
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().into_owned();
    let cap = machine::capacity(&cwd)?;
    let weights = resolve_weights(state, flags, cmd, &cwd_str, &cap);

    let (tx, rx) = channel();
    let sig = spawn_signal_thread(tx.clone())?;
    let _watcher = watch(&state.jobs_dir(), tx)?;

    let pid = std::process::id();
    let enqueued_ms = now_ms();
    let mut job = Job {
        id: format!("{enqueued_ms:013}-{pid}"),
        pid,
        start_time: probe_start(pid),
        weights,
        cmd: cmd.to_vec(),
        cwd: cwd_str,
        state: State::Queued,
        enqueued_ms,
        started_ms: None,
    };
    let _guard = JobGuard { state, id: job.id.clone() };
    {
        let _lock = state.lock()?;
        state.write_job(&job)?;
    }
    sig.waiting.store(true, Ordering::SeqCst);
    let acquired = acquire(state, &cwd, &cap, &mut job, &rx);
    sig.waiting.store(false, Ordering::SeqCst);
    match acquired {
        Ok(()) => {}
        Err(Error::Interrupted(s)) => return Ok(u8::try_from(128 + s).unwrap_or(130)),
        Err(Error::DiskFull(why)) => {
            eprintln!("heavy: {why}");
            return Ok(DISK_FULL_EXIT);
        }
        Err(e) => return Err(e),
    }
    let queued_ms = u64::try_from(now_ms() - enqueued_ms).unwrap_or(0);
    execute(state, &cap, &job, &sig, flags.timeout(), queued_ms)
}

/// The process start time, retried; when it cannot be read, the marker that makes liveness check the pid only
/// (never 0 or a wrong time, which would let other processes delete a live job's file).
fn probe_start(pid: u32) -> u64 {
    for _ in 0..5 {
        if let Some(t) = process_start(pid) {
            return t;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    START_UNKNOWN
}

fn watch(dir: &Path, tx: Sender<Event>) -> Result<notify::RecommendedWatcher, Error> {
    use notify::Watcher;
    let mut w = notify::recommended_watcher(move |_res: notify::Result<notify::Event>| {
        let _ = tx.send(Event::Fs);
    })?;
    w.watch(dir, notify::RecursiveMode::NonRecursive)?;
    Ok(w)
}

/// Waits in the queue. Wakes on job-file changes; re-samples on a timer only when live data blocks.
fn acquire(state: &StateDir, cwd: &Path, cap: &Capacity, job: &mut Job, rx: &Receiver<Event>) -> Result<(), Error> {
    let mut smoother = Smoother::default();
    let mut said = String::new();
    loop {
        let running: Vec<Job> = state.read_jobs().into_iter().filter(|j| j.state == State::Running).collect();
        let mut live = machine::live(cwd, cap, &running)?;
        live.non_heavy = smoother.push(live.non_heavy);
        let decision = {
            let _lock = state.lock()?;
            let jobs = state.live_jobs();
            live.stopped = machine::stopped_ids(&jobs);
            let d = evaluate(job, &jobs, cap, &live, now_ms());
            if d == Decision::Start {
                job.state = State::Running;
                job.started_ms = Some(now_ms());
                state.write_job(job)?;
            }
            d
        };
        let (reason, live_blocked) = match decision {
            Decision::Start => return Ok(()),
            Decision::Fail(why) => return Err(Error::DiskFull(why)),
            Decision::Wait { reason, live } => (reason, live),
        };
        if said != reason {
            eprintln!("heavy: waiting ({reason})");
            said = reason;
        }
        match rx.recv_timeout(if live_blocked { LIVE_RESAMPLE } else { SWEEP }) {
            Ok(Event::Signal(s)) => return Err(Error::Interrupted(s)),
            Ok(Event::Stop) => park(state, job)?,
            Ok(Event::Fs) => while rx.try_recv().is_ok_and(|e| matches!(e, Event::Fs)) {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(Error::Probe("event channel closed".into())),
        }
    }
}

/// Ctrl-Z while waiting. The job file says `stopped` first (that file change is the event that wakes the waiters
/// behind this one), then the process stops itself. After SIGCONT it is `queued` again, keeping its place.
fn park(state: &StateDir, job: &mut Job) -> Result<(), Error> {
    set_state(state, job, State::Stopped)?;
    kill(Pid::this(), Signal::SIGSTOP)?;
    set_state(state, job, State::Queued)
}

fn set_state(state: &StateDir, job: &mut Job, to: State) -> Result<(), Error> {
    let _lock = state.lock()?;
    job.state = to;
    state.write_job(job)
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Samples machine-wide GPU memory every 2 s while the job runs (monitoring); returns the peak.
fn vram_sampler(cap: &Capacity) -> Option<(Sender<()>, std::thread::JoinHandle<u64>)> {
    if cap.vram == 0 || cap.vram_is_ram {
        return None;
    }
    let (stop_tx, stop_rx) = channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut peak = 0;
        loop {
            peak = peak.max(vram().map_or(0, |(_, used)| used));
            if !matches!(stop_rx.recv_timeout(Duration::from_secs(2)), Err(RecvTimeoutError::Timeout)) {
                return peak;
            }
        }
    });
    Some((stop_tx, handle))
}

/// Samples the whole process tree (heavy's own pid down) while the job runs; returns the peak CPU and RSS.
/// Uses the same sampler as the live accounting of running jobs.
fn tree_sampler(root: u32) -> Option<(Sender<()>, std::thread::JoinHandle<Usage>)> {
    let mut sampler = TreeSampler::new(root).ok()?;
    let (stop_tx, stop_rx) = channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut peak = Usage::default();
        loop {
            peak = peak.max(sampler.sample());
            if !matches!(stop_rx.recv_timeout(TREE_SAMPLE), Err(RecvTimeoutError::Timeout)) {
                return peak;
            }
        }
    });
    Some((stop_tx, handle))
}

/// Waits for the child, also noticing when it stops (Ctrl-Z, SIGTTIN, SIGSTOP). A stopped child stops
/// heavy too (the shell then sees one stopped job and gets the terminal back); when heavy is continued
/// (`fg`), it hands the terminal to the child again and sends it SIGCONT.
fn wait_child(pid: Pid, term: Option<Terminal>) -> Result<ExitStatus, nix::Error> {
    loop {
        match waitpid(pid, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Exited(_, code)) => return Ok(ExitStatus::from_raw(code << 8)),
            Ok(WaitStatus::Signaled(_, sig, _)) => return Ok(ExitStatus::from_raw(sig as i32)),
            Ok(WaitStatus::Stopped(..)) => {
                if let Some(t) = term {
                    t.take_back();
                }
                let _ = kill(Pid::this(), Signal::SIGSTOP);
                if let Some(t) = term {
                    t.give_to_child();
                }
                let _ = killpg(pid, Signal::SIGCONT);
            }
            Ok(_) | Err(Errno::EINTR) => {}
            Err(e) => return Err(e),
        }
    }
}

fn execute(dir: &StateDir, cap: &Capacity, job: &Job, sig: &Sig, timeout: Duration, queued_ms: u64) -> Result<u8, Error> {
    let (first, rest) = job.cmd.split_first().ok_or_else(|| Error::Usage("no command".into()))?;
    let mut command = Command::new(first);
    command.args(rest).process_group(0).env("HEAVY_JOB", &job.id);

    let usage_before = children_usage()?;
    let net_before = net_bytes();
    let steal_before = cpu_times().map(|t| t.steal);
    let sampler = vram_sampler(cap);
    let tree = tree_sampler(job.pid);
    let started = Instant::now();
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("heavy: cannot run {first}: {e}");
            return Ok(127);
        }
    };
    let pgid = i32::try_from(child.id()).map_err(|_| Error::Probe("pid out of range".into()))?;
    sig.pgid.store(pgid, Ordering::SeqCst);
    let pending = sig.last.load(Ordering::SeqCst);
    if pending != 0 {
        forward(pgid, pending);
    }

    let term = Terminal::claim(Pid::from_raw(pgid));
    let tty = TerminalGuard(term);
    let (wtx, wrx) = channel();
    std::thread::spawn(move || {
        let _ = wtx.send(wait_child(Pid::from_raw(pgid), term));
    });
    let grace = Duration::from_millis(std::env::var("HEAVY_KILL_GRACE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_GRACE_MS));
    let (result, timed_out) = if let Ok(r) = wrx.recv_timeout(timeout) {
        (r, false)
    } else {
        // Over time: SIGTERM to the group, a grace period, then SIGKILL.
        forward(pgid, SIGTERM);
        let r = wrx.recv_timeout(grace).unwrap_or_else(|_| {
            forward(pgid, 9);
            wrx.recv().unwrap_or_else(|_| Ok(ExitStatus::from_raw(0)))
        });
        (r, true)
    };
    // Whatever way the child ended, nothing it left behind may keep running (or hold our pipes open).
    forward(pgid, 9);
    drop(tty);
    let exit = result?;
    let run_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let mut stats = compute(queued_ms, run_ms, usage_before, children_usage()?);
    let net_after = net_bytes();
    stats.net_in = net_after.0.saturating_sub(net_before.0);
    stats.net_out = net_after.1.saturating_sub(net_before.1);
    stats.steal_ms = steal_before.zip(cpu_times()).map(|(a, b)| b.steal.saturating_sub(a) * 10);
    stats.peak_vram = sampler.and_then(|(stop, h)| {
        drop(stop);
        h.join().ok().filter(|v| *v > 0)
    });
    if let Some(peak) = tree.and_then(|(stop, h)| {
        drop(stop);
        h.join().ok()
    }) {
        stats.peak_tree_rss = peak.mem;
        stats.peak_centi_cores = peak.cpu;
    }
    stats.timed_out = timed_out;
    eprintln!("{}", format_stats(&stats));

    let code = if timed_out { i32::from(TIMEOUT_EXIT) } else { exit_code(exit) };
    let entry = Entry { ts: Timestamp::now(), cmd: job.cmd.clone(), cwd: job.cwd.clone(), requested: job.weights, measured: stats, exit_code: code };
    if let Err(e) = history::append(&dir.history_path(), &entry) {
        eprintln!("heavy: could not write history: {e}");
    }
    Ok(u8::try_from(code & 0xff).unwrap_or(1))
}
