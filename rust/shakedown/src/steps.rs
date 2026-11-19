//! The `check` runner: steps form a graph, independent ones run in parallel, results print in declared order.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

use crate::config::Step;
use crate::error::Error;

/// How one step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Passed.
    Ok,
    /// Failed, with a reason.
    Fail(String),
    /// Not run, with the reason (a step before it failed).
    Skipped(String),
}

/// One finished step.
#[derive(Debug, Clone)]
pub struct StepResult {
    /// Step name.
    pub name: String,
    /// Outcome.
    pub status: Status,
    /// The one-line summary printed after `ok`.
    pub detail: String,
    /// Wall time.
    pub secs: f64,
    /// Combined stdout and stderr, kept for failures and the log.
    pub output: String,
}

/// What a node does: run a shell command, or a tool-run built-in.
#[derive(Debug, Clone)]
pub enum Job {
    /// `sh -c`.
    Shell(String),
    /// A built-in, by its config name.
    Builtin(String),
}

/// A node in the graph.
#[derive(Debug, Clone)]
pub struct Node {
    /// Unique name.
    pub name: String,
    /// Names that must pass first.
    pub after: Vec<String>,
    /// The work.
    pub job: Job,
}

/// Validates the graph and returns step indices in a dependency-respecting order.
///
/// # Errors
///
/// A dependency cycle.
pub fn order(steps: &[Step]) -> Result<Vec<usize>, Error> {
    let mut done: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    while out.len() < steps.len() {
        let before = out.len();
        for (i, s) in steps.iter().enumerate() {
            if !out.contains(&i) && s.after.iter().all(|d| done.contains(d.as_str())) {
                out.push(i);
                done.insert(s.name.as_str());
            }
        }
        if out.len() == before {
            let stuck: Vec<&str> = steps
                .iter()
                .filter(|s| !done.contains(s.name.as_str()))
                .map(|s| s.name.as_str())
                .collect();
            return Err(Error::Config(format!(
                "steps form a cycle: {}",
                stuck.join(", ")
            )));
        }
    }
    Ok(out)
}

/// Runs `nodes`, calling `builtin` for [`Job::Builtin`]. Results come back in `nodes` order.
pub fn run(
    nodes: &[Node],
    root: &Path,
    log_dir: &Path,
    builtin: &(dyn Fn(&str) -> (Status, String, String) + Sync),
) -> Vec<StepResult> {
    let limit = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .max(2);
    let mut results: Vec<Option<StepResult>> = vec![None; nodes.len()];
    let mut started = vec![false; nodes.len()];
    let mut running = 0_usize;
    let _ = std::fs::create_dir_all(log_dir);
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<(usize, StepResult)>();
        loop {
            for i in 0..nodes.len() {
                if started[i] || running >= limit {
                    continue;
                }
                let deps: Vec<Option<&StepResult>> = nodes[i]
                    .after
                    .iter()
                    .map(|d| {
                        nodes
                            .iter()
                            .position(|n| &n.name == d)
                            .and_then(|j| results[j].as_ref())
                    })
                    .collect();
                if deps.iter().any(Option::is_none) {
                    continue;
                }
                started[i] = true;
                if let Some(bad) = deps.iter().flatten().find(|r| r.status != Status::Ok) {
                    let why = format!("{} did not pass", bad.name);
                    results[i] = Some(StepResult {
                        name: nodes[i].name.clone(),
                        status: Status::Skipped(why.clone()),
                        detail: why,
                        secs: 0.0,
                        output: String::new(),
                    });
                    continue;
                }
                running += 1;
                let tx = tx.clone();
                let node = &nodes[i];
                scope.spawn(move || {
                    let r = execute(node, root, builtin);
                    let _ = tx.send((i, r));
                });
            }
            if running == 0 {
                if started.iter().all(|s| *s) {
                    break;
                }
                continue;
            }
            if let Ok((i, r)) = rx.recv() {
                running -= 1;
                let _ = std::fs::write(log_dir.join(format!("{}.log", r.name)), &r.output);
                results[i] = Some(r);
            }
        }
    });
    results.into_iter().flatten().collect()
}

fn execute(
    node: &Node,
    root: &Path,
    builtin: &(dyn Fn(&str) -> (Status, String, String) + Sync),
) -> StepResult {
    let start = Instant::now();
    let (status, detail, output) = match &node.job {
        Job::Builtin(name) => builtin(name),
        Job::Shell(cmd) => shell(cmd, root),
    };
    StepResult {
        name: node.name.clone(),
        status,
        detail,
        secs: start.elapsed().as_secs_f64(),
        output,
    }
}

fn shell(cmd: &str, root: &Path) -> (Status, String, String) {
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{cmd}"))
        .current_dir(root)
        .env("SHAKEDOWN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return (
                Status::Fail(format!("could not start: {e}")),
                String::new(),
                String::new(),
            );
        }
    };
    let mut output = String::new();
    if let Some(mut out) = child.stdout.take() {
        let mut bytes = Vec::new();
        let _ = out.read_to_end(&mut bytes);
        output = String::from_utf8_lossy(&bytes).into_owned();
    }
    match child.wait() {
        Ok(s) if s.success() => (Status::Ok, String::new(), output),
        Ok(s) => {
            let why = s
                .code()
                .map_or_else(|| "killed by a signal".to_string(), |c| format!("exit {c}"));
            (Status::Fail(why), String::new(), output)
        }
        Err(e) => (
            Status::Fail(format!("wait failed: {e}")),
            String::new(),
            output,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, after: &[&str], cmd: &str) -> Node {
        Node {
            name: name.into(),
            after: after.iter().map(|s| (*s).into()).collect(),
            job: Job::Shell(cmd.into()),
        }
    }

    fn no_builtin(_: &str) -> (Status, String, String) {
        (Status::Ok, String::new(), String::new())
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("shakedown-steps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn passing_failing_and_skipped() {
        let d = tmp("mix");
        let nodes = [
            node("a", &[], "echo hi"),
            node("b", &["a"], "echo boom; exit 3"),
            node("c", &["b"], "true"),
            node("d", &[], "true"),
        ];
        let r = run(&nodes, &d, &d.join("logs"), &no_builtin);
        assert_eq!(
            r.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        assert_eq!(r[0].status, Status::Ok);
        assert_eq!(r[1].status, Status::Fail("exit 3".into()));
        assert!(r[1].output.contains("boom"));
        assert!(matches!(r[2].status, Status::Skipped(_)));
        assert_eq!(r[3].status, Status::Ok);
        assert!(d.join("logs/b.log").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stderr_is_captured_and_order_is_respected() {
        let d = tmp("order");
        let nodes = [
            node("first", &[], "echo one > marker; echo err >&2"),
            node("second", &["first"], "test -f marker"),
        ];
        let r = run(&nodes, &d, &d.join("logs"), &no_builtin);
        assert_eq!(r[1].status, Status::Ok);
        assert!(r[0].output.contains("err"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn order_detects_cycles() {
        let s = |n: &str, a: &[&str]| Step {
            name: n.into(),
            run: "true".into(),
            after: a.iter().map(|x| (*x).into()).collect(),
            manual: false,
        };
        assert!(order(&[s("a", &["b"]), s("b", &["a"])]).is_err());
        assert_eq!(order(&[s("a", &["b"]), s("b", &[])]).unwrap(), vec![1, 0]);
    }
}
