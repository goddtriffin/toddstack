//! Verifying webhooks from the accounts service.
//!
//! Each delivery is a POST with JSON and `X-Accounts-Signature: t=<unix
//! seconds>,v1=<hex HMAC-SHA256 of "<t>.<raw body>">` under your app's
//! webhook secret. Verify against the raw body bytes, before parsing.
//! Deliveries can repeat (retries); use the event `id` to ignore duplicates.

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{Error, Result};

/// The header name.
pub const HEADER: &str = "X-Accounts-Signature";

/// How old a signature may be.
pub const TOLERANCE_SECONDS: i64 = 300;

/// What happened. Matches the wire `type`; [`EventKind::as_str`] gives it
/// back. Types this version doesn't know yet arrive as [`EventKind::Other`],
/// so a newer accounts service never breaks verification.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(from = "String")]
#[non_exhaustive]
pub enum EventKind {
    /// `user.updated`: `data` adds the fields that changed.
    UserUpdated,
    /// `user.deleted`: delete or anonymize the person's rows.
    UserDeleted,
    /// `user.suspended`: end the person's sessions.
    UserSuspended,
    /// `user.reactivated`.
    UserReactivated,
    /// `user.signed_out`: signed out everywhere; end the person's sessions.
    UserSignedOut,
    /// `session.signed_out`: the person signed out of one accounts browser
    /// session; `sub` and [`Event::sid`] say which. Drop every local session
    /// with that `sid`.
    SessionSignedOut,
    /// `grant.revoked`: the person removed your app; end their sessions.
    GrantRevoked,
    /// `friendship.created`: `sub` and `friend_sub` became friends. Sent once
    /// per side, so each person gets an event with themselves as `sub`.
    FriendshipCreated,
    /// `friendship.removed`: `sub` and `friend_sub` are no longer friends
    /// (unfriended or blocked). Sent once per side.
    FriendshipRemoved,
    /// `friend_request.created`: `from_sub` asked `to_sub`. Sent once per
    /// side. An accepted request sends only `friendship.created`.
    FriendRequestCreated,
    /// `friend_request.cancelled`: `from_sub` took back the request to
    /// `to_sub`. Sent once per side.
    FriendRequestCancelled,
    /// `friend_request.declined`: `to_sub` turned down `from_sub`'s request.
    /// Sent once per side.
    FriendRequestDeclined,
    /// `tokens.revoked`: these access-token families are dead (`data.families`,
    /// see [`Event::families`]). [`crate::revocation::Revocations::apply`]
    /// records them.
    TokensRevoked,
    /// A type this version doesn't know.
    Other(String),
}

impl EventKind {
    /// The wire name, such as `user.deleted`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::UserUpdated => "user.updated",
            Self::UserDeleted => "user.deleted",
            Self::UserSuspended => "user.suspended",
            Self::UserReactivated => "user.reactivated",
            Self::UserSignedOut => "user.signed_out",
            Self::SessionSignedOut => "session.signed_out",
            Self::GrantRevoked => "grant.revoked",
            Self::FriendshipCreated => "friendship.created",
            Self::FriendshipRemoved => "friendship.removed",
            Self::FriendRequestCreated => "friend_request.created",
            Self::FriendRequestCancelled => "friend_request.cancelled",
            Self::FriendRequestDeclined => "friend_request.declined",
            Self::TokensRevoked => "tokens.revoked",
            Self::Other(kind) => kind,
        }
    }
}

impl From<String> for EventKind {
    fn from(kind: String) -> Self {
        match kind.as_str() {
            "user.updated" => Self::UserUpdated,
            "user.deleted" => Self::UserDeleted,
            "user.suspended" => Self::UserSuspended,
            "user.reactivated" => Self::UserReactivated,
            "user.signed_out" => Self::UserSignedOut,
            "session.signed_out" => Self::SessionSignedOut,
            "grant.revoked" => Self::GrantRevoked,
            "friendship.created" => Self::FriendshipCreated,
            "friendship.removed" => Self::FriendshipRemoved,
            "friend_request.created" => Self::FriendRequestCreated,
            "friend_request.cancelled" => Self::FriendRequestCancelled,
            "friend_request.declined" => Self::FriendRequestDeclined,
            "tokens.revoked" => Self::TokensRevoked,
            _ => Self::Other(kind),
        }
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for EventKind {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for EventKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// A verified event.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Event {
    /// Unique per event; repeated on retries.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub created_at: i64,
    /// Always has `sub`; `session.signed_out` adds `sid`; `user.updated` adds the fields that changed (and `visibility`, `review` or `normal`), and the
    /// friendship events add `friend_sub`, and the friend request events add `from_sub` and `to_sub`.
    pub data: Value,
}

impl Event {
    /// The person the event is about.
    #[must_use]
    pub fn sub(&self) -> Option<&str> {
        self.data.get("sub").and_then(Value::as_str)
    }

    /// The accounts browser session that ended, for
    /// [`EventKind::SessionSignedOut`]; matches [`crate::IdClaims::sid`].
    #[must_use]
    pub fn sid(&self) -> Option<&str> {
        self.data.get("sid").and_then(Value::as_str)
    }

    /// The other person, for [`EventKind::FriendshipCreated`] and
    /// [`EventKind::FriendshipRemoved`].
    #[must_use]
    pub fn friend_sub(&self) -> Option<&str> {
        self.data.get("friend_sub").and_then(Value::as_str)
    }

    /// Who sent the request, for the `friend_request.*` events.
    #[must_use]
    pub fn from_sub(&self) -> Option<&str> {
        self.data.get("from_sub").and_then(Value::as_str)
    }

    /// Whether the person is a review account, for [`EventKind::UserUpdated`]
    /// (`data.visibility`). `None` when the event carries no such field.
    #[must_use]
    pub fn visibility(&self) -> Option<crate::Visibility> {
        self.data
            .get("visibility")
            .and_then(Value::as_str)
            .map(|v| crate::Visibility::from_wire(Some(v)))
    }

    /// The dead token families, for [`EventKind::TokensRevoked`]
    /// (`data.families`). Empty when the event carries none.
    #[must_use]
    pub fn families(&self) -> Vec<&str> {
        self.data
            .get("families")
            .and_then(Value::as_array)
            .map_or_default(|list| list.iter().filter_map(Value::as_str).collect())
    }

    /// Who the request was sent to, for the `friend_request.*` events.
    #[must_use]
    pub fn to_sub(&self) -> Option<&str> {
        self.data.get("to_sub").and_then(Value::as_str)
    }
}

/// Checks the signature and age, then parses the event. The header may carry
/// several `v1=` signatures (the service does while a webhook secret is being
/// rotated): the event is accepted when any one matches `secret`. `now` is the current
/// Unix time (a parameter so tests can fix it).
///
/// # Errors
/// [`Error::InvalidWebhook`] for an empty secret, a missing or malformed header, a wrong
/// signature, an old or future timestamp, or a body that isn't an event.
pub fn verify(secret: &[u8], signature_header: &str, body: &[u8], now: i64) -> Result<Event> {
    if secret.is_empty() {
        return Err(Error::InvalidWebhook("empty secret"));
    }
    let mut t: Option<i64> = None;
    // More than one `v1` while a secret is being rotated: the service signs
    // with both, and either may be the one you still hold.
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    for part in signature_header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => t = value.parse().ok(),
            Some(("v1", value)) => candidates.extend(hex::decode(value).ok()),
            _ => {}
        }
    }
    let t: i64 = match (t, candidates.is_empty()) {
        (Some(t), false) => t,
        _ => return Err(Error::InvalidWebhook("malformed signature header")),
    };
    if now.abs_diff(t) > TOLERANCE_SECONDS.unsigned_abs() {
        return Err(Error::InvalidWebhook(
            "signature too old (or from the future)",
        ));
    }
    let mut mac: Hmac<Sha256> = <Hmac<Sha256> as KeyInit>::new_from_slice(secret)
        .map_err(|_| Error::InvalidWebhook("bad secret"))?;
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    // Every candidate is compared, none skipped on a match, so timing doesn't
    // say which one matched.
    let mut matched: bool = false;
    for candidate in &candidates {
        matched |= bool::from(expected.as_slice().ct_eq(candidate));
    }
    if !matched {
        return Err(Error::InvalidWebhook("signature mismatch"));
    }
    serde_json::from_slice(body).map_err(|_| Error::InvalidWebhook("body is not an event"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], t: i64, body: &str) -> String {
        let mut mac: Hmac<Sha256> = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).unwrap();
        mac.update(format!("{t}.{body}").as_bytes());
        format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    const BODY: &str =
        r#"{"id":"e1","type":"user.deleted","created_at":1700000000,"data":{"sub":"u1"}}"#;

    #[test]
    fn valid_webhooks_verify() {
        let event: Event = verify(
            b"secret",
            &sign(b"secret", 1_700_000_000, BODY),
            BODY.as_bytes(),
            1_700_000_010,
        )
        .unwrap();
        let expected: Event = Event {
            id: String::from("e1"),
            kind: EventKind::UserDeleted,
            created_at: 1_700_000_000,
            data: serde_json::json!({ "sub": "u1" }),
        };
        assert_eq!(expected, event);
        assert_eq!(Some("u1"), event.sub());
        assert_eq!(None, event.friend_sub());
    }

    fn verified(body: &str) -> Event {
        verify(
            b"secret",
            &sign(b"secret", 1_700_000_000, body),
            body.as_bytes(),
            1_700_000_000,
        )
        .unwrap()
    }

    #[test]
    fn friendship_events_name_both_people() {
        for (wire, kind) in [
            ("friendship.created", EventKind::FriendshipCreated),
            ("friendship.removed", EventKind::FriendshipRemoved),
        ] {
            let body: String = format!(
                r#"{{"id":"e2","type":"{wire}","created_at":1700000000,"data":{{"sub":"u1","friend_sub":"u2"}}}}"#
            );
            let event: Event = verified(&body);
            assert_eq!(kind, event.kind);
            assert_eq!(wire, event.kind.as_str());
            assert_eq!(Some("u1"), event.sub());
            assert_eq!(Some("u2"), event.friend_sub());
        }
    }

    #[test]
    fn user_updated_carries_the_visibility() {
        use crate::Visibility;
        let review: Event = verified(
            r#"{"id":"e6","type":"user.updated","created_at":1700000000,"data":{"sub":"u1","visibility":"review"}}"#,
        );
        assert_eq!(Some(Visibility::Review), review.visibility());
        let normal: Event = verified(
            r#"{"id":"e7","type":"user.updated","created_at":1700000000,"data":{"sub":"u1","visibility":"normal"}}"#,
        );
        assert_eq!(Some(Visibility::Normal), normal.visibility());
        assert_eq!(None, verified(BODY).visibility());
    }

    #[test]
    fn session_signed_out_carries_the_person_and_the_browser_session() {
        let body: &str = r#"{"id":"e5","type":"session.signed_out","created_at":1700000000,"data":{"sub":"u1","sid":"s-1"}}"#;
        let event: Event = verified(body);
        assert_eq!(EventKind::SessionSignedOut, event.kind);
        assert_eq!("session.signed_out", event.kind.as_str());
        assert_eq!(Some("u1"), event.sub());
        assert_eq!(Some("s-1"), event.sid());
        assert_eq!(None, verified(BODY).sid());
        let header: String = sign(b"other", 1_700_000_000, body);
        assert_eq!(
            "signature mismatch",
            webhook_error(verify(b"secret", &header, body.as_bytes(), 1_700_000_000))
        );
    }

    #[test]
    fn friend_request_events_name_sender_and_receiver() {
        for (wire, kind) in [
            ("friend_request.created", EventKind::FriendRequestCreated),
            (
                "friend_request.cancelled",
                EventKind::FriendRequestCancelled,
            ),
            ("friend_request.declined", EventKind::FriendRequestDeclined),
        ] {
            let body: String = format!(
                r#"{{"id":"e4","type":"{wire}","created_at":1700000000,"data":{{"sub":"u2","from_sub":"u1","to_sub":"u2"}}}}"#
            );
            let event: Event = verified(&body);
            assert_eq!(kind, event.kind);
            assert_eq!(Some("u2"), event.sub());
            assert_eq!(Some("u1"), event.from_sub());
            assert_eq!(Some("u2"), event.to_sub());
            assert_eq!(None, event.friend_sub());
        }
    }

    #[test]
    fn every_known_type_round_trips_through_its_wire_name() {
        for kind in [
            EventKind::UserUpdated,
            EventKind::UserDeleted,
            EventKind::UserSuspended,
            EventKind::UserReactivated,
            EventKind::UserSignedOut,
            EventKind::SessionSignedOut,
            EventKind::GrantRevoked,
            EventKind::FriendshipCreated,
            EventKind::FriendshipRemoved,
            EventKind::FriendRequestCreated,
            EventKind::FriendRequestCancelled,
            EventKind::FriendRequestDeclined,
        ] {
            assert_eq!(kind, EventKind::from(kind.as_str().to_owned()));
            assert_ne!(EventKind::Other(kind.as_str().to_owned()), kind);
        }
    }

    #[test]
    fn a_type_this_version_does_not_know_still_verifies() {
        let event: Event = verified(
            r#"{"id":"e3","type":"something.new","created_at":1700000000,"data":{"sub":"u1"}}"#,
        );
        assert_eq!(EventKind::Other(String::from("something.new")), event.kind);
        assert_eq!("something.new", event.kind.as_str());
    }

    #[test]
    fn tampering_wrong_secret_and_age_are_refused() {
        let header: String = sign(b"secret", 1_700_000_000, BODY);
        let tampered: String = BODY.replace("u1", "u2");
        assert!(verify(b"secret", &header, tampered.as_bytes(), 1_700_000_000).is_err());
        assert!(verify(b"other", &header, BODY.as_bytes(), 1_700_000_000).is_err());
        assert!(verify(b"secret", &header, BODY.as_bytes(), 1_700_000_301).is_err());
        assert!(verify(b"secret", "nonsense", BODY.as_bytes(), 1_700_000_000).is_err());
    }

    #[test]
    fn an_extreme_timestamp_is_refused_instead_of_overflowing() {
        let header: String = sign(b"secret", i64::MIN, BODY);
        let actual: &str =
            webhook_error(verify(b"secret", &header, BODY.as_bytes(), 1_700_000_000));
        assert_eq!("signature too old (or from the future)", actual);
    }

    fn webhook_error(result: Result<Event>) -> &'static str {
        match result {
            Err(Error::InvalidWebhook(why)) => why,
            other => panic!("expected a rejected webhook, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_secret_is_refused_even_with_a_signature_made_under_it() {
        let header: String = sign(b"", 1_700_000_000, BODY);
        let actual: &str = webhook_error(verify(b"", &header, BODY.as_bytes(), 1_700_000_000));
        assert_eq!("empty secret", actual);
    }

    #[test]
    fn any_one_of_several_signatures_may_match_during_a_secret_rotation() {
        let old: String = sign(b"old-secret", 1_700_000_000, BODY);
        let new: String = sign(b"new-secret", 1_700_000_000, BODY);
        let old_v1: &str = old.split_once("v1=").unwrap().1;
        let new_v1: &str = new.split_once("v1=").unwrap().1;
        // Either secret verifies, whichever position its signature is in.
        for header in [
            format!("t=1700000000,v1={old_v1},v1={new_v1}"),
            format!("t=1700000000,v1={new_v1},v1={old_v1}"),
        ] {
            for secret in [&b"old-secret"[..], &b"new-secret"[..]] {
                let event: Event = verify(secret, &header, BODY.as_bytes(), 1_700_000_010).unwrap();
                assert_eq!("e1", event.id);
            }
        }
        // A malformed signature beside the right one is skipped, not fatal.
        let junk: String = format!("v1=zz,t=1700000000,v1={new_v1},v1=00");
        assert!(verify(b"new-secret", &junk, BODY.as_bytes(), 1_700_000_010).is_ok());
    }

    #[test]
    fn several_signatures_none_of_which_match_are_refused() {
        let a: String = sign(b"a", 1_700_000_000, BODY);
        let b: String = sign(b"b", 1_700_000_000, BODY);
        let header: String = format!(
            "t=1700000000,v1={},v1={}",
            a.split_once("v1=").unwrap().1,
            b.split_once("v1=").unwrap().1
        );
        let actual: &str = webhook_error(verify(b"c", &header, BODY.as_bytes(), 1_700_000_000));
        assert_eq!("signature mismatch", actual);
    }

    #[test]
    fn a_header_whose_signatures_are_all_malformed_hex_is_malformed() {
        let actual: &str = webhook_error(verify(
            b"secret",
            "t=1700000000,v1=zz,v1=abc",
            BODY.as_bytes(),
            1_700_000_000,
        ));
        assert_eq!("malformed signature header", actual);
    }
}
