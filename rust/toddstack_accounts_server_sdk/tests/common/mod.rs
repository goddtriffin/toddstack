//! A fake accounts API: records every request and answers from a script.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One request as the server saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    /// Path and query.
    pub target: String,
    pub authorization: Option<String>,
    pub body: String,
}

/// `(status, JSON body)`.
pub type Reply = (u16, String);

pub struct Fake {
    pub issuer: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    /// Answers request `n` with `answer(n, &request)`.
    pub async fn start(answer: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static) -> Self {
        let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer: String = format!("http://{}/oidc", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let kept: Arc<Mutex<Vec<Seen>>> = Arc::clone(&seen);
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (kept, answer) = (Arc::clone(&kept), Arc::clone(&answer));
                tokio::spawn(async move {
                    let request: Seen = read_request(&mut socket).await;
                    let n: usize = {
                        let mut all = kept.lock().unwrap();
                        all.push(request.clone());
                        all.len() - 1
                    };
                    let (status, body) = answer(n, &request);
                    let response: String = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self { issuer, seen }
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Seen {
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk: [u8; 4096] = [0; 4096];
    loop {
        let n: usize = socket.read(&mut chunk).await.unwrap();
        bytes.extend_from_slice(&chunk[..n]);
        let text: String = String::from_utf8_lossy(&bytes).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let header = |name: &str| {
                head.lines().find_map(|l| {
                    l.to_ascii_lowercase()
                        .starts_with(&format!("{name}:"))
                        .then(|| l.split_once(':').unwrap().1.trim().to_owned())
                })
            };
            let length: usize = header("content-length").map_or(0, |v| v.parse().unwrap());
            if body.len() >= length || n == 0 {
                let mut first = head.lines().next().unwrap().split(' ');
                return Seen {
                    method: first.next().unwrap().to_owned(),
                    target: first.next().unwrap().to_owned(),
                    authorization: header("authorization"),
                    body: body.to_owned(),
                };
            }
        }
        assert!(n != 0, "connection closed mid-request");
    }
}
