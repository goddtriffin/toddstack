//! The in-app friends router against a fake accounts API.
#![cfg(feature = "axum")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toddstack_accounts::friends_router::{self, Credential, FriendsAuth};
use toddstack_accounts::{Accounts, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

const ORIGIN: &str = "https://www.example.com";

/// Signed in when the request has a `cookie: session=<token>` or `Bearer <token>`.
struct Auth;

impl FriendsAuth for Auth {
    fn access_token(&self, headers: &HeaderMap) -> impl Future<Output = Option<Credential>> + Send {
        let cookie: Option<&str> = headers.get("cookie").and_then(|v| v.to_str().ok());
        let bearer: Option<&str> = headers.get("authorization").and_then(|v| v.to_str().ok());
        // A cookie wins when both are present, and is reported as the credential used.
        let credential: Option<Credential> = cookie
            .and_then(|c| c.strip_prefix("session="))
            .map(|t| Credential::Cookie(t.to_owned()))
            .or_else(|| {
                bearer
                    .and_then(|b| b.strip_prefix("Bearer "))
                    .map(|t| Credential::Bearer(t.to_owned()))
            });
        std::future::ready(credential)
    }

    fn public_origin(&self) -> &str {
        ORIGIN
    }
}

type Seen = Arc<Mutex<Vec<String>>>;

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

/// A fake accounts service: `answers` maps "METHOD /path" to (status, body);
/// anything unmapped is a 404. Keeps every raw request.
async fn service(answers: Vec<(&'static str, u16, Value)>) -> (String, Seen) {
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base: String = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let kept: Seen = Arc::clone(&seen);
    let answers: HashMap<&'static str, (u16, Value)> =
        answers.into_iter().map(|(k, s, b)| (k, (s, b))).collect();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: String = read_request(&mut socket).await;
            let line: String = request.lines().next().unwrap().to_owned();
            let key: String = line
                .rsplit_once(' ')
                .map_or(line.clone(), |(k, _)| k.to_owned());
            kept.lock().unwrap().push(request);
            let (status, body): (u16, Value) = answers
                .get(key.as_str())
                .cloned()
                .unwrap_or((404, json!({ "error": "nope" })));
            let text: String = if body.is_null() {
                String::new()
            } else {
                body.to_string()
            };
            let response: String = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (base, seen)
}

fn app(base: &str) -> Router {
    let accounts: Accounts = Accounts::new(
        Config::toddgriffin_at(base, "example", None, "https://www.example.com/cb").unwrap(),
    )
    .unwrap();
    friends_router::router(Arc::new(accounts), Arc::new(Auth))
}

async fn send(
    app: &Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let body: Body = body.map_or_else(Body::empty, |b| {
        request = std::mem::take(&mut request).header("content-type", "application/json");
        Body::from(b.to_string())
    });
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status: StatusCode = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

const COOKIE: [(&str, &str); 2] = [("cookie", "session=tok"), ("sec-fetch-site", "same-origin")];

fn person() -> Value {
    json!({ "sub": "u2", "username": "alex", "name": null, "picture": null, "since": "2026-11-15T10:00:00Z" })
}

#[tokio::test]
async fn a_guest_gets_401_and_never_reaches_the_accounts_service() {
    let (base, seen) = service(vec![]).await;
    let a: Router = app(&base);
    let expected: Value =
        json!({ "error": "account_required", "message": "Sign in to use friends." });
    let (status, body) = send(&a, Method::GET, "/api/v1/friends", &[], None).await;
    assert_eq!(StatusCode::UNAUTHORIZED, status);
    assert_eq!(expected, body);
    let (status, _) = send(
        &a,
        Method::POST,
        "/api/v1/friends/blocks",
        &[("sec-fetch-site", "same-origin")],
        Some(json!({"sub":"u2"})),
    )
    .await;
    assert_eq!(StatusCode::UNAUTHORIZED, status);
    assert_eq!(0, seen.lock().unwrap().len());
}

#[tokio::test]
async fn listing_friends_forwards_the_callers_token() {
    let (base, seen) = service(vec![(
        "GET /api/v1/friends",
        200,
        json!({ "friends": [person()] }),
    )])
    .await;
    let (status, body) = send(&app(&base), Method::GET, "/api/v1/friends", &COOKIE, None).await;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(json!({ "friends": [person()] }), body);
    let request: String = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(request.contains("authorization: bearer tok"));
}

#[tokio::test]
async fn requests_lists_incoming_and_outgoing() {
    let answer: Value = json!({ "incoming": [person()], "outgoing": [] });
    let (base, _) = service(vec![("GET /api/v1/friends/requests", 200, answer.clone())]).await;
    let (status, body) = send(
        &app(&base),
        Method::GET,
        "/api/v1/friends/requests",
        &COOKIE,
        None,
    )
    .await;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(answer, body);
}

#[tokio::test]
async fn sending_by_username_and_by_code_answers_the_result() {
    let (base, seen) = service(vec![(
        "POST /api/v1/friends/requests",
        200,
        json!({ "result": "requested", "sub": "u2" }),
    )])
    .await;
    let a: Router = app(&base);
    let (status, body) = send(
        &a,
        Method::POST,
        "/api/v1/friends/requests",
        &COOKIE,
        Some(json!({ "username": "alex" })),
    )
    .await;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(json!({ "result": "requested" }), body);
    let (status, _) = send(
        &a,
        Method::POST,
        "/api/v1/friends/requests",
        &COOKIE,
        Some(json!({ "friend_code": "ABCD2345" })),
    )
    .await;
    assert_eq!(StatusCode::OK, status);
    let requests: Vec<String> = seen.lock().unwrap().clone();
    assert!(requests[0].contains(r#"{"username":"alex"}"#));
    assert!(requests[1].contains(r#"{"friend_code":"ABCD2345"}"#));
}

#[tokio::test]
async fn sending_needs_exactly_one_of_username_and_code() {
    let (base, seen) = service(vec![]).await;
    let a: Router = app(&base);
    let (status, body) = send(
        &a,
        Method::POST,
        "/api/v1/friends/requests",
        &COOKIE,
        Some(json!({})),
    )
    .await;
    assert_eq!(StatusCode::BAD_REQUEST, status);
    assert_eq!("bad_request", body["error"]);
    let (status, _) = send(
        &a,
        Method::POST,
        "/api/v1/friends/requests",
        &COOKIE,
        Some(json!({ "username": "a", "friend_code": "b" })),
    )
    .await;
    assert_eq!(StatusCode::BAD_REQUEST, status);
    assert_eq!(0, seen.lock().unwrap().len());
}

#[tokio::test]
async fn accept_decline_cancel_remove_block_and_unblock_each_hit_their_endpoint() {
    let (base, seen) = service(vec![
        ("POST /api/v1/friends/requests/u2/accept", 204, Value::Null),
        ("POST /api/v1/friends/requests/u3/decline", 204, Value::Null),
        ("DELETE /api/v1/friends/requests/u4", 204, Value::Null),
        ("DELETE /api/v1/friends/u5", 204, Value::Null),
        ("POST /api/v1/blocks", 204, Value::Null),
        ("DELETE /api/v1/blocks/u6", 204, Value::Null),
    ])
    .await;
    let a: Router = app(&base);
    for (method, path, body) in [
        (Method::POST, "/api/v1/friends/requests/u2/accept", None),
        (Method::POST, "/api/v1/friends/requests/u3/decline", None),
        (Method::DELETE, "/api/v1/friends/requests/u4", None),
        (Method::DELETE, "/api/v1/friends/u5", None),
        (
            Method::POST,
            "/api/v1/friends/blocks",
            Some(json!({ "sub": "u7" })),
        ),
        (Method::DELETE, "/api/v1/friends/blocks/u6", None),
    ] {
        let (status, _) = send(&a, method, path, &COOKIE, body).await;
        assert_eq!(StatusCode::NO_CONTENT, status, "{path}");
    }
    let lines: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.lines().next().unwrap().to_owned())
        .collect();
    let expected: Vec<String> = [
        "POST /api/v1/friends/requests/u2/accept HTTP/1.1",
        "POST /api/v1/friends/requests/u3/decline HTTP/1.1",
        "DELETE /api/v1/friends/requests/u4 HTTP/1.1",
        "DELETE /api/v1/friends/u5 HTTP/1.1",
        "POST /api/v1/blocks HTTP/1.1",
        "DELETE /api/v1/blocks/u6 HTTP/1.1",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(expected, lines);
    assert!(seen.lock().unwrap()[4].contains(r#"{"sub":"u7"}"#));
}

#[tokio::test]
async fn blocks_and_relationships_are_read_through() {
    let blocks: Value = json!({ "blocked": [person()], "hidden": ["u2"] });
    let (base, _) = service(vec![
        ("GET /api/v1/blocks", 200, blocks.clone()),
        (
            "GET /api/v1/friends/relationships?subs=u2%2Cu3",
            200,
            json!({ "relationships": { "u2": "friends", "u3": "none" } }),
        ),
    ])
    .await;
    let a: Router = app(&base);
    let (_, body) = send(&a, Method::GET, "/api/v1/friends/blocks", &COOKIE, None).await;
    assert_eq!(blocks, body);
    let (status, body) = send(
        &a,
        Method::GET,
        "/api/v1/friends/relationships?subs=u2,u3",
        &COOKIE,
        None,
    )
    .await;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(
        json!({ "relationships": { "u2": "friends", "u3": "none" } }),
        body
    );
    let (status, _) = send(
        &a,
        Method::GET,
        "/api/v1/friends/relationships",
        &COOKIE,
        None,
    )
    .await;
    assert_eq!(StatusCode::BAD_REQUEST, status);
}

#[tokio::test]
async fn accounts_errors_become_clear_json_errors() {
    let (base, _) = service(vec![
        (
            "POST /api/v1/friends/requests",
            429,
            json!({ "error": "That's a lot of requests. Please wait an hour." }),
        ),
        (
            "POST /api/v1/friends/requests/u2/accept",
            400,
            json!({ "error": "That request is gone." }),
        ),
        (
            "DELETE /api/v1/friends/u2",
            401,
            json!({ "error": "invalid_token" }),
        ),
        (
            "DELETE /api/v1/friends/u3",
            403,
            json!({ "error": "insufficient_scope" }),
        ),
        ("GET /api/v1/friends", 500, Value::Null),
    ])
    .await;
    let a: Router = app(&base);
    let cases: [(Method, &str, Option<Value>, StatusCode, Value); 5] = [
        (
            Method::POST,
            "/api/v1/friends/requests",
            Some(json!({ "username": "x" })),
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "rate_limited", "message": "That's a lot of requests. Please wait an hour." }),
        ),
        (
            Method::POST,
            "/api/v1/friends/requests/u2/accept",
            None,
            StatusCode::BAD_REQUEST,
            json!({ "error": "refused", "message": "That request is gone." }),
        ),
        (
            Method::DELETE,
            "/api/v1/friends/u2",
            None,
            StatusCode::UNAUTHORIZED,
            json!({ "error": "session_expired", "message": "Please sign in again." }),
        ),
        (
            Method::DELETE,
            "/api/v1/friends/u3",
            None,
            StatusCode::FORBIDDEN,
            json!({ "error": "missing_scope", "message": "Friends aren't enabled for this account here." }),
        ),
        (
            Method::GET,
            "/api/v1/friends",
            None,
            StatusCode::BAD_GATEWAY,
            json!({ "error": "accounts_unavailable", "message": "Friends are unavailable right now. Try again soon." }),
        ),
    ];
    for (method, path, body, status, expected) in cases {
        let (actual_status, actual) = send(&a, method, path, &COOKIE, body).await;
        assert_eq!(status, actual_status, "{path}");
        assert_eq!(expected, actual, "{path}");
    }
}

const ACCEPT: &str = "/api/v1/friends/requests/u2/accept";

async fn accept_with(headers: &[(&str, &str)]) -> (StatusCode, usize) {
    let (base, seen) = service(vec![(
        "POST /api/v1/friends/requests/u2/accept",
        204,
        Value::Null,
    )])
    .await;
    let (status, _) = send(&app(&base), Method::POST, ACCEPT, headers, None).await;
    let count: usize = seen.lock().unwrap().len();
    (status, count)
}

#[tokio::test]
async fn a_cookie_write_must_be_provably_same_origin() {
    let cookie: (&str, &str) = ("cookie", "session=tok");
    let refused: [Vec<(&str, &str)>; 5] = [
        vec![cookie, ("sec-fetch-site", "cross-site")],
        vec![cookie, ("sec-fetch-site", "same-site")],
        vec![cookie, ("sec-fetch-site", "none")],
        vec![cookie, ("origin", "https://evil.example")],
        vec![cookie], // neither header: refused, never assumed same-origin
    ];
    for headers in refused {
        assert_eq!(
            (StatusCode::FORBIDDEN, 0),
            accept_with(&headers).await,
            "{headers:?}"
        );
    }
    assert_eq!(
        (StatusCode::NO_CONTENT, 1),
        accept_with(&[cookie, ("sec-fetch-site", "same-origin")]).await
    );
    assert_eq!(
        (StatusCode::NO_CONTENT, 1),
        accept_with(&[cookie, ("origin", ORIGIN)]).await
    );
}

#[tokio::test]
async fn a_bearer_header_never_excuses_a_cookie_authenticated_write() {
    let both: [(&str, &str); 3] = [
        ("cookie", "session=tok"),
        ("authorization", "Bearer apptok"),
        ("sec-fetch-site", "cross-site"),
    ];
    assert_eq!((StatusCode::FORBIDDEN, 0), accept_with(&both).await);
    assert_eq!(
        (StatusCode::NO_CONTENT, 1),
        accept_with(&[("authorization", "Bearer apptok")]).await
    );
}

#[tokio::test]
async fn an_id_that_could_leave_the_friends_paths_is_refused_before_any_call() {
    let (base, seen) = service(vec![]).await;
    let a: Router = app(&base);
    for path in [
        "/api/v1/friends/requests/..%2F..%2Foidc%2Fx/accept",
        "/api/v1/friends/requests/u2%2F..%2Fx/decline",
        "/api/v1/friends/requests/u2%3Fx=1/accept",
        "/api/v1/friends/requests/u2%23x/accept",
    ] {
        let method: Method = if path.ends_with('/') {
            Method::DELETE
        } else {
            Method::POST
        };
        let (status, body) = send(&a, method, path, &COOKIE, None).await;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{path}");
        assert_eq!("bad_request", body["error"], "{path}");
    }
    for (method, path) in [
        (Method::DELETE, "/api/v1/friends/requests/a%2Fb"),
        (Method::DELETE, "/api/v1/friends/a%3Fb"),
        (Method::DELETE, "/api/v1/friends/blocks/%2E%2E%2Fx"),
    ] {
        let (status, _) = send(&a, method, path, &COOKIE, None).await;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{path}");
    }
    let (status, _) = send(
        &a,
        Method::POST,
        "/api/v1/friends/blocks",
        &COOKIE,
        Some(json!({ "sub": "../x" })),
    )
    .await;
    assert_eq!(StatusCode::BAD_REQUEST, status);
    let (status, _) = send(
        &a,
        Method::GET,
        "/api/v1/friends/relationships?subs=u2,a%2Fb",
        &COOKIE,
        None,
    )
    .await;
    assert_eq!(StatusCode::BAD_REQUEST, status);
    assert_eq!(0, seen.lock().unwrap().len());
}

#[tokio::test]
async fn conflicts_and_unprocessable_answers_keep_their_meaning() {
    let (base, _) = service(vec![
        (
            "DELETE /api/v1/friends/u2",
            409,
            json!({ "error": "Already changed." }),
        ),
        (
            "DELETE /api/v1/friends/u3",
            422,
            json!({ "error": "Can't do that." }),
        ),
    ])
    .await;
    let a: Router = app(&base);
    let (status, body) = send(&a, Method::DELETE, "/api/v1/friends/u2", &COOKIE, None).await;
    assert_eq!(StatusCode::CONFLICT, status);
    assert_eq!(
        json!({ "error": "conflict", "message": "Already changed." }),
        body
    );
    let (status, body) = send(&a, Method::DELETE, "/api/v1/friends/u3", &COOKIE, None).await;
    assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
    assert_eq!(
        json!({ "error": "unprocessable", "message": "Can't do that." }),
        body
    );
}

/// Signed in as the player `sub` named by `x-player` (never a token): the router must act over the Server API.
struct PlayerAuth;

impl FriendsAuth for PlayerAuth {
    fn player_sub(
        &self,
        headers: &HeaderMap,
    ) -> impl Future<Output = Option<(String, Credential)>> + Send {
        let sub: Option<String> = headers
            .get("x-player")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let credential: Credential = if headers.contains_key("cookie") {
            Credential::Cookie(String::new())
        } else {
            Credential::Bearer(String::new())
        };
        std::future::ready(sub.map(|s| (s, credential)))
    }

    fn public_origin(&self) -> &str {
        ORIGIN
    }
}

fn player_app(base: &str, secret: Option<&str>) -> Router {
    let accounts: Accounts = Accounts::new(
        Config::toddgriffin_at(
            base,
            "example",
            secret.map(str::to_owned),
            "https://www.example.com/cb",
        )
        .unwrap(),
    )
    .unwrap();
    friends_router::router(Arc::new(accounts), Arc::new(PlayerAuth))
}

#[tokio::test]
async fn a_player_sub_acts_over_the_server_api_with_client_credentials_and_no_token() {
    let (base, seen) = service(vec![
        (
            "GET /api/v1/players/u1/friends",
            200,
            json!({ "friends": [person()] }),
        ),
        (
            "POST /api/v1/players/u1/friends/requests/u2/accept",
            204,
            Value::Null,
        ),
    ])
    .await;
    let a: Router = player_app(&base, Some("s3cret"));
    let (status, body) = send(
        &a,
        Method::GET,
        "/api/v1/friends",
        &[("x-player", "u1")],
        None,
    )
    .await;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(json!({ "friends": [person()] }), body);
    let (status, _) = send(
        &a,
        Method::POST,
        "/api/v1/friends/requests/u2/accept",
        &[("x-player", "u1")],
        None,
    )
    .await;
    assert_eq!(StatusCode::NO_CONTENT, status);

    let requests: Vec<String> = seen.lock().unwrap().clone();
    assert_eq!(2, requests.len());
    for request in &requests {
        // HTTP Basic of `example:s3cret`: the project, not a player's bearer token.
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: basic zxhhbxbsztpzm2nyzxq="),
            "{request}"
        );
    }
}

#[tokio::test]
async fn a_player_cookie_write_still_needs_the_same_origin_check_and_a_guest_gets_401() {
    let (base, seen) = service(vec![(
        "POST /api/v1/players/u1/friends/requests/u2/accept",
        204,
        Value::Null,
    )])
    .await;
    let a: Router = player_app(&base, Some("s3cret"));
    let accept = "/api/v1/friends/requests/u2/accept";
    let (status, _) = send(
        &a,
        Method::POST,
        accept,
        &[("x-player", "u1"), ("cookie", "s=1")],
        None,
    )
    .await;
    assert_eq!(StatusCode::FORBIDDEN, status);
    let (status, _) = send(&a, Method::POST, accept, &[], None).await;
    assert_eq!(StatusCode::UNAUTHORIZED, status);
    assert_eq!(0, seen.lock().unwrap().len());
    let (status, _) = send(
        &a,
        Method::POST,
        accept,
        &[
            ("x-player", "u1"),
            ("cookie", "s=1"),
            ("sec-fetch-site", "same-origin"),
        ],
        None,
    )
    .await;
    assert_eq!(StatusCode::NO_CONTENT, status);
}

#[tokio::test]
async fn server_api_errors_keep_their_meaning_and_a_missing_secret_is_unavailable() {
    let (base, _) = service(vec![(
        "POST /api/v1/players/u1/friends/requests/u2/accept",
        404,
        json!({ "error": "not_found", "message": "No such request." }),
    )])
    .await;
    let (status, body) = send(
        &player_app(&base, Some("s3cret")),
        Method::POST,
        "/api/v1/friends/requests/u2/accept",
        &[("x-player", "u1")],
        None,
    )
    .await;
    assert_eq!(StatusCode::NOT_FOUND, status);
    assert_eq!(
        json!({ "error": "not_found", "message": "No such request." }),
        body
    );
    let (status, _) = send(
        &player_app(&base, None),
        Method::GET,
        "/api/v1/friends",
        &[("x-player", "u1")],
        None,
    )
    .await;
    assert_eq!(StatusCode::BAD_GATEWAY, status);
}
