//! End-to-end tests of the `heavy` binary with a temp state dir and overridden capacity numbers.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn stokehold(state: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_stokehold"));
    c.env("STOKEHOLD_STATE_DIR", state)
        .env("STOKEHOLD_CAPACITY", "cpu=8,mem=16G,vram=0,disk=100G")
        .env(
            "STOKEHOLD_LIVE",
            "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=0,nonheavy_mem=0,nonheavy_vram=0",
        )
        .env_remove("STOKEHOLD_JOB")
        .env_remove("STOKEHOLD_TIMEOUT")
        .env_remove("HEAVY_JOB")
        .env_remove("HEAVY_TIMEOUT")
        .env_remove("HEAVY_STATE_DIR")
        .env_remove("HEAVY_KILL_GRACE_MS")
        .env_remove("HEAVY_CAPACITY")
        .env_remove("HEAVY_LIVE");
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
    assert!(
        Command::new("kill")
            .args([sig, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

/// Starts heavy on a shell script that prints `ready` once its trap is installed; returns after that.
fn start_ready(
    state: &Path,
    extra: &[&str],
    script: &str,
) -> (Child, BufReader<std::process::ChildStdout>) {
    let mut child = stokehold(state)
        .args(extra)
        .args(["sh", "-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    (child, out)
}

#[test]
fn exit_code_passes_through() {
    let t = TempDir::new().unwrap();
    assert_eq!(
        code(&run(stokehold(t.path()).args(["sh", "-c", "exit 7"]))),
        7
    );
    assert_eq!(code(&run(stokehold(t.path()).args(["true"]))), 0);
    assert_eq!(
        code(&run(stokehold(t.path()).args(["--", "sh", "-c", "exit 3"]))),
        3
    );
    assert_eq!(
        code(&run(
            stokehold(t.path()).args(["definitely-not-a-command-xyz"])
        )),
        127
    );
}

#[test]
fn stats_go_to_stderr_with_review_prompt() {
    let t = TempDir::new().unwrap();
    let o = run(stokehold(t.path()).args(["sh", "-c", "echo hi"]));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "hi\n");
    let err = stderr(&o);
    for needle in [
        "stokehold: ran ",
        "avg ",
        "peak rss",
        "disk blocks read",
        "network (machine-wide)",
        "Review these stats",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }
    assert!(
        err.trim_end().ends_with("file an issue to fix it."),
        "{err}"
    );
    let history = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
    assert_eq!(history.lines().count(), 1);
    assert!(
        history.contains("\"exit_code\":0")
            && history.contains("\"cmd\":[\"sh\",\"-c\",\"echo hi\"]"),
        "{history}"
    );
    assert!(
        std::fs::read_dir(t.path().join("jobs"))
            .unwrap()
            .next()
            .is_none(),
        "job file removed"
    );
}

#[test]
fn history_drives_default_weights() {
    let t = TempDir::new().unwrap();
    for _ in 0..2 {
        assert_eq!(code(&run(stokehold(t.path()).args(["true"]))), 0);
    }
    let history = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
    assert!(
        history.lines().last().unwrap().contains("\"cpu\":1"),
        "second run defaults to the measured 1 core: {history}"
    );
}

#[test]
fn nested_call_execs_directly_under_a_live_job() {
    let t = TempDir::new().unwrap();
    // Outer holds the whole machine; the nested call would wait forever if it queued.
    let o = run(stokehold(t.path())
        .args(["--cpu", "8", "sh", "-c"])
        .arg(format!(
            "{} --cpu 8 echo nested",
            env!("CARGO_BIN_EXE_stokehold")
        )));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("nested"));
    // Only the outer job left stats: the nested call exec'd without queueing or reporting.
    assert_eq!(
        stderr(&o).matches("Review these stats").count(),
        1,
        "{}",
        stderr(&o)
    );
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
    let o = run(stokehold(t.path()).args(["--cpu", "8", "true"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!stderr(&o).contains("waiting"), "{}", stderr(&o));
    assert!(std::fs::read_dir(&jobs).unwrap().next().is_none());
}

#[test]
fn second_big_job_waits_for_the_first_fifo() {
    let t = TempDir::new().unwrap();
    let log = t.path().join("order.log");
    let script = |name: &str, sleep: &str| {
        format!(
            "echo start-{name} >> {p}; sleep {sleep}; echo end-{name} >> {p}",
            p = log.display()
        )
    };
    let a = stokehold(t.path())
        .args(["--mem", "10G", "sh", "-c", &script("a", "1")])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Wait until A is running (its job file says so), so B really queues behind it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.exists() {
        assert!(Instant::now() < deadline, "a never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let b = run(stokehold(t.path()).args(["--mem", "10G", "sh", "-c", &script("b", "0")]));
    assert_eq!(code(&b), 0);
    assert!(stderr(&b).contains("waiting"), "b queued: {}", stderr(&b));
    let _ = a.wait_with_output().unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(
        lines,
        ["start-a", "end-a", "start-b", "end-b"],
        "never overlapped"
    );
}

#[test]
fn small_jobs_run_together_with_no_job_cap() {
    let t = TempDir::new().unwrap();
    let kids: Vec<Child> = (0..6)
        .map(|_| {
            stokehold(t.path())
                .args(["--cpu", "1", "--mem", "1M", "sleep", "1"])
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let start = Instant::now();
    for k in kids {
        let o = k.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        // Racing waiters let the oldest one go first ("which can start now"); nothing else may hold them.
        assert!(
            err.lines()
                .filter(|l| l.contains("waiting"))
                .all(|l| l.contains("which can start now")),
            "{err}"
        );
    }
    assert!(start.elapsed() < Duration::from_secs(4), "ran in parallel");
}

#[test]
fn non_heavy_usage_blocks_a_second_job_but_never_the_first() {
    let t = TempDir::new().unwrap();
    // Other programs use 7 of 8 cores and the running job holds its 1 core while it ramps: no core is free.
    let busy = "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=700";
    let mut a = stokehold(t.path())
        .env("STOKEHOLD_LIVE", busy)
        .args(["--cpu", "1", "sleep", "3"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(t.path().join("jobs")).map_or(true, |mut d| d.next().is_none()) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut b = stokehold(t.path())
        .env("STOKEHOLD_LIVE", busy)
        .args(["--cpu", "2", "true"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        b.try_wait().unwrap().is_none(),
        "b must still be waiting while a runs"
    );
    a.wait().unwrap();
    let o = b.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("waiting"));
    assert_eq!(code(&o), 0);
}

#[test]
fn sigterm_is_forwarded_to_the_process_group() {
    let t = TempDir::new().unwrap();
    let (child, _out) = start_ready(
        t.path(),
        &[],
        "trap 'exit 7' TERM; echo ready; sleep 30 & wait",
    );
    kill(&child, "-TERM");
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 7, "child saw TERM and exited 7");
}

#[test]
fn ctrl_c_sigint_is_forwarded_and_hup_too() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let (child, _out) = start_ready(
        t.path(),
        &[],
        "trap 'exit 9' INT; echo ready; sleep 30 & wait",
    );
    kill(&child, "-INT");
    assert_eq!(code(&child.wait_with_output().unwrap()), 9);
    let (child, _out) = start_ready(
        t.path(),
        &[],
        "trap 'exit 11' HUP; echo ready; sleep 30 & wait",
    );
    kill(&child, "-HUP");
    assert_eq!(code(&child.wait_with_output().unwrap()), 11);
    // The leftover `sleep 30` held the pipes open before; the group is killed after the child exits.
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "leftovers were not killed: {:?}",
        start.elapsed()
    );
}

/// Job files in `<state>/jobs`, leaving out the per-job `.lock` files.
fn job_files(state: &std::path::Path) -> usize {
    std::fs::read_dir(state.join("jobs"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json"))
        })
        .count()
}

#[test]
fn signal_while_queued_removes_the_job_file() {
    let t = TempDir::new().unwrap();
    let (holder, _out) = start_ready(t.path(), &["--mem", "10G"], "echo ready; sleep 30");
    let mut waiter = stokehold(t.path())
        .args(["--mem", "10G", "true"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while job_files(t.path()) < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    kill(&waiter, "-TERM");
    assert_eq!(waiter.wait().unwrap().code(), Some(143));
    assert_eq!(job_files(t.path()), 1, "only the holder's file is left");
    kill(&holder, "-TERM");
    let _ = holder.wait_with_output();
}

#[test]
fn timeout_sends_term_then_exits_124() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(stokehold(t.path()).args([
        "--timeout",
        "1s",
        "sh",
        "-c",
        "trap 'exit 0' TERM; sleep 30 & wait",
    ]));
    assert_eq!(code(&o), 124, "{}", stderr(&o));
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "TERM was enough, no grace wait"
    );
    assert!(stderr(&o).contains("TIMED OUT"), "{}", stderr(&o));
}

#[test]
fn timeout_escalates_to_sigkill_after_the_grace_period() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(stokehold(t.path())
        .env("STOKEHOLD_KILL_GRACE_MS", "500")
        .args([
            "--timeout",
            "1s",
            "sh",
            "-c",
            "trap '' TERM; while :; do sleep 1; done",
        ]));
    assert_eq!(code(&o), 124, "{}", stderr(&o));
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(1400) && took < Duration::from_secs(10),
        "{took:?}"
    );
    assert!(stderr(&o).contains("TIMED OUT"));
}

#[test]
fn leftover_background_processes_are_killed_when_the_child_exits() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(stokehold(t.path()).args(["sh", "-c", "sleep 30 & exit 0"]));
    assert_eq!(code(&o), 0);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn full_disk_exits_75_at_once() {
    let t = TempDir::new().unwrap();
    let start = Instant::now();
    let o = run(stokehold(t.path())
        .env(
            "STOKEHOLD_LIVE",
            "load=0.1,mem_pct=10,disk_free=5G,nonheavy_cpu=0",
        )
        .args(["--disk", "20G", "true"]));
    assert_eq!(code(&o), 75, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("disk is full") && stderr(&o).contains("ask Todd"),
        "{}",
        stderr(&o)
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(
        std::fs::read_dir(t.path().join("jobs"))
            .unwrap()
            .next()
            .is_none(),
        "job file removed"
    );
}

#[test]
fn history_records_tree_peaks() {
    let t = TempDir::new().unwrap();
    let o = run(stokehold(t.path()).args(["sh", "-c", "sleep 1"]));
    assert_eq!(code(&o), 0);
    let history = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
    assert!(
        history.contains("\"peak_tree_rss\":") && !history.contains("\"peak_tree_rss\":0,"),
        "{history}"
    );
    assert!(stderr(&o).contains("whole process tree"), "{}", stderr(&o));
}

#[test]
fn status_lists_running_jobs() {
    let t = TempDir::new().unwrap();
    let (holder, _out) = start_ready(t.path(), &["--cpu", "2"], "echo ready; sleep 30");
    let o = run(stokehold(t.path()).arg("status"));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        text.contains("running") && text.contains("sleep 30") && text.contains("capacity"),
        "{text}"
    );
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
    let stokehold_bin = env!("CARGO_BIN_EXE_stokehold");
    let inner = "echo on-a-tty; test -t 0 && echo stdin-is-tty; exit 5";
    let mut c = if cfg!(target_os = "macos") {
        let mut c = Command::new("script");
        c.args(["-q", "/dev/null", stokehold_bin, "sh", "-c", inner]);
        c
    } else {
        let mut c = Command::new("script");
        c.args([
            "-qec",
            &format!("{stokehold_bin} sh -c '{inner}'"),
            "/dev/null",
        ]);
        c
    };
    c.env("STOKEHOLD_STATE_DIR", t.path())
        .env("STOKEHOLD_CAPACITY", "cpu=8,mem=16G,vram=0,disk=100G")
        .env(
            "STOKEHOLD_LIVE",
            "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=0,nonheavy_mem=0,nonheavy_vram=0",
        )
        .env_remove("STOKEHOLD_JOB")
        .env_remove("HEAVY_JOB")
        .env_remove("HEAVY_STATE_DIR")
        .env_remove("HEAVY_CAPACITY")
        .env_remove("HEAVY_LIVE")
        .stdin(Stdio::null());
    let o = c.output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("on-a-tty") && out.contains("stdin-is-tty"),
        "{out}"
    );
    assert_eq!(code(&o), 5, "{out}");
}

/// A head stopped with Ctrl-Z while waiting must wake the waiter behind it at once (a file event), not after the
/// 30 s sweep.
#[test]
fn stopped_waiting_head_wakes_the_follower_at_once() {
    let t = TempDir::new().unwrap();
    // Holder (5 of 8 cores) has just started and settles for 10 s. The head (2 cores) does not fit under the 80%
    // line (6.4 cores) beside it until then; the follower (1 core) would, but waits behind the settle-blocked head.
    let (holder, _out) = start_ready(
        t.path(),
        &["--cpu", "5", "--mem", "1G"],
        "echo ready; sleep 60",
    );
    let spawn = |extra: &[&str]| {
        stokehold(t.path())
            .args(extra)
            .args(["true"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut head = spawn(&["--cpu", "2", "--mem", "1G"]);
    let mut head_err = BufReader::new(head.stderr.take().unwrap());
    let mut line = String::new();
    head_err.read_line(&mut line).unwrap();
    assert!(line.contains("waiting"), "{line}");
    let mut follower = spawn(&["--cpu", "1", "--mem", "1G"]);
    let mut follower_err = BufReader::new(follower.stderr.take().unwrap());
    line.clear();
    follower_err.read_line(&mut line).unwrap();
    assert!(line.contains("waiting"), "{line}");
    let start = Instant::now();
    kill(&head, "-TSTP");
    let status = follower.wait().unwrap();
    let elapsed = start.elapsed();
    let mut rest = String::new();
    let _ = follower_err.read_to_string(&mut rest);
    assert!(status.success(), "{line}{rest}");
    assert!(
        elapsed < Duration::from_secs(10),
        "follower stalled for {elapsed:?}: {line}{rest}"
    );
    // The head resumes and is queued again, then is torn down.
    kill(&head, "-CONT");
    kill(&head, "-TERM");
    let _ = head.wait();
    kill(&holder, "-TERM");
    let _ = holder.wait_with_output();
}

fn event_lines(state: &Path) -> Vec<serde_json::Value> {
    let mut lines = Vec::new();
    for e in std::fs::read_dir(state).unwrap().map(Result::unwrap) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("history-")
            && Path::new(&name).extension().is_some_and(|x| x == "jsonl")
        {
            lines.extend(
                std::fs::read_to_string(e.path())
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap()),
            );
        }
    }
    lines
}

#[test]
fn status_flag_is_an_alias_of_status() {
    let t = TempDir::new().unwrap();
    let a = run(stokehold(t.path()).arg("status"));
    let b = run(stokehold(t.path()).arg("--status"));
    assert_eq!(code(&b), 0, "{}", stderr(&b));
    assert!(String::from_utf8_lossy(&b.stdout).starts_with("capacity"));
    assert_eq!(code(&a), 0);
}

#[test]
fn events_and_a_sample_are_recorded_and_history_reads_them() {
    let t = TempDir::new().unwrap();

    let o = run(stokehold(t.path()).args(["sh", "-c", "sleep 1.5; exit 3"]));
    assert_eq!(code(&o), 3);
    let lines = event_lines(t.path());
    let of = |k: &str| lines.iter().filter(|l| l["event"] == k).collect::<Vec<_>>();
    let (queued, started, finished) = (of("queued"), of("started"), of("finished"));
    assert_eq!(
        (queued.len(), started.len(), finished.len()),
        (1, 1, 1),
        "{lines:?}"
    );
    assert_eq!(queued[0]["id"], finished[0]["id"]);
    assert_eq!(queued[0]["command"], "sh -c");
    assert!(
        queued[0]["cwd"].is_string()
            && queued[0]["pid"].is_u64()
            && queued[0]["cpu"].is_u64()
            && queued[0]["mem"].is_u64()
    );
    assert!(started[0]["wait_ms"].is_u64() && started[0]["reasons"].is_array());
    assert_eq!(finished[0]["exit"], 3);
    assert!(
        finished[0]["wall_ms"].as_u64().unwrap() >= 1400
            && finished[0]["cpu_ms"].is_u64()
            && finished[0]["peak_rss"].is_u64()
    );
    assert!(
        !of("sample").is_empty(),
        "a job that runs for a while is sampled: {lines:?}"
    );
    let o = run(stokehold(t.path()).args(["history", "--since", "1h"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        text.contains("jobs 1,")
            && text.contains("idle while waiting")
            && text.contains("cpu-hours"),
        "{text}"
    );
    let html = t.path().join("h.html");
    let o = run(stokehold(t.path()).args(["history", "--html"]).arg(&html));
    assert_eq!(code(&o), 0);
    assert!(
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .ends_with("h.html")
    );
    let page = std::fs::read_to_string(&html).unwrap();
    assert!(
        page.contains("<svg") && !page.contains("<script") && !page.contains("http"),
        "no external anything"
    );
}

#[test]
fn a_waiter_records_why_it_waited() {
    let t = TempDir::new().unwrap();
    // Non-heavy usage blocks the second job (see the test above); the first still runs alone.
    let mut first = stokehold(t.path());
    first.env(
        "STOKEHOLD_LIVE",
        "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=700,nonheavy_mem=0,nonheavy_vram=0",
    );
    let mut a = first
        .args(["--cpu", "1", "sleep", "2"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let mut second = stokehold(t.path());
    second.env(
        "STOKEHOLD_LIVE",
        "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=700,nonheavy_mem=0,nonheavy_vram=0",
    );
    let o = run(second.args(["--cpu", "1", "true"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    a.wait().unwrap();
    let lines = event_lines(t.path());
    let waited: Vec<_> = lines
        .iter()
        .filter(|l| l["event"] == "started" && l["wait_ms"].as_u64().unwrap() > 500)
        .collect();
    assert_eq!(waited.len(), 1, "{lines:?}");
    assert!(
        !waited[0]["reasons"].as_array().unwrap().is_empty(),
        "{waited:?}"
    );
}

#[test]
fn works_through_a_symlink_named_heavy() {
    let t = tempfile::tempdir().unwrap();
    let link = t.path().join("heavy");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_stokehold"), &link).unwrap();
    let mut c = Command::new(&link);
    c.env("STOKEHOLD_STATE_DIR", t.path().join("state"))
        .env("STOKEHOLD_CAPACITY", "cpu=8,mem=16G,vram=0,disk=100G")
        .env(
            "STOKEHOLD_LIVE",
            "load=0.1,mem_pct=10,disk_free=90G,nonheavy_cpu=0,nonheavy_mem=0,nonheavy_vram=0",
        )
        .args(["sh", "-c", "exit 4"]);
    assert_eq!(code(&run(&mut c)), 4);
}
