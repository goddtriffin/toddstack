//! `Accounts::sign_out` against a fake revocation endpoint.

use std::sync::{Arc, Mutex};

use toddstack_accounts::logout::{Options, SignOut};
use toddstack_accounts::{Accounts, Config, Error};
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

fn accounts(issuer: &str) -> Accounts {
    Accounts::new(Config {
        issuer: issuer.to_owned(),
        client_id: String::from("boggledygook"),
        client_secret: Some(String::from("s3cret")),
        redirect_uri: String::from("https://www.boggledygook.com/auth/callback"),
    })
    .unwrap()
}

#[tokio::test]
async fn sign_out_revokes_the_refresh_token_and_returns_the_end_session_url() {
    let (issuer, seen) = service(200).await;
    let options: Options<'_> = Options {
        post_logout_redirect_uri: Some("/"),
        ..Options::default()
    };
    let signed_out: SignOut = accounts(&issuer).sign_out(Some("rt1"), &options).await;

    assert!(signed_out.revoke_error.is_none());
    let expected: String = format!(
        "{issuer}/session/end?client_id=boggledygook&post_logout_redirect_uri=https%3A%2F%2Fwww.boggledygook.com%2F"
    );
    assert_eq!(Some(expected), signed_out.url);
    let requests: Vec<String> = seen.lock().unwrap().clone();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("POST /oidc/token/revocation "));
    assert!(requests[0].ends_with("token=rt1&token_type_hint=refresh_token"));
}

#[tokio::test]
async fn a_failed_revoke_still_signs_out() {
    let (issuer, _) = service(503).await;
    let signed_out: SignOut = accounts(&issuer)
        .sign_out(Some("rt1"), &Options::default())
        .await;
    assert!(matches!(
        signed_out.revoke_error,
        Some(Error::Api { status: 503, .. })
    ));
    assert_eq!(
        Some(format!("{issuer}/session/end?client_id=boggledygook")),
        signed_out.url
    );
}

#[tokio::test]
async fn without_a_refresh_token_nothing_is_revoked() {
    let (issuer, seen) = service(200).await;
    let signed_out: SignOut = accounts(&issuer).sign_out(None, &Options::default()).await;
    assert!(signed_out.revoke_error.is_none());
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_url_carries_the_id_token_hint_redirect_and_state() {
    let (issuer, _) = service(200).await;
    let options: Options<'_> = Options {
        id_token_hint: Some("a.b.c"),
        post_logout_redirect_uri: Some("/bye"),
        state: Some("s1"),
        local_only: false,
    };
    let signed_out: SignOut = accounts(&issuer).sign_out(Some("rt1"), &options).await;
    let expected: String = format!(
        "{issuer}/session/end?client_id=boggledygook&id_token_hint=a.b.c&post_logout_redirect_uri=https%3A%2F%2Fwww.boggledygook.com%2Fbye&state=s1"
    );
    assert_eq!(Some(expected), signed_out.url);
}

#[tokio::test]
async fn local_only_revokes_the_token_but_returns_no_url() {
    let (issuer, seen) = service(200).await;
    let options: Options<'_> = Options {
        id_token_hint: Some("a.b.c"),
        local_only: true,
        ..Options::default()
    };
    let signed_out: SignOut = accounts(&issuer).sign_out(Some("rt1"), &options).await;
    assert!(signed_out.revoke_error.is_none());
    assert_eq!(None, signed_out.url);
    assert_eq!(1, seen.lock().unwrap().len());
}
