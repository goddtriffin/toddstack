//! The guard: its own CDP connection to the browser that attaches to every target before it runs (pages, iframes
//! out of process or sandboxed, popups, workers), recursively, and arms each with `guard.js` before resuming it.
//! `chromiumoxide` resumes the targets it sees on its own, with only its own scripts; this connection is what makes
//! "no WebRTC, no popups" hold for every frame. Violations are reported by a CDP binding, not the console.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_tungstenite::tokio::connect_async;
use async_tungstenite::tungstenite::Message;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};

/// Receives `(rule, message)` for a violation in one page (and its frames, workers and popups).
pub type Report = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// How long a new page waits to be armed.
const ARM_TIMEOUT: Duration = Duration::from_secs(10);
/// The binding's name in the page, taken out of its reach by the guard script before page code runs.
const BINDING: &str = "__shakedown_report";
const BROWSER: &str = "browser";

#[derive(Default)]
struct Shared {
    /// Targets whose guard is in place and which have been resumed (and `BROWSER` once attached to the browser).
    armed: Mutex<HashSet<String>>,
    /// A page's target id to where the violations of it and everything under it go.
    pages: Mutex<HashMap<String, Report>>,
}

/// A running guard.
pub struct Guard {
    shared: Arc<Shared>,
}

impl Guard {
    /// Sends the violations of page `page` (a top-level target) and of its frames, workers and popups to `report`.
    /// Many pages can share one browser context: the page, not the context, says whose violation it is.
    pub fn register(&self, page: &str, report: Report) {
        if let Ok(mut c) = self.shared.pages.lock() {
            c.insert(page.to_string(), report);
        }
    }

    /// Forgets page `page` (it is closed).
    pub fn unregister(&self, page: &str) {
        if let Ok(mut c) = self.shared.pages.lock() {
            c.remove(page);
        }
    }

    /// Waits until `target` has its guard and is running.
    ///
    /// # Errors
    ///
    /// The guard never reached the target.
    pub async fn armed(&self, target: &str) -> Result<(), String> {
        wait_for(&self.shared, target).await
    }
}

async fn wait_for(shared: &Shared, key: &str) -> Result<(), String> {
    let start = std::time::Instant::now();
    while !shared.armed.lock().is_ok_and(|a| a.contains(key)) {
        if start.elapsed() > ARM_TIMEOUT {
            return Err(format!("guard: {key} was never armed"));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

/// Connects to the browser's debugging endpoint and starts guarding.
///
/// # Errors
///
/// The connection or the first command fails.
pub async fn start(ws_url: &str) -> Result<Guard, String> {
    let (ws, _) = connect_async(ws_url)
        .await
        .map_err(|e| format!("guard connection: {e}"))?;
    let shared = Arc::new(Shared::default());
    tokio::spawn(run(ws, shared.clone()));
    wait_for(&shared, BROWSER).await?;
    Ok(Guard { shared })
}

fn script() -> String {
    include_str!("guard.js").replace("__BINDING__", BINDING)
}

#[expect(clippy::too_many_lines, reason = "one event loop")]
async fn run<S>(ws: S, shared: Arc<Shared>)
where
    S: futures::Stream<Item = Result<Message, async_tungstenite::tungstenite::Error>>
        + futures::Sink<Message, Error = async_tungstenite::tungstenite::Error>
        + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    let mut next_id: u64 = 0;
    // Session id to (top-level page it belongs to, its own target id); a frame or worker belongs to the page that holds it.
    let mut sessions: HashMap<String, (String, String)> = HashMap::new();
    // Target id to its top-level page.
    let mut roots: HashMap<String, String> = HashMap::new();
    // Command id of a final `runIfWaitingForDebugger` to the target it arms (`BROWSER` for the first command).
    let mut arming: HashMap<u64, String> = HashMap::new();
    let source = script();
    let attach = json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true});

    let mut out: Vec<(Option<String>, &str, Value, Option<String>)> = vec![(
        None,
        "Target.setAutoAttach",
        attach.clone(),
        Some(BROWSER.into()),
    )];
    loop {
        for (session, method, params, arms) in out.drain(..) {
            next_id += 1;
            let mut msg = json!({"id": next_id, "method": method, "params": params});
            if let Some(s) = session {
                msg["sessionId"] = Value::String(s);
            }
            if let Some(t) = arms {
                arming.insert(next_id, t);
            }
            if sink.send(Message::text(msg.to_string())).await.is_err() {
                return;
            }
        }
        let Some(Ok(msg)) = stream.next().await else {
            return;
        };
        let Message::Text(text) = msg else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            if let Some(target) = arming.remove(&id)
                && let Ok(mut a) = shared.armed.lock()
            {
                a.insert(target);
            }
            continue;
        }
        let p = &v["params"];
        let session_of = v.get("sessionId").and_then(Value::as_str);
        match v.get("method").and_then(Value::as_str) {
            Some("Target.attachedToTarget") => {
                let (Some(sid), info) = (p["sessionId"].as_str(), &p["targetInfo"]) else {
                    continue;
                };
                let target = info["targetId"].as_str().unwrap_or_default().to_string();
                let kind = info["type"].as_str().unwrap_or_default();
                let opener = info["openerId"].as_str().map(String::from);
                // A frame or worker is held by the page whose session announced it; a popup by its opener.
                let root = session_of
                    .and_then(|s| sessions.get(s).map(|(r, _)| r.clone()))
                    .or_else(|| {
                        opener
                            .as_ref()
                            .map(|o| roots.get(o).cloned().unwrap_or(o.clone()))
                    })
                    .unwrap_or_else(|| target.clone());
                sessions.insert(sid.to_string(), (root.clone(), target.clone()));
                roots.insert(target.clone(), root.clone());
                if kind == "page" && info.get("openerId").is_some() {
                    // A popup: never resumed, so it never runs; closed at once.
                    report(&shared, Some(&root), "popup");
                    out.push((
                        None,
                        "Target.closeTarget",
                        json!({"targetId": target}),
                        None,
                    ));
                    continue;
                }
                let sid = sid.to_string();
                let mut cmd = |m: &'static str, params: Value, arms: Option<String>| {
                    out.push((Some(sid.clone()), m, params, arms));
                };
                cmd("Target.setAutoAttach", attach.clone(), None);
                if kind == "page" || kind == "iframe" {
                    cmd("Page.enable", json!({}), None);
                    cmd("Runtime.enable", json!({}), None);
                    cmd("Runtime.addBinding", json!({"name": BINDING}), None);
                    cmd(
                        "Page.addScriptToEvaluateOnNewDocument",
                        json!({"source": source, "runImmediately": true}),
                        None,
                    );
                }
                cmd("Runtime.runIfWaitingForDebugger", json!({}), Some(target));
            }
            Some("Target.detachedFromTarget") => {
                if let Some(sid) = p["sessionId"].as_str() {
                    // Both maps are pruned when a target detaches, so a long run does not grow them.
                    if let Some((_, target)) = sessions.remove(sid) {
                        roots.remove(&target);
                    }
                }
            }
            Some("Runtime.bindingCalled") if p["name"] == BINDING => {
                let page = session_of
                    .and_then(|s| sessions.get(s))
                    .map(|(r, _)| r.as_str());
                report(&shared, page, p["payload"].as_str().unwrap_or_default());
            }
            _ => {}
        }
    }
}

fn report(shared: &Shared, page: Option<&str>, what: &str) {
    let (rule, message) = match what {
        "webrtc" => ("webrtc", "WebRTC is blocked: nothing leaves the machine"),
        "popup" => ("popup", "popups are blocked in checks"),
        _ => return,
    };
    let to = page.and_then(|c| shared.pages.lock().ok().and_then(|m| m.get(c).cloned()));
    if let Some(to) = to {
        to(rule, message);
    }
}
