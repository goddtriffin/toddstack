//! `Accounts::report_as_guest` against a fake accounts API.

use std::sync::{Arc, Mutex};

use toddstack_accounts_server_sdk::report::{GuestReport, Reason};
use toddstack_accounts_server_sdk::{Accounts, Config, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One whole request: headers, then `content-length` bytes of body (they
/// can arrive in separate reads).
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk: [u8; 4096] = [0; 4096];
    loop {
        let n: usize = socket.read(&mut chunk).await.unwrap();
        bytes.extend_from_slice(&chunk[..n]);
        let text: String = String::from_utf8_lossy(&bytes).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if body.len() >= length || n == 0 {
                return text;
            }
        }
        if n == 0 {
            return text;
        }
    }
}

/// Answers every request with `status`, keeping the raw requests.
async fn service(status: u16) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer: String = format!("http://{}/oidc", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let kept: Arc<Mutex<Vec<String>>> = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: String = read_request(&mut socket).await;
            kept.lock().unwrap().push(request);
            let response: String =
                format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (issuer, seen)
}

fn accounts(issuer: &str, client_secret: Option<&str>) -> Accounts {
    Accounts::new(Config {
        issuer: issuer.to_owned(),
        client_id: String::from("boggledygook"),
        client_secret: client_secret.map(str::to_owned),
        redirect_uri: String::from("https://www.boggledygook.com/auth/callback"),
    })
    .unwrap()
}

fn report() -> GuestReport {
    GuestReport {
        sub: String::from("u2"),
        reason: Reason::Harassment,
        note: None,
        guest_id: Some(String::from("g1")),
        guest_ip: Some(String::from("203.0.113.9")),
    }
}

#[tokio::test]
async fn a_guest_report_goes_to_the_api_with_the_client_credentials() {
    let (issuer, seen) = service(204).await;
    accounts(&issuer, Some("s3cret"))
        .report_as_guest(&report())
        .await
        .unwrap();
    let request: String = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("POST /api/v1/reports HTTP/1.1"),
        "{request}"
    );
    // base64 of "boggledygook:s3cret".
    assert!(
        request.contains("authorization: Basic Ym9nZ2xlZHlnb29rOnMzY3JldA=="),
        "{request}"
    );
    let expected: &str =
        r#"{"sub":"u2","reason":"harassment","guest_id":"g1","guest_ip":"203.0.113.9"}"#;
    assert!(request.ends_with(expected), "{request}");
}

#[tokio::test]
async fn a_refused_guest_report_carries_the_status() {
    let (issuer, _seen) = service(429).await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .report_as_guest(&report())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api { status: 429, .. }), "{error}");
}

#[tokio::test]
async fn a_public_client_cannot_report_for_guests() {
    let (issuer, seen) = service(204).await;
    let error: Error = accounts(&issuer, None)
        .report_as_guest(&report())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error}");
    assert_eq!(0, seen.lock().unwrap().len());
}
