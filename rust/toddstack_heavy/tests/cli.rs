//! End-to-end tests of the `heavy` binary with a temp state dir and overridden capacity numbers.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn heavy(state: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_heavy"));
    c.env("HEAVY_STATE_DIR", state)
        .env("HEAVY_CAPACITY", "cpu=8,mem=16G,vram=0,disk=100G")
        .env("HEAVY_LIVE", "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=0,nonheavy_mem=0,nonheavy_vram=0")
        .env_remove("HEAVY_JOB")
        .env_remove("HEAVY_TIMEOUT");
    c
}

fn run(c: &mut Command) -> Output {
    c.output().unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn kill(child: &Child, sig: &str) {
    assert!(Command::new("kill").args([sig, &child.id().to_string()]).status().unwrap().success());
}

/// Starts heavy on a shell script that prints `ready` once its trap is installed; returns after that.
fn start_ready(state: &Path, extra: &[&str], script: &str) -> (Child, BufReader<std::process::ChildStdout>) {
    let mut child = heavy(state).args(extra).args(["sh", "-c", script]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    (child, out)
}

#[test]
fn exit_code_passes_through() {
    let t = TempDir::new().unwrap();
    assert_eq!(code(&run(heavy(t.path()).args(["sh", "-c", "exit 7"]))), 7);
    assert_eq!(code(&run(heavy(t.path()).args(["true"]))), 0);
    assert_eq!(code(&run(heavy(t.path()).args(["--", "sh", "-c", "exit 3"]))), 3);
    assert_eq!(code(&run(heavy(t.path()).args(["definitely-not-a-command-xyz"]))), 127);
}

#[test]
fn stats_go_to_stderr_with_review_prompt() {
    let t = TempDir::new().unwrap();
    let o = run(heavy(t.path()).args(["sh", "-c", "echo hi"]));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "hi\n");
    let err = stderr(&o);
    for needle in ["heavy: ran ", "avg ", "peak rss", "disk blocks read", "network (machine-wide)", "Review these stats"] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }
    assert!(err.trim_end().ends_with("file an issue to fix it."), "{err}");
    let history = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
    assert_eq!(history.lines().count(), 1);
    assert!(history.contains("\"exit_code\":0") && history.contains("\"cmd\":[\"sh\",\"-c\",\"echo hi\"]"), "{history}");
    assert!(std::fs::read_dir(t.path().join("jobs")).unwrap().next().is_none(), "job file removed");
}

#[test]
fn history_drives_default_weights() {
    let t = TempDir::new().unwrap();
    for _ in 0..2 {
        assert_eq!(code(&run(heavy(t.path()).args(["true"]))), 0);
    }
    let history = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
    assert!(history.lines().last().unwrap().contains("\"cpu\":1"), "second run defaults to the measured 1 core: {history}");
}

#[test]
fn nested_call_execs_directly_under_a_live_job() {
    let t = TempDir::new().unwrap();
    // Outer holds the whole machine; the nested call would wait forever if it queued.
    let o = run(heavy(t.path()).args(["--cpu", "8", "sh", "-c"]).arg(format!("{} --cpu 8 echo nested", env!("CARGO_BIN_EXE_heavy"))));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("nested"));
    // Only the outer job left stats: the nested call exec'd without queueing or reporting.
    assert_eq!(stderr(&o).matches("Review these stats").count(), 1, "{}", stderr(&o));
}

#[test]
fn stale_job_files_are_cleaned_and_do_not_block() {
    let t = TempDir::new().unwrap();
    let jobs = t.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let job = format!(
        r#"{{"id":"0000000000001-{dead}","pid":{dead},"start_time":1,"weights":{{"cpu":8,"mem":17179869184,"vram":0,"disk":0}},"cmd":["x"],"cwd":"/","state":"running","enqueued_ms":1,"started_ms":1}}"#
    );
    std::fs::write(jobs.join(format!("0000000000001-{dead}.json")), job).unwrap();
    let start = Instant::now();
    let o = run(heavy(t.path()).args(["--cpu", "8", "true"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!stderr(&o).contains("waiting"), "{}", stderr(&o));
    assert!(std::fs::read_dir(&jobs).unwrap().next().is_none());
}

#[test]
fn second_big_job_waits_for_the_first_fifo() {
    let t = TempDir::new().unwrap();
    let log = t.path().join("order.log");
    let script = |name: &str, sleep: &str| format!("echo start-{name} >> {p}; sleep {sleep}; echo end-{name} >> {p}", p = log.display());
    let a = heavy(t.path()).args(["--mem", "10G", "sh", "-c", &script("a", "1")]).stderr(Stdio::null()).spawn().unwrap();
    // Wait until A is running (its job file says so), so B really queues behind it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.exists() {
        assert!(Instant::now() < deadline, "a never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let b = run(heavy(t.path()).args(["--mem", "10G", "sh", "-c", &script("b", "0")]));
    assert_eq!(code(&b), 0);
    assert!(stderr(&b).contains("waiting"), "b queued: {}", stderr(&b));
    let _ = a.wait_with_output().unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
    assert_eq!(lines, ["start-a", "end-a", "start-b", "end-b"], "never overlapped");
}

#[test]
fn small_jobs_run_together_with_no_job_cap() {
    let t = TempDir::new().unwrap();
    let kids: Vec<Child> = (0..6).map(|_| heavy(t.path()).args(["--cpu", "1", "--mem", "1M", "sleep", "1"]).stderr(Stdio::piped()).spawn().unwrap()).collect();
    let start = Instant::now();
    for k in kids {
        let o = k.wait_with_output().unwrap();
        assert!(!String::from_utf8_lossy(&o.stderr).contains("waiting"), "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert!(start.elapsed() < Duration::from_secs(4), "ran in parallel");
}

#[test]
fn non_heavy_usage_blocks_a_second_job_but_never_the_first() {
    let t = TempDir::new().unwrap();
    // Other programs use 6 of 8 cores: a running job plus 2 more cores does not fit under 90% (7.2).
    let busy = "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=600";
    let mut a = heavy(t.path()).env("HEAVY_LIVE", busy).args(["--cpu", "1", "sleep", "3"]).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(t.path().join("jobs")).map_or(true, |mut d| d.next().is_none()) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut b = heavy(t.path()).env("HEAVY_LIVE", busy).args(["--cpu", "2", "true"]).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(b.try_wait().unwrap().is_none(), "b must still be waiting while a runs");
    a.wait().unwrap();
    let o = b.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("waiting"));
    assert_eq!(code(&o), 0);
}

#[test]
fn sigterm_is_forwarded_to_the_process_group() {
    let t = TempDir::new().unwrap();
    let (child, _out) = start_ready(t.path(), &[], "trap 'exit 7' TERM; echo ready; sleep 30 & wait");
    kill(&child, "-TERM");
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 7, "child saw TERM and exited 7");
}

#[test]
fn ctrl_c_sigint_is_forwarded_and_hup_too() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let (child, _out) = start_ready(t.path(), &[], "trap 'exit 9' INT; echo ready; sleep 30 & wait");
    kill(&child, "-INT");
    assert_eq!(code(&child.wait_with_output().unwrap()), 9);
    let (child, _out) = start_ready(t.path(), &[], "trap 'exit 11' HUP; echo ready; sleep 30 & wait");
    kill(&child, "-HUP");
    assert_eq!(code(&child.wait_with_output().unwrap()), 11);
    // The leftover `sleep 30` held the pipes open before; the group is killed after the child exits.
    assert!(start.elapsed() < Duration::from_secs(10), "leftovers were not killed: {:?}", start.elapsed());
}

#[test]
fn signal_while_queued_removes_the_job_file() {
    let t = TempDir::new().unwrap();
    let (holder, _out) = start_ready(t.path(), &["--mem", "10G"], "echo ready; sleep 30");
    let mut waiter = heavy(t.path()).args(["--mem", "10G", "true"]).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(t.path().join("jobs")).unwrap().count() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    kill(&waiter, "-TERM");
    assert_eq!(waiter.wait().unwrap().code(), Some(143));
    assert_eq!(std::fs::read_dir(t.path().join("jobs")).unwrap().count(), 1, "only the holder's file is left");
    kill(&holder, "-TERM");
    let _ = holder.wait_with_output();
}

#[test]
fn timeout_sends_term_then_exits_124() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(heavy(t.path()).args(["--timeout", "1s", "sh", "-c", "trap 'exit 0' TERM; sleep 30 & wait"]));
    assert_eq!(code(&o), 124, "{}", stderr(&o));
    assert!(start.elapsed() < Duration::from_secs(8), "TERM was enough, no grace wait");
    assert!(stderr(&o).contains("TIMED OUT"), "{}", stderr(&o));
}

#[test]
fn timeout_escalates_to_sigkill_after_the_grace_period() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(heavy(t.path()).env("HEAVY_KILL_GRACE_MS", "500").args(["--timeout", "1s", "sh", "-c", "trap '' TERM; while :; do sleep 1; done"]));
    assert_eq!(code(&o), 124, "{}", stderr(&o));
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(1400) && took < Duration::from_secs(10), "{took:?}");
    assert!(stderr(&o).contains("TIMED OUT"));
}

#[test]
fn leftover_background_processes_are_killed_when_the_child_exits() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(heavy(t.path()).args(["sh", "-c", "sleep 30 & exit 0"]));
    assert_eq!(code(&o), 0);
    assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
}

#[test]
fn full_disk_exits_75_at_once() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(heavy(t.path()).env("HEAVY_LIVE", "load=0.1,mem_pct=10,disk_free=5G,nonheavy_cpu=0").args(["--disk", "20G", "true"]));
    assert_eq!(code(&o), 75, "{}", stderr(&o));
    assert!(stderr(&o).contains("disk is full") && stderr(&o).contains("ask Todd"), "{}", stderr(&o));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(std::fs::read_dir(t.path().join("jobs")).unwrap().next().is_none(), "job file removed");
}

#[test]
fn history_records_tree_peaks() {
    let t = TempDir::new().unwrap();
    let o = run(heavy(t.path()).args(["sh", "-c", "sleep 1"]));
    assert_eq!(code(&o), 0);
    let history = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
    assert!(history.contains("\"peak_tree_rss\":") && !history.contains("\"peak_tree_rss\":0,"), "{history}");
    assert!(stderr(&o).contains("whole process tree"), "{}", stderr(&o));
}

#[test]
fn status_lists_running_jobs() {
    let t = TempDir::new().unwrap();
    let (holder, _out) = start_ready(t.path(), &["--cpu", "2"], "echo ready; sleep 30");
    let o = run(heavy(t.path()).arg("status"));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("running") && text.contains("sleep 30") && text.contains("capacity"), "{text}");
    kill(&holder, "-TERM");
    let _ = holder.wait_with_output();
}

/// Runs heavy under a real pty (via `script`) so the terminal-foreground handoff path is exercised.
#[test]
fn runs_under_a_pty_and_passes_the_exit_code() {
    if Command::new("script").arg("--help").output().is_err() {
        return;
    }
    let t = TempDir::new().unwrap();
    let heavy_bin = env!("CARGO_BIN_EXE_heavy");
    let inner = "echo on-a-tty; test -t 0 && echo stdin-is-tty; exit 5";
    let mut c = if cfg!(target_os = "macos") {
        let mut c = Command::new("script");
        c.args(["-q", "/dev/null", heavy_bin, "sh", "-c", inner]);
        c
    } else {
        let mut c = Command::new("script");
        c.args(["-qec", &format!("{heavy_bin} sh -c '{inner}'"), "/dev/null"]);
        c
    };
    c.env("HEAVY_STATE_DIR", t.path())
        .env("HEAVY_CAPACITY", "cpu=8,mem=16G,vram=0,disk=100G")
        .env("HEAVY_LIVE", "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=0,nonheavy_mem=0,nonheavy_vram=0")
        .env_remove("HEAVY_JOB")
        .stdin(Stdio::null());
    let o = c.output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("on-a-tty") && out.contains("stdin-is-tty"), "{out}");
    assert_eq!(code(&o), 5, "{out}");
}
