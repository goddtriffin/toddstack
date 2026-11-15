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
    /// `grant.revoked`: the person removed your app; end their sessions.
    GrantRevoked,
    /// `friendship.created`: `sub` and `friend_sub` became friends. Sent once
    /// per side, so each person gets an event with themselves as `sub`.
    FriendshipCreated,
    /// `friendship.removed`: `sub` and `friend_sub` are no longer friends
    /// (unfriended or blocked). Sent once per side.
    FriendshipRemoved,
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
            Self::GrantRevoked => "grant.revoked",
            Self::FriendshipCreated => "friendship.created",
            Self::FriendshipRemoved => "friendship.removed",
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
            "grant.revoked" => Self::GrantRevoked,
            "friendship.created" => Self::FriendshipCreated,
            "friendship.removed" => Self::FriendshipRemoved,
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
    /// Always has `sub`; `user.updated` adds the fields that changed, and the
    /// friendship events add `friend_sub`.
    pub data: Value,
}

impl Event {
    /// The person the event is about.
    #[must_use]
    pub fn sub(&self) -> Option<&str> {
        self.data.get("sub").and_then(Value::as_str)
    }

    /// The other person, for [`EventKind::FriendshipCreated`] and
    /// [`EventKind::FriendshipRemoved`].
    #[must_use]
    pub fn friend_sub(&self) -> Option<&str> {
        self.data.get("friend_sub").and_then(Value::as_str)
    }
}

/// Checks the signature and age, then parses the event. `now` is the current
/// Unix time (a parameter so tests can fix it).
///
/// # Errors
/// [`Error::InvalidWebhook`] for a missing or malformed header, a wrong
/// signature, an old or future timestamp, or a body that isn't an event.
pub fn verify(secret: &[u8], signature_header: &str, body: &[u8], now: i64) -> Result<Event> {
    let mut t: Option<i64> = None;
    let mut v1: Option<Vec<u8>> = None;
    for part in signature_header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => t = value.parse().ok(),
            Some(("v1", value)) => v1 = hex::decode(value).ok(),
            _ => {}
        }
    }
    let (Some(t), Some(v1)) = (t, v1) else {
        return Err(Error::InvalidWebhook("malformed signature header"));
    };
    if (now - t).abs() > TOLERANCE_SECONDS {
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
    if !bool::from(expected.as_slice().ct_eq(&v1)) {
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
    fn every_known_type_round_trips_through_its_wire_name() {
        for kind in [
            EventKind::UserUpdated,
            EventKind::UserDeleted,
            EventKind::UserSuspended,
            EventKind::UserReactivated,
            EventKind::UserSignedOut,
            EventKind::GrantRevoked,
            EventKind::FriendshipCreated,
            EventKind::FriendshipRemoved,
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
}
