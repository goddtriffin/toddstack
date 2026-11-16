//! Notifications: what your server sends to players, and the time zone hint
//! that makes `send_at_local` land on their own clock.
//!
//! [`Accounts::send_notification`], [`Accounts::cancel_notifications`] and
//! [`Accounts::notification_status`] authenticate as your project (HTTP Basic,
//! a confidential client), with no person signed in. [`crate::friends::Friends::set_time_zone`]
//! uses a person's access token instead.
//!
//! The rules, so your code can lean on them:
//!
//! - **Always pass an [`Notification::idempotency_key`].** A retry after a
//!   timeout then gets the recorded send back (`replayed`) instead of sending
//!   twice. This crate retries a send itself only when a key is set.
//! - There are no caps and no quiet hours: every notification is delivered,
//!   one per friend who finishes a daily, say. The phone's own Focus handles
//!   quiet time, so a streak warning is never delayed. Only a runaway guard
//!   (over 100 to one player in an hour) drops the rest, counted as
//!   `dropped_runaway`.
//! - [`Notification::collapse_key`] replaces that player's unread item with
//!   the same key from your project, and names a scheduled notification so
//!   [`Accounts::cancel_notifications`] can remove it. Nothing collapses
//!   without a key.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};

use crate::retry::{self, RetryPolicy};
use crate::tokens::Accounts;
use crate::{Error, Result};

/// Who a notification is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recipient {
    /// One player (by `sub`) who has signed in to your project: "streak at
    /// risk". Delivered right away unless `send_at_local` is set.
    User(String),
    /// Each friend of this player (by `sub`) who uses your project and isn't
    /// blocked either way, shown as from that player: "beat your score".
    FriendsOf(String),
    /// Everyone who has signed in to your project: "new dailies are out".
    AllPlayers,
}

impl Serialize for Recipient {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::User(sub) => map.serialize_entry("user", sub)?,
            Self::FriendsOf(sub) => map.serialize_entry("friends_of", sub)?,
            Self::AllPlayers => map.serialize_entry("all_players", &true)?,
        }
        map.end()
    }
}

/// A notification to send with [`Accounts::send_notification`].
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Notification {
    pub recipient: Recipient,
    /// 1-40 of `a-z`, `0-9`, `_`; free-form, the first use creates it (at
    /// most 50 per project). Players can mute a type.
    #[serde(rename = "type")]
    pub kind: String,
    /// 1-80 characters.
    pub title: String,
    /// At most 280 characters.
    pub body: String,
    /// An `https` link on an origin of your registered redirect or logout URIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `HH:MM` on the player's own clock: the next time it reads that. Needs
    /// their time zone ([`crate::friends::Friends::set_time_zone`]); UTC if unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_at_local: Option<String>,
    /// 1-64 characters. Replaces the player's unread item with this key, and
    /// replaces or lets you cancel a scheduled one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse_key: Option<String>,
    /// 1-100 characters, kept 7 days. Always set one (a date or a game id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl Notification {
    /// A notification with no body, link, schedule or keys.
    #[must_use]
    pub fn new(recipient: Recipient, kind: &str, title: &str) -> Self {
        Self {
            recipient,
            kind: kind.to_owned(),
            title: title.to_owned(),
            body: String::new(),
            url: None,
            send_at_local: None,
            collapse_key: None,
            idempotency_key: None,
        }
    }
}

/// What happened to one player.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Outcome {
    /// In their inbox now.
    Delivered,
    /// Waiting for their `send_at_local`.
    Scheduled,
    /// Muted, left your project, or suspended: deliberately not told apart,
    /// so a send can't be used to probe players.
    NotDelivered,
    /// Dropped by the runaway guard.
    DroppedRunaway,
    /// An outcome newer than this crate.
    #[serde(other)]
    Unknown,
}

/// Outcomes, counted.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Counts {
    #[serde(default)]
    pub delivered: u64,
    #[serde(default)]
    pub scheduled: u64,
    #[serde(default)]
    pub not_delivered: u64,
    #[serde(default)]
    pub dropped_runaway: u64,
}

/// Where a send stands.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Status {
    /// A fan-out still being delivered: poll [`Accounts::notification_status`].
    Queued,
    /// Every player was handled.
    Done,
    /// A fan-out whose runs failed repeatedly; it is not retried.
    Failed,
    /// A status newer than this crate.
    #[serde(other)]
    Unknown,
}

/// The service's answer to a send (202), and to [`Accounts::notification_status`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub id: String,
    pub status: Status,
    /// How many players it was addressed to.
    pub recipients: u64,
    pub counts: Counts,
    /// For a send to one `user`: what happened to them.
    #[serde(default)]
    pub result: Option<Outcome>,
    /// True when your idempotency key matched a recorded send: this is its
    /// answer, and nothing was sent again.
    #[serde(default)]
    pub replayed: bool,
}

/// The accounts API root, from the issuer.
fn api_base(accounts: &Accounts) -> String {
    format!(
        "{}/api/v1",
        accounts.config().issuer.trim_end_matches("/oidc")
    )
}

/// Maps a refused call to the typed errors the notification calls document.
fn refused(status: u16, text: &str) -> Error {
    let message: String = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| text.to_owned());
    match status {
        404 => Error::NotFound(message),
        422 => Error::Unprocessable(message),
        429 => Error::RateLimited(message),
        _ => Error::Api { status, message },
    }
}

impl Accounts {
    /// One call as your project, with your client credentials. `keyed` lets a
    /// POST be retried (see [`retry::execute_keyed`]); a GET is always retried.
    async fn project_call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        keyed: bool,
    ) -> Result<Value> {
        let secret: &str = self.config().client_secret.as_deref().ok_or_else(|| {
            Error::Config(String::from(
                "notifications need the client secret: set Config::client_secret",
            ))
        })?;
        // RFC 6749 2.3.1, as at the token endpoint: form-encode both parts.
        let encode =
            |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let mut request = self
            .http()
            .request(method, format!("{}{path}", api_base(self)))
            .basic_auth(encode(&self.config().client_id), Some(encode(secret)));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let request: reqwest::Request = request.build()?;
        let response: reqwest::Response = if keyed {
            retry::execute_keyed(self.http(), request, &RetryPolicy::KEYED_SEND).await?
        } else {
            retry::execute(self.http(), request, &RetryPolicy::NOTIFICATION_STATUS).await?
        };
        let status: reqwest::StatusCode = response.status();
        let text: String = response.text().await?;
        if !status.is_success() {
            return Err(refused(status.as_u16(), &text));
        }
        serde_json::from_str(&text).map_err(|e| Error::Api {
            status: status.as_u16(),
            message: e.to_string(),
        })
    }

    /// Sends a notification as your project. The service records it and
    /// answers at once: a send to one player is delivered inline
    /// ([`Receipt::result`]); a fan-out ([`Recipient::FriendsOf`],
    /// [`Recipient::AllPlayers`]) is [`Status::Queued`], so poll
    /// [`Accounts::notification_status`] for its counts.
    ///
    /// Always set [`Notification::idempotency_key`]: it is what makes a retry
    /// safe. With a key this call retries an intermittent failure itself;
    /// without one it sends exactly once, since a repeat could notify twice.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret; [`Error::NotFound`] when
    /// the named player never signed in to your project;
    /// [`Error::Unprocessable`] for the same idempotency key with a different
    /// request, or a field the service can't parse; [`Error::RateLimited`] (6000 requests an hour, 10
    /// fan-outs an hour); [`Error::Api`] for a bad request (400) or anything
    /// else; or the request failed.
    pub async fn send_notification(&self, notification: &Notification) -> Result<Receipt> {
        let body: Value = serde_json::to_value(notification).map_err(|e| Error::Api {
            status: 0,
            message: e.to_string(),
        })?;
        let keyed: bool = notification.idempotency_key.is_some();
        receipt(
            self.project_call(reqwest::Method::POST, "/notifications", Some(body), keyed)
                .await?,
        )
    }

    /// Removes scheduled notifications with `collapse_key` that haven't gone
    /// out ("they played, so no 8 PM reminder"): one player's (`user`, a
    /// `sub`), or all of your project's. Returns how many were removed.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret; [`Error::RateLimited`]; a
    /// bad `collapse_key` (400, [`Error::Api`]); or the request failed.
    pub async fn cancel_notifications(
        &self,
        collapse_key: &str,
        user: Option<&str>,
    ) -> Result<u64> {
        let mut body: Value = json!({ "collapse_key": collapse_key });
        if let Some(user) = user {
            body["user"] = Value::from(user);
        }
        let answer: Value = self
            .project_call(
                reqwest::Method::POST,
                "/notifications/cancel",
                Some(body),
                false,
            )
            .await?;
        answer
            .get("cancelled")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Api {
                status: 200,
                message: String::from("missing field `cancelled`"),
            })
    }

    /// Your project's own send, by [`Receipt::id`], with its counts so far.
    /// Finished sends are kept 7 days.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret; [`Error::NotFound`] for an
    /// unknown id or another project's; [`Error::Api`] (400) for a malformed
    /// id; [`Error::RateLimited`]; or the request failed.
    pub async fn notification_status(&self, id: &str) -> Result<Receipt> {
        let id: String = encode_path_segment(id);
        receipt(
            self.project_call(
                reqwest::Method::GET,
                &format!("/notifications/{id}"),
                None,
                false,
            )
            .await?,
        )
    }
}

/// Percent-encodes everything but what `encodeURIComponent` leaves alone, so
/// an id can never add path segments or a query.
fn encode_path_segment(segment: &str) -> String {
    let mut encoded: String = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&hex::encode_upper([byte]));
        }
    }
    encoded
}

fn receipt(value: Value) -> Result<Receipt> {
    serde_json::from_value(value).map_err(|e| Error::Api {
        status: 200,
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_percent_encoded_so_it_cannot_change_the_path() {
        let expected: &str = "a%2Fb%3Fc%20d%C3%A9-_.~";
        let actual: String = encode_path_segment("a/b?c dé-_.~");
        assert_eq!(expected, actual);
    }

    #[test]
    fn a_notification_serializes_to_the_wire_shape_without_unset_fields() {
        let mut notification: Notification = Notification::new(
            Recipient::FriendsOf(String::from("u1")),
            "beat_your_score",
            "Alex beat your score",
        );
        notification.idempotency_key = Some(String::from("daily-2026-11-15"));
        let expected: Value = json!({
            "recipient": { "friends_of": "u1" },
            "type": "beat_your_score",
            "title": "Alex beat your score",
            "body": "",
            "idempotency_key": "daily-2026-11-15",
        });
        let actual: Value = serde_json::to_value(&notification).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn every_recipient_uses_the_service_shape() {
        let expected: Value = json!([
            { "user": "u1" },
            { "friends_of": "u2" },
            { "all_players": true },
        ]);
        let actual: Value = serde_json::to_value([
            Recipient::User(String::from("u1")),
            Recipient::FriendsOf(String::from("u2")),
            Recipient::AllPlayers,
        ])
        .unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn a_receipt_reads_every_field_and_tolerates_newer_values() {
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
            replayed: true,
        };
        let actual: Receipt = receipt(json!({
            "id": "b1", "status": "done", "recipients": 1, "result": "delivered", "replayed": true,
            "counts": { "delivered": 1, "scheduled": 0, "not_delivered": 0, "dropped_runaway": 0 },
        }))
        .unwrap();
        assert_eq!(expected, actual);

        let newer: Receipt = receipt(json!({
            "id": "b2", "status": "paused", "recipients": 5, "result": "something_new",
            "counts": { "delivered": 5 },
        }))
        .unwrap();
        assert_eq!(Status::Unknown, newer.status);
        assert_eq!(Some(Outcome::Unknown), newer.result);
        assert!(!newer.replayed);
    }

    #[test]
    fn refusals_become_the_typed_errors() {
        assert!(matches!(
            refused(404, r#"{"error":"Not found."}"#),
            Error::NotFound(m) if m == "Not found."
        ));
        assert!(matches!(refused(422, "no"), Error::Unprocessable(m) if m == "no"));
        assert!(matches!(refused(429, "slow"), Error::RateLimited(m) if m == "slow"));
        assert!(matches!(
            refused(400, "bad"),
            Error::Api { status: 400, .. }
        ));
    }
}
