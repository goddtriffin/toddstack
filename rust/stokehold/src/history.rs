//! `history.jsonl`: one line per finished job; the median of past peaks (whole process tree) is the default weight (the max for the pooled last tier).
//!
//! Lookup goes from the most to the least specific match: the same command in the same cwd; else the same command in
//! the same place of the same repository (any worktree or clone of it: `repo`); else the same program and first
//! argument there (`make ui-check`, `cargo test`). Agents run each task in a fresh worktree, so keyed on the cwd alone
//! 75% of the Mac's jobs on 2026-11-20 were cold and held the whole machine for 10 s each, though their median peak
//! was 2 cores.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

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
    /// History version: 0 (absent) before 0.3.2, 1 from 0.3.2 on. On Linux, v0 peaks counted threads as child
    /// processes (about 17x inflated), so learned defaults ignore them there.
    #[serde(default)]
    pub v: u32,
    /// Where in which repository the job ran (`repo_key`): shared by every worktree of a repository. Absent before
    /// 0.4.1 and outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Identifies a place in a repository across its worktrees: the repository's common git dir, then the cwd relative
/// to the worktree root (`<common git dir>:<relative path>`). `None` outside git.
#[must_use]
pub fn repo_key(cwd: &Path) -> Option<String> {
    let root = cwd.ancestors().find(|d| d.join(".git").exists())?;
    let git = root.join(".git");
    let common = if git.is_dir() {
        git
    } else {
        // A worktree (or submodule): `.git` is a file naming its own git dir, whose `commondir` names the shared one.
        let text = fs::read_to_string(&git).ok()?;
        let own = root.join(text.trim().strip_prefix("gitdir:")?.trim());
        match fs::read_to_string(own.join("commondir")) {
            Ok(c) => own.join(c.trim()),
            Err(_) => own,
        }
    };
    let common = common.canonicalize().unwrap_or(common);
    let rel = cwd
        .strip_prefix(root)
        .map_or_else(|_| PathBuf::new(), Path::to_path_buf);
    Some(format!("{}:{}", common.display(), rel.display()))
}

/// The version written into new entries.
pub const VERSION: u32 = 1;

/// # Errors
/// Fails on I/O or serialization errors.
pub fn append(path: &Path, entry: &Entry) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    // One write call on an O_APPEND file: concurrent writers do not interleave.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)?;
    Ok(())
}

/// Defaults measured from past successful runs of the same command in the same cwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Defaults {
    pub cpu: u32,
    pub mem: u64,
    pub vram: Option<u64>,
    /// Median time (ms after the start) of the memory peak; `None` when no run recorded it.
    pub mem_peak_at_ms: Option<u64>,
    /// Median time (ms after the start) at which the CPU reached 3/4 of its peak; `None` when no run recorded it
    /// (history from before 0.4.3).
    pub cpu_peak_at_ms: Option<u64>,
    /// Median time (ms after the start) at which the CPU reached 3/4 of its peak: from then on a run whose CPU has
    /// stopped rising has shown its real use, and its hold is released (`admit::cpu_plateaued`).
    pub cpu_peak_median_ms: Option<u64>,
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

#[must_use]
pub fn defaults_from(
    entries: &[Entry],
    cmd: &[String],
    cwd: &str,
    repo: Option<&str>,
) -> Option<Defaults> {
    // macOS never counted threads as children (sysinfo reports no thread kind there), so old entries stay valid.
    defaults_with(
        entries,
        cmd,
        cwd,
        repo,
        if cfg!(target_os = "linux") {
            VERSION
        } else {
            0
        },
    )
}

/// How many leading words of the command the last lookup tier compares (`make ui-check`, `cargo test`).
const PROGRAM_WORDS: usize = 2;

/// Like `defaults_from`, using only entries whose version is at least `min_v`. Tries the same command in the same
/// cwd, then the same command in the same repository place, then the same program and first argument there.
#[must_use]
pub fn defaults_with(
    entries: &[Entry],
    cmd: &[String],
    cwd: &str,
    repo: Option<&str>,
    min_v: u32,
) -> Option<Defaults> {
    let ok: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.v >= min_v && e.exit_code == 0)
        .collect();
    let same_repo = |e: &Entry| repo.is_some() && e.repo.as_deref() == repo;
    let pick =
        |tier: &dyn Fn(&Entry) -> bool| ok.iter().copied().filter(|e| tier(e)).collect::<Vec<_>>();
    if let Some(d) = aggregate(&pick(&|e| e.cwd == cwd && e.cmd == cmd), median) {
        return Some(d);
    }
    if let Some(d) = aggregate(&pick(&|e| same_repo(e) && e.cmd == cmd), median) {
        return Some(d);
    }
    // Third tier pools different commands, so it takes the largest peak, never the median, and only for a real
    // program with a subcommand: not wrappers, not a bare program, not a flag first argument.
    if !program_pools(cmd) {
        return None;
    }
    let program = &cmd[..PROGRAM_WORDS];
    aggregate(
        &pick(&|e| same_repo(e) && e.cmd.starts_with(program)),
        |v| v.into_iter().max(),
    )
}

/// Wrappers and shells: what they run says nothing about the first argument's weight.
const WRAPPERS: [&str; 11] = [
    "env",
    "bash",
    "sh",
    "zsh",
    "sudo",
    "nice",
    "time",
    "nohup",
    "xargs",
    "stokehold",
    "heavy",
];

/// Whether `cmd` may use the pooled program + first argument tier: more than two words, the program is no wrapper,
/// the first argument is no flag.
fn program_pools(cmd: &[String]) -> bool {
    if cmd.len() <= PROGRAM_WORDS {
        return false;
    }
    let base = Path::new(&cmd[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&cmd[0]);
    !WRAPPERS.contains(&base) && !cmd[1].starts_with('-')
}

fn aggregate(mine: &[&Entry], pick: impl Fn(Vec<u64>) -> Option<u64>) -> Option<Defaults> {
    let mine = &mine[mine.len().saturating_sub(KEEP)..];
    // Peaks of the whole process tree; runs too short to sample (or old entries) fall back to the average
    // cores and the largest single process.
    let cores = pick(
        mine.iter()
            .map(|e| {
                if e.measured.peak_centi_cores > 0 {
                    e.measured.peak_centi_cores
                } else {
                    e.measured.centi_cores
                }
            })
            .collect(),
    )?;
    let mem = pick(
        mine.iter()
            .map(|e| e.measured.peak_tree_rss.max(e.measured.peak_rss))
            .collect(),
    )?;
    let vram = pick(mine.iter().filter_map(|e| e.measured.peak_vram).collect());
    let mem_peak_at_ms = pick(
        mine.iter()
            .map(|e| e.measured.peak_mem_at_ms)
            .filter(|ms| *ms > 0)
            .collect(),
    );
    // The slowest recent ramp, not the median: about half of all runs ramp later than their median, and a hold that
    // ended there let the next build in before their CPU showed.
    let ramps: Vec<u64> = mine
        .iter()
        .map(|e| e.measured.peak_cpu_at_ms)
        .filter(|ms| *ms > 0)
        .collect();
    let cpu_peak_at_ms = ramps.iter().copied().max();
    let cpu_peak_median_ms = median(ramps);
    Some(Defaults {
        cpu: u32::try_from(cores.div_ceil(100).max(1)).unwrap_or(u32::MAX),
        mem,
        vram,
        mem_peak_at_ms,
        cpu_peak_at_ms,
        cpu_peak_median_ms,
    })
}

/// Reads the history file (a missing or damaged one just means no defaults).
#[must_use]
pub fn defaults_for(
    path: &Path,
    cmd: &[String],
    cwd: &str,
    repo: Option<&str>,
) -> Option<Defaults> {
    let text = fs::read_to_string(path).ok()?;
    let entries: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    defaults_from(&entries, cmd, cwd, repo)
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
            measured: Stats {
                centi_cores: centi,
                peak_rss: rss,
                ..Stats::default()
            },
            exit_code: code,
            v: VERSION,
            repo: None,
        }
    }

    #[test]
    fn median_defaults_for_same_cmd_and_cwd() {
        let all = vec![
            entry("t", "/a", 150, 100, 0),
            entry("t", "/a", 350, 300, 0),
            entry("t", "/a", 250, 200, 0),
            entry("t", "/b", 900, 900, 0),
            entry("t", "/a", 900, 900, 1),
        ];
        let d = defaults_from(&all, &["t".to_string()], "/a", None).unwrap();
        assert_eq!(
            d,
            Defaults {
                cpu: 3,
                mem: 200,
                vram: None,
                mem_peak_at_ms: None,
                cpu_peak_at_ms: None,
                cpu_peak_median_ms: None
            }
        );
        assert!(defaults_from(&all, &["other".to_string()], "/a", None).is_none());
    }

    #[test]
    fn cpu_time_to_peak_is_the_slowest_of_the_runs_that_recorded_it() {
        let runs: Vec<Entry> = [0, 20_000, 30_000, 40_000]
            .into_iter()
            .map(|ms| {
                let mut e = entry("t", "/a", 100, 50, 0);
                e.measured.peak_cpu_at_ms = ms;
                e
            })
            .collect();
        let d = defaults_from(&runs, &["t".to_string()], "/a", None).unwrap();
        assert_eq!(d.cpu_peak_at_ms, Some(40_000), "0 (unknown) is left out");
        assert_eq!(
            d.cpu_peak_median_ms,
            Some(30_000),
            "the median of 20, 30 and 40 s"
        );
        let old = defaults_from(&runs[..1], &["t".to_string()], "/a", None).unwrap();
        assert_eq!(old.cpu_peak_at_ms, None);
    }

    #[test]
    fn defaults_use_tree_peaks_not_averages() {
        let mut e = entry("t", "/a", 100, 50, 0);
        e.measured.peak_centi_cores = 650;
        e.measured.peak_tree_rss = 900;
        let d = defaults_from(&[e], &["t".to_string()], "/a", None).unwrap();
        assert_eq!(
            d,
            Defaults {
                cpu: 7,
                mem: 900,
                vram: None,
                mem_peak_at_ms: None,
                cpu_peak_at_ms: None,
                cpu_peak_median_ms: None
            }
        );
    }

    #[test]
    fn v0_entries_are_ignored_when_min_version_is_one() {
        let mut old = entry("t", "/a", 100, 46_000, 0);
        old.v = 0;
        let new = entry("t", "/a", 100, 2_600, 0);
        let c = ["t".to_string()];
        assert_eq!(defaults_with(&[old.clone()], &c, "/a", None, 1), None);
        assert_eq!(
            defaults_with(&[old, new], &c, "/a", None, 1).unwrap().mem,
            2_600
        );
    }

    #[test]
    fn v0_entries_are_used_when_min_version_is_zero() {
        let mut old = entry("t", "/a", 100, 700, 0);
        old.v = 0;
        assert_eq!(
            defaults_with(&[old], &["t".to_string()], "/a", None, 0)
                .unwrap()
                .mem,
            700
        );
    }

    #[test]
    fn entries_without_a_version_field_parse_as_v0_and_new_ones_write_v1() {
        let mut value = serde_json::to_value(entry("t", "/a", 1, 1, 0)).unwrap();
        value.as_object_mut().unwrap().remove("v");
        assert_eq!(serde_json::from_value::<Entry>(value).unwrap().v, 0);
        let json = serde_json::to_string(&entry("t", "/a", 1, 1, 0)).unwrap();
        assert!(json.contains("\"v\":1"), "{json}");
    }

    #[test]
    fn append_writes_one_json_line() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/history.jsonl");
        append(&p, &entry("t", "/a", 100, 1, 0)).unwrap();
        append(&p, &entry("t", "/a", 100, 1, 0)).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 2);
        assert_eq!(
            defaults_for(&p, &["t".to_string()], "/a", None)
                .unwrap()
                .cpu,
            1
        );
    }

    fn in_repo(cmd: &[&str], cwd: &str, repo: &str, peak: u64) -> Entry {
        let mut e = entry("x", cwd, peak, 1 << 30, 0);
        e.cmd = cmd.iter().map(|s| (*s).to_string()).collect();
        e.measured.peak_centi_cores = peak;
        e.repo = Some(repo.into());
        e
    }

    #[test]
    fn a_fresh_worktree_learns_from_the_other_worktrees_of_its_repo() {
        // 2026-11-20 on the Mac: every agent task runs in a new worktree (wt216, wt225, ...), so keyed on the cwd 75% of
        // jobs were cold and held all 16 cores for 10 s, though their median peak was 2 cores.
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let bdg = "/code/bdg/.git:apple";
        let all = vec![
            in_repo(&["make", "ui-check"], "/s/wt216/apple", bdg, 200),
            in_repo(&["make", "ui-check"], "/s/wt218/apple", bdg, 300),
            in_repo(&["make", "ui-check"], "/s/wt219/apple", bdg, 250),
            in_repo(&["make", "lint"], "/s/wt216/apple", bdg, 900),
            in_repo(
                &["make", "ui-check"],
                "/s/wt9/web",
                "/code/other/.git:web",
                1600,
            ),
        ];
        // Same cwd first.
        assert_eq!(
            defaults_from(&all, &s(&["make", "ui-check"]), "/s/wt218/apple", Some(bdg))
                .unwrap()
                .cpu,
            3
        );
        // A new worktree of the same repo: the median of the same command there.
        assert_eq!(
            defaults_from(&all, &s(&["make", "ui-check"]), "/s/wt225/apple", Some(bdg))
                .unwrap()
                .cpu,
            3
        );
        // A command never run in this repo place, but the same program and first argument: `make ui-check SCREENS=a`.
        assert_eq!(
            defaults_from(
                &all,
                &s(&["make", "ui-check", "SCREENS=a"]),
                "/s/wt225/apple",
                Some(bdg)
            )
            .unwrap()
            .cpu,
            3
        );
        // Another repo, another directory or no repo at all stays cold.
        assert!(
            defaults_from(
                &all,
                &s(&["make", "ui-check"]),
                "/s/wt225/apple",
                Some("/code/bdg/.git:web")
            )
            .is_none()
        );
        assert!(defaults_from(&all, &s(&["make", "ui-check"]), "/s/wt225/apple", None).is_none());
        assert!(defaults_from(&all, &s(&["make", "test"]), "/s/wt225/apple", Some(bdg)).is_none());
    }

    #[test]
    fn pooled_tier_uses_the_max_not_the_median() {
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let r = "/c/.git:a";
        let all = vec![
            in_repo(&["cargo", "test", "-p", "a"], "/w1", r, 200),
            in_repo(&["cargo", "test", "-p", "b"], "/w1", r, 300),
            in_repo(&["cargo", "test", "-p", "c"], "/w1", r, 1600),
        ];
        assert_eq!(
            defaults_from(&all, &s(&["cargo", "test", "-p", "d"]), "/w2", Some(r))
                .unwrap()
                .cpu,
            16
        );
    }

    #[test]
    fn pooled_tier_skips_bare_programs_wrappers_and_flags() {
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let r = "/c/.git:a";
        let all = vec![
            in_repo(&["make", "lint"], "/w1", r, 100),
            in_repo(&["bash", "-c", "x"], "/w1", r, 100),
            in_repo(&["env", "A=1", "make"], "/w1", r, 100),
            in_repo(&["xcodebuild", "-scheme", "A"], "/w1", r, 100),
        ];
        let d = |c: &[&str]| defaults_from(&all, &s(c), "/w2", Some(r));
        assert!(d(&["make"]).is_none(), "a bare program is not pooled");
        assert!(d(&["make", "other"]).is_none(), "two words is not pooled");
        assert!(d(&["bash", "-c", "y"]).is_none());
        assert!(d(&["env", "A=1", "make", "lint"]).is_none());
        assert!(d(&["xcodebuild", "-scheme", "B"]).is_none());
        assert!(d(&["/usr/bin/sudo", "make", "lint"]).is_none());
        assert!(d(&["make", "lint", "X=1"]).is_some());
    }

    #[test]
    fn repo_key_is_shared_by_worktrees_and_absent_outside_git() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("repo");
        fs::create_dir_all(main.join(".git/worktrees/wt1")).unwrap();
        fs::create_dir_all(main.join("apple")).unwrap();
        fs::write(main.join(".git/worktrees/wt1/commondir"), "../..\n").unwrap();
        let wt = t.path().join("scratch/wt1");
        fs::create_dir_all(wt.join("apple")).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt1").display()),
        )
        .unwrap();
        let a = repo_key(&main.join("apple")).unwrap();
        assert_eq!(
            repo_key(&wt.join("apple")).unwrap(),
            a,
            "a worktree shares the main checkout's key"
        );
        assert!(a.ends_with(":apple"), "{a}");
        assert_ne!(
            repo_key(&wt).unwrap(),
            a,
            "the worktree root is another place in the repo"
        );
        assert_eq!(repo_key(t.path()), None);
        // Entries without the field (before 0.4.1) still parse.
        let mut value = serde_json::to_value(entry("t", "/a", 1, 1, 0)).unwrap();
        assert!(value.get("repo").is_none(), "absent repo is not written");
        value
            .as_object_mut()
            .unwrap()
            .insert("repo".into(), a.clone().into());
        assert_eq!(
            serde_json::from_value::<Entry>(value).unwrap().repo,
            Some(a)
        );
    }
}
