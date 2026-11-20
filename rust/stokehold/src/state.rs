//! The shared state dir: one JSON file per job, coordinated with `flock`.
//!
//! The kernel drops a flock when its holder dies, so there is no stale-mutex reaping. Dead jobs (pid plus
//! start time no longer match) are removed whenever someone holds the lock.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;

use crate::error::Error;
use crate::job::{Job, START_UNKNOWN};
use crate::machine::process_start;

/// Reads `STOKEHOLD_<X>`, falling back to the legacy `HEAVY_<X>` (pass the `STOKEHOLD_` name).
#[must_use]
pub fn env_var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).or_else(|| std::env::var_os(name.replacen("STOKEHOLD_", "HEAVY_", 1)))
}

/// Moves a real `old` directory to `new` and leaves a symlink `old -> new`. Does nothing when `new` exists or
/// `old` is missing or already a symlink. Losing a race to another process is harmless (the rename just fails).
fn migrate_legacy(old: &Path, new: &Path) {
    let is_real_dir = fs::symlink_metadata(old).is_ok_and(|m| m.is_dir());
    if !is_real_dir || fs::symlink_metadata(new).is_ok() {
        return;
    }
    if fs::rename(old, new).is_ok() {
        let _ = std::os::unix::fs::symlink(new, old);
    }
}

#[derive(Debug, Clone)]
pub struct StateDir {
    base: PathBuf,
}

impl StateDir {
    /// `STOKEHOLD_STATE_DIR` (or the legacy `HEAVY_STATE_DIR`), else `~/.local/state/stokehold`.
    ///
    /// The tool was called `heavy` until 0.3.x and kept its state in `~/.local/state/heavy`. On the first run
    /// the old directory is renamed to the new one and a symlink is left at the old path, so history carries
    /// over and a still-running old `heavy` binary follows the link to the same `jobs/` and lock files (flock
    /// is per inode, so both binaries admit against one queue).
    ///
    /// # Errors
    /// Fails when there is neither the variable nor `HOME`.
    pub fn from_env() -> Result<Self, Error> {
        if let Some(dir) = env_var("STOKEHOLD_STATE_DIR") {
            return Ok(Self { base: dir.into() });
        }
        let home = std::env::var_os("HOME").ok_or_else(|| Error::Usage("HOME is not set".into()))?;
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
        fs::read_to_string(self.base.join("last_sample")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0)
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
        let text = all.iter().map(|(ts, pct)| format!("{ts} {pct}")).collect::<Vec<_>>().join("\n");
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
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(self.base.join("lock"))?;
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
    }

    #[must_use]
    pub fn read_job(&self, id: &str) -> Option<Job> {
        serde_json::from_slice(&fs::read(self.jobs_dir().join(format!("{id}.json"))).ok()?).ok()
    }

    /// Every readable job file, oldest first.
    #[must_use]
    pub fn read_jobs(&self) -> Vec<Job> {
        let Ok(rd) = fs::read_dir(self.jobs_dir()) else { return Vec::new() };
        let mut jobs: Vec<Job> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_str().is_some_and(|n| !n.starts_with('.') && Path::new(n).extension().is_some_and(|x| x == "json")))
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect();
        jobs.sort_by(|a: &Job, b: &Job| (a.enqueued_ms, &a.id).cmp(&(b.enqueued_ms, &b.id)));
        jobs
    }

    /// Removes leftover `.tmp` files of writers that died mid-write. Writers hold the lock, so with the lock
    /// held every `.tmp` file is stale.
    fn sweep_tmp(&self) {
        let Ok(rd) = fs::read_dir(self.jobs_dir()) else { return };
        for e in rd.filter_map(Result::ok) {
            if e.file_name().to_str().is_some_and(|n| n.starts_with('.') && Path::new(n).extension().is_some_and(|x| x == "tmp")) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// Reads the jobs and deletes the files of dead ones and stale `.tmp` files (call with the lock held).
    #[must_use]
    pub fn live_jobs(&self) -> Vec<Job> {
        self.sweep_tmp();
        let (alive, dead): (Vec<Job>, Vec<Job>) = self.read_jobs().into_iter().partition(is_alive);
        for j in dead {
            self.remove_job(&j.id);
        }
        alive
    }
}

/// A job is alive when its pid exists and that process started when the job file says. A failed probe of
/// the process start (here or when the file was written) never makes a live job look dead.
#[must_use]
pub fn is_alive(job: &Job) -> bool {
    let Ok(pid) = i32::try_from(job.pid) else { return false };
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => {}
        Err(_) => return false,
    }
    match process_start(job.pid) {
        Some(t) => job.start_time == START_UNKNOWN || t == job.start_time,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{State, Weights};

    pub fn job(id: &str, pid: u32, start: u64, state: State) -> Job {
        Job { id: id.into(), pid, start_time: start, weights: Weights::default(), cmd: vec!["x".into()], cwd: "/".into(), state, enqueued_ms: 1, started_ms: None, blocked_by: None }
    }

    fn dir() -> (tempfile::TempDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::at(t.path().to_path_buf());
        s.ensure().unwrap();
        (t, s)
    }

    #[test]
    fn busy_readings_keep_only_the_window() {
        let (_t, s) = dir();
        assert_eq!(s.busy_readings(), vec![]);
        s.push_busy(1_000, 100, 20_000);
        s.push_busy(15_000, 98, 20_000);
        s.push_busy(25_000, 60, 20_000);
        assert_eq!(s.busy_readings(), vec![(15_000, 98), (25_000, 60)], "the reading from 24 s ago is dropped");
    }

    #[test]
    fn stale_jobs_are_dropped_live_ones_kept() {
        let (_t, s) = dir();
        let me = std::process::id();
        let start = process_start(me).unwrap();
        s.write_job(&job("alive", me, start, State::Running)).unwrap();
        s.write_job(&job("reused-pid", me, start + 10_000, State::Running)).unwrap();
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
        s.write_job(&job("unknown", std::process::id(), START_UNKNOWN, State::Running)).unwrap();
        std::fs::write(s.jobs_dir().join(".stale.tmp"), "{").unwrap();
        let ids: Vec<String> = s.live_jobs().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["unknown".to_string()]);
        assert!(!s.jobs_dir().join(".stale.tmp").exists());
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
