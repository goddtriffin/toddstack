//! The world a screen is drawn in: the project's local server, started and stopped here. A fake world answers
//! `/api` from fixtures (see `ui.rs`); a seeded world is the real server on a throwaway per-checkout database.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};

use signal_hook::consts::{SIGINT, SIGTERM};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::Server;
use crate::error::Error;

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

/// What one signal does while a run is alive and after it.
struct Hooks {
    /// True when the next signal must take its default action (no run is alive, or one signal was seen already).
    default_now: Arc<AtomicBool>,
    /// The action that sets `default_now` after a signal; taken away while a run is shielded.
    latch: Mutex<Option<signal_hook::SigId>>,
    sig: i32,
}

impl Hooks {
    /// Makes a signal set `default_now` again (or stops doing so, for a shielded run).
    fn set_latch(&self, on: bool) {
        let mut latch = self.latch.lock().unwrap_or_else(PoisonError::into_inner);
        if on && latch.is_none() {
            *latch = signal_hook::flag::register(self.sig, self.default_now.clone()).ok();
        } else if !on && let Some(id) = latch.take() {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn hooks() -> &'static [Hooks; 2] {
    static HOOKS: OnceLock<[Hooks; 2]> = OnceLock::new();
    HOOKS.get_or_init(|| {
        // Actions run in the order registered: first "take the default if `default_now`", then note the signal,
        // then "from now on take the default". So the first signal during a run is only noted, and every other
        // one (a second Ctrl-C, or any signal outside a run) ends the process the way an unhandled one does.
        let one = |sig: i32| {
            let default_now = Arc::new(AtomicBool::new(true));
            let _ = signal_hook::flag::register_conditional_default(sig, default_now.clone());
            let _ = signal_hook::flag::register(sig, SEEN.clone());
            let latch = signal_hook::flag::register(sig, default_now.clone()).ok();
            Hooks {
                default_now,
                latch: Mutex::new(latch),
                sig,
            }
        };
        [one(SIGINT), one(SIGTERM)]
    })
}

static SEEN: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));

/// Counts the signals that were caught, so each run can tell "one arrived since I started" without a flag that
/// another run's start could reset.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Every live run shares this: how many there are, the server process groups they own, and the one thread that
/// turns a caught signal into stopping all of those groups.
struct Registry {
    live: usize,
    shields: usize,
    groups: Vec<u32>,
    watcher: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    live: 0,
    shields: 0,
    groups: Vec::new(),
    watcher: None,
});

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Guards the signal handling for one run's life. Dropping the last live guard puts SIGINT and SIGTERM back to
/// their default action (a signal handler cannot be uninstalled safely, so "back" means the handler takes the
/// default action).
pub struct Signals {
    /// The signal count when this run started: a bigger count now means a signal arrived during this run.
    since: u64,
    shielded: bool,
}

/// Listens for SIGINT and SIGTERM while a returned guard lives. Runs may overlap: the handling is installed when
/// the first guard is created and removed when the last one is dropped. A caught signal stops every registered
/// server group at once (even while `start` is still waiting for one to be ready) and `interrupted()` turns true
/// for every live run. The first signal is caught; a second, or any with no guard alive, takes the default
/// action, so Ctrl-C always ends the process.
#[must_use = "the signals are only watched while the guard lives"]
pub fn watch_signals() -> Signals {
    watch(false)
}

/// Like [`watch_signals`], but every signal during the run is only noted, never fatal: the run must still clean
/// up (smoke stops its container) however often Ctrl-C arrives. Outside the run the default action is back.
#[must_use = "the signals are only watched while the guard lives"]
pub fn watch_signals_shielded() -> Signals {
    watch(true)
}

fn watch(shielded: bool) -> Signals {
    let hooks = hooks();
    let mut reg = registry();
    if shielded {
        reg.shields += 1;
        if reg.shields == 1 {
            for h in hooks {
                h.set_latch(false);
            }
        }
    }
    let since = EPOCH.load(Ordering::SeqCst);
    if reg.live == 0 {
        SEEN.store(false, Ordering::SeqCst);
        for h in hooks {
            h.default_now.store(false, Ordering::SeqCst);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                if SEEN.swap(false, Ordering::SeqCst) {
                    EPOCH.fetch_add(1, Ordering::SeqCst);
                    // Not waited for here: runs end through `interrupted()`, and each Drop finishes the job.
                    let groups = registry().groups.clone();
                    for pid in groups {
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
        reg.watcher = Some((stop, handle));
    }
    reg.live += 1;
    Signals { since, shielded }
}

impl Signals {
    /// Has SIGINT or SIGTERM arrived since this run started?
    pub fn interrupted(&self) -> bool {
        EPOCH.load(Ordering::SeqCst) > self.since
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let watcher = {
            let mut reg = registry();
            reg.live = reg.live.saturating_sub(1);
            if self.shielded {
                reg.shields = reg.shields.saturating_sub(1);
                if reg.shields == 0 {
                    for h in hooks() {
                        h.set_latch(true);
                    }
                }
            }
            if reg.live > 0 {
                return;
            }
            for h in hooks() {
                h.default_now.store(true, Ordering::SeqCst);
            }
            reg.watcher.take()
        };
        // Joined outside the lock: the thread takes it too.
        if let Some((stop, handle)) = watcher {
            stop.store(true, Ordering::SeqCst);
            let _ = handle.join();
        }
    }
}

/// Lets a caught signal stop this server process group.
fn track_group(pid: u32) {
    registry().groups.push(pid);
}

fn untrack_group(pid: u32) {
    registry().groups.retain(|g| *g != pid);
}

/// A running server. Dropping it stops the server and everything it started.
pub struct Running {
    child: Child,
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    /// After `child` is stopped (see `Drop`), this goes: the database drop.
    _database: Database,
}

/// A free local port (best effort: another process could take it before the server does).
pub fn free_port() -> Result<u16, Error> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| Error::Environment(format!("no free port: {e}")))
}

fn clean_project(project: &str) -> String {
    project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn db_prefix(project: &str) -> String {
    format!("shakedown_{}_", clean_project(project))
}

/// The throwaway database name: the project, a short hash of the checkout path (two worktrees never share a
/// database: a shared one keeps the migrations another branch applied) and this process's id (two runs in one
/// checkout never share one either, and a leftover names the process that owned it, so it can be told from a live
/// run's).
pub fn db_name(project: &str, root: &Path) -> String {
    db_name_for(project, root, std::process::id())
}

fn db_name_for(project: &str, root: &Path, pid: u32) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    let short = digest.iter().take(4).fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    format!("{}{short}_p{pid}", db_prefix(project))
}

/// The pid in a name this tool made for `project`, or `None` for any other database (never touched).
fn owner_of(project: &str, db: &str) -> Option<u32> {
    let rest = db.strip_prefix(&db_prefix(project))?;
    let (hash, pid) = rest.split_once("_p")?;
    if hash.len() != 8 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Where the per-run lock files live: on this host, under the shakedown cache.
fn lock_dir() -> PathBuf {
    crate::browser::cache_dir().join("locks")
}

fn lock_path(dir: &Path, db: &str) -> PathBuf {
    dir.join(format!("{db}.lock"))
}

/// Takes the run's exclusive advisory lock, held for as long as the returned file lives (the OS releases it when
/// the process ends, however it ends). The lock exists before the database does.
fn hold_lock(dir: &Path, db: &str) -> Result<std::fs::File, Error> {
    let path = lock_path(dir, db);
    let fail = |e: &dyn std::fmt::Display| Error::Environment(format!("{}: {e}", path.display()));
    std::fs::create_dir_all(dir).map_err(|e| fail(&e))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| fail(&e))?;
    file.try_lock().map_err(|e| fail(&e))?;
    Ok(file)
}

/// Is this run certainly dead? Only when its lock file is on this host and can be taken: a live run holds it, and a
/// database with no lock file here (another machine or container) is never ours to judge. Any error means "not
/// certain". Returns the lock (kept while the database is dropped) when the run is dead.
fn dead_run_lock(dir: &Path, db: &str) -> Option<std::fs::File> {
    let path = lock_path(dir, db);
    if !path.try_exists().unwrap_or(false) {
        return None;
    }
    let file = std::fs::OpenOptions::new().write(true).open(&path).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Runs `drop_db` for `db`. A failure is only noted: the next run's sweep tries again.
fn drop_db(server: &Server, root: &Path, db: &str) {
    let Some(cmd) = &server.drop_db else { return };
    let ok = Command::new("sh")
        .arg("-c")
        .arg(fill(cmd, 0, db))
        .current_dir(root)
        .env("SHAKEDOWN_DB", db)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("warning: could not drop the throwaway database {db}; the next run drops it");
    }
}

/// Drops the leftover databases of runs that are gone: the `list_dbs` names that this tool made for `project`
/// and whose run is certainly dead (its lock file here can be taken). Anything else (another project's, an old-format name, a live run's) is left alone.
fn sweep(server: &Server, root: &Path, project: &str) {
    sweep_in(&lock_dir(), server, root, project);
}

fn sweep_in(locks: &Path, server: &Server, root: &Path, project: &str) {
    let Some(list) = &server.list_dbs else { return };
    let Ok(o) = Command::new("sh")
        .arg("-c")
        .arg(list)
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if !o.status.success() {
        return;
    }
    let me = std::process::id();
    for name in String::from_utf8_lossy(&o.stdout).lines() {
        let name = name.trim();
        if let Some(pid) = owner_of(project, name)
            && pid != me
            && let Some(lock) = dead_run_lock(locks, name)
        {
            drop_db(server, root, name);
            let _ = std::fs::remove_file(lock_path(locks, name));
            drop(lock);
        }
    }
}

/// Drops the run's database when it goes out of scope: success, failure, panic (unwinding) and a caught signal
/// (the run ends through `interrupted()`) all end here.
struct Database {
    server: Server,
    root: PathBuf,
    name: String,
    /// The run's lock, held until the database is dropped, then released.
    lock: std::fs::File,
}

impl Drop for Database {
    fn drop(&mut self) {
        drop_db(&self.server, &self.root, &self.name);
        let _ = std::fs::remove_file(lock_path(&lock_dir(), &self.name));
        let _ = self.lock.unlock();
    }
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
pub fn start(
    server: &Server,
    root: &Path,
    project: &str,
    out: &Path,
    signals: &Signals,
) -> Result<Running, Error> {
    std::fs::create_dir_all(out)
        .map_err(|e| Error::Environment(format!("{}: {e}", out.display())))?;
    let port = free_port()?;
    let db = db_name(project, root);
    let lock = hold_lock(&lock_dir(), &db)?;
    sweep(server, root, project);
    // Dropped last, after the server has stopped (fields drop in order: `child` is stopped by `Running::drop`).
    let database = Database {
        server: server.clone(),
        root: root.to_path_buf(),
        name: db.clone(),
        lock,
    };
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
    track_group(child.id());
    let mut running = Running {
        child,
        base,
        _database: database,
    };
    let deadline = Instant::now() + Duration::from_secs(server.timeout);
    loop {
        if signals.interrupted() {
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
        untrack_group(pid);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn db_names_are_stable_per_checkout_and_safe() {
        let a = db_name("accounts-service", Path::new("/tmp/a"));
        assert_eq!(a, db_name("accounts-service", Path::new("/tmp/a")));
        assert_ne!(a, db_name("accounts-service", Path::new("/tmp/b")));
        assert!(a.starts_with("shakedown_accounts_service_"));
        assert_eq!(owner_of("accounts-service", &a), Some(std::process::id()));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    /// Tests that watch signals in this process must not overlap the one that signals the process itself.
    static SIGNALS: Mutex<()> = Mutex::new(());

    pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
        SIGNALS.lock().unwrap_or_else(PoisonError::into_inner)
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
        let _serial = serial();
        let first = watch_signals();
        send("-TERM", std::process::id());
        wait_until("the first signal is seen", || first.interrupted());
        drop(first);
        let second = watch_signals();
        assert!(
            !second.interrupted(),
            "a new run starts clean, not already interrupted"
        );
        send("-INT", std::process::id());
        wait_until("the second run sees its own signal", || {
            second.interrupted()
        });
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
            "overlap" => {
                // Two runs overlap, the first ends, and the second must still catch a signal and clean up.
                let second = watch_signals();
                let mut server = Command::new("sleep")
                    .arg("300")
                    .stdout(Stdio::null())
                    .process_group(0)
                    .spawn()
                    .unwrap();
                track_group(server.id());
                drop(guard);
                println!("first-dropped");
                wait_until("the second run sees the signal", || second.interrupted());
                wait_until("the server group is stopped", || {
                    matches!(server.try_wait(), Ok(Some(_)))
                });
                untrack_group(server.id());
                println!("caught");
                drop(second);
                println!("cleaned");
            }
            "twice" => {
                wait_until("first signal", || guard.interrupted());
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
    fn overlapping_runs_share_the_signal_handling() {
        use std::os::unix::process::ExitStatusExt as _;
        let mut c = child(
            "world::tests::a_signal_child_for_the_tests_below",
            "overlap",
        );
        let mut out = lines(&mut c);
        wait_for_line(&mut out, "first-dropped");
        send("-INT", c.id());
        // The process lives, and the second run stopped its server.
        wait_for_line(&mut out, "caught");
        wait_for_line(&mut out, "cleaned");
        // Both runs are over: the default action is back.
        send("-TERM", c.id());
        let status = c.wait().unwrap();
        assert!(
            status.signal().is_some(),
            "a signal after both runs ended kills the process: {status:?}"
        );
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

    /// A real throwaway "database": a directory entry the commands create and remove, so every exit path can be
    /// seen without Postgres.
    fn fake_server(dir: &Path, seed: &str) -> Server {
        let d = dir.display();
        toml::from_str(&format!(
            "start = \"python3 -m http.server {{port}} --bind 127.0.0.1\"\nseed = {seed:?}\n\
             drop_db = \"rm -f {d}/{{db}}\"\nlist_dbs = \"ls {d}\"\n"
        ))
        .unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shakedown-world-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The seed command creates the database, as the project's start command does for real.
    fn seed_creating(dir: &Path, then: &str) -> String {
        format!("touch {}/$SHAKEDOWN_DB && {then}", dir.display())
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_database_is_gone_after_a_normal_run() {
        let _serial = serial();
        let dir = scratch("normal");
        let server = fake_server(&dir, &seed_creating(&dir, "true"));
        let signals = watch_signals();
        let running = start(&server, &dir, "demo", &dir.join("out"), &signals).unwrap();
        assert_eq!(
            leftovers(&dir).len(),
            2,
            "the database exists while the run lives (and out/)"
        );
        drop(running);
        assert_eq!(leftovers(&dir), vec!["out".to_string()]);
        drop(signals);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_database_is_gone_after_a_failing_run() {
        let _serial = serial();
        let dir = scratch("failing");
        let server = fake_server(&dir, &seed_creating(&dir, "false"));
        let signals = watch_signals();
        let failed = start(&server, &dir, "demo", &dir.join("out"), &signals);
        assert!(failed.is_err(), "the seed fails the run");
        assert_eq!(leftovers(&dir), vec!["out".to_string()]);
        drop(signals);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_database_is_gone_after_a_panic() {
        let _serial = serial();
        let dir = scratch("panic");
        let server = fake_server(&dir, &seed_creating(&dir, "true"));
        let signals = watch_signals();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _running = start(&server, &dir, "demo", &dir.join("out"), &signals).unwrap();
            panic!("the run panics");
        }));
        assert!(caught.is_err());
        assert_eq!(leftovers(&dir), vec!["out".to_string()]);
        drop(signals);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_start_sweeps_unlocked_leftovers_only() {
        let dir = scratch("sweep");
        let locks = dir.join("locks");
        let name = |pid: u32| db_name_for("demo", Path::new("/tmp/x"), pid);
        let (live, leftover, unknown) = (name(1_000_001), name(1_000_002), name(1_000_003));
        // A live run holds its lock; a leftover has a lock file nobody holds; an unknown-host database has none.
        let _held = hold_lock(&locks, &live).unwrap();
        drop(hold_lock(&locks, &leftover).unwrap());
        let keep = [
            live.clone(),
            unknown.clone(),
            "shakedown_demo_74191f67".to_string(),
            "shakedown_other_74191f67_p1".to_string(),
            "accounts".to_string(),
            "shakedown_demo_xyz_p1".to_string(),
        ];
        for k in keep.iter().chain([&leftover]) {
            std::fs::write(dir.join(k), "").unwrap();
        }
        let server = fake_server(&dir, "true");
        sweep_in(&locks, &server, &dir, "demo");
        let mut expected = keep.to_vec();
        expected.push("locks".to_string());
        expected.sort();
        assert_eq!(
            leftovers(&dir),
            expected,
            "only the unlocked leftover goes: not the live run's, not an unknown host's"
        );
        assert!(lock_path(&locks, &live).exists());
        assert!(!lock_path(&locks, &leftover).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unlockable_lock_file_means_not_swept() {
        let dir = scratch("sweep-err");
        let locks = dir.join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let db = db_name_for("demo", Path::new("/tmp/x"), 1_000_004);
        // A directory where the lock file should be: opening it for writing fails, so the run is not "certainly dead".
        std::fs::create_dir(lock_path(&locks, &db)).unwrap();
        std::fs::write(dir.join(&db), "").unwrap();
        sweep_in(&locks, &fake_server(&dir, "true"), &dir, "demo");
        assert!(dir.join(&db).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholders_are_filled() {
        assert_eq!(fill("run {port} {db} {port}", 80, "d"), "run 80 d 80");
    }
}
