//! A scripted upstream for the relay and retry tests: request `n` gets reply
//! `n` (the last reply repeats), and every request is counted.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::net::TcpListener;

#[derive(Clone)]
pub(super) struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
    delay: Duration,
}

impl Reply {
    pub(super) const fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: "",
            delay: Duration::ZERO,
        }
    }

    pub(super) const fn script(body: &'static str) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
        }
    }

    pub(super) fn header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    pub(super) const fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub(super) struct FakeUpstream {
    pub(super) url: String,
    replies: Arc<Mutex<Vec<Reply>>>,
    hits: Arc<AtomicUsize>,
}

impl FakeUpstream {
    pub(super) async fn start(replies: Vec<Reply>) -> Self {
        let replies: Arc<Mutex<Vec<Reply>>> = Arc::new(Mutex::new(replies));
        let hits: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let app: Router = Router::new().route(
            "/script.js",
            any({
                let replies: Arc<Mutex<Vec<Reply>>> = Arc::clone(&replies);
                let hits: Arc<AtomicUsize> = Arc::clone(&hits);
                move || {
                    let n: usize = hits.fetch_add(1, Ordering::SeqCst);
                    let reply: Reply = {
                        let replies = replies.lock().expect("unpoisoned");
                        replies[n.min(replies.len() - 1)].clone()
                    };
                    async move {
                        tokio::time::sleep(reply.delay).await;
                        let mut response: Response = (
                            StatusCode::from_u16(reply.status).expect("a status"),
                            reply.body,
                        )
                            .into_response();
                        for (name, value) in reply.headers {
                            response.headers_mut().insert(
                                HeaderName::from_static(name),
                                HeaderValue::from_static(value),
                            );
                        }
                        response
                    }
                }
            }),
        );
        let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let url: String = format!(
            "http://{}/script.js",
            listener.local_addr().expect("an address")
        );
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serves") });
        Self { url, replies, hits }
    }

    /// Replaces the replies; the request count carries on.
    pub(super) fn reply_from_now_on(&self, reply: Reply) {
        let n: usize = self.hits();
        let mut replies = self.replies.lock().expect("unpoisoned");
        replies.truncate(n);
        while replies.len() < n {
            replies.push(reply.clone());
        }
        replies.push(reply);
    }

    pub(super) fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}
