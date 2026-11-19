//! Everything your server does for a signed-in player, as your project.
//!
//! [`Accounts::player`] names a player by `sub` (one you authenticated with
//! your own session) and returns a [`Player`] with a method for each route
//! under `/api/v1/players/{sub}`. Calls authenticate with your client
//! credentials (HTTP Basic, so the config needs its client secret); a
//! player's access or refresh token is never involved, so your server holds
//! none past the sign-in code exchange.
//!
//! ```no_run
//! # async fn example(accounts: toddstack_accounts_server_sdk::Accounts) -> Result<(), toddstack_accounts_server_sdk::Error> {
//! let player = accounts.player("4b0b2c6e-0000-4000-8000-000000000001")?;
//! for friend in player.friends().await? {
//!     println!("{}", friend.username);
//! }
//! # Ok(()) }
//! ```
//!
//! Rules your code can lean on:
//!
//! - **Errors carry the code.** A refusal is [`Error::Server`] with the API's
//!   `error` code (`not_found`, `suspended`, `scope`, `rate_limited`, ...);
//!   read it with [`Error::code`]. Everything but a 429 and a 5xx is final.
//! - **Writes are idempotent, so this crate retries them** (3 attempts, on a
//!   429, a 5xx or a connection failure only): friend requests, accepts,
//!   blocks, mutes and the rest are no-op successes when the state already
//!   holds, and an event is deduplicated by its required idempotency key.
//! - A player who never signed in to your project, a deleted one and one who
//!   removed your app are the same `not_found`. A suspended player can be
//!   read, not written for (`suspended`).
//! - You only ever see people who also use your project, in every list.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::friends::{
    ALL_KINDS, Blocks, Event, EventResult, Inbox, MAX_RELATIONSHIPS_PER_CALL, Person, Relationship,
    Requests, Sent, is_valid_id,
};
use crate::report::{PlayerReport, Reason};
use crate::retry::{self, RetryPolicy};
use crate::tokens::{Accounts, Visibility};
use crate::{Error, Result};

/// The most characters of an idempotency key the service accepts.
pub const MAX_IDEMPOTENCY_KEY_CHARS: usize = 100;

/// What [`Player::profile`] answers: the claims for the scopes in both the
/// player's grant and your project's allowed scopes, plus `visibility` and
/// `suspended`. Fields behind a scope are `None` without it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Profile {
    pub sub: String,
    /// With the `profile` scope.
    pub username: Option<String>,
    pub preferred_username: Option<String>,
    pub name: Option<String>,
    pub picture: Option<String>,
    /// Unix seconds.
    pub updated_at: Option<i64>,
    /// With the `email` scope.
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    /// [`Visibility::Review`] for a review account: never show it to others.
    #[serde(default)]
    pub visibility: Visibility,
    /// Suspended players can be read but not written for: end their session.
    #[serde(default)]
    pub suspended: bool,
}

/// What [`Accounts::profile_by_sub`] and [`Accounts::profile_by_username`]
/// answer: the public profile of any account holder, and nothing more.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicProfile {
    pub sub: String,
    pub username: String,
    /// The stable public avatar URL.
    pub picture: String,
}

/// An event with the idempotency key the Server API requires, as sent.
#[derive(Serialize)]
struct KeyedEvent<'a> {
    #[serde(flatten)]
    event: &'a Event,
    idempotency_key: &'a str,
}

fn bad_request(message: &str) -> Error {
    Error::Server {
        status: 400,
        code: String::from("bad_request"),
        message: message.to_owned(),
    }
}

fn not_found() -> Error {
    Error::Server {
        status: 404,
        code: String::from("not_found"),
        message: String::from("Not found."),
    }
}

fn checked(value: &str) -> Result<&str> {
    if is_valid_id(value) {
        Ok(value)
    } else {
        Err(bad_request("not a valid id"))
    }
}

fn read<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Error::Api {
        status: 200,
        message: e.to_string(),
    })
}

fn field<T: for<'de> Deserialize<'de>>(mut value: Value, name: &str) -> Result<T> {
    read(value.get_mut(name).map_or(Value::Null, Value::take))
}

impl Accounts {
    /// The Server API handle for the player `sub`.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret (only a confidential server
    /// client may call these routes), or [`Error::Server`] (`bad_request`)
    /// for a `sub` that isn't a plain id.
    pub fn player(&self, sub: &str) -> Result<Player<'_>> {
        if self.config().client_secret.is_none() {
            return Err(Error::Config(String::from(
                "the Server API needs the client secret: set Config::client_secret",
            )));
        }
        Ok(Player {
            accounts: self,
            sub: checked(sub)?.to_owned(),
        })
    }

    /// One call to the Server API as your project. Reads and writes are both
    /// retried: every write is idempotent.
    pub(crate) async fn server_call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let secret: &str = self.config().client_secret.as_deref().ok_or_else(|| {
            Error::Config(String::from(
                "the Server API needs the client secret: set Config::client_secret",
            ))
        })?;
        let policy: RetryPolicy = if method == reqwest::Method::GET {
            RetryPolicy::READ
        } else {
            RetryPolicy::KEYED_SEND
        };
        // RFC 6749 2.3.1, as at the token endpoint: form-encode both parts.
        let encode =
            |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let base: &str = self.config().issuer.trim_end_matches("/oidc");
        let mut request = self
            .http()
            .request(method, format!("{base}/api/v1{path}"))
            .basic_auth(encode(&self.config().client_id), Some(encode(secret)));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response: reqwest::Response =
            retry::execute_keyed(self.http(), request.build()?, &policy).await?;
        let status: reqwest::StatusCode = response.status();
        let text: String = response.text().await?;
        if !status.is_success() {
            let parsed: Option<Value> = serde_json::from_str(&text).ok();
            let text_of = |key: &str| {
                parsed
                    .as_ref()
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            return Err(Error::Server {
                status: status.as_u16(),
                code: text_of("error").unwrap_or_else(|| format!("http_{}", status.as_u16())),
                message: text_of("message").unwrap_or(text),
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| Error::Api {
            status: status.as_u16(),
            message: e.to_string(),
        })
    }

    /// The public profile (`sub`, `username`, `picture`) of any account
    /// holder by `sub`, including people who never used your project, so you
    /// can show, block and report them. Pass the `viewer` (the player asking)
    /// to get `not_found` when either has blocked the other.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret, [`Error::Server`]
    /// (`bad_request` for a malformed sub or viewer; a blank sub or username is `not_found` locally, with no request; `not_found`, the same for
    /// an unknown, suspended, review or deleted account and for a block), or
    /// the request failed.
    pub async fn profile_by_sub(&self, sub: &str, viewer: Option<&str>) -> Result<PublicProfile> {
        if sub.trim().is_empty() {
            return Err(not_found());
        }
        self.lookup(&format!("/profiles/{}", checked(sub)?), viewer)
            .await
    }

    /// As [`Accounts::profile_by_sub`], by exact username (case-insensitive).
    ///
    /// # Errors
    /// As for [`Accounts::profile_by_sub`].
    pub async fn profile_by_username(
        &self,
        username: &str,
        viewer: Option<&str>,
    ) -> Result<PublicProfile> {
        if username.trim().is_empty() {
            return Err(not_found());
        }
        let name: String = url::form_urlencoded::byte_serialize(username.trim().as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        self.lookup(&format!("/profiles/by-username/{name}"), viewer)
            .await
    }

    async fn lookup(&self, path: &str, viewer: Option<&str>) -> Result<PublicProfile> {
        let path: String = match viewer {
            Some(viewer) => format!("{path}?viewer={}", checked(viewer)?),
            None => path.to_owned(),
        };
        read(self.server_call(reqwest::Method::GET, &path, None).await?)
    }

    /// Reports `sub` to Todd's review queue on behalf of the signed-in player
    /// `reporter_sub` (the same as [`Player::report`]). Idempotent: an open
    /// report with the same reporter, person and reason is stored once.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret, [`Error::Server`]
    /// (`bad_request` reporting themselves or a bad note, `not_found` an
    /// unknown account or a guest, `suspended`, `rate_limited` over 10 an
    /// hour), or the request failed.
    pub async fn report_as_player(&self, report: &PlayerReport) -> Result<()> {
        checked(&report.reporter_sub)?;
        let body: Value = serde_json::to_value(report).map_err(|e| bad_request(&e.to_string()))?;
        self.server_call(reqwest::Method::POST, "/reports", Some(body))
            .await
            .map(|_| ())
    }
}

/// The Server API for one player. Make it with [`Accounts::player`].
#[derive(Clone)]
pub struct Player<'a> {
    accounts: &'a Accounts,
    sub: String,
}

impl Player<'_> {
    /// The player's `sub`.
    #[must_use]
    pub fn sub(&self) -> &str {
        &self.sub
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        self.accounts
            .server_call(method, &format!("/players/{}{path}", self.sub), body)
            .await
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<()> {
        self.call(method, path, body).await.map(|_| ())
    }

    /// The player's claims for the scopes they granted and you may use, plus
    /// `visibility` and `suspended`. Replaces `/oidc/me` after sign-in.
    ///
    /// # Errors
    /// [`Error::Server`] (`not_found` for a player who never signed in to your
    /// project) or the request failed.
    pub async fn profile(&self) -> Result<Profile> {
        read(self.call(reqwest::Method::GET, "", None).await?)
    }

    /// Friends who also use your project. Needs the `friends` scope.
    ///
    /// # Errors
    /// As for [`Player::profile`], and `scope` without the `friends` scope.
    pub async fn friends(&self) -> Result<Vec<Person>> {
        field(
            self.call(reqwest::Method::GET, "/friends", None).await?,
            "friends",
        )
    }

    /// Where the player stands with each of `subs`, keyed by sub. Any number:
    /// longer lists go out as several calls.
    ///
    /// # Errors
    /// A malformed sub, or as for [`Player::friends`].
    pub async fn relationships(&self, subs: &[&str]) -> Result<HashMap<String, Relationship>> {
        for sub in subs {
            checked(sub)?;
        }
        let mut found: HashMap<String, Relationship> = HashMap::new();
        for chunk in subs.chunks(MAX_RELATIONSHIPS_PER_CALL) {
            let query: String =
                url::form_urlencoded::byte_serialize(chunk.join(",").as_bytes()).collect();
            let value: Value = self
                .call(
                    reqwest::Method::GET,
                    &format!("/friends/relationships?subs={query}"),
                    None,
                )
                .await?;
            found.extend(field::<HashMap<String, Relationship>>(
                value,
                "relationships",
            )?);
        }
        Ok(found)
    }

    /// Where the player stands with `sub`.
    ///
    /// # Errors
    /// As for [`Player::relationships`].
    pub async fn relationship(&self, sub: &str) -> Result<Relationship> {
        Ok(self
            .relationships(&[sub])
            .await?
            .remove(sub)
            .unwrap_or(Relationship::None))
    }

    /// Requests with people who use your project.
    ///
    /// # Errors
    /// As for [`Player::friends`].
    pub async fn requests(&self) -> Result<Requests> {
        read(
            self.call(reqwest::Method::GET, "/friends/requests", None)
                .await?,
        )
    }

    async fn request_with(&self, body: Value) -> Result<Sent> {
        field(
            self.call(reqwest::Method::POST, "/friends/requests", Some(body))
                .await?,
            "result",
        )
    }

    /// Asks `sub` (any account holder, not only your players) to be friends.
    /// [`Sent::Accepted`] means they had already asked.
    ///
    /// # Errors
    /// `not_found` for anyone who can't be asked (unknown, blocked either
    /// way, private): the same answer for every cause. `rate_limited`.
    pub async fn request(&self, sub: &str) -> Result<Sent> {
        self.request_with(json!({ "sub": checked(sub)? })).await
    }

    /// Asks by exact username.
    ///
    /// # Errors
    /// As for [`Player::request`].
    pub async fn request_by_username(&self, username: &str) -> Result<Sent> {
        self.request_with(json!({ "username": username })).await
    }

    /// Asks by friend code (works even when the person only takes requests
    /// from people with their code).
    ///
    /// # Errors
    /// As for [`Player::request`].
    pub async fn request_by_code(&self, friend_code: &str) -> Result<Sent> {
        self.request_with(json!({ "friend_code": friend_code }))
            .await
    }

    /// Accepts a request from `sub`, a player of your project. A no-op when
    /// already friends.
    ///
    /// # Errors
    /// `not_found` when there is no request and no friendship.
    pub async fn accept(&self, sub: &str) -> Result<()> {
        self.send(
            reqwest::Method::POST,
            &format!("/friends/requests/{}/accept", checked(sub)?),
            None,
        )
        .await
    }

    /// Declines a request from `sub`. A no-op when there is none.
    ///
    /// # Errors
    /// The request failed.
    pub async fn decline(&self, sub: &str) -> Result<()> {
        self.send(
            reqwest::Method::POST,
            &format!("/friends/requests/{}/decline", checked(sub)?),
            None,
        )
        .await
    }

    /// Takes back a request the player sent. A no-op when there is none.
    ///
    /// # Errors
    /// The request failed.
    pub async fn cancel(&self, sub: &str) -> Result<()> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/friends/requests/{}", checked(sub)?),
            None,
        )
        .await
    }

    /// Ends a friendship. A no-op when not friends.
    ///
    /// # Errors
    /// The request failed.
    pub async fn remove(&self, sub: &str) -> Result<()> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/friends/{}", checked(sub)?),
            None,
        )
        .await
    }

    /// Blocks `sub`, a player of your project: ends the friendship and
    /// requests, and stops events both ways.
    ///
    /// # Errors
    /// `not_found` for someone who doesn't use your project.
    pub async fn block(&self, sub: &str) -> Result<()> {
        self.send(
            reqwest::Method::PUT,
            &format!("/blocks/{}", checked(sub)?),
            None,
        )
        .await
    }

    /// Lifts a block, even on someone who doesn't use your project. A no-op
    /// when not blocked.
    ///
    /// # Errors
    /// The request failed.
    pub async fn unblock(&self, sub: &str) -> Result<()> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/blocks/{}", checked(sub)?),
            None,
        )
        .await
    }

    /// Who the player blocked, and everyone to keep apart from them (people
    /// of your project only).
    ///
    /// # Errors
    /// As for [`Player::friends`].
    pub async fn blocks(&self) -> Result<Blocks> {
        read(self.call(reqwest::Method::GET, "/blocks", None).await?)
    }

    /// Posts an event to the player's friends who use your project. The
    /// `idempotency_key` (1 to 100 characters, kept 7 days) is required: a
    /// retry with it answers the same ([`EventResult::replayed`]) and sends
    /// nothing; the same key with a different event is `unprocessable`. Use
    /// something stable for the occasion, like a game id. Needs
    /// `friends:events`.
    ///
    /// # Errors
    /// A key that is empty or too long, an invalid event, `scope`,
    /// `unprocessable`, `rate_limited` (60 an hour per player).
    pub async fn post_event(&self, event: &Event, idempotency_key: &str) -> Result<EventResult> {
        if idempotency_key.is_empty() || idempotency_key.chars().count() > MAX_IDEMPOTENCY_KEY_CHARS
        {
            return Err(bad_request(
                "idempotency_key is required: 1 to 100 characters.",
            ));
        }
        let body: Value = serde_json::to_value(KeyedEvent {
            event,
            idempotency_key,
        })
        .map_err(|e| bad_request(&e.to_string()))?;
        read(
            self.call(reqwest::Method::POST, "/events", Some(body))
                .await?,
        )
    }

    /// Reports `sub` to Todd's review queue, from this player, at your
    /// project. `note` is at most [`crate::report::MAX_NOTE_CHARS`].
    ///
    /// # Errors
    /// As for [`Accounts::report_as_player`].
    pub async fn report(&self, sub: &str, reason: Reason, note: Option<&str>) -> Result<()> {
        self.accounts
            .report_as_player(&PlayerReport {
                reporter_sub: self.sub.clone(),
                sub: sub.to_owned(),
                reason,
                note: note.map(str::to_owned),
            })
            .await
    }

    /// The player's inbox: your project's items plus account-level ones,
    /// newest first, with the unread counts for your badge.
    ///
    /// # Errors
    /// As for [`Player::friends`].
    pub async fn inbox(&self, limit: u32) -> Result<Inbox> {
        read(
            self.call(reqwest::Method::GET, &format!("/inbox?limit={limit}"), None)
                .await?,
        )
    }

    /// Marks an inbox item read. Another project's item is a no-op.
    ///
    /// # Errors
    /// A malformed id, or the request failed.
    pub async fn mark_read(&self, id: &str) -> Result<()> {
        self.send(
            reqwest::Method::POST,
            &format!("/inbox/{}/read", checked(id)?),
            None,
        )
        .await
    }

    /// The event kinds from your project the player muted ([`ALL_KINDS`]
    /// means all of them).
    ///
    /// # Errors
    /// As for [`Player::friends`].
    pub async fn mutes(&self) -> Result<Vec<String>> {
        field(
            self.call(reqwest::Method::GET, "/mutes", None).await?,
            "mutes",
        )
    }

    fn mute_path(kind: &str) -> Result<String> {
        let kind: &str = if kind == ALL_KINDS {
            kind
        } else {
            checked(kind)?
        };
        Ok(format!("/mutes/{kind}"))
    }

    /// Stops friends' events of `kind` reaching the player ([`ALL_KINDS`] for
    /// every kind).
    ///
    /// # Errors
    /// A malformed kind, or the request failed.
    pub async fn mute(&self, kind: &str) -> Result<()> {
        self.send(reqwest::Method::PUT, &Self::mute_path(kind)?, None)
            .await
    }

    /// Undoes [`Player::mute`].
    ///
    /// # Errors
    /// A malformed kind, or the request failed.
    pub async fn unmute(&self, kind: &str) -> Result<()> {
        self.send(reqwest::Method::DELETE, &Self::mute_path(kind)?, None)
            .await
    }

    /// Tells the service the player's time zone (an IANA name such as
    /// `America/Chicago`), so a notification's `send_at_local` lands on their
    /// clock. The last write wins.
    ///
    /// # Errors
    /// A name the service doesn't know, or the request failed.
    pub async fn set_time_zone(&self, time_zone: &str) -> Result<()> {
        self.send(
            reqwest::Method::PUT,
            "/time-zone",
            Some(json!({ "time_zone": time_zone })),
        )
        .await
    }
}
