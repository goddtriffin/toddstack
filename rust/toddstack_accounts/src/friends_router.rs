//! In-app friend requests as one mountable [axum] router (feature `axum`).
//!
//! Your web and native apps list, send, accept, decline, cancel, remove, block
//! and unblock friends inside your own UI, through your own server, which
//! forwards to the accounts service with the signed-in person's access token
//! ([`Friends`]). The browser and the apps never see that token.
//!
//! You supply the one thing this crate cannot know: who the caller is
//! ([`FriendsAuth`]). Guests and signed-out callers get `401`.
//!
//! ```ignore
//! let app = Router::new().merge(friends_router::router(accounts, Arc::new(my_auth)));
//! ```
//!
//! Every route lives under [`BASE`] and answers JSON. Errors are
//! `{"error": <code>, "message": <text for the person>}`; see [`Failure`].

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
use crate::friends::{Friends, MAX_RELATIONSHIPS_PER_CALL, Sent};
use crate::tokens::Accounts;

/// Where [`router`] mounts every route. Chosen to sit beside, not on top of,
/// a project's `/api/v1/app/*` sign-in routes and its own `/api/v1/profile/*`.
pub const BASE: &str = "/api/v1/friends";

/// Who is calling, for the router. Implement it over your sessions.
pub trait FriendsAuth: Send + Sync + 'static {
    /// The caller's accounts access token (scope `friends`), refreshed if it
    /// expired; `None` for a guest or signed-out caller. Read your session
    /// cookie or your `Authorization: Bearer` app token from `headers`.
    fn access_token(&self, headers: &HeaderMap) -> impl Future<Output = Option<String>> + Send;

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

/// Cookie-authenticated writes must be same-origin; a Bearer app token is not
/// a cookie, so a forged cross-site request can't carry one.
fn same_origin<A: FriendsAuth>(auth: &A, headers: &HeaderMap) -> bool {
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer "))
    {
        return true;
    }
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return site == "same-origin";
    }
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| origin == auth.public_origin())
}

async fn token<A: FriendsAuth>(
    shared: &Shared<A>,
    headers: &HeaderMap,
    write: bool,
) -> Result<String, Box<Response>> {
    if write && !same_origin(shared.auth.as_ref(), headers) {
        return Err(Box::new(
            Failure::CrossSite.respond("That request came from another site."),
        ));
    }
    shared
        .auth
        .access_token(headers)
        .await
        .ok_or_else(|| Box::new(Failure::AccountRequired.respond("Sign in to use friends.")))
}

fn done(result: Result<(), Error>) -> Response {
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(&e),
    }
}

type St<A> = State<Arc<Shared<A>>>;

async fn list<A: FriendsAuth>(State(s): St<A>, headers: HeaderMap) -> Response {
    let token: String = match token(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match Friends::new(&s.accounts, &token).list().await {
        Ok(friends) => Json(json!({ "friends": friends })).into_response(),
        Err(e) => fail(&e),
    }
}

async fn requests<A: FriendsAuth>(State(s): St<A>, headers: HeaderMap) -> Response {
    let token: String = match token(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match Friends::new(&s.accounts, &token).requests().await {
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
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let Ok(Json(body)) = body else {
        return Failure::BadRequest.respond("Send a username or a friend code.");
    };
    let friends: Friends<'_> = Friends::new(&s.accounts, &token);
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
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    done(Friends::new(&s.accounts, &token).accept(&sub).await)
}

async fn decline<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    done(Friends::new(&s.accounts, &token).decline(&sub).await)
}

async fn cancel<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    done(Friends::new(&s.accounts, &token).cancel(&sub).await)
}

async fn remove<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    done(Friends::new(&s.accounts, &token).remove(&sub).await)
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
    let token: String = match token(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let subs: String = query.subs.unwrap_or_default();
    let subs: Vec<&str> = subs.split(',').filter(|v| !v.is_empty()).collect();
    if subs.is_empty() || subs.len() > MAX_RELATIONSHIPS_PER_CALL {
        return Failure::BadRequest.respond("Send 1 to 100 comma-separated subs.");
    }
    match Friends::new(&s.accounts, &token).relationships(&subs).await {
        Ok(found) => {
            let found: HashMap<String, crate::friends::Relationship> = found;
            Json(json!({ "relationships": found })).into_response()
        }
        Err(e) => fail(&e),
    }
}

async fn blocks<A: FriendsAuth>(State(s): St<A>, headers: HeaderMap) -> Response {
    let token: String = match token(&s, &headers, false).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match Friends::new(&s.accounts, &token).blocks().await {
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
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let Ok(Json(body)) = body else {
        return Failure::BadRequest.respond("Send the sub to block.");
    };
    done(Friends::new(&s.accounts, &token).block(&body.sub).await)
}

async fn unblock<A: FriendsAuth>(
    State(s): St<A>,
    headers: HeaderMap,
    Path(sub): Path<String>,
) -> Response {
    let token: String = match token(&s, &headers, true).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    done(Friends::new(&s.accounts, &token).unblock(&sub).await)
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
