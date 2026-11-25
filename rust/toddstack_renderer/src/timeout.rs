//! A race between a future and a timer, with no dependency: the WebGPU adapter request can hang.

use std::future::Future;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

/// `Some(output)` if `work` finishes first, `None` if `timer` does. `work` wins a tie.
pub(crate) async fn race<T>(
    work: impl Future<Output = T>,
    timer: impl Future<Output = ()>,
) -> Option<T> {
    let mut work = pin!(work);
    let mut timer = pin!(timer);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(v));
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

/// Resolves after `d` (the host's `setTimeout`, so it also works in a worker). Never resolves if
/// there is no `setTimeout`.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn sleep(d: Duration) {
    use wasm_bindgen::JsCast;
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let global = js_sys::global();
        if let Ok(f) = js_sys::Reflect::get(&global, &"setTimeout".into())
            && let Ok(f) = f.dyn_into::<js_sys::Function>()
        {
            let ms = wasm_bindgen::JsValue::from_f64(d.as_secs_f64() * 1000.0);
            let _ = f.call2(&global, &resolve, &ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Only the web times an adapter request out: a native `request_adapter` answers or fails.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn sleep(_d: Duration) {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::race;
    use std::future::{pending, ready};

    #[test]
    fn work_that_finishes_wins() {
        assert_eq!(pollster::block_on(race(ready(7), pending::<()>())), Some(7));
    }

    #[test]
    fn a_timer_that_fires_first_beats_work_that_never_answers() {
        assert_eq!(pollster::block_on(race(pending::<u8>(), ready(()))), None);
    }
}
