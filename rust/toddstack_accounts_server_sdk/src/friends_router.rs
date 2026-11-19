//! In-app friend requests as one mountable [axum] router (feature `axum`).
//!
//! Your web and native apps list, send, accept, decline, cancel, remove, block
//! and unblock friends inside your own UI, through your own server, which
//! forwards to the accounts service. The browser and the apps never see
//! accounts credentials.
//!
//! You supply the one thing this crate cannot know: who the caller is
//! ([`FriendsAuth`]). Answer [`FriendsAuth::player_sub`] and the router acts
//! for that player over the Server API with your client credentials
//! ([`Accounts::player`]), so you store no accounts token. The older
//! [`FriendsAuth::access_token`] (forwarding the person's own token) still
//! works but is deprecated and goes in 0.10.0. Guests and signed-out callers
//! get `401`.
//!
//! ```ignore
//! let app = Router::new().merge(friends_router::router(accounts, Arc::new(my_auth)));
//! ```
//!
//! Every route lives under [`BASE`] and answers JSON. Errors are
//! `{"error": <code>, "message": <text for the person>}`; see [`Failure`].

#![expect(
    deprecated,
    reason = "still serves the token-based `FriendsAuth::access_token`; removed in 0.10.0"
)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::json;

use crate::Error;
use crate::friends::{
    Blocks, Friends, MAX_RELATIONSHIPS_PER_CALL, Person, Relationship, Requests, Sent, is_valid_id,
};
use crate::player::Player;
use crate::tokens::Accounts;

/// Where [`router`] mounts every route. Chosen to sit beside, not on top of,
/// a project's `/api/v1/app/*` sign-in routes and its own `/api/v1/profile/*`.
pub const BASE: &str = "/api/v1/friends";

/// The credential that proved who is calling. A cookie is sent by the browser on forged cross-site requests, a
/// `Bearer` app token is not, so only cookie writes get the same-origin check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// Authenticated by a session cookie: its accounts access token.
    Cookie(String),
    /// Authenticated by an `Authorization: Bearer` app token (native apps): its accounts access token.
    Bearer(String),
}

impl Credential {
    fn token(&self) -> &str {
        match self {
            Self::Cookie(t) | Self::Bearer(t) => t,
        }
    }
}

/// Who is calling, for the router. Implement it over your sessions.
pub trait FriendsAuth: Send + Sync + 'static {
    /// The caller's accounts access token (scope `friends`, refreshed if it
    /// expired) and which credential proved it; `None` for a guest or
    /// signed-out caller (the default). Only asked when `player_sub` has no answer. Report [`Credential::Cookie`] whenever a cookie
    /// authenticated the request, even if a `Bearer` header is also present.
    #[deprecated(
        since = "0.9.0",
        note = "implement `player_sub`: the router then acts for the player with your client credentials"
    )]
    fn access_token(&self, headers: &HeaderMap) -> impl Future<Output = Option<Credential>> + Send {
        let _ = headers;
        std::future::ready(None)
    }

    /// The caller's `sub` and which credential proved it; `None` for a guest or signed-out caller, or to fall
    /// back to [`FriendsAuth::access_token`]. When it answers, the router acts for that player over the Server API
    /// ([`Accounts::player`]) with your client credentials, so no player token is needed (the client secret is).
    /// Only the credential's kind matters here (a cookie write gets the same-origin check); its string is ignored,
    /// so pass an empty one. Report [`Credential::Cookie`] whenever a cookie authenticated the request.
    fn player_sub(
        &self,
        headers: &HeaderMap,
    ) -> impl Future<Output = Option<(String, Credential)>> + Send {
        let _ = headers;
        std::future::ready(None)
    }

    /// Your site's origin (`https://www.example.com`), the only cross-origin
    /// answer a cookie-authenticated write accepts.
    fn public_origin(&self) -> &str;
}

struct Shared<A> {
    accounts: Arc<Accounts>,
    auth: Arc<A>,
}

/// The code in an error body's `error` field, with its status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Failure {
    /// 401: a guest or signed-out caller. Offer sign-in.
    AccountRequired,
    /// 401: the accounts service rejected the token. Sign in again.
    SessionExpired,
    /// 403: a cookie write from another site.
    CrossSite,
    /// 403: the project isn't allowed the `friends` scope or the person didn't grant it.
    MissingScope,
    /// 400: a malformed body.
    BadRequest,
    /// 400: the accounts service said no (not found, blocked, the person's
    /// privacy setting, or a limit). Its `message` is meant for the person
    /// and is deliberately vague so a block or setting can't be probed.
    Refused,
    /// 404.
    NotFound,
    /// 409: the accounts service reported a conflict.
    Conflict,
    /// 422: the accounts service couldn't process the request.
    Unprocessable,
    /// 429: too many requests an hour. `message` says to wait.
    RateLimited,
    /// 502: the accounts service failed or is unreachable.
    AccountsUnavailable,
}

impl Failure {
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::AccountRequired | Self::SessionExpired => StatusCode::UNAUTHORIZED,
            Self::CrossSite | Self::MissingScope => StatusCode::FORBIDDEN,
            Self::BadRequest | Self::Refused => StatusCode::BAD_REQUEST,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::AccountsUnavailable => StatusCode::BAD_GATEWAY,
        }
    }

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AccountRequired => "account_required",
            Self::SessionExpired => "session_expired",
            Self::CrossSite => "cross_site",
            Self::MissingScope => "missing_scope",
            Self::BadRequest => "bad_request",
            Self::Refused => "refused",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Unprocessable => "unprocessable",
            Self::RateLimited => "rate_limited",
            Self::AccountsUnavailable => "accounts_unavailable",
        }
    }

    fn respond(self, message: &str) -> Response {
        (
            self.status(),
            Json(json!({ "error": self.code(), "message": message })),
        )
            .into_response()
    }
}

fn classify(error: &Error) -> (Failure, String) {
    // The Server API reports `Error::Server`; it means what the same status means from the bearer API.
    let as_api: Error;
    let error: &Error = if let Error::Server {
        status, message, ..
    } = error
    {
        as_api = Error::Api {
            status: *status,
            message: message.clone(),
        };
        &as_api
    } else {
        error
    };
    match error {
        Error::Api {
            status: 400,
            message,
        } => (Failure::Refused, message.clone()),
        Error::Api { status: 401, .. } => (
            Failure::SessionExpired,
            String::from("Please sign in again."),
        ),
        Error::Api { status: 403, .. } => (
            Failure::MissingScope,
            String::from("Friends aren't enabled for this account here."),
        ),
        Error::Api {
            status: 404,
            message,
        }
        | Error::NotFound(message) => (Failure::NotFound, message.clone()),
        Error::Api {
            status: 409,
            message,
        }
        | Error::Conflict(message) => (Failure::Conflict, message.clone()),
        Error::Api {
            status: 422,
            message,
        }
        | Error::Unprocessable(message) => (Failure::Unprocessable, message.clone()),
        Error::Api {
            status: 429,
            message,
        }
        | Error::RateLimited(message) => (Failure::RateLimited, message.clone()),
        _ => (
            Failure::AccountsUnavailable,
            String::from("Friends are unavailable right now. Try again soon."),
        ),
    }
}

fn fail(error: &Error) -> Response {
    let (failure, message): (Failure, String) = classify(error);
    failure.respond(&message)
}

/// A cookie write must be provably same-origin: `Sec-Fetch-Site: same-origin`, or (without that header) an
/// `Origin` equal to `public_origin`. `same-site`, `cross-site`, `none` and a request with neither header are refused.
fn same_origin<A: FriendsAuth>(auth: &A, headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return site == "same-origin";
    }
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| origin == auth.public_origin())
}

/// Who the router acts for on one request: the player over the Server API, or a person's own access token.
enum Act<'a> {
    Player(Player<'a>),
    Token(&'a Accounts, String),
}

impl Act<'_> {
    async fn list(&self) -> Result<Vec<Person>, Error> {
        match self {
            Self::Player(p) => p.friends().await,
            Self::Token(a, t) => Friends::new(a, t).list().await,
        }
    }

    async fn requests(&self) -> Result<Requests, Error> {
        match self {
            Self::Player(p) => p.requests().await,
            Self::Token(a, t) => Friends::new(a, t).requests().await,
        }
    }

    async fn request_by_username(&self, username: &str) -> Result<Sent, Error> {
        match self {
            Self::Player(p) => p.request_by_username(username).await,
            Self::Token(a, t) => Friends::new(a, t).request_by_username(username).await,
        }
    }

    async fn request_by_code(&self, code: &str) -> Result<Sent, Error> {
        match self {
            Self::Player(p) => p.request_by_code(code).await,
            Self::Token(a, t) => Friends::new(a, t).request_by_code(code).await,
        }
    }

    async fn accept(&self, sub: &str) -> Result<(), Error> {
        match self {
            Self::Player(p) => p.accept(sub).await,
            Self::Token(a, t) => Friends::new(a, t).accept(sub).await,
        }
    }

    async fn decline(&self, sub: &str) -> Result<(), Error> {
        match self {
            Self::Player(p) => p.decline(sub).await,
            Self::Token(a, t) => Friends::new(a, t).decline(sub).await,
        }
    }

    async fn cancel(&self, sub: &str) -> Result<(), Error> {
        match self {
            Self::Player(p) => p.cancel(sub).await,
            Self::Token(a, t) => Friends::new(a, t).cancel(sub).await,
        }
    }

    async fn remove(&self, sub: &str) -> Result<(), Error> {
        match self {
            Self::Player(p) => p.remove(sub).await,
            Self::Token(a, t) => Friends::new(a, t).remove(sub).await,
        }
    }

    async fn relationships(&self, subs: &[&str]) -> Result<HashMap<String, Relationship>, Error> {
        match self {
            Self::Player(p) => p.relationships(subs).await,
            Self::Token(a, t) => Friends::new(a, t).relationships(subs).await,
        }
    }

    async fn blocks(&self) -> Result<Blocks, Error> {
        match self {
            Self::Player(p) => p.blocks().await,
            Self::Token(a, t) => Friends::new(a, t).blocks().await,
        }
    }

    async fn block(&self, sub: &str) -> Result<(), Error> {
        match self {
            Self::Player(p) => p.block(sub).await,
            Self::Token(a, t) => Friends::new(a, t).block(sub).await,
        }
    }

    async fn unblock(&self, sub: &str) -> Result<(), Error> {
        match self {
            Self::Player(p) => p.unblock(sub).await,
            Self::Token(a, t) => Friends::new(a, t).unblock(sub).await,
        }
    }
}

async fn actor<'a, A: FriendsAuth>(
    shared: &'a Shared<A>,
    headers: &HeaderMap,
    write: bool,
) -> Result<Act<'a>, Box<Response>> {
    let (credential, player): (Credential, Option<String>) =
        if let Some((sub, credential)) = shared.auth.player_sub(headers).await {
            (credential, Some(sub))
        } else {
            let credential: Credential =
                shared.auth.access_token(headers).await.ok_or_else(|| {
                    Box::new(Failure::AccountRequired.respond("Sign in to use friends."))
                })?;
            (credential, None)
        };
    if write
        && matches!(credential, Credential::Cookie(_))
        && !same_origin(shared.auth.as_ref(), headers)
    {
        return Err(Box::new(
            Failure::CrossSite.respond("That request came from another site."),
        ));
    }
    match player {
        Some(sub) => shared
            .accounts
            .player(&sub)
            .map(Act::Player)
            .map_err(|e| Box::new(fail(&e))),
        None => Ok(Act::Token(&shared.accounts, credential.token().to_owned())),
    }
}

/// A `sub` from the URL is percent-decoded by axum: refuse anything that isn't a plain id before it reaches a path.
fn id(value: &str) -> Result<(), Box<Response>> {
    if is_valid_id(value) {
        Ok(())
    } else {
        Err(Box::new(
            Failure::BadRequest.respond("That isn't a valid person id."),
        ))
    }
}

fn done(result: Result<(), Error>) -> Response {
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(&e),
    }
}

type St<A> = State<Arc<Shared<A>>>;

async fn list<A: FriendsAuth>(State(s): St<A>, headers: HeaderMap) -> Response {
    let act: Act<'_> = match actor(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match act.list().await {
        Ok(friends) => Json(json!({ "friends": friends })).into_response(),
        Err(e) => fail(&e),
    }
}

async fn requests<A: FriendsAuth>(State(s): St<A>, headers: HeaderMap) -> Response {
    let act: Act<'_> = match actor(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match act.requests().await {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize)]
struct SendBody {
    username: Option<String>,
    friend_code: Option<String>,
}

async fn send<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    body: Result<Json<SendBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let Ok(Json(body)) = body else {
        return Failure::BadRequest.respond("Send a username or a friend code.");
    };
    let friends: &Act<'_> = &act;
    let sent: Result<Sent, Error> = match (body.username, body.friend_code) {
        (Some(username), None) => friends.request_by_username(&username).await,
        (None, Some(code)) => friends.request_by_code(&code).await,
        _ => return Failure::BadRequest.respond("Send a username or a friend code, not both."),
    };
    match sent {
        Ok(sent) => Json(json!({ "result": sent })).into_response(),
        Err(e) => fail(&e),
    }
}

async fn accept<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if let Err(r) = id(&sub) {
        return *r;
    }
    done(act.accept(&sub).await)
}

async fn decline<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if let Err(r) = id(&sub) {
        return *r;
    }
    done(act.decline(&sub).await)
}

async fn cancel<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if let Err(r) = id(&sub) {
        return *r;
    }
    done(act.cancel(&sub).await)
}

async fn remove<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if let Err(r) = id(&sub) {
        return *r;
    }
    done(act.remove(&sub).await)
}

#[derive(Deserialize)]
struct RelationshipsQuery {
    subs: Option<String>,
}

async fn relationships<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Query(query): Query<RelationshipsQuery>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let subs: String = query.subs.unwrap_or_default();
    let subs: Vec<&str> = subs.split(',').filter(|v| !v.is_empty()).collect();
    if subs.is_empty()
        || subs.len() > MAX_RELATIONSHIPS_PER_CALL
        || !subs.iter().all(|v| is_valid_id(v))
    {
        return Failure::BadRequest.respond("Send 1 to 100 comma-separated subs.");
    }
    match act.relationships(&subs).await {
        Ok(found) => {
            let found: HashMap<String, crate::friends::Relationship> = found;
            Json(json!({ "relationships": found })).into_response()
        }
        Err(e) => fail(&e),
    }
}

async fn blocks<A: FriendsAuth>(State(s): St<A>, headers: HeaderMap) -> Response {
    let act: Act<'_> = match actor(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match act.blocks().await {
        Ok(b) => Json(b).into_response(),
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize)]
struct BlockBody {
    sub: String,
}

async fn block<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    body: Result<Json<BlockBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let Ok(Json(body)) = body else {
        return Failure::BadRequest.respond("Send the sub to block.");
    };
    if let Err(r) = id(&body.sub) {
        return *r;
    }
    done(act.block(&body.sub).await)
}

async fn unblock<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let act: Act<'_> = match actor(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if let Err(r) = id(&sub) {
        return *r;
    }
    done(act.unblock(&sub).await)
}

/// The friends routes, ready to merge into your app. Routes:
///
/// | method and path | does |
/// |---|---|
/// | `GET /api/v1/friends` | `{friends: [Person]}` |
/// | `GET /api/v1/friends/requests` | `{incoming, outgoing}` |
/// | `POST /api/v1/friends/requests` `{username}` or `{friend_code}` | `{result: requested\|accepted\|already_friends}` |
/// | `POST /api/v1/friends/requests/{sub}/accept` and `/decline` | 204 |
/// | `DELETE /api/v1/friends/requests/{sub}` | cancel: 204 |
/// | `DELETE /api/v1/friends/{sub}` | remove a friend: 204 |
/// | `GET /api/v1/friends/relationships?subs=a,b` | `{relationships: {sub: state}}` |
/// | `GET /api/v1/friends/blocks` | `{blocked: [Person], hidden: [sub]}` |
/// | `POST /api/v1/friends/blocks` `{sub}` | 204 |
/// | `DELETE /api/v1/friends/blocks/{sub}` | unblock: 204 |
pub fn router<A: FriendsAuth>(accounts: Arc<Accounts>, auth: Arc<A>) -> Router {
    let shared: Arc<Shared<A>> = Arc::new(Shared { accounts, auth });
    Router::new()
        .route("/api/v1/friends", get(list::<A>))
        .route(
            "/api/v1/friends/requests",
            get(requests::<A>).post(send::<A>),
        )
        .route("/api/v1/friends/relationships", get(relationships::<A>))
        .route("/api/v1/friends/requests/{sub}", delete(cancel::<A>))
        .route("/api/v1/friends/requests/{sub}/accept", post(accept::<A>))
        .route("/api/v1/friends/requests/{sub}/decline", post(decline::<A>))
        .route("/api/v1/friends/blocks", get(blocks::<A>).post(block::<A>))
        .route("/api/v1/friends/blocks/{sub}", delete(unblock::<A>))
        .route("/api/v1/friends/{sub}", delete(remove::<A>))
        .with_state(shared)
}
