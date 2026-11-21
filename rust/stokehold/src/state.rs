//! The shared state dir: one JSON file per job, coordinated with `flock`.
//!
//! The kernel drops a flock when its holder dies, so there is no stale-mutex reaping. Each job's process also holds a
//! flock on its own `<id>.lock` for as long as it lives: a job whose lock can be taken is dead, however it died
//! (SIGKILL included), and whatever process now has its pid. Job files without a lock file (written by 0.4.2 and
//! older) fall back to the pid plus its start time. Dead jobs are removed whenever someone holds the state lock.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;

use crate::error::Error;
use crate::job::{Job, START_UNKNOWN};
use crate::machine::{ProcessProbe, process_probe};

/// Reads `STOKEHOLD_<X>`, falling back to the legacy `HEAVY_<X>` (pass the `STOKEHOLD_` name). Empty counts as unset.
#[must_use]
pub fn env_var(name: &str) -> Option<std::ffi::OsString> {
    prefer(std::env::var_os(name), || {
        std::env::var_os(name.replacen("STOKEHOLD_", "HEAVY_", 1))
    })
}

fn prefer(
    new: Option<std::ffi::OsString>,
    legacy: impl FnOnce() -> Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    new.filter(|v| !v.is_empty())
        .or_else(|| legacy().filter(|v| !v.is_empty()))
}

/// What `migrate_legacy` found and did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// `old` was already a symlink to `new`.
    Linked,
    /// `old` was missing; a symlink to `new` was created.
    LinkCreated,
    /// `old` was a real directory and `new` missing; it was renamed and a symlink left behind.
    Migrated,
    /// Two queues exist (both real dirs, or `old` points elsewhere). Nothing deleted; `new` keeps being used.
    Split,
}

fn warn_split(old: &Path, new: &Path, why: &str) {
    eprintln!(
        "stokehold: WARNING: {} {why}; using {} only. Jobs of a still-running old `heavy` are NOT seen by this queue. \
         After they finish, fix with: mv {} {}.bak && ln -s {} {}",
        old.display(),
        new.display(),
        old.display(),
        old.display(),
        new.display(),
        old.display()
    );
}

/// Makes `old` (the legacy `heavy` dir) a symlink to `new`, on every run, idempotently. Never deletes data.
///
/// Race safety: all new binaries serialize on a flock on `<parent>/.stokehold-migrate.lock`. An old binary cannot
/// take that lock, so the order is chosen so its window is minimal and detected: the symlink is built under a
/// temp name first and renamed over `old` (atomic) right after `old` is renamed away. If an old binary slips in
/// and recreates a real `old` in that gap, the final rename fails (cannot replace a directory), and we take the
/// `Split` path: its job files are moved (never overwritten) into `new/jobs`, and a loud warning with the exact
/// fix command is printed. Either way both queues are never silently split.
pub fn migrate_legacy(old: &Path, new: &Path) -> Migration {
    use std::os::unix::fs::symlink;
    let parent = old.parent().unwrap_or_else(|| Path::new("."));
    let _ = fs::create_dir_all(parent);
    // Held until return; if the lock file cannot be opened we proceed unlocked (best effort).
    let _lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join(".stokehold-migrate.lock"))
        .ok()
        .and_then(|f| Flock::lock(f, FlockArg::LockExclusive).ok());
    let points_at_new = |old: &Path| match fs::read_link(old) {
        Ok(t) => {
            t == new
                || (t.is_relative() && old.parent().map(|p| p.join(&t)).as_deref() == Some(new))
        }
        Err(_) => false,
    };
    let link_over_old = |old: &Path| -> bool {
        let tmp = old.with_extension(format!("link-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        symlink(new, &tmp).is_ok()
            && fs::rename(&tmp, old)
                .map_err(|_| {
                    let _ = fs::remove_file(&tmp);
                })
                .is_ok()
    };
    match fs::symlink_metadata(old) {
        Err(_) => {
            let _ = fs::create_dir_all(new);
            if link_over_old(old) {
                Migration::LinkCreated
            } else {
                Migration::Split
            }
        }
        Ok(m) if m.file_type().is_symlink() => {
            if points_at_new(old) {
                Migration::Linked
            } else {
                warn_split(old, new, "is a symlink pointing elsewhere");
                Migration::Split
            }
        }
        Ok(m) if m.is_dir() && fs::symlink_metadata(new).is_err() => {
            if fs::rename(old, new).is_err() {
                return Migration::Split;
            }
            if link_over_old(old) {
                return Migration::Migrated;
            }
            // An old binary recreated `old` in the gap: fold its jobs in and warn.
            merge_jobs(old, new);
            warn_split(old, new, "was recreated by an old binary during migration");
            Migration::Split
        }
        Ok(m) if m.is_dir() => {
            merge_jobs(old, new);
            warn_split(old, new, "and the new dir are both real directories");
            Migration::Split
        }
        Ok(_) => {
            warn_split(old, new, "is not a directory or symlink");
            Migration::Split
        }
    }
}

/// Moves job files from `old/jobs` into `new/jobs`, never overwriting. Leaves anything that collides behind.
fn merge_jobs(old: &Path, new: &Path) {
    let dest = new.join("jobs");
    let _ = fs::create_dir_all(&dest);
    let Ok(entries) = fs::read_dir(old.join("jobs")) else {
        return;
    };
    for e in entries.flatten() {
        let to = dest.join(e.file_name());
        if fs::symlink_metadata(&to).is_err() {
            let _ = fs::rename(e.path(), to);
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateDir {
    base: PathBuf,
}

impl StateDir {
    /// `STOKEHOLD_STATE_DIR` (or the legacy `HEAVY_STATE_DIR`), else `~/.local/state/stokehold`.
    ///
    /// The tool was called `heavy` until 0.3.x and kept its state in `~/.local/state/heavy`. On every run
    /// `migrate_legacy` makes sure that path is a symlink to the new dir (migrating, creating or warning), so
    /// history carries over and a still-running old `heavy` binary follows the link to the same `jobs/` and
    /// lock files (flock is per inode, so both binaries admit against one queue).
    ///
    /// # Errors
    /// Fails when there is neither the variable nor `HOME`.
    pub fn from_env() -> Result<Self, Error> {
        if let Some(dir) = env_var("STOKEHOLD_STATE_DIR") {
            return Ok(Self { base: dir.into() });
        }
        let home =
            std::env::var_os("HOME").ok_or_else(|| Error::Usage("HOME is not set".into()))?;
        let base = PathBuf::from(home).join(".local/state");
        let (new, old) = (base.join("stokehold"), base.join("heavy"));
        migrate_legacy(&old, &new);
        Ok(Self { base: new })
    }

    #[cfg(test)]
    #[must_use]
    pub const fn at(base: PathBuf) -> Self {
        Self { base }
    }

    #[must_use]
    pub fn jobs_dir(&self) -> PathBuf {
        self.base.join("jobs")
    }

    /// Where the monthly event logs live.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// When the last machine sample was written (ms since the epoch; 0 for never). Shared by every heavy process.
    #[must_use]
    pub fn last_sample_ms(&self) -> i64 {
        fs::read_to_string(self.base.join("last_sample"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Call with the lock held.
    pub fn set_last_sample_ms(&self, ms: i64) {
        let _ = fs::write(self.base.join("last_sample"), ms.to_string());
    }

    /// Recent machine-wide CPU readings `(ms since the epoch, busy %)`, oldest first. Every heavy process that measures
    /// the machine adds one, so a waiter that has only just arrived still sees the last seconds, not one 250 ms window.
    #[must_use]
    pub fn busy_readings(&self) -> Vec<(i64, u64)> {
        let text = fs::read_to_string(self.base.join("busy")).unwrap_or_default();
        text.lines()
            .filter_map(|l| {
                let (ts, pct) = l.split_once(' ')?;
                Some((ts.parse().ok()?, pct.parse().ok()?))
            })
            .collect()
    }

    /// Call with the lock held: records a reading and keeps only those from the last `keep_ms`.
    pub fn push_busy(&self, now_ms: i64, pct: u64, keep_ms: i64) {
        let mut all = self.busy_readings();
        all.push((now_ms, pct));
        all.retain(|(ts, _)| *ts <= now_ms && now_ms - *ts <= keep_ms);
        all.sort_unstable();
        let text = all
            .iter()
            .map(|(ts, pct)| format!("{ts} {pct}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tmp = self.base.join(".busy.tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(tmp, self.base.join("busy"));
        }
    }

    #[must_use]
    pub fn history_path(&self) -> PathBuf {
        self.base.join("history.jsonl")
    }

    /// # Errors
    /// Fails when the directories cannot be created.
    pub fn ensure(&self) -> Result<(), Error> {
        fs::create_dir_all(self.jobs_dir())?;
        Ok(())
    }

    /// Takes the machine-wide lock; it is released when the guard drops (or the process dies).
    ///
    /// # Errors
    /// Fails when the lock file cannot be opened or locked.
    pub fn lock(&self) -> Result<Flock<File>, Error> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.base.join("lock"))?;
        Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, e)| Error::Sys(e))
    }

    /// Atomic write: temp file, then rename, so watchers never see half a file.
    ///
    /// # Errors
    /// Fails on I/O or serialization errors.
    pub fn write_job(&self, job: &Job) -> Result<(), Error> {
        let dir = self.jobs_dir();
        let tmp = dir.join(format!(".{}.tmp", job.id));
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(job)?)?;
        fs::rename(tmp, dir.join(format!("{}.json", job.id)))?;
        Ok(())
    }

    pub fn remove_job(&self, id: &str) {
        let _ = fs::remove_file(self.jobs_dir().join(format!("{id}.json")));
        let _ = fs::remove_file(self.job_lock_path(id));
    }

    fn job_lock_path(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.lock"))
    }

    /// Creates and locks the job's own lock file. The caller holds the guard for the job's whole life; the kernel drops
    /// the lock when the process dies. The file is opened close-on-exec, so the job's children never inherit (and
    /// never keep holding) it: a server the job left behind cannot keep a dead job alive.
    ///
    /// # Errors
    /// Fails when the lock file cannot be opened or locked.
    pub fn hold_job_lock(&self, id: &str) -> Result<Flock<File>, Error> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.job_lock_path(id))?;
        Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| Error::Sys(e))
    }

    /// Is the job's process alive? Its lock file decides when there is one: held means alive, takeable means its
    /// holder is gone. Without one (a job of an older binary), the pid and its start time decide (`is_alive`).
    #[must_use]
    pub fn job_alive(&self, job: &Job) -> bool {
        match job_lock_held(&self.job_lock_path(&job.id)) {
            Some(held) => held,
            None => is_alive(job),
        }
    }

    #[must_use]
    pub fn read_job(&self, id: &str) -> Option<Job> {
        serde_json::from_slice(&fs::read(self.jobs_dir().join(format!("{id}.json"))).ok()?).ok()
    }

    /// Every readable job file, oldest first.
    #[must_use]
    pub fn read_jobs(&self) -> Vec<Job> {
        let Ok(rd) = fs::read_dir(self.jobs_dir()) else {
            return Vec::new();
        };
        let mut jobs: Vec<Job> = rd
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name().to_str().is_some_and(|n| {
                    !n.starts_with('.') && Path::new(n).extension().is_some_and(|x| x == "json")
                })
            })
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect();
        jobs.sort_by(|a: &Job, b: &Job| (a.enqueued_ms, &a.id).cmp(&(b.enqueued_ms, &b.id)));
        jobs
    }

    /// Removes leftover `.tmp` files of writers that died mid-write. Writers hold the lock, so with the lock
    /// held every `.tmp` file is stale.
    fn sweep_tmp(&self) {
        let Ok(rd) = fs::read_dir(self.jobs_dir()) else {
            return;
        };
        for e in rd.filter_map(Result::ok) {
            if e.file_name().to_str().is_some_and(|n| {
                n.starts_with('.') && Path::new(n).extension().is_some_and(|x| x == "tmp")
            }) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// Removes lock files whose job file is gone and whose holder is dead (call with the lock held: a job takes its
    /// lock and writes its file under it, so a live job never has a lock file without a job file then).
    fn sweep_locks(&self) {
        let Ok(rd) = fs::read_dir(self.jobs_dir()) else {
            return;
        };
        for e in rd.filter_map(Result::ok) {
            let path = e.path();
            if path.extension().is_some_and(|x| x == "lock")
                && !path.with_extension("json").exists()
                && job_lock_held(&path) == Some(false)
            {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// Reads the jobs and deletes the files of dead ones, stale `.tmp` files and orphaned lock files (call with the
    /// lock held). A dead job never keeps a reservation: it is gone from the result.
    #[must_use]
    pub fn live_jobs(&self) -> Vec<Job> {
        self.sweep_tmp();
        let (alive, dead): (Vec<Job>, Vec<Job>) = self
            .read_jobs()
            .into_iter()
            .partition(|j| self.job_alive(j));
        for j in dead {
            self.remove_job(&j.id);
        }
        self.sweep_locks();
        alive
    }
}

/// Whether a job lock file is held by a live process: `Some(true)` held, `Some(false)` takeable (its holder is dead, or
/// it never had one), `None` when there is no lock file or it cannot be probed. Flock conflicts between separate opens
/// even within one process, so a job sees its own lock as held.
fn job_lock_held(path: &Path) -> Option<bool> {
    let file = OpenOptions::new().write(true).open(path).ok()?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(_probe) => Some(false),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Some(true),
        Err(_) => None,
    }
}

/// The fallback for a job file without a lock file: alive when its pid exists, is no zombie (an exited process its
/// parent has not reaped still answers `kill(pid, 0)`), and started when the job file says (pid reuse). A failed probe
/// of the process start (here or when the file was written) never makes a live job look dead.
#[must_use]
pub fn is_alive(job: &Job) -> bool {
    let Ok(pid) = i32::try_from(job.pid) else {
        return false;
    };
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => {}
        Err(_) => return false,
    }
    match process_probe(job.pid) {
        Some(ProcessProbe { zombie: true, .. }) => false,
        Some(ProcessProbe { start, .. }) => {
            job.start_time == START_UNKNOWN || start == job.start_time
        }
        // sysinfo lists no zombies on macOS: ask `ps`, which does.
        None => ps_says_alive(job.pid),
    }
}

/// `ps -o stat= -p <pid>`: alive unless `ps` ran and lists no such process or a zombie (`Z`). A `ps` that cannot run
/// never makes a live job look dead.
fn ps_says_alive(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) => {
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Capacity, State, Weights};
    use crate::machine::process_start;

    pub fn job(id: &str, pid: u32, start: u64, state: State) -> Job {
        Job {
            id: id.into(),
            pid,
            start_time: start,
            weights: Weights::default(),
            cmd: vec!["x".into()],
            cwd: "/".into(),
            state,
            enqueued_ms: 1,
            started_ms: None,
            blocked_by: None,
            mem_declared: false,
            mem_peak_at_ms: None,
            cpu_hold_ms: None,
            cpu_settled: false,
            non_heavy_cpu_at_start: 0,
        }
    }

    fn dir() -> (tempfile::TempDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::at(t.path().to_path_buf());
        s.ensure().unwrap();
        (t, s)
    }

    fn mk(p: &Path) {
        fs::create_dir_all(p.join("jobs")).unwrap();
    }

    fn paths() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (old, new) = (t.path().join("heavy"), t.path().join("stokehold"));
        (t, old, new)
    }

    #[test]
    fn migrate_missing_old_creates_symlink() {
        let (_t, old, new) = paths();
        mk(&new);
        assert_eq!(migrate_legacy(&old, &new), Migration::LinkCreated);
        assert_eq!(fs::read_link(&old).unwrap(), new);
    }

    #[test]
    fn migrate_missing_both_creates_new_and_symlink() {
        let (_t, old, new) = paths();
        assert_eq!(migrate_legacy(&old, &new), Migration::LinkCreated);
        assert!(new.is_dir() && fs::read_link(&old).unwrap() == new);
    }

    #[test]
    fn migrate_existing_symlink_is_noop_and_idempotent() {
        let (_t, old, new) = paths();
        mk(&new);
        migrate_legacy(&old, &new);
        assert_eq!(migrate_legacy(&old, &new), Migration::Linked);
        assert_eq!(migrate_legacy(&old, &new), Migration::Linked);
    }

    #[test]
    fn migrate_real_old_moves_data() {
        let (_t, old, new) = paths();
        mk(&old);
        fs::write(old.join("jobs/a.json"), "a").unwrap();
        assert_eq!(migrate_legacy(&old, &new), Migration::Migrated);
        assert_eq!(fs::read_to_string(new.join("jobs/a.json")).unwrap(), "a");
        assert_eq!(fs::read_link(&old).unwrap(), new);
        assert_eq!(migrate_legacy(&old, &new), Migration::Linked);
    }

    #[test]
    fn migrate_both_real_merges_jobs_without_deleting() {
        let (_t, old, new) = paths();
        mk(&old);
        mk(&new);
        fs::write(old.join("jobs/a.json"), "old-a").unwrap();
        fs::write(old.join("jobs/b.json"), "old-b").unwrap();
        fs::write(new.join("jobs/b.json"), "new-b").unwrap();
        assert_eq!(migrate_legacy(&old, &new), Migration::Split);
        assert_eq!(
            fs::read_to_string(new.join("jobs/a.json")).unwrap(),
            "old-a"
        );
        assert_eq!(
            fs::read_to_string(new.join("jobs/b.json")).unwrap(),
            "new-b",
            "no overwrite"
        );
        assert_eq!(
            fs::read_to_string(old.join("jobs/b.json")).unwrap(),
            "old-b",
            "collision left behind"
        );
        assert!(
            old.is_dir() && !fs::symlink_metadata(&old).unwrap().file_type().is_symlink(),
            "old dir untouched"
        );
    }

    #[test]
    fn migrate_symlink_elsewhere_warns_and_changes_nothing() {
        let (t, old, new) = paths();
        mk(&new);
        let other = t.path().join("other");
        mk(&other);
        std::os::unix::fs::symlink(&other, &old).unwrap();
        assert_eq!(migrate_legacy(&old, &new), Migration::Split);
        assert_eq!(fs::read_link(&old).unwrap(), other);
    }

    #[test]
    fn migrate_recreated_old_during_gap_is_merged_not_lost() {
        // Simulates the gap: the symlink cannot replace a real dir, so we land in Split with data kept.
        let (_t, old, new) = paths();
        mk(&new);
        mk(&old);
        fs::write(old.join("jobs/x.json"), "x").unwrap();
        assert_eq!(migrate_legacy(&old, &new), Migration::Split);
        assert!(new.join("jobs/x.json").exists());
    }

    #[test]
    fn empty_stokehold_env_does_not_hide_heavy() {
        let os = |v: &str| Some(std::ffi::OsString::from(v));
        assert_eq!(prefer(os(""), || os("legacy")), os("legacy"));
        assert_eq!(prefer(os("new"), || os("legacy")), os("new"));
        assert_eq!(prefer(None, || os("")), None);
    }

    #[test]
    fn busy_readings_keep_only_the_window() {
        let (_t, s) = dir();
        assert_eq!(s.busy_readings(), vec![]);
        s.push_busy(1_000, 100, 20_000);
        s.push_busy(15_000, 98, 20_000);
        s.push_busy(25_000, 60, 20_000);
        assert_eq!(
            s.busy_readings(),
            vec![(15_000, 98), (25_000, 60)],
            "the reading from 24 s ago is dropped"
        );
    }

    #[test]
    fn stale_jobs_are_dropped_live_ones_kept() {
        let (_t, s) = dir();
        let me = std::process::id();
        let start = process_start(me).unwrap();
        s.write_job(&job("alive", me, start, State::Running))
            .unwrap();
        s.write_job(&job("reused-pid", me, start + 10_000, State::Running))
            .unwrap();
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        s.write_job(&job("dead", dead, 1, State::Queued)).unwrap();
        let ids: Vec<String> = s.live_jobs().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["alive".to_string()]);
        assert_eq!(s.read_jobs().len(), 1, "dead files are deleted from disk");
    }

    #[test]
    fn unknown_start_time_is_alive_while_the_pid_exists_and_tmp_files_are_swept() {
        let (_t, s) = dir();
        s.write_job(&job(
            "unknown",
            std::process::id(),
            START_UNKNOWN,
            State::Running,
        ))
        .unwrap();
        std::fs::write(s.jobs_dir().join(".stale.tmp"), "{").unwrap();
        let ids: Vec<String> = s.live_jobs().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["unknown".to_string()]);
        assert!(!s.jobs_dir().join(".stale.tmp").exists());
    }

    /// A process that holds `path`'s flock until killed, like a job's stokehold. Perl is on macOS and every Linux here.
    fn lock_holder(path: &Path) -> std::process::Child {
        let mut child = std::process::Command::new("perl")
            .args([
                "-e",
                "use Fcntl qw(:flock); open(my $f, '>>', $ARGV[0]) or die; flock($f, LOCK_EX) or die; print qq(ok\n); \
                 $| = 1; sleep 600",
            ])
            .arg(path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        assert_eq!(line.trim(), "ok", "the holder has the lock");
        child
    }

    fn ids(s: &StateDir) -> Vec<String> {
        s.live_jobs().into_iter().map(|j| j.id).collect()
    }

    #[test]
    fn a_sigkilled_job_is_reaped_at_once_and_its_reservation_released() {
        // 2026-11-20 on the Mac: `stokehold status` listed `running 50052 16 cores make dev` with no pid 50052.
        let (_t, s) = dir();
        let lock = s.job_lock_path("phantom");
        let mut holder = lock_holder(&lock);
        let pid = holder.id();
        let start = process_start(pid).unwrap();
        let phantom = Job {
            weights: Weights {
                cpu: 16,
                mem: 4 << 30,
                ..Weights::default()
            },
            started_ms: Some(1_000),
            ..job("phantom", pid, start, State::Running)
        };
        s.write_job(&phantom).unwrap();
        // A whole-machine waiter: a 1-core one would backfill next to the phantom's held peak.
        let waiter = Job {
            weights: Weights {
                cpu: 16,
                ..Weights::default()
            },
            ..job("waiter", std::process::id(), START_UNKNOWN, State::Queued)
        };
        let cap = Capacity {
            cpu: 16,
            mem: 64 << 30,
            vram: 0,
            disk_total: 1 << 40,
            vram_is_ram: true,
        };
        let live = crate::job::Live {
            load1: 1.0,
            disk_free: 1 << 39,
            ..crate::job::Live::default()
        };
        let decide = |jobs: &[Job]| {
            let mut all = jobs.to_vec();
            all.push(waiter.clone());
            crate::admit::evaluate(&waiter, &all, &cap, &live, 2_000)
        };
        assert_eq!(
            ids(&s),
            vec!["phantom".to_string()],
            "alive while its lock is held"
        );
        assert!(matches!(
            decide(&s.live_jobs()),
            crate::admit::Decision::Wait { .. }
        ));
        holder.kill().unwrap();
        // Not waited for yet: the process is a zombie, which still answers kill(pid, 0). The kernel dropped its lock.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ids(&s).is_empty() {
            assert!(std::time::Instant::now() < deadline, "never reaped");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            s.read_jobs().is_empty() && !lock.exists(),
            "job and lock files deleted"
        );
        assert_eq!(
            decide(&s.live_jobs()),
            crate::admit::Decision::Start,
            "reservation released"
        );
        holder.wait().unwrap();
    }

    #[test]
    fn the_job_lock_decides_over_a_reused_pid() {
        let (_t, s) = dir();
        let me = std::process::id();
        let start = process_start(me).unwrap();
        // Our own pid and start time, as if the pid had been reused by a live process that started at that second:
        // the lock is free, so the job's process is gone.
        s.write_job(&job("reused", me, start, State::Running))
            .unwrap();
        drop(s.hold_job_lock("reused").unwrap());
        // A live job: its lock is held (by this process, through another open: flock conflicts anyway).
        let held = s.hold_job_lock("live").unwrap();
        s.write_job(&job("live", me, start, State::Running))
            .unwrap();
        // An old binary's job file (no lock file) whose pid now belongs to another process: the start time differs.
        s.write_job(&job("old-reused", me, start + 10_000, State::Running))
            .unwrap();
        assert_eq!(ids(&s), vec!["live".to_string()]);
        // An orphaned lock file (its job file gone, holder dead) is swept; a held one is kept.
        drop(s.hold_job_lock("orphan").unwrap());
        let _ = s.live_jobs();
        assert!(!s.job_lock_path("orphan").exists());
        assert!(s.job_lock_path("live").exists());
        drop(held);
        assert!(ids(&s).is_empty(), "released with its holder");
    }

    #[test]
    fn a_zombie_without_a_lock_file_is_dead() {
        let (_t, s) = dir();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ps_says_alive(pid) {
            assert!(std::time::Instant::now() < deadline, "never exited");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        s.write_job(&job("zombie", pid, START_UNKNOWN, State::Running))
            .unwrap();
        assert!(
            ids(&s).is_empty(),
            "an unreaped exited process is no live job"
        );
        child.wait().unwrap();
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let (_t, s) = dir();
        let a = s.lock().unwrap();
        let s2 = s.clone();
        let h = std::thread::spawn(move || drop(s2.lock().unwrap()));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!h.is_finished());
        drop(a);
        h.join().unwrap();
    }
}

#[cfg(test)]
mod migrate_tests {
    use super::*;

    #[test]
    fn old_dir_moves_and_leaves_a_symlink() {
        let t = tempfile::tempdir().unwrap();
        let (old, new) = (t.path().join("heavy"), t.path().join("stokehold"));
        fs::create_dir_all(old.join("jobs")).unwrap();
        fs::write(old.join("history.jsonl"), "x").unwrap();
        migrate_legacy(&old, &new);
        assert_eq!(fs::read_to_string(new.join("history.jsonl")).unwrap(), "x");
        assert!(fs::symlink_metadata(&old).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(old.join("history.jsonl")).unwrap(), "x");
        migrate_legacy(&old, &new); // second run changes nothing
        assert!(new.join("jobs").is_dir());
    }
}
