//! `Accounts::player` and `report_as_player` against a fake Server API.

mod common;

use base64::Engine;
use common::{Fake, Reply, Seen};
use serde_json::{Value, json};
use toddstack_accounts_server_sdk::friends::{Event, Relationship, Sent};
use toddstack_accounts_server_sdk::report::Reason;
use toddstack_accounts_server_sdk::{Accounts, Config, Error, Visibility};

const SUB: &str = "4b0b2c6e-0000-4000-8000-000000000001";
const OTHER: &str = "4b0b2c6e-0000-4000-8000-000000000002";

fn accounts(issuer: &str, secret: Option<&str>) -> Accounts {
    Accounts::new(Config {
        issuer: issuer.to_owned(),
        client_id: String::from("boggledygook"),
        client_secret: secret.map(str::to_owned),
        redirect_uri: String::from("https://www.boggledygook.com/auth/callback"),
    })
    .unwrap()
}

fn ok(body: &Value) -> Reply {
    (200, body.to_string())
}

fn no_content() -> Reply {
    (204, String::new())
}

fn basic() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("boggledygook:s3cret")
    )
}

fn only(fake: &Fake) -> Seen {
    let all: Vec<Seen> = fake.requests();
    assert_eq!(1, all.len(), "{all:?}");
    all[0].clone()
}

#[tokio::test]
async fn the_profile_reads_every_claim_and_the_review_and_suspended_flags() {
    let fake: Fake = Fake::start(|_, _| {
        ok(&json!({
            "sub": SUB, "username": "alex", "preferred_username": "alex", "name": "Alex",
            "picture": "https://assets.toddgriffin.me/a.jpg", "updated_at": 1_700_000_000,
            "email": "a@b.c", "email_verified": true, "visibility": "review", "suspended": true,
        }))
    })
    .await;
    let profile = accounts(&fake.issuer, Some("s3cret"))
        .player(SUB)
        .unwrap()
        .profile()
        .await
        .unwrap();
    let expected = toddstack_accounts_server_sdk::player::Profile {
        sub: String::from(SUB),
        username: Some(String::from("alex")),
        preferred_username: Some(String::from("alex")),
        name: Some(String::from("Alex")),
        picture: Some(String::from("https://assets.toddgriffin.me/a.jpg")),
        updated_at: Some(1_700_000_000),
        email: Some(String::from("a@b.c")),
        email_verified: Some(true),
        visibility: Visibility::Review,
        suspended: true,
    };
    assert_eq!(expected, profile);
    let seen: Seen = only(&fake);
    assert_eq!("GET", seen.method);
    assert_eq!(format!("/api/v1/players/{SUB}"), seen.target);
    assert_eq!(Some(basic()), seen.authorization);
}

#[tokio::test]
async fn every_route_uses_the_documented_method_path_and_body() {
    let fake: Fake = Fake::start(|_, seen| {
        if seen.target.ends_with("/friends/requests") && seen.method == "POST" {
            return ok(&json!({ "result": "accepted", "sub": OTHER }));
        }
        if seen.target.ends_with("/events") {
            return (202, json!({ "delivered": 2, "dropped": 0 }).to_string());
        }
        if seen.method == "GET" {
            return ok(&json!({
                "friends": [], "relationships": {}, "incoming": [], "outgoing": [],
                "blocked": [], "hidden": [], "items": [], "mutes": [],
            }));
        }
        no_content()
    })
    .await;
    let accounts: Accounts = accounts(&fake.issuer, Some("s3cret"));
    let p = accounts.player(SUB).unwrap();

    p.friends().await.unwrap();
    p.relationships(&[OTHER, "u3"]).await.unwrap();
    p.requests().await.unwrap();
    assert_eq!(Sent::Accepted, p.request(OTHER).await.unwrap());
    p.request_by_username("alex").await.unwrap();
    p.request_by_code("ABCD-1234").await.unwrap();
    p.accept(OTHER).await.unwrap();
    p.decline(OTHER).await.unwrap();
    p.cancel(OTHER).await.unwrap();
    p.remove(OTHER).await.unwrap();
    p.blocks().await.unwrap();
    p.block(OTHER).await.unwrap();
    p.unblock(OTHER).await.unwrap();
    p.post_event(
        &Event {
            kind: String::from("beat_score"),
            title: String::from("Alex won"),
            ..Event::default()
        },
        "game-1",
    )
    .await
    .unwrap();
    p.inbox(20).await.unwrap();
    p.mark_read("i1").await.unwrap();
    p.mutes().await.unwrap();
    p.mute("beat_score").await.unwrap();
    p.mute("*").await.unwrap();
    p.unmute("beat_score").await.unwrap();
    p.set_time_zone("America/Chicago").await.unwrap();
    p.report(OTHER, Reason::Cheating, Some("bot"))
        .await
        .unwrap();

    let base: String = format!("/api/v1/players/{SUB}");
    let expected: Vec<(String, String, Value)> = vec![
        ("GET", format!("{base}/friends"), Value::Null),
        (
            "GET",
            format!("{base}/friends/relationships?subs={OTHER}%2Cu3"),
            Value::Null,
        ),
        ("GET", format!("{base}/friends/requests"), Value::Null),
        ("POST", format!("{base}/friends/requests"), json!({ "sub": OTHER })),
        ("POST", format!("{base}/friends/requests"), json!({ "username": "alex" })),
        ("POST", format!("{base}/friends/requests"), json!({ "friend_code": "ABCD-1234" })),
        ("POST", format!("{base}/friends/requests/{OTHER}/accept"), Value::Null),
        ("POST", format!("{base}/friends/requests/{OTHER}/decline"), Value::Null),
        ("DELETE", format!("{base}/friends/requests/{OTHER}"), Value::Null),
        ("DELETE", format!("{base}/friends/{OTHER}"), Value::Null),
        ("GET", format!("{base}/blocks"), Value::Null),
        ("PUT", format!("{base}/blocks/{OTHER}"), Value::Null),
        ("DELETE", format!("{base}/blocks/{OTHER}"), Value::Null),
        (
            "POST",
            format!("{base}/events"),
            json!({ "kind": "beat_score", "title": "Alex won", "body": "", "idempotency_key": "game-1" }),
        ),
        ("GET", format!("{base}/inbox?limit=20"), Value::Null),
        ("POST", format!("{base}/inbox/i1/read"), Value::Null),
        ("GET", format!("{base}/mutes"), Value::Null),
        ("PUT", format!("{base}/mutes/beat_score"), Value::Null),
        ("PUT", format!("{base}/mutes/*"), Value::Null),
        ("DELETE", format!("{base}/mutes/beat_score"), Value::Null),
        (
            "PUT",
            format!("{base}/time-zone"),
            json!({ "time_zone": "America/Chicago" }),
        ),
        (
            "POST",
            String::from("/api/v1/reports"),
            json!({ "reporter_sub": SUB, "sub": OTHER, "reason": "cheating", "note": "bot" }),
        ),
    ]
    .into_iter()
    .map(|(m, t, b)| (String::from(m), t, b))
    .collect();
    let actual: Vec<(String, String, Value)> = fake
        .requests()
        .into_iter()
        .map(|s| {
            assert_eq!(Some(basic()), s.authorization, "{}", s.target);
            let body: Value = if s.body.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&s.body).unwrap()
            };
            (s.method, s.target, body)
        })
        .collect();
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn an_error_carries_the_status_code_and_message_and_is_not_retried() {
    let fake: Fake = Fake::start(|_, _| {
        (
            404,
            json!({ "error": "not_found", "message": "Not found." }).to_string(),
        )
    })
    .await;
    let error: Error = accounts(&fake.issuer, Some("s3cret"))
        .player(SUB)
        .unwrap()
        .request(OTHER)
        .await
        .unwrap_err();
    assert_eq!(Some("not_found"), error.code());
    assert!(
        matches!(&error, Error::Server { status: 404, code, message } if code == "not_found" && message == "Not found."),
        "{error:?}"
    );
    assert_eq!(1, fake.requests().len());
}

#[tokio::test]
async fn a_suspended_players_write_is_final_with_its_code() {
    let fake: Fake = Fake::start(|_, _| {
        (
            403,
            json!({ "error": "suspended", "message": "Suspended." }).to_string(),
        )
    })
    .await;
    let error: Error = accounts(&fake.issuer, Some("s3cret"))
        .player(SUB)
        .unwrap()
        .block(OTHER)
        .await
        .unwrap_err();
    assert_eq!(Some("suspended"), error.code());
    assert_eq!(1, fake.requests().len());
}

#[tokio::test]
async fn a_write_is_retried_after_a_server_error_because_every_write_is_idempotent() {
    let fake: Fake = Fake::start(|n, _| {
        if n == 0 {
            (
                503,
                json!({ "error": "server_error", "message": "x" }).to_string(),
            )
        } else {
            no_content()
        }
    })
    .await;
    accounts(&fake.issuer, Some("s3cret"))
        .player(SUB)
        .unwrap()
        .block(OTHER)
        .await
        .unwrap();
    let methods: Vec<String> = fake.requests().into_iter().map(|s| s.method).collect();
    assert_eq!(vec![String::from("PUT"), String::from("PUT")], methods);
}

#[tokio::test]
async fn a_rate_limit_is_final_for_a_write_and_its_code_is_readable() {
    let fake: Fake = Fake::start(|_, _| {
        (
            429,
            json!({ "error": "rate_limited", "message": "Slow down." }).to_string(),
        )
    })
    .await;
    let error: Error = accounts(&fake.issuer, Some("s3cret"))
        .player(SUB)
        .unwrap()
        .request(OTHER)
        .await
        .unwrap_err();
    assert_eq!(Some("rate_limited"), error.code());
}

#[tokio::test]
async fn an_event_needs_a_key_of_one_to_a_hundred_characters_and_sends_nothing_without() {
    let fake: Fake = Fake::start(|_, _| (202, json!({ "delivered": 0 }).to_string())).await;
    let accounts: Accounts = accounts(&fake.issuer, Some("s3cret"));
    let player = accounts.player(SUB).unwrap();
    let event: Event = Event {
        kind: String::from("k"),
        title: String::from("t"),
        ..Event::default()
    };
    for key in [String::new(), "k".repeat(101)] {
        let error: Error = player.post_event(&event, &key).await.unwrap_err();
        assert_eq!(Some("bad_request"), error.code());
    }
    assert!(fake.requests().is_empty());
    player.post_event(&event, &"k".repeat(100)).await.unwrap();
    assert_eq!(1, fake.requests().len());
}

#[tokio::test]
async fn a_replayed_event_says_so_and_carries_the_counts() {
    let fake: Fake = Fake::start(|_, _| {
        (
            202,
            json!({
                "delivered": 3, "dropped": 1, "replayed": true,
                "counts": { "delivered": 3, "scheduled": 0, "not_delivered": 2, "dropped_runaway": 1 },
            })
            .to_string(),
        )
    })
    .await;
    let result = accounts(&fake.issuer, Some("s3cret"))
        .player(SUB)
        .unwrap()
        .post_event(&Event::default(), "k")
        .await
        .unwrap();
    let expected = toddstack_accounts_server_sdk::friends::EventResult {
        replayed: true,
        delivered: 3,
        dropped: 1,
        counts: Some(toddstack_accounts_server_sdk::notifications::Counts {
            delivered: 3,
            scheduled: 0,
            not_delivered: 2,
            dropped_runaway: 1,
        }),
    };
    assert_eq!(expected, result);
}

#[tokio::test]
async fn relationships_over_a_hundred_subs_go_out_as_several_calls() {
    let fake: Fake = Fake::start(|_, _| ok(&json!({ "relationships": { "a": "friends" } }))).await;
    let accounts: Accounts = accounts(&fake.issuer, Some("s3cret"));
    let subs: Vec<String> = (0..150).map(|n| format!("u{n}")).collect();
    let refs: Vec<&str> = subs.iter().map(String::as_str).collect();
    let found = accounts
        .player(SUB)
        .unwrap()
        .relationships(&refs)
        .await
        .unwrap();
    assert_eq!(2, fake.requests().len());
    assert_eq!(Some(&Relationship::Friends), found.get("a"));
}

#[tokio::test]
async fn a_client_without_a_secret_or_a_path_unsafe_id_is_refused_before_any_request() {
    let fake: Fake = Fake::start(|_, _| no_content()).await;
    assert!(matches!(
        accounts(&fake.issuer, None).player(SUB),
        Err(Error::Config(_))
    ));
    let accounts: Accounts = accounts(&fake.issuer, Some("s3cret"));
    assert_eq!(
        Some("bad_request"),
        accounts
            .player("../x")
            .err()
            .and_then(|e| e.code().map(str::to_owned))
            .as_deref()
    );
    let player = accounts.player(SUB).unwrap();
    for bad in ["a/b", "", "a?b", "..", "a%2Fb"] {
        assert_eq!(
            Some("bad_request"),
            player.accept(bad).await.unwrap_err().code(),
            "{bad}"
        );
        assert_eq!(
            Some("bad_request"),
            player.mute(bad).await.unwrap_err().code(),
            "{bad}"
        );
    }
    assert!(matches!(
        accounts
            .report_as_player(&toddstack_accounts_server_sdk::report::PlayerReport {
                reporter_sub: String::from("a/b"),
                sub: String::from(OTHER),
                reason: Reason::Name,
                note: None,
            })
            .await,
        Err(Error::Server { .. })
    ));
    assert!(fake.requests().is_empty());
}
