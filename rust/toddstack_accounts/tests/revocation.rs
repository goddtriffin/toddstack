//! Local access-token verification with instant logout: the revoked-families
//! set, fed by the `tokens.revoked` webhook and the poller.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Fake;
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use p256::SecretKey;
use p256::pkcs8::EncodePrivateKey;
use serde_json::{Value, json};
use sha2::Sha256;
use toddstack_accounts::revocation::{PollerOptions, Revocations};
use toddstack_accounts::{Accounts, Config, Error};

const KID: &str = "test-key";
const SUB: &str = "4b0b2c6e-0000-4000-8000-000000000001";

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn rfc3339(unix: i64) -> String {
    jiff::Timestamp::from_second(unix).unwrap().to_string()
}

struct Signer {
    encoding: EncodingKey,
    jwks: JwkSet,
}

fn signer() -> Signer {
    let secret: SecretKey = SecretKey::from_slice(&[7_u8; 32]).unwrap();
    let der = secret.to_pkcs8_der().unwrap();
    let point: Box<[u8]> = secret.public_key().to_sec1_bytes();
    let jwks: JwkSet = serde_json::from_value(json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": KID, "use": "sig", "alg": "ES256",
        "x": b64(&point[1..33]), "y": b64(&point[33..65])
    }]}))
    .unwrap();
    Signer {
        encoding: EncodingKey::from_ec_der(der.as_bytes()),
        jwks,
    }
}

fn token(s: &Signer, issuer: &str, fam: Option<&str>, exp: i64) -> String {
    let mut claims: Value = json!({
        "iss": issuer, "aud": "boggledygook", "sub": SUB, "client_id": "boggledygook",
        "scope": "openid", "iat": now(), "exp": exp,
    });
    if let Some(fam) = fam {
        claims["fam"] = json!(fam);
    }
    let mut header: Header = Header::new(Algorithm::ES256);
    header.kid = Some(String::from(KID));
    header.typ = Some(String::from("at+jwt"));
    encode(&header, &claims, &s.encoding).unwrap()
}

fn accounts(s: &Signer, issuer: &str) -> Arc<Accounts> {
    Arc::new(
        Accounts::with_keys(
            Config {
                issuer: issuer.to_owned(),
                client_id: String::from("boggledygook"),
                client_secret: Some(String::from("s3cret")),
                redirect_uri: String::from("https://www.boggledygook.com/auth/callback"),
            },
            s.jwks.clone(),
        )
        .unwrap(),
    )
}

#[test]
fn a_family_is_revoked_until_its_expiry_and_repeats_keep_the_later_expiry() {
    let set: Revocations = Revocations::new();
    assert!(!set.is_revoked("f1"));
    set.revoke("f1", now() + 60);
    set.revoke("f1", now() + 600);
    set.revoke("f1", now() + 10);
    set.revoke("old", now() - 1);
    assert!(set.is_revoked("f1"));
    assert!(!set.is_revoked("old"));
    assert!(!set.is_revoked("other"));
    assert_eq!(1, set.len());
}

fn signed(secret: &str, t: i64, body: &str) -> String {
    let mut mac: Hmac<Sha256> =
        <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
}

#[tokio::test]
async fn a_verified_tokens_revoked_webhook_kills_those_families_at_once() {
    let s: Signer = signer();
    let accounts: Arc<Accounts> = accounts(&s, "https://accounts.toddgriffin.me/oidc");
    let good: String = token(
        &s,
        "https://accounts.toddgriffin.me/oidc",
        Some("f-dead"),
        now() + 600,
    );
    let other: String = token(
        &s,
        "https://accounts.toddgriffin.me/oidc",
        Some("f-live"),
        now() + 600,
    );
    assert_eq!(
        Some(String::from("f-dead")),
        accounts.verify_access_token(&good).await.unwrap().fam
    );

    let body: String = json!({
        "id": "e1", "type": "tokens.revoked", "created_at": now(),
        "data": { "sub": SUB, "families": ["f-dead", "f-dead", "f-x"] },
    })
    .to_string();
    let event = accounts
        .handle_webhook(b"whsec", &signed("whsec", now(), &body), body.as_bytes())
        .unwrap();
    assert_eq!(vec!["f-dead", "f-dead", "f-x"], event.families());

    let refused: Error = accounts.verify_access_token(&good).await.unwrap_err();
    assert!(
        matches!(&refused, Error::InvalidToken(why) if why == "token revoked"),
        "{refused:?}"
    );
    assert!(accounts.verify_access_token(&other).await.is_ok());
}

#[tokio::test]
async fn a_webhook_with_a_bad_signature_or_another_type_revokes_nothing() {
    let s: Signer = signer();
    let accounts: Arc<Accounts> = accounts(&s, "https://accounts.toddgriffin.me/oidc");
    let body: String = json!({
        "id": "e1", "type": "tokens.revoked", "created_at": now(),
        "data": { "sub": SUB, "families": ["f1"] },
    })
    .to_string();
    assert!(
        accounts
            .handle_webhook(b"whsec", &signed("wrong", now(), &body), body.as_bytes())
            .is_err()
    );
    let other: String = json!({
        "id": "e2", "type": "user.deleted", "created_at": now(), "data": { "sub": SUB, "families": ["f1"] },
    })
    .to_string();
    accounts
        .handle_webhook(b"whsec", &signed("whsec", now(), &other), other.as_bytes())
        .unwrap();
    assert!(accounts.revocations().is_empty());
}

#[tokio::test]
async fn a_token_without_a_family_still_verifies_and_the_other_checks_still_run() {
    let s: Signer = signer();
    let issuer: &str = "https://accounts.toddgriffin.me/oidc";
    let accounts: Arc<Accounts> = accounts(&s, issuer);
    assert_eq!(
        None,
        accounts
            .verify_access_token(&token(&s, issuer, None, now() + 600))
            .await
            .unwrap()
            .fam
    );
    // Small skew allowed, an old token not.
    assert!(
        accounts
            .verify_access_token(&token(&s, issuer, None, now() - 10))
            .await
            .is_ok()
    );
    assert!(
        accounts
            .verify_access_token(&token(&s, issuer, None, now() - 120))
            .await
            .is_err()
    );
    assert!(
        accounts
            .verify_access_token(&token(&s, "https://evil.example/oidc", None, now() + 600))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_poll_records_families_dedupes_repeats_and_moves_the_cursor() {
    let s: Signer = signer();
    let t1: i64 = now() + 600;
    let fake: Fake = Fake::start(move |n, _| {
        let body: Value = if n == 0 {
            json!({
                "families": [{ "fam": "f1", "expires_at": rfc3339(t1) }, { "fam": "f1", "expires_at": rfc3339(t1) }],
                "now": "2026-11-18T10:00:00Z",
            })
        } else {
            json!({ "families": [{ "fam": "f2", "expires_at": rfc3339(t1) }], "now": "2026-11-18T10:00:30Z" })
        };
        (200, body.to_string())
    })
    .await;
    let accounts: Arc<Accounts> = accounts(&s, &fake.issuer);
    let mut cursor: Option<String> = None;
    assert_eq!(2, accounts.poll_revocations(&mut cursor).await.unwrap());
    assert_eq!(Some(String::from("2026-11-18T10:00:00Z")), cursor);
    assert_eq!(1, accounts.poll_revocations(&mut cursor).await.unwrap());
    assert_eq!(Some(String::from("2026-11-18T10:00:30Z")), cursor);

    assert_eq!(2, accounts.revocations().len());
    assert!(accounts.revocations().is_revoked("f1"));
    assert!(accounts.revocations().is_revoked("f2"));

    let seen = fake.requests();
    assert_eq!("/api/v1/revoked-tokens", seen[0].target);
    assert_eq!(
        "/api/v1/revoked-tokens?since=2026-11-18T10%3A00%3A00Z",
        seen[1].target
    );
    assert_eq!("GET", seen[0].method);
    assert_eq!("", seen[0].body);
    assert!(
        seen[0]
            .authorization
            .as_deref()
            .is_some_and(|a| a.starts_with("Basic "))
    );
}

#[tokio::test]
async fn the_poller_polls_at_startup_before_returning_then_keeps_polling_until_dropped() {
    let s: Signer = signer();
    let fake: Fake = Fake::start(move |n, _| {
        let fam: String = format!("f{n}");
        (
            200,
            json!({
                "families": [{ "fam": fam, "expires_at": rfc3339(now() + 600) }],
                "now": rfc3339(now()),
            })
            .to_string(),
        )
    })
    .await;
    let accounts: Arc<Accounts> = accounts(&s, &fake.issuer);
    let poller = accounts
        .start_revocation_poller_with(PollerOptions {
            interval: Duration::from_millis(30),
            on_error: None,
        })
        .await
        .unwrap();
    // The startup poll is awaited: its family is already refused.
    assert!(accounts.revocations().is_revoked("f0"));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        accounts.revocations().is_revoked("f2"),
        "{:?}",
        fake.requests().len()
    );
    drop(poller);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let before: usize = fake.requests().len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(before, fake.requests().len());
}

#[tokio::test]
async fn a_failing_poll_reaches_on_error_keeps_verifying_and_recovers() {
    let s: Signer = signer();
    let fake: Fake = Fake::start(|n, _| {
        if n < 3 {
            (
                500,
                json!({ "error": "server_error", "message": "x" }).to_string(),
            )
        } else {
            (
                200,
                json!({
                    "families": [{ "fam": "late", "expires_at": rfc3339(now() + 600) }],
                    "now": rfc3339(now()),
                })
                .to_string(),
            )
        }
    })
    .await;
    let accounts: Arc<Accounts> = accounts(&s, &fake.issuer);
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let heard: Arc<Mutex<Vec<String>>> = Arc::clone(&errors);
    let poller = accounts
        .start_revocation_poller_with(PollerOptions {
            interval: Duration::from_millis(30),
            on_error: Some(Arc::new(move |e: &Error| {
                heard.lock().unwrap().push(e.to_string());
            })),
        })
        .await
        .unwrap();
    assert_eq!(1, errors.lock().unwrap().len());
    assert!(
        accounts
            .verify_access_token(&token(&s, &fake.issuer, Some("late"), now() + 600))
            .await
            .is_ok()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(accounts.revocations().is_revoked("late"));
    drop(poller);
}

#[tokio::test]
async fn the_poller_needs_the_client_secret() {
    let accounts: Arc<Accounts> = Arc::new(
        Accounts::with_keys(
            Config {
                issuer: String::from("https://accounts.toddgriffin.me/oidc"),
                client_id: String::from("boggledygook"),
                client_secret: None,
                redirect_uri: String::new(),
            },
            signer().jwks,
        )
        .unwrap(),
    );
    assert!(matches!(
        accounts.start_revocation_poller().await,
        Err(Error::Config(_))
    ));
}
