//! `history.jsonl`: one line per finished job; the median of past runs is the default weight.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::job::Weights;
use crate::stats::Stats;

const KEEP: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub ts: Timestamp,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub requested: Weights,
    pub measured: Stats,
    pub exit_code: i32,
}

/// # Errors
/// Fails on I/O or serialization errors.
pub fn append(path: &Path, entry: &Entry) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    // One write call on an O_APPEND file: concurrent writers do not interleave.
    OpenOptions::new().create(true).append(true).open(path)?.write_all(&line)?;
    Ok(())
}

/// Defaults measured from past successful runs of the same command in the same cwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Defaults {
    pub cpu: u32,
    pub mem: u64,
    pub vram: Option<u64>,
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

#[must_use]
pub fn defaults_from(entries: &[Entry], cmd: &[String], cwd: &str) -> Option<Defaults> {
    let mine: Vec<&Entry> = entries.iter().filter(|e| e.exit_code == 0 && e.cwd == cwd && e.cmd == cmd).collect();
    let mine = &mine[mine.len().saturating_sub(KEEP)..];
    let cores = median(mine.iter().map(|e| e.measured.centi_cores).collect())?;
    let mem = median(mine.iter().map(|e| e.measured.peak_rss).collect())?;
    let vram = median(mine.iter().filter_map(|e| e.measured.peak_vram).collect());
    Some(Defaults { cpu: u32::try_from(cores.div_ceil(100).max(1)).unwrap_or(u32::MAX), mem, vram })
}

/// Reads the history file (a missing or damaged one just means no defaults).
#[must_use]
pub fn defaults_for(path: &Path, cmd: &[String], cwd: &str) -> Option<Defaults> {
    let text = fs::read_to_string(path).ok()?;
    let entries: Vec<Entry> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    defaults_from(&entries, cmd, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cmd: &str, cwd: &str, centi: u64, rss: u64, code: i32) -> Entry {
        Entry {
            ts: Timestamp::UNIX_EPOCH,
            cmd: vec![cmd.into()],
            cwd: cwd.into(),
            requested: Weights::default(),
            measured: Stats { centi_cores: centi, peak_rss: rss, ..Stats::default() },
            exit_code: code,
        }
    }

    #[test]
    fn median_defaults_for_same_cmd_and_cwd() {
        let all = vec![entry("t", "/a", 150, 100, 0), entry("t", "/a", 350, 300, 0), entry("t", "/a", 250, 200, 0), entry("t", "/b", 900, 900, 0), entry("t", "/a", 900, 900, 1)];
        let d = defaults_from(&all, &["t".to_string()], "/a").unwrap();
        assert_eq!(d, Defaults { cpu: 3, mem: 200, vram: None });
        assert!(defaults_from(&all, &["other".to_string()], "/a").is_none());
    }

    #[test]
    fn append_writes_one_json_line() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/history.jsonl");
        append(&p, &entry("t", "/a", 100, 1, 0)).unwrap();
        append(&p, &entry("t", "/a", 100, 1, 0)).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 2);
        assert_eq!(defaults_for(&p, &["t".to_string()], "/a").unwrap().cpu, 1);
    }
}
