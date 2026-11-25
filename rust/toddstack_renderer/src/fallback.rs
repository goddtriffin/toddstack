//! The fallback policy: when a backend counts as failed, what is reported, and the chain walk.
//!
//! A backend is abandoned only on a real failure ([`FallbackReason`]), never on a preference or a
//! heuristic about speed. Every step down is reported through the callback registered with
//! [`Options::with_fallback_callback`](crate::Options::with_fallback_callback); the crate itself
//! stays free of any error-reporting dependency.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::backend::Backend;
use crate::error::{Error, Result};

/// The version a host should key its stored backend on: `major.minor` of this crate. A stored
/// backend is only trusted while this string is unchanged, so every device retries the top of the
/// chain after an upgrade. See the README ("Remembering the winner").
pub const MEMORY_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

/// Why a backend was given up on. Every variant is a real failure.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FallbackReason {
    /// The backend has no adapter here.
    NoAdapter,
    /// The adapter request did not answer within the adapter timeout
    /// ([`Options::with_adapter_timeout`](crate::Options::with_adapter_timeout); WebGPU on the
    /// web only).
    Timeout,
    /// The adapter refused the device request (the message).
    RequestDevice(String),
    /// The surface could not be created, configured or recovered (the message).
    Surface(String),
    /// The device was lost (the reason and message).
    DeviceLost(String),
    /// The adapter was found but cannot be used: a software adapter, or one whose surface cannot
    /// take the render format (the message).
    Rejected(String),
}

impl FallbackReason {
    /// A short stable tag (`no_adapter`, `timeout`, `request_device`, `surface`, `device_lost`,
    /// `rejected`), for a Sentry tag or a metric label.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::NoAdapter => "no_adapter",
            Self::Timeout => "timeout",
            Self::RequestDevice(_) => "request_device",
            Self::Surface(_) => "surface",
            Self::DeviceLost(_) => "device_lost",
            Self::Rejected(_) => "rejected",
        }
    }
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter => f.write_str("no adapter"),
            Self::Timeout => f.write_str("timeout"),
            Self::RequestDevice(m) => write!(f, "device request failed: {m}"),
            Self::Surface(m) => write!(f, "surface failed: {m}"),
            Self::DeviceLost(m) => write!(f, "device lost: {m}"),
            Self::Rejected(m) => write!(f, "rejected: {m}"),
        }
    }
}

/// One step down the chain (or the end of it).
///
/// - `to: Some(next)`: `from` failed and `next` is tried. Report it (a warning is fitting).
/// - `to: None` with a reason other than `DeviceLost`: nothing in the chain works; the call also
///   returns [`Error::NoRendererAvailable`]. Report it with high severity.
/// - `to: None` with `DeviceLost`: the running renderer's device is gone; rebuild the renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FallbackEvent {
    /// The backend that failed.
    pub from: Backend,
    /// The backend tried next, or `None` when there is none.
    pub to: Option<Backend>,
    /// Why `from` failed.
    pub reason: FallbackReason,
}

/// One failed backend, in [`Error::NoRendererAvailable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The backend tried.
    pub backend: Backend,
    /// Why it failed.
    pub reason: FallbackReason,
}

/// The callback type: shared, so the renderer keeps it for device-lost events. On the web it need
/// not be `Send` (JS values are not).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Callback = Arc<dyn Fn(&FallbackEvent) + Send + Sync>;
/// See the native alias.
#[cfg(target_arch = "wasm32")]
pub(crate) type Callback = Arc<dyn Fn(&FallbackEvent)>;

/// How one attempt ended badly.
pub(crate) enum Step {
    /// A real failure: report it and try the next backend.
    Fall(FallbackReason),
    /// Not a backend failure (bad input, an internal error): stop and return it.
    Fatal(Error),
}

/// The order to try: just `force`; else `preferred` first (if the platform chain has it) and then
/// the whole chain from the top; else the chain.
pub(crate) fn plan(
    chain: &[Backend],
    force: Option<Backend>,
    preferred: Option<Backend>,
) -> Vec<Backend> {
    if let Some(f) = force {
        return vec![f];
    }
    match preferred.filter(|p| chain.contains(p)) {
        Some(p) => std::iter::once(p)
            .chain(chain.iter().copied().filter(|b| *b != p))
            .collect(),
        None => chain.to_vec(),
    }
}

/// Tries `order` in turn until `attempt` succeeds, reporting every failure through `emit`.
///
/// # Errors
/// A [`Step::Fatal`] error as is; `NoRendererAvailable` (after a final event with `to: None`)
/// when every backend failed.
pub(crate) async fn run_chain<T, F, Fut>(
    order: &[Backend],
    mut attempt: F,
    emit: &dyn Fn(&FallbackEvent),
) -> Result<(Backend, T)>
where
    F: FnMut(Backend) -> Fut,
    Fut: Future<Output = std::result::Result<T, Step>>,
{
    let mut attempts: Vec<Attempt> = Vec::new();
    for (i, &backend) in order.iter().enumerate() {
        match attempt(backend).await {
            Ok(t) => return Ok((backend, t)),
            Err(Step::Fatal(e)) => return Err(e),
            Err(Step::Fall(reason)) => {
                emit(&FallbackEvent {
                    from: backend,
                    to: order.get(i + 1).copied(),
                    reason: reason.clone(),
                });
                attempts.push(Attempt { backend, reason });
            }
        }
    }
    Err(Error::NoRendererAvailable { attempts })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const WEB: [Backend; 3] = [Backend::WebGpu, Backend::WebGl2, Backend::Cpu];

    type Outcome = (Result<Backend>, Vec<FallbackEvent>, Vec<Backend>);

    /// Runs `order` where each backend in `fail` fails with its reason and the rest succeed.
    fn run(order: &[Backend], fail: &[(Backend, FallbackReason)]) -> Outcome {
        let events: RefCell<Vec<FallbackEvent>> = RefCell::new(Vec::new());
        let tried: RefCell<Vec<Backend>> = RefCell::new(Vec::new());
        let got = pollster::block_on(run_chain(
            order,
            |b| {
                tried.borrow_mut().push(b);
                let r = fail.iter().find(|(f, _)| *f == b).map(|(_, r)| r.clone());
                async move { r.map_or(Ok(()), |r| Err(Step::Fall(r))) }
            },
            &|e| events.borrow_mut().push(e.clone()),
        ));
        (
            got.map(|(b, ())| b),
            events.into_inner(),
            tried.into_inner(),
        )
    }

    #[test]
    fn first_working_backend_wins_with_no_events() {
        let (got, events, tried) = run(&WEB, &[]);
        assert_eq!(got.expect("works"), Backend::WebGpu);
        assert_eq!(events, []);
        assert_eq!(tried, [Backend::WebGpu]);
    }

    #[test]
    fn a_forced_failure_steps_down_and_reports_each_step() {
        let (got, events, _) = run(
            &WEB,
            &[
                (Backend::WebGpu, FallbackReason::NoAdapter),
                (Backend::WebGl2, FallbackReason::RequestDevice("x".into())),
            ],
        );
        assert_eq!(got.expect("cpu"), Backend::Cpu);
        assert_eq!(
            events,
            [
                FallbackEvent {
                    from: Backend::WebGpu,
                    to: Some(Backend::WebGl2),
                    reason: FallbackReason::NoAdapter
                },
                FallbackEvent {
                    from: Backend::WebGl2,
                    to: Some(Backend::Cpu),
                    reason: FallbackReason::RequestDevice("x".into())
                },
            ]
        );
    }

    #[test]
    fn a_simulated_timeout_falls_to_webgl2_with_reason_timeout() {
        let (got, events, _) = run(&WEB, &[(Backend::WebGpu, FallbackReason::Timeout)]);
        assert_eq!(got.expect("webgl2"), Backend::WebGl2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, FallbackReason::Timeout);
        assert_eq!(events[0].to, Some(Backend::WebGl2));
    }

    #[test]
    fn everything_failing_returns_no_renderer_available_after_a_final_event() {
        let fail = [
            (Backend::WebGpu, FallbackReason::NoAdapter),
            (Backend::WebGl2, FallbackReason::Surface("s".into())),
            (Backend::Cpu, FallbackReason::Surface("2d".into())),
        ];
        let (got, events, _) = run(&WEB, &fail);
        let Err(Error::NoRendererAvailable { attempts }) = got else {
            panic!("expected NoRendererAvailable");
        };
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[1].backend, Backend::WebGl2);
        assert_eq!(events.len(), 3);
        let last = events.last().expect("final event");
        assert_eq!(last.from, Backend::Cpu);
        assert_eq!(last.to, None);
    }

    #[test]
    fn a_fatal_error_stops_the_walk_without_an_event() {
        let events: RefCell<usize> = RefCell::new(0);
        let got = pollster::block_on(run_chain::<(), _, _>(
            &WEB,
            |_| async { Err(Step::Fatal(Error::BadInput("size".into()))) },
            &|_| *events.borrow_mut() += 1,
        ));
        assert!(matches!(got, Err(Error::BadInput(_))));
        assert_eq!(*events.borrow(), 0);
    }

    #[test]
    fn plan_starts_at_the_preferred_backend_then_runs_the_whole_chain() {
        assert_eq!(plan(&WEB, None, None), WEB);
        assert_eq!(
            plan(&WEB, None, Some(Backend::WebGl2)),
            [Backend::WebGl2, Backend::WebGpu, Backend::Cpu]
        );
        assert_eq!(plan(&WEB, None, Some(Backend::WebGpu)), WEB);
        // Not in this platform's chain: ignored.
        assert_eq!(plan(&WEB, None, Some(Backend::Metal)), WEB);
        // Force wins over everything.
        assert_eq!(
            plan(&WEB, Some(Backend::Cpu), Some(Backend::WebGl2)),
            [Backend::Cpu]
        );
    }

    #[test]
    fn a_failing_preferred_backend_falls_through_the_full_chain_with_events() {
        let order = plan(&WEB, None, Some(Backend::WebGl2));
        let (got, events, tried) = run(&order, &[(Backend::WebGl2, FallbackReason::NoAdapter)]);
        assert_eq!(got.expect("webgpu"), Backend::WebGpu);
        assert_eq!(tried, [Backend::WebGl2, Backend::WebGpu]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].from, Backend::WebGl2);
        assert_eq!(events[0].to, Some(Backend::WebGpu));
    }

    #[test]
    fn reasons_have_stable_tags() {
        assert_eq!(FallbackReason::Timeout.tag(), "timeout");
        assert_eq!(FallbackReason::DeviceLost("x".into()).tag(), "device_lost");
        assert!(MEMORY_VERSION.starts_with("0."));
    }
}
