//! The event log: `history-YYYY-MM.jsonl` (UTC) in the state dir. One JSON line per job event (queued, started,
//! finished) and per machine sample, read back by `stokehold history`. Five monthly files are kept.
//!
//! (`history.jsonl`, see `history.rs`, is separate: it holds the finished runs that set the default weights.)

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::admit::Block;
use crate::error::Error;
use crate::job::Weights;

/// Monthly files kept.
pub const KEEP_FILES: usize = 5;
/// A line must stay well under the 4 KB that a single `write()` to an `O_APPEND` file keeps atomic.
const MAX_LINE: usize = 4000;
const MAX_COMMAND: usize = 80;
const MAX_CWD: usize = 512;

/// Who a job event is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub pid: u32,
    pub cwd: String,
    /// The git toplevel's basename, if the cwd is in a repo.
    pub repo: Option<String>,
    /// `argv[0..2]` joined, at most 80 characters.
    pub command: String,
    /// Requested cores and RAM.
    pub cpu: u32,
    pub mem: u64,
}

impl JobInfo {
    #[must_use]
    pub fn new(id: &str, pid: u32, cwd: &str, cmd: &[String], w: Weights) -> Self {
        Self {
            id: id.into(),
            pid,
            cwd: cwd.chars().take(MAX_CWD).collect(),
            repo: repo_of(Path::new(cwd)),
            command: short_command(cmd),
            cpu: w.cpu,
            mem: w.mem,
        }
    }
}

/// Time spent blocked by one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonTime {
    pub reason: Block,
    pub ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    Queued {
        ts: Timestamp,
        #[serde(flatten)]
        job: JobInfo,
    },
    Started {
        ts: Timestamp,
        #[serde(flatten)]
        job: JobInfo,
        wait_ms: u64,
        reasons: Vec<ReasonTime>,
    },
    Finished {
        ts: Timestamp,
        #[serde(flatten)]
        job: JobInfo,
        exit: i32,
        wall_ms: u64,
        cpu_ms: u64,
        peak_rss: u64,
    },
    Sample {
        ts: Timestamp,
        cpu_pct: u64,
        mem_pct: u64,
        load1: f64,
        load5: f64,
        ncpu: u32,
        running: u32,
        waiting: u32,
        /// Waiting jobs by what blocks them right now.
        blocked: BTreeMap<String, u32>,
    },
}

impl Event {
    #[must_use]
    pub const fn ts(&self) -> Timestamp {
        match self {
            Self::Queued { ts, .. }
            | Self::Started { ts, .. }
            | Self::Finished { ts, .. }
            | Self::Sample { ts, .. } => *ts,
        }
    }
}

/// `argv[0..2]` joined with a space, cut to 80 characters.
#[must_use]
pub fn short_command(cmd: &[String]) -> String {
    cmd.iter()
        .take(2)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_COMMAND)
        .collect()
}

/// The basename of the nearest ancestor that holds a `.git` (a directory, or a file in a worktree).
#[must_use]
pub fn repo_of(cwd: &Path) -> Option<String> {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
}

/// `history-2026-11.jsonl`: the month in UTC.
#[must_use]
pub fn file_name(ts: Timestamp) -> String {
    format!("history-{}.jsonl", ts.strftime("%Y-%m"))
}

/// Appends one line: a single `write()` on an `O_APPEND` file, so concurrent processes never interleave.
///
/// # Errors
/// Fails on I/O or serialization errors.
pub fn append(dir: &Path, event: &Event) -> Result<(), Error> {
    fs::create_dir_all(dir)?;
    let mut line = serde_json::to_vec(event)?;
    if line.len() >= MAX_LINE {
        return Err(Error::Probe("history line too long".into()));
    }
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file_name(event.ts())))?
        .write_all(&line)?;
    Ok(())
}

/// Records an event; a failure only warns (history must never fail a job).
pub fn record(dir: &Path, event: &Event) {
    if let Err(e) = append(dir, event) {
        eprintln!("stokehold: could not write history: {e}");
    }
}

fn monthly_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("history-") && Path::new(n).extension().is_some_and(|x| x == "jsonl")
            })
        })
        .collect();
    files.sort();
    files
}

/// Deletes all but the newest `KEEP_FILES` monthly files (the names sort by month).
pub fn prune(dir: &Path) {
    let files = monthly_files(dir);
    for old in files.iter().take(files.len().saturating_sub(KEEP_FILES)) {
        let _ = fs::remove_file(old);
    }
}

/// Every parseable event at or after `since`, oldest first. Damaged lines are skipped.
#[must_use]
pub fn read_since(dir: &Path, since: Timestamp) -> Vec<Event> {
    let mut events: Vec<Event> = monthly_files(dir)
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .flat_map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Event>(l).ok())
                .collect::<Vec<_>>()
        })
        .filter(|e| e.ts() >= since)
        .collect();
    events.sort_by_key(Event::ts);
    events
}

/// Adds up how long a waiter was blocked by each check.
#[derive(Debug, Default)]
pub struct WaitTally {
    current: Option<(Block, i64)>,
    totals: BTreeMap<Block, u64>,
}

impl WaitTally {
    /// At `now_ms` the waiter is blocked by `next` (or by nothing). The time since the last note goes to the
    /// check that blocked then.
    pub fn note(&mut self, now_ms: i64, next: Option<Block>) {
        if let Some((block, since)) = self.current {
            *self.totals.entry(block).or_default() += u64::try_from(now_ms - since).unwrap_or(0);
        }
        self.current = next.map(|b| (b, now_ms));
    }

    #[must_use]
    pub fn reasons(&self) -> Vec<ReasonTime> {
        self.totals
            .iter()
            .map(|(reason, ms)| ReasonTime {
                reason: *reason,
                ms: *ms,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str) -> JobInfo {
        JobInfo {
            id: id.into(),
            pid: 7,
            cwd: "/w".into(),
            repo: Some("r".into()),
            command: "cargo test".into(),
            cpu: 4,
            mem: 1 << 30,
        }
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn lines_are_written_and_parsed() {
        let t = tempfile::tempdir().unwrap();
        let when = ts("2026-11-19T10:00:00Z");
        let events = vec![
            Event::Queued {
                ts: when,
                job: info("a"),
            },
            Event::Started {
                ts: when,
                job: info("a"),
                wait_ms: 1500,
                reasons: vec![ReasonTime {
                    reason: Block::Ramp,
                    ms: 1500,
                }],
            },
            Event::Finished {
                ts: when,
                job: info("a"),
                exit: 0,
                wall_ms: 9000,
                cpu_ms: 30_000,
                peak_rss: 1 << 20,
            },
            Event::Sample {
                ts: when,
                cpu_pct: 42,
                mem_pct: 50,
                load1: 3.5,
                load5: 2.0,
                ncpu: 16,
                running: 1,
                waiting: 2,
                blocked: BTreeMap::from([("cpu".to_string(), 2)]),
            },
        ];
        for e in &events {
            append(t.path(), e).unwrap();
        }
        let text = fs::read_to_string(t.path().join("history-2026-11.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 4);
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["event"], "queued");
        assert_eq!(first["repo"], "r");
        assert_eq!(first["command"], "cargo test");
        assert_eq!(read_since(t.path(), ts("2026-11-19T00:00:00Z")), events);
        assert_eq!(read_since(t.path(), ts("2026-11-20T00:00:00Z")).len(), 0);
    }

    #[test]
    fn command_and_cwd_are_cut_and_lines_stay_small() {
        let long = "x".repeat(500);
        let cmd = vec!["cargo".to_string(), long.clone(), "ignored".to_string()];
        let c = short_command(&cmd);
        assert_eq!(c.chars().count(), 80);
        assert!(c.starts_with("cargo xx"));
        let j = JobInfo::new("id", 1, &"/y".repeat(1000), &cmd, Weights::default());
        let line = serde_json::to_string(&Event::Queued {
            ts: Timestamp::UNIX_EPOCH,
            job: j,
        })
        .unwrap();
        assert!(line.len() < 1000, "{}", line.len());
    }

    #[test]
    fn months_split_by_utc_and_only_five_are_kept() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            file_name(ts("2026-11-30T23:59:59Z")),
            "history-2026-11.jsonl"
        );
        assert_eq!(
            file_name(ts("2026-12-01T00:00:00Z")),
            "history-2026-12.jsonl"
        );
        for m in 1..=7 {
            fs::write(t.path().join(format!("history-2026-{m:02}.jsonl")), "").unwrap();
        }
        fs::write(t.path().join("history.jsonl"), "").unwrap();
        prune(t.path());
        let mut names: Vec<String> = fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "history-2026-03.jsonl",
                "history-2026-04.jsonl",
                "history-2026-05.jsonl",
                "history-2026-06.jsonl",
                "history-2026-07.jsonl",
                "history.jsonl"
            ]
        );
    }

    #[test]
    fn repo_is_the_git_toplevel_basename() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("my-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("a/b")).unwrap();
        assert_eq!(repo_of(&repo.join("a/b")).as_deref(), Some("my-repo"));
    }

    #[test]
    fn wait_tally_attributes_time_to_the_blocking_check() {
        let mut w = WaitTally::default();
        w.note(1000, Some(Block::Queue));
        w.note(1400, Some(Block::Ramp));
        w.note(1500, Some(Block::Queue));
        w.note(2500, None);
        assert_eq!(
            w.reasons(),
            vec![
                ReasonTime {
                    reason: Block::Queue,
                    ms: 1400
                },
                ReasonTime {
                    reason: Block::Ramp,
                    ms: 100
                }
            ]
        );
    }
}
