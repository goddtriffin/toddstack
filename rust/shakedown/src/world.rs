//! The world a screen is drawn in: the project's local server, started and stopped here. A fake world answers
//! `/api` from fixtures (see `ui.rs`); a seeded world is the real server on a throwaway per-checkout database.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use signal_hook::consts::{SIGINT, SIGTERM};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::Server;
use crate::error::Error;

/// The process group of the running server (0: none), so a signal can stop it from anywhere.
static GROUP: AtomicU32 = AtomicU32::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Stops the process group: TERM, a moment to leave, then KILL.
fn kill_group(pid: u32, mut wait_for: impl FnMut() -> bool) {
    let group = format!("-{pid}");
    let signal = |sig: &str| {
        let _ = Command::new("kill")
            .args([sig, "--", &group])
            .stderr(Stdio::null())
            .status();
    };
    signal("-TERM");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if wait_for() {
            // The shell is gone; anything left in its group gets the same signal.
            signal("-KILL");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    signal("-KILL");
}

/// Has SIGINT or SIGTERM arrived since `watch_signals`?
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// What one signal does while a world is alive and after it.
struct Hooks {
    /// True when the next signal must take its default action (no world is alive, or one signal was seen already).
    default_now: Arc<AtomicBool>,
}

fn hooks() -> &'static [Hooks; 2] {
    static HOOKS: OnceLock<[Hooks; 2]> = OnceLock::new();
    HOOKS.get_or_init(|| {
        // Actions run in the order registered: first "take the default if `default_now`", then note the signal,
        // then "from now on take the default". So the first signal during a world is only noted, and every other
        // one (a second Ctrl-C, or any signal outside a world) ends the process the way an unhandled one does.
        let one = |sig: i32| {
            let default_now = Arc::new(AtomicBool::new(true));
            let _ = signal_hook::flag::register_conditional_default(sig, default_now.clone());
            let _ = signal_hook::flag::register(sig, SEEN.clone());
            let _ = signal_hook::flag::register(sig, default_now.clone());
            Hooks { default_now }
        };
        [one(SIGINT), one(SIGTERM)]
    })
}

static SEEN: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));

/// Guards the signal handling for one world's life: dropping it puts SIGINT and SIGTERM back to their default
/// action (a signal handler cannot be uninstalled safely, so "back" means the handler takes the default action).
pub struct Signals {
    stop: Arc<AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

/// Listens for SIGINT and SIGTERM while the returned guard lives: the server's process group is stopped at once
/// (even while `start` is still waiting for it to be ready), and `interrupted()` turns true so the run ends. The
/// first signal is caught; a second, or any after the guard is dropped, takes the default action, so Ctrl-C always
/// ends the process. Every call starts clean, so a library caller can run twice.
#[must_use = "the signals are only watched while the guard lives"]
pub fn watch_signals() -> Signals {
    let hooks = hooks();
    SEEN.store(false, Ordering::SeqCst);
    INTERRUPTED.store(false, Ordering::SeqCst);
    for h in hooks {
        h.default_now.store(false, Ordering::SeqCst);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let watcher = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            if SEEN.load(Ordering::SeqCst) && !INTERRUPTED.swap(true, Ordering::SeqCst) {
                let pid = GROUP.load(Ordering::SeqCst);
                if pid != 0 {
                    // Not waited for here: the run ends through `interrupted()`, and Drop finishes the job.
                    let group = format!("-{pid}");
                    let _ = Command::new("kill")
                        .args(["-TERM", "--", &group])
                        .stderr(Stdio::null())
                        .status();
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    Signals {
        stop,
        watcher: Some(watcher),
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for h in hooks() {
            h.default_now.store(true, Ordering::SeqCst);
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.watcher.take() {
            let _ = w.join();
        }
    }
}

/// A running server. Dropping it stops the server and everything it started.
pub struct Running {
    child: Child,
    /// `http://127.0.0.1:<port>`.
    pub base: String,
}

/// A free local port (best effort: another process could take it before the server does).
fn free_port() -> Result<u16, Error> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| Error::Environment(format!("no free port: {e}")))
}

/// The per-checkout database name: the project plus a short hash of the checkout path, so two worktrees never
/// share a database (a shared one keeps the migrations another branch applied).
pub fn db_name(project: &str, root: &Path) -> String {
    let clean: String = project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    let short = digest.iter().take(4).fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    format!("shakedown_{clean}_{short}")
}

fn fill(template: &str, port: u16, db: &str) -> String {
    template
        .replace("{port}", &port.to_string())
        .replace("{db}", db)
}

/// `GET path`: the status code, or `None` if nothing answered.
fn status_of(port: u16, path: &str) -> Option<u16> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut head = [0_u8; 32];
    let n = s.read(&mut head).ok()?;
    let line = String::from_utf8_lossy(&head[..n]);
    line.split_whitespace().nth(1)?.parse().ok()
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Starts the server, waits until it is ready, then runs the seed command.
///
/// # Errors
///
/// The server exits or is not ready in time (with the end of its log), or the seed fails.
pub fn start(server: &Server, root: &Path, project: &str, out: &Path) -> Result<Running, Error> {
    std::fs::create_dir_all(out)
        .map_err(|e| Error::Environment(format!("{}: {e}", out.display())))?;
    let port = free_port()?;
    let db = db_name(project, root);
    let base = format!("http://127.0.0.1:{port}");
    let log: PathBuf = out.join("server.log");
    let log_file = std::fs::File::create(&log)
        .map_err(|e| Error::Environment(format!("{}: {e}", log.display())))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| Error::Environment(e.to_string()))?;
    let envs = [
        ("SHAKEDOWN", "1".to_string()),
        ("SHAKEDOWN_PORT", port.to_string()),
        ("SHAKEDOWN_DB", db.clone()),
        ("SHAKEDOWN_BASE_URL", base.clone()),
        ("SHAKEDOWN_NOW", server.now.clone()),
    ];
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(fill(&server.start, port, &db))
        .current_dir(root)
        .envs(envs.clone())
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        // Its own process group, so stopping it also stops what `make` or `cargo run` started.
        .process_group(0);
    let child = cmd
        .spawn()
        .map_err(|e| Error::Environment(format!("cannot start the server: {e}")))?;
    GROUP.store(child.id(), Ordering::SeqCst);
    let mut running = Running { child, base };
    let deadline = Instant::now() + Duration::from_secs(server.timeout);
    loop {
        if interrupted() {
            return Err(Error::Failed("interrupted".into()));
        }
        if let Ok(Some(status)) = running.child.try_wait() {
            return Err(Error::Environment(format!(
                "the server exited ({status}) before it was ready; end of target/shakedown/web/server.log:\n{}",
                tail(&log, 15)
            )));
        }
        if status_of(port, &server.ready).is_some_and(|s| s < 500) {
            break;
        }
        if Instant::now() > deadline {
            return Err(Error::Environment(format!(
                "the server was not ready on {} after {}s; end of target/shakedown/web/server.log:\n{}",
                server.ready,
                server.timeout,
                tail(&log, 15)
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(seed) = &server.seed {
        let o = Command::new("sh")
            .arg("-c")
            .arg(fill(seed, port, &db))
            .current_dir(root)
            .envs(envs)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Environment(format!("cannot run the seed: {e}")))?;
        if !o.status.success() {
            let text = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
            let lines: Vec<&str> = text.lines().collect();
            return Err(Error::Environment(format!(
                "the seed command failed ({}):\n{}",
                o.status,
                lines[lines.len().saturating_sub(15)..].join("\n")
            )));
        }
    }
    Ok(running)
}

impl Drop for Running {
    fn drop(&mut self) {
        let pid = self.child.id();
        kill_group(pid, || matches!(self.child.try_wait(), Ok(Some(_))));
        let _ = self.child.wait();
        let _ = GROUP.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_names_are_stable_per_checkout_and_safe() {
        let a = db_name("accounts-service", Path::new("/tmp/a"));
        assert_eq!(a, db_name("accounts-service", Path::new("/tmp/a")));
        assert_ne!(a, db_name("accounts-service", Path::new("/tmp/b")));
        assert!(a.starts_with("shakedown_accounts_service_"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    fn send(sig: &str, pid: u32) {
        let _ = Command::new("kill").args([sig, &pid.to_string()]).status();
    }

    fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Runs the named child test below in its own process, so a signal that takes its default action cannot
    /// end the test run.
    fn child(name: &str, mode: &str) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("SHAKEDOWN_SIGNAL_CHILD", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn lines(
        child: &mut std::process::Child,
    ) -> std::io::Lines<std::io::BufReader<std::process::ChildStdout>> {
        use std::io::BufRead as _;
        std::io::BufReader::new(child.stdout.take().unwrap()).lines()
    }

    fn wait_for_line(
        lines: &mut std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
        line: &str,
    ) {
        loop {
            let got = lines.next().expect("the child ended early").unwrap();
            if got.ends_with(line) {
                return;
            }
        }
    }

    #[test]
    fn a_second_run_in_the_same_process_works_after_a_signal_ended_the_first() {
        // Not a child: it signals itself only while the guard lives, which the guard turns into a flag.
        let first = watch_signals();
        send("-TERM", std::process::id());
        wait_until("the first signal is seen", interrupted);
        drop(first);
        let second = watch_signals();
        assert!(
            !interrupted(),
            "a new run starts clean, not already interrupted"
        );
        send("-INT", std::process::id());
        wait_until("the second run sees its own signal", interrupted);
        drop(second);
    }

    #[test]
    fn a_signal_child_for_the_tests_below() {
        let Ok(mode) = std::env::var("SHAKEDOWN_SIGNAL_CHILD") else {
            return;
        };
        let guard = watch_signals();
        println!("ready");
        match mode.as_str() {
            "after-cleanup" => {
                drop(guard);
                println!("cleaned");
            }
            "twice" => {
                wait_until("first signal", interrupted);
                println!("first");
            }
            other => panic!("unknown mode {other}"),
        }
        // A bounded wait, so a child that wrongly survives a signal is a failing test, not a hang.
        std::thread::sleep(Duration::from_secs(8));
        println!("survived");
    }

    #[test]
    fn a_signal_after_cleanup_takes_the_default_action_and_ends_the_process() {
        use std::os::unix::process::ExitStatusExt as _;
        for sig in ["-TERM", "-INT"] {
            let mut c = child(
                "world::tests::a_signal_child_for_the_tests_below",
                "after-cleanup",
            );
            let mut out = lines(&mut c);
            wait_for_line(&mut out, "cleaned");
            send(sig, c.id());
            let status = c.wait().unwrap();
            assert!(
                status.signal().is_some(),
                "{sig} after cleanup kills the process: {status:?}"
            );
        }
    }

    #[test]
    fn a_second_signal_during_a_run_takes_the_default_action() {
        use std::os::unix::process::ExitStatusExt as _;
        let mut c = child("world::tests::a_signal_child_for_the_tests_below", "twice");
        let mut out = lines(&mut c);
        wait_for_line(&mut out, "ready");
        send("-INT", c.id());
        wait_for_line(&mut out, "first");
        send("-INT", c.id());
        let status = c.wait().unwrap();
        assert!(
            status.signal().is_some(),
            "a second Ctrl-C ends the process: {status:?}"
        );
    }

    #[test]
    fn placeholders_are_filled() {
        assert_eq!(fill("run {port} {db} {port}", 80, "d"), "run 80 d 80");
    }
}
