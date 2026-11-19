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
use crate::job::Job;
use crate::machine::process_start;

#[derive(Debug, Clone)]
pub struct StateDir {
    base: PathBuf,
}

impl StateDir {
    /// `HEAVY_STATE_DIR`, else `~/.local/state/heavy`.
    ///
    /// # Errors
    /// Fails when there is neither the variable nor `HOME`.
    pub fn from_env() -> Result<Self, Error> {
        if let Some(dir) = std::env::var_os("HEAVY_STATE_DIR") {
            return Ok(Self { base: dir.into() });
        }
        let home = std::env::var_os("HOME").ok_or_else(|| Error::Usage("HOME is not set".into()))?;
        Ok(Self { base: PathBuf::from(home).join(".local/state/heavy") })
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

    /// Reads the jobs and deletes the files of dead ones (call with the lock held).
    #[must_use]
    pub fn live_jobs(&self) -> Vec<Job> {
        let (alive, dead): (Vec<Job>, Vec<Job>) = self.read_jobs().into_iter().partition(is_alive);
        for j in dead {
            self.remove_job(&j.id);
        }
        alive
    }
}

/// A job is alive when its pid exists and that process started when the job file says.
#[must_use]
pub fn is_alive(job: &Job) -> bool {
    let Ok(pid) = i32::try_from(job.pid) else { return false };
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => {}
        Err(_) => return false,
    }
    process_start(job.pid).is_some_and(|t| t == job.start_time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{State, Weights};

    pub fn job(id: &str, pid: u32, start: u64, state: State) -> Job {
        Job { id: id.into(), pid, start_time: start, weights: Weights::default(), cmd: vec!["x".into()], cwd: "/".into(), state, enqueued_ms: 1, started_ms: None }
    }

    fn dir() -> (tempfile::TempDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::at(t.path().to_path_buf());
        s.ensure().unwrap();
        (t, s)
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
