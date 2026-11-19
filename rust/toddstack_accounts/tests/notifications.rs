//! The notification API, the device API and the time zone hint against a fake accounts API.
#![expect(deprecated, reason = "covers `Friends`, which stays until 0.10.0")]

use std::sync::{Arc, Mutex};

use toddstack_accounts::devices::{Device, Environment, Platform};
use toddstack_accounts::friends::Friends;
use toddstack_accounts::notifications::{
    Counts, Notification, Outcome, Receipt, Recipient, Status,
};
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

/// A fake service: request `n` gets reply `n` (the last repeats) as
/// `(status, body)`, and every raw request is kept.
async fn service(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer: String = format!("http://{}/oidc", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let kept: Arc<Mutex<Vec<String>>> = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: String = read_request(&mut socket).await;
            let n: usize = {
                let mut all = kept.lock().unwrap();
                all.push(request);
                all.len() - 1
            };
            let (status, body) = replies[n.min(replies.len() - 1)];
            let response: String = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
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

fn notification(key: Option<&str>) -> Notification {
    let mut notification: Notification = Notification::new(
        Recipient::User(String::from("u1")),
        "streak_at_risk",
        "Your streak ends tonight",
    );
    notification.send_at_local = Some(String::from("20:00"));
    notification.collapse_key = Some(String::from("streak"));
    notification.idempotency_key = key.map(str::to_owned);
    notification
}

const DONE: &str = r#"{"id":"b1","status":"done","recipients":1,"result":"delivered","counts":{"delivered":1,"scheduled":0,"not_delivered":0,"dropped_runaway":0}}"#;

#[tokio::test]
async fn a_send_goes_to_the_api_with_the_client_credentials_and_reads_the_receipt() {
    let (issuer, seen) = service(vec![(202, DONE)]).await;
    let actual: Receipt = accounts(&issuer, Some("s3cret"))
        .send_notification(&notification(Some("daily-2026-11-15")))
        .await
        .unwrap();
    let expected: Receipt = Receipt {
        id: String::from("b1"),
        status: Status::Done,
        recipients: 1,
        counts: Counts {
            delivered: 1,
            scheduled: 0,
            not_delivered: 0,
            dropped_runaway: 0,
        },
        result: Some(Outcome::Delivered),
        replayed: false,
    };
    assert_eq!(expected, actual);
    let request: String = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("POST /api/v1/notifications HTTP/1.1"),
        "{request}"
    );
    // base64 of "boggledygook:s3cret".
    assert!(
        request.contains("authorization: Basic Ym9nZ2xlZHlnb29rOnMzY3JldA=="),
        "{request}"
    );
    // Compared as JSON: the key order on the wire is not part of the contract.
    let expected_body: serde_json::Value = serde_json::json!({
        "recipient": { "user": "u1" },
        "type": "streak_at_risk",
        "title": "Your streak ends tonight",
        "body": "",
        "send_at_local": "20:00",
        "collapse_key": "streak",
        "idempotency_key": "daily-2026-11-15",
    });
    let actual_body: serde_json::Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(expected_body, actual_body);
}

#[tokio::test]
async fn a_replayed_fan_out_is_marked_and_polled_by_id() {
    let (issuer, seen) = service(vec![
        (
            202,
            r#"{"id":"b2","status":"queued","recipients":40,"counts":{"delivered":0,"scheduled":0,"not_delivered":0,"dropped_runaway":0},"replayed":true}"#,
        ),
        (
            200,
            r#"{"id":"b2","status":"done","recipients":40,"counts":{"delivered":37,"scheduled":0,"not_delivered":3,"dropped_runaway":0}}"#,
        ),
    ])
    .await;
    let accounts: Accounts = accounts(&issuer, Some("s3cret"));
    let mut fan_out: Notification = Notification::new(
        Recipient::AllPlayers,
        "daily_ready",
        "Today's daily is ready",
    );
    fan_out.idempotency_key = Some(String::from("daily-2026-11-15"));
    let sent: Receipt = accounts.send_notification(&fan_out).await.unwrap();
    assert_eq!(Status::Queued, sent.status);
    assert!(sent.replayed);
    assert_eq!(None, sent.result);

    let status: Receipt = accounts.notification_status(&sent.id).await.unwrap();
    let expected: Counts = Counts {
        delivered: 37,
        scheduled: 0,
        not_delivered: 3,
        dropped_runaway: 0,
    };
    assert_eq!(expected, status.counts);
    assert_eq!(Status::Done, status.status);
    assert!(!status.replayed);
    let request: String = seen.lock().unwrap()[1].clone();
    assert!(
        request.starts_with("GET /api/v1/notifications/b2 HTTP/1.1"),
        "{request}"
    );
    assert!(
        seen.lock().unwrap()[0].contains(r#""recipient":{"all_players":true}"#),
        "{}",
        seen.lock().unwrap()[0]
    );
}

#[tokio::test]
async fn cancelling_names_the_key_and_optionally_the_player() {
    let (issuer, seen) = service(vec![(200, r#"{"cancelled":2}"#)]).await;
    let accounts: Accounts = accounts(&issuer, Some("s3cret"));
    assert_eq!(
        2,
        accounts
            .cancel_notifications("streak", Some("u1"))
            .await
            .unwrap()
    );
    assert_eq!(
        2,
        accounts.cancel_notifications("streak", None).await.unwrap()
    );
    let requests: Vec<String> = seen.lock().unwrap().clone();
    assert!(
        requests[0].starts_with("POST /api/v1/notifications/cancel HTTP/1.1"),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].ends_with(r#"{"collapse_key":"streak","user":"u1"}"#),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].ends_with(r#"{"collapse_key":"streak"}"#),
        "{}",
        requests[1]
    );
}

#[tokio::test]
async fn refusals_become_typed_errors() {
    for (status, body) in [
        (404, r#"{"error":"Not found."}"#),
        (
            422,
            r#"{"error":"That idempotency key was used for a different request."}"#,
        ),
        (429, r#"{"error":"Too many notifications."}"#),
    ] {
        let (issuer, _seen) = service(vec![(status, body)]).await;
        let accounts: Accounts = accounts(&issuer, Some("s3cret"));
        let error: Error = accounts
            .send_notification(&notification(None))
            .await
            .unwrap_err();
        match (status, &error) {
            (404, Error::NotFound(m)) => assert_eq!("Not found.", m),
            (422, Error::Unprocessable(m)) => {
                assert_eq!("That idempotency key was used for a different request.", m);
            }
            (429, Error::RateLimited(m)) => assert_eq!("Too many notifications.", m),
            _ => panic!("{status} gave {error:?}"),
        }
    }
    let (issuer, _seen) = service(vec![(404, r#"{"error":"Not found."}"#)]).await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .notification_status("nope")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
}

#[tokio::test]
async fn a_send_without_a_key_is_sent_once_even_on_a_503() {
    let (issuer, seen) = service(vec![(503, "{}"), (202, DONE)]).await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .send_notification(&notification(None))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api { status: 503, .. }), "{error:?}");
    assert_eq!(1, seen.lock().unwrap().len());
}

#[tokio::test]
async fn a_send_with_a_key_is_retried_after_a_503() {
    let (issuer, seen) = service(vec![(503, "{}"), (202, DONE)]).await;
    let receipt: Receipt = accounts(&issuer, Some("s3cret"))
        .send_notification(&notification(Some("k1")))
        .await
        .unwrap();
    assert_eq!("b1", receipt.id);
    let requests: Vec<String> = seen.lock().unwrap().clone();
    assert_eq!(2, requests.len());
    assert!(
        requests
            .iter()
            .all(|r| r.contains(r#""idempotency_key":"k1""#))
    );
}

#[tokio::test]
async fn a_public_client_cannot_send_notifications() {
    let (issuer, seen) = service(vec![(202, DONE)]).await;
    let accounts: Accounts = accounts(&issuer, None);
    let error: Error = accounts
        .send_notification(&notification(Some("k1")))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(matches!(
        accounts.cancel_notifications("streak", None).await,
        Err(Error::Config(_))
    ));
    assert!(matches!(
        accounts.notification_status("b1").await,
        Err(Error::Config(_))
    ));
    assert_eq!(0, seen.lock().unwrap().len());
}

#[tokio::test]
async fn the_time_zone_hint_is_a_put_with_the_persons_token() {
    let (issuer, seen) = service(vec![(204, "")]).await;
    let accounts: Accounts = accounts(&issuer, None);
    Friends::new(&accounts, "tok")
        .set_time_zone("America/Chicago")
        .await
        .unwrap();
    let request: String = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("PUT /api/v1/me/time-zone HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains("authorization: Bearer tok"), "{request}");
    assert!(
        request.ends_with(r#"{"time_zone":"America/Chicago"}"#),
        "{request}"
    );
}

#[tokio::test]
async fn the_inbox_and_event_answers_carry_the_new_fields() {
    let (issuer, _seen) = service(vec![
        (
            200,
            r#"{"items":[{"id":"i1","client_id":null,"from":null,"from_username":null,"kind":"friend_accepted","title":"t","body":"","url":null,"created_at":"c","read_at":null}],"unread":{"project":0,"account":1,"total":1}}"#,
        ),
        (
            202,
            r#"{"delivered":2,"dropped":1,"counts":{"delivered":2,"scheduled":0,"not_delivered":0,"dropped_runaway":1}}"#,
        ),
    ])
    .await;
    let accounts: Accounts = accounts(&issuer, None);
    let friends: Friends<'_> = Friends::new(&accounts, "tok");
    let inbox = friends.inbox_with_unread(20).await.unwrap();
    assert_eq!(None, inbox.items[0].client_id);
    assert_eq!(1, inbox.unread.unwrap().account);
    let result = friends
        .post_event_result(&toddstack_accounts::friends::Event {
            kind: String::from("k"),
            title: String::from("t"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!((2, 1), (result.delivered, result.dropped));
    assert_eq!(1, result.counts.unwrap().dropped_runaway);
}

fn device() -> Device {
    Device {
        sub: String::from("u1"),
        platform: Platform::Ios,
        token: String::from("ab12cd34"),
        app_id: String::from("me.toddgriffin.boggledygook"),
        environment: Environment::Production,
    }
}

#[tokio::test]
async fn registering_a_device_posts_it_with_the_client_credentials() {
    let (issuer, seen) = service(vec![(204, "")]).await;
    accounts(&issuer, Some("s3cret"))
        .register_device(&device())
        .await
        .unwrap();
    let request: String = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("POST /api/v1/devices HTTP/1.1"),
        "{request}"
    );
    assert!(
        request.contains("authorization: Basic Ym9nZ2xlZHlnb29rOnMzY3JldA=="),
        "{request}"
    );
    let expected: &str = r#"{"app_id":"me.toddgriffin.boggledygook","environment":"production","platform":"ios","sub":"u1","token":"ab12cd34"}"#;
    assert!(request.ends_with(expected), "{request}");
}

#[tokio::test]
async fn unregistering_a_device_deletes_by_platform_and_token() {
    let (issuer, seen) = service(vec![(204, "")]).await;
    accounts(&issuer, Some("s3cret"))
        .unregister_device(Platform::Android, "fcm-token")
        .await
        .unwrap();
    let request: String = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("DELETE /api/v1/devices HTTP/1.1"),
        "{request}"
    );
    assert!(
        request.ends_with(r#"{"platform":"android","token":"fcm-token"}"#),
        "{request}"
    );
}

#[tokio::test]
async fn device_refusals_become_typed_errors() {
    let (issuer, _seen) = service(vec![(
        409,
        r#"{"error":"That device token is registered to another project."}"#,
    )])
    .await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .register_device(&device())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Conflict(m) if m == "That device token is registered to another project."),
        "{error:?}"
    );

    let (issuer, _seen) = service(vec![(404, r#"{"error":"Not found."}"#)]).await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .register_device(&device())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");

    let (issuer, _seen) = service(vec![(
        403,
        r#"{"error":"That app id isn't registered to this project."}"#,
    )])
    .await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .register_device(&device())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api { status: 403, .. }), "{error:?}");
}

#[tokio::test]
async fn a_device_call_is_retried_after_a_503_because_it_is_idempotent() {
    let (issuer, seen) = service(vec![(503, "{}"), (204, "")]).await;
    accounts(&issuer, Some("s3cret"))
        .register_device(&device())
        .await
        .unwrap();
    assert_eq!(2, seen.lock().unwrap().len());
}

#[tokio::test]
async fn a_public_client_cannot_register_devices() {
    let (issuer, seen) = service(vec![(204, "")]).await;
    let accounts: Accounts = accounts(&issuer, None);
    assert!(matches!(
        accounts.register_device(&device()).await,
        Err(Error::Config(_))
    ));
    assert!(matches!(
        accounts.unregister_device(Platform::Ios, "ab12").await,
        Err(Error::Config(_))
    ));
    assert_eq!(0, seen.lock().unwrap().len());
}

const FRIENDS: &str = r#"{"friends":{"u1":["u2","u3"],"u2":["u1"],"u4":[]}}"#;

#[tokio::test]
async fn friends_of_posts_the_distinct_subs_as_the_project_and_reads_the_map() {
    let (issuer, seen) = service(vec![(200, FRIENDS)]).await;
    let actual: std::collections::BTreeMap<String, Vec<String>> = accounts(&issuer, Some("s3cret"))
        .friends_of(&["u2", "u1", "u4", "u1"])
        .await
        .unwrap();
    let expected: std::collections::BTreeMap<String, Vec<String>> = [
        (
            String::from("u1"),
            vec![String::from("u2"), String::from("u3")],
        ),
        (String::from("u2"), vec![String::from("u1")]),
        (String::from("u4"), vec![]),
    ]
    .into();
    assert_eq!(expected, actual);
    let request: String = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("POST /api/v1/friends-of HTTP/1.1"),
        "{request}"
    );
    assert!(
        request.contains("authorization: Basic Ym9nZ2xlZHlnb29rOnMzY3JldA=="),
        "{request}"
    );
    let expected_body: serde_json::Value = serde_json::json!({ "subs": ["u1", "u2", "u4"] });
    let actual_body: serde_json::Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(expected_body, actual_body);
}

#[tokio::test]
async fn friends_of_lowercases_subs_before_deduping() {
    let (issuer, seen) = service(vec![(200, FRIENDS)]).await;
    accounts(&issuer, Some("s3cret"))
        .friends_of(&["ABC-1", "abc-1", "Abc-1"])
        .await
        .unwrap();
    let request: String = seen.lock().unwrap()[0].clone();
    let actual_body: serde_json::Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(serde_json::json!({ "subs": ["abc-1"] }), actual_body);
}

#[tokio::test]
async fn friends_of_is_retried_after_a_503_and_maps_refusals_to_typed_errors() {
    let (issuer, seen) = service(vec![(503, "{}"), (200, FRIENDS)]).await;
    let friends = accounts(&issuer, Some("s3cret"))
        .friends_of(&["u1"])
        .await
        .unwrap();
    assert_eq!(3, friends.len());
    assert_eq!(2, seen.lock().unwrap().len());

    let (issuer, _) = service(vec![(404, r#"{"error":"Not found."}"#)]).await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .friends_of(&["nope"])
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::NotFound(m) if m == "Not found."),
        "{error:?}"
    );

    let (issuer, _) = service(vec![(400, r#"{"error":"Give 1 to 100 distinct subs."}"#)]).await;
    let error: Error = accounts(&issuer, Some("s3cret"))
        .friends_of(&[])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api { status: 400, .. }), "{error:?}");
}

#[tokio::test]
async fn a_public_client_cannot_ask_for_friends_of() {
    let (issuer, seen) = service(vec![(200, FRIENDS)]).await;
    let error: Error = accounts(&issuer, None)
        .friends_of(&["u1"])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert_eq!(0, seen.lock().unwrap().len());
}
