//! Response compression, through the router `run` serves.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{StreamExt, stream};
use tower::ServiceExt;

use crate::environment::Environment;

use super::{Shutdown, WebServer, WebServerState};

/// Repetitive enough to compress well, long enough to clear the size floor.
fn big_body() -> String {
    "{\"word\":\"example\",\"points\":11}".repeat(400)
}

fn routes() -> Router<WebServerState> {
    Router::new()
        .route("/big", get(|| async { big_body() }))
        .route(
            "/upgrade",
            get(|| async {
                (
                    StatusCode::SWITCHING_PROTOCOLS,
                    [
                        (header::CONNECTION, "upgrade"),
                        (header::UPGRADE, "websocket"),
                    ],
                )
                    .into_response()
            }),
        )
        .route("/tiny", get(|| async { "ok" }))
        .route(
            "/events",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(
                        stream::iter([Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            big_body(),
                        ))])
                        .chain(stream::pending()),
                    ),
                )
                    .into_response()
            }),
        )
}

fn server() -> WebServer {
    WebServer::new("127.0.0.1", 0, Environment::Local)
}

fn router(server: WebServer) -> Router {
    server
        .merge(routes())
        .into_router(Shutdown::manual())
        .expect("a router with no frontend always boots")
}

async fn fetch(router: &Router, uri: &str, accept_encoding: Option<&str>) -> Response {
    let mut request = Request::builder().method(Method::GET).uri(uri);
    if let Some(value) = accept_encoding {
        request = request.header(header::ACCEPT_ENCODING, value);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).expect("a valid request"))
        .await
        .expect("a router never fails")
}

fn header_text(response: &Response, name: header::HeaderName) -> &str {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

async fn bytes(response: Response) -> Bytes {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a readable body")
}

#[tokio::test]
async fn gzip_is_used_when_the_client_accepts_only_gzip() {
    let response: Response = fetch(&router(server()), "/big", Some("gzip")).await;
    assert_eq!("gzip", header_text(&response, header::CONTENT_ENCODING));
    let body: Bytes = bytes(response).await;
    assert_eq!([0x1f, 0x8b], body[..2]);
    assert!(body.len() < big_body().len() / 10, "{}", body.len());
}

#[tokio::test]
async fn zstd_is_used_when_the_client_accepts_only_zstd() {
    let response: Response = fetch(&router(server()), "/big", Some("zstd")).await;
    assert_eq!("zstd", header_text(&response, header::CONTENT_ENCODING));
    let body: Bytes = bytes(response).await;
    assert_eq!([0x28, 0xb5, 0x2f, 0xfd], body[..4]);
}

#[tokio::test]
async fn brotli_is_used_when_the_client_accepts_only_brotli() {
    let response: Response = fetch(&router(server()), "/big", Some("br")).await;
    assert_eq!("br", header_text(&response, header::CONTENT_ENCODING));
    assert!(bytes(response).await.len() < big_body().len() / 10);
}

#[tokio::test]
async fn a_client_that_accepts_nothing_gets_the_identity_body() {
    let response: Response = fetch(&router(server()), "/big", None).await;
    assert_eq!("", header_text(&response, header::CONTENT_ENCODING));
    assert_eq!(big_body(), String::from_utf8_lossy(&bytes(response).await));
}

#[tokio::test]
async fn vary_names_accept_encoding_whether_or_not_the_body_was_compressed() {
    let router: Router = router(server());
    for accept in [Some("gzip"), None] {
        let response: Response = fetch(&router, "/big", accept).await;
        assert_eq!(
            "accept-encoding",
            header_text(&response, header::VARY).to_ascii_lowercase(),
            "{accept:?}"
        );
    }
}

#[tokio::test]
async fn a_tiny_body_is_left_alone() {
    let response: Response = fetch(&router(server()), "/tiny", Some("gzip, br, zstd")).await;
    assert_eq!("", header_text(&response, header::CONTENT_ENCODING));
    assert_eq!("ok", String::from_utf8_lossy(&bytes(response).await));
}

#[tokio::test]
async fn a_compressed_response_drops_content_length() {
    let response: Response = fetch(&router(server()), "/big", Some("gzip")).await;
    assert_eq!(StatusCode::OK, response.status());
    assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
}

#[tokio::test]
async fn an_uncompressed_response_keeps_its_content_length() {
    let response: Response = fetch(&router(server()), "/big", None).await;
    assert_eq!(
        big_body().len().to_string(),
        header_text(&response, header::CONTENT_LENGTH)
    );
}

#[tokio::test]
async fn an_event_stream_is_never_compressed_and_flushes_each_event() {
    let response: Response = fetch(&router(server()), "/events", Some("gzip, br, zstd")).await;
    assert_eq!("", header_text(&response, header::CONTENT_ENCODING));
    assert_eq!(
        "text/event-stream",
        header_text(&response, header::CONTENT_TYPE)
    );
    let mut events = response.into_body().into_data_stream();
    // The stream never ends on its own; the first chunk must still arrive.
    let first: Bytes = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
        .await
        .expect("the first event arrives before the stream ends")
        .expect("the stream has a first event")
        .expect("a readable chunk");
    assert_eq!(big_body().as_bytes(), &first[..]);
}

#[tokio::test]
async fn without_compression_serves_identity_even_to_a_client_that_asks() {
    let response: Response = fetch(
        &router(server().without_compression()),
        "/big",
        Some("gzip, br, zstd"),
    )
    .await;
    assert_eq!("", header_text(&response, header::CONTENT_ENCODING));
    assert_eq!(big_body(), String::from_utf8_lossy(&bytes(response).await));
}

#[tokio::test]
async fn a_websocket_upgrade_response_is_not_touched() {
    let response: Response = fetch(&router(server()), "/upgrade", Some("gzip, br, zstd")).await;
    assert_eq!(StatusCode::SWITCHING_PROTOCOLS, response.status());
    assert_eq!("", header_text(&response, header::CONTENT_ENCODING));
    assert_eq!("websocket", header_text(&response, header::UPGRADE));
}
