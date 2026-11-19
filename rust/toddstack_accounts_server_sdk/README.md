# toddstack-accounts-server-sdk

*Formerly `toddstack-accounts` (renamed in 0.9.1; same API; the old name stays on crates.io at 0.9.0). The library is
`toddstack_accounts_server_sdk`. Rust is the only SDK that gets new features.*

Client for Todd Griffin's Projects accounts (`accounts.toddgriffin.me`): what a project's backend needs to sign
people in, check tokens, verify webhooks, and act for players. Part of
[toddstack](https://github.com/goddtriffin/toddstack).

Web apps use the **backend-for-frontend** pattern: your Rust backend is the OIDC client and keeps the tokens;
the browser only ever holds your own session cookie. Native apps sign in through the system browser
(`ASWebAuthenticationSession`, Custom Tabs) as public clients and send their access token to your backend,
which checks it with [`Accounts::verify_access_token`].

## Sign in (two routes)

```rust
use toddstack_accounts_server_sdk::{Accounts, Config, login};

let accounts = Accounts::new(Config::toddgriffin(
    "boggledygook",
    Some(client_secret),                               // from your env (1Password)
    "https://www.boggledygook.com/auth/callback",      // registered exactly
))?;

// GET /auth/login?return=/play
let (url, pending) = login::start(accounts.config(), &["openid", "profile", "offline_access", "friends"], Some("/play"));
// Store `pending` server-side or in a signed HttpOnly cookie, then redirect to `url`.

// GET /auth/callback?code=..&state=..&iss=..
let signed_in = accounts.finish_login(&pending, raw_query).await?;
// signed_in.claims.sub is the person's permanent id: key everything on it.
// Keep signed_in.tokens (refresh_token rotates: always store the new one).
// Redirect to signed_in.return_to (already checked to be a path on your site).
```

`finish_login` checks `state` and `iss`, exchanges the code with PKCE, and verifies the ID token (ES256 signature
against the published keys, issuer, audience, expiry, nonce).

Signing keys rotate on a schedule, and a new key is published two days before it signs. The client keeps the key set
for an hour, and a token whose `kid` isn't in it triggers a refetch (at most once a minute, shared by concurrent
checks), so rotation never rejects a valid token.

| Call | What |
|---|---|
| `login::start_with(.., &Options { prompt, login_hint, max_age })` | `prompt=login` forces a fresh sign-in, `none` checks silently, `create` opens sign-up |
| `accounts.refresh(token)` | New tokens; `Error::TokenRefused` means sign in again (expired, reused, or the person removed your app) |
| `accounts.revoke(token)` | Revokes a refresh token (and its family); `sign_out` does this for you |
| `accounts.verify_access_token(token)` | For your API: checks `typ`, issuer, audience = your client id, expiry |
| `accounts.userinfo(token)` | The profile JSON |
| `return_to::safe(path)` | Keeps redirects on your site |
| `config.account_url("/account")` | A link to the accounts site tagged `?client=<your id>`: use it for every link there ("Edit profile and account", "Account settings"), so the page shows "Back to <your project>" and Todd sees where people came from |

## Sign out (one route)

```rust
use toddstack_accounts_server_sdk::logout;

// GET /logout: end your own session first, then:
let signed_out = accounts.sign_out(refresh_token.as_deref(), &logout::Options {
    id_token_hint: Some(&id_token),               // the raw ID token you kept (tokens.id_token), expired is fine
    post_logout_redirect_uri: Some("/"),          // a path, or an absolute URL on your redirect URI's origin
    state: None,                                  // echoed back as ?state=
    local_only: false,                            // true: sign out of your app alone, no URL
}).await;
if let Some(error) = signed_out.revoke_error { /* log it; sign-out still goes ahead */ }
if let Some(url) = signed_out.url { /* redirect the browser to url */ }
```

Sign-out is global by default. `sign_out` revokes the refresh token (pass `None` if you don't keep one) and builds the
end-session URL (`logout::end_session_url`, with your `client_id`). **Redirect the browser to `signed_out.url`**: with
a valid `id_token_hint` the accounts service ends the browser session at once (no confirm page), which signs the
person out of every project in that browser (never their other devices) and sends each project a `session.signed_out`
webhook. Skip the redirect and the accounts session survives, so "Sign in" silently signs them back in. Without a
hint the service asks "Sign out?" first. Set `local_only: true` to sign out of your app alone: the token is revoked
and `url` is `None`.

What a project must do:

- Keep `tokens.id_token` (the raw string) and `claims.sid` with your session, to pass as the hint and to match
  `session.signed_out`. `sid` is `None` on tokens issued before the service added it.
- `claims.visibility` is `Visibility::Review` for a review account (the `toddgriffin_visibility: "review"` claim) and
  `Visibility::Normal` otherwise, including when the claim is absent; `claims.is_review()` is the shortcut. A
  `user.updated` webhook carries the same value: `event.visibility()` (`None` when the event has no such field).
- Register a post-logout redirect URI for your client, exactly (Todd runs
  `accounts-service admin client update ... --logout <URI>` or uses the admin UI). The service skips its "Sign out?" confirm
  page only when the ID token hint is valid and the redirect URI is registered (or absent). Otherwise the person sees
  the confirm page.
- Handle `session.signed_out` and `user.signed_out` (see Webhooks).

## Webhooks

```rust
use toddstack_accounts_server_sdk::webhook::{self, EventKind};

let event = webhook::verify(webhook_secret, signature_header, &raw_body, now_unix)?;
match event.kind {
    EventKind::UserDeleted => { /* delete or anonymize event.sub()'s rows */ }
    EventKind::UserUpdated => { /* refresh cached username / picture */ }
    EventKind::UserSignedOut | EventKind::UserSuspended | EventKind::GrantRevoked => { /* end event.sub()'s sessions */ }
    EventKind::SessionSignedOut => { /* drop every local session of event.sub() with sid == event.sid() */ }
    EventKind::FriendshipCreated => { /* event.sub() and event.friend_sub() are now friends */ }
    EventKind::FriendshipRemoved => { /* drop friend-only state between them */ }
    EventKind::FriendRequestCreated | EventKind::FriendRequestCancelled | EventKind::FriendRequestDeclined => {
        /* event.from_sub() asked event.to_sub(), took it back, or was turned down: refresh friend buttons */
    }
    _ => {} // EventKind::Other(type) for types newer than this crate
}
```

`event.kind.as_str()` gives the wire name (`"user.deleted"`). Friendship and friend request events are sent once
per side: each person gets one with themselves as `sub`, so handle only `sub`'s view. An accepted request sends
only `friendship.created`.

Your sessions are your own: accounts can't end them. On `user.signed_out` (the person signed out everywhere,
changed or reset their password, or an admin signed them or everyone out), `user.suspended` and
`grant.revoked`, delete every session you hold for `event.sub()`. Their refresh tokens are already revoked.
On `session.signed_out` (the person signed out of one accounts browser session, from any project), delete only the
sessions you hold for `event.sub()` whose stored `sid` equals `event.sid()`; their other devices stay signed in.

Verify the raw body before parsing. An empty secret is refused. While you rotate a webhook secret the service signs
with both, so the header carries several `v1=` values and the event is accepted when any one matches the secret you
hold. Signatures older than 5 minutes are refused. Deliveries retry, so ignore
repeated `event.id`s.

## Acting for a player (the Server API)

Your server calls accounts **as itself** (your client credentials, so the config needs its client secret), naming the
player by `sub`. It never sends a player's access or refresh token, so keep none past the sign-in code exchange.
`accounts.player(sub)` has a method for every route under `/api/v1/players/{sub}`:

```rust
use toddstack_accounts_server_sdk::friends::Event;

let player = accounts.player(&sub)?;
let profile = player.profile().await?;      // claims for the granted scopes, plus `visibility` and `suspended`
let friends = player.friends().await?;      // only friends who also use your project
player.request_by_username("alex").await?;  // or request(sub), request_by_code(code)
player.post_event(&Event {
    kind: String::from("beat_your_score"),
    title: String::from("Alex beat your high score"),
    ..Event::default()
}, "daily-2026-11-18").await?;              // the idempotency key is required; needs friends:events
player.report(&other, Reason::Harassment, None).await?;
```

Also `relationships(&[..])`, `relationship(sub)`, `requests()`, `accept`, `decline`, `cancel`, `remove`, `block`,
`unblock`, `blocks()`, `inbox(limit)`, `mark_read(id)`, `mutes()`, `mute(kind)`, `unmute(kind)` (`ALL_KINDS` is every kind)
and `set_time_zone(tz)`. `accounts.report_as_player(&PlayerReport { reporter_sub, .. })` is the report with its
`reporter_sub`.

Any account holder's public profile, for showing, blocking and reporting people who never used your project:
`accounts.profile_by_sub(sub, viewer)` and `accounts.profile_by_username(name, viewer)` return `PublicProfile { sub,
username, picture }` (nothing else). Pass the `viewer` (the player asking) so a block either way is the same
`not_found` as an unknown, suspended, review or deleted account.

- **Errors carry the code.** A refusal is `Error::Server { status, code, message }`; `error.code()` is `not_found`,
  `suspended`, `scope`, `rate_limited`, `bad_request`, `unprocessable`, ... Match on it. Everything but a 429 and a 5xx is
  final. A player who never signed in to your project, a deleted one and one who removed your app are all `not_found`;
  a suspended player can be read, not written for (`suspended`).
- **Writes are idempotent and retried.** Each is a no-op success when the state already holds (a repeated block, accept,
  mute), and an event is deduplicated by its key (`EventResult::replayed`; the same key with another event is
  `unprocessable`). Three attempts on a 429, a 5xx or a connection failure.
- Every list shows only people who also use your project. Sending a request may name any account holder; `not_found`
  is the one answer for everyone who can't be asked.

## Local token checks and instant logout

A native app sends its accounts access token to your server, which checks it with no call to accounts:
`verify_access_token` verifies the ES256 signature against the signing keys (fetched from `/oidc/jwks`, cached, refetched
on an unknown `kid`), `iss`, `aud` (your client id) and `exp` (30 s of skew), and reads `sub` and `fam` (the token
family). Local checks can't see a sign-out, so accounts pushes revocations and the client refuses a token whose `fam`
is revoked. Two feeds fill one in-memory set (`accounts.revocations()`), deduplicated by `fam` and kept until each
entry's `expires_at`:

```rust
// Starts the poll (once now, awaited, then every 30 s) and gives you the client to share. Errors if the first
// poll still fails after 3 quick tries (a few seconds): until a poll lands, revoked tokens verify, so you decide
// whether to boot without instant logout.
let (accounts, _poller) = Accounts::with_revocation_poller(config).await?; // keep _poller alive

// In your webhook route: verifies the signature, then records `tokens.revoked` families at once.
let event = accounts.handle_webhook(secret, signature_header, body)?;
```

`start_revocation_poller_with(PollerOptions { interval, on_error, startup_backoff })` runs the poll on an existing
`Arc<Accounts>`; after startup, `on_error` hears each failed poll (it is retried; verification keeps working meanwhile;
a panic in it is caught, so the poll never dies). `poller.last_success()` is the time of the last good poll, for a
health check. Dropping the poller stops it. A mass revocation is cheap: `Revocations::revoke_many` takes a batch under
one lock and expired entries are pruned at most every 10 s.

## Friends and events (deprecated)

**Deprecated, removed in 0.10.0.** `Friends` calls the bearer API with a player's access token, which a project server
must not hold. Use `accounts.player(sub)` (above): the same method names, with `post_event` taking an idempotency key.
`userinfo` is deprecated the same way: use the ID token's claims from `finish_login`, then `player(sub)?.profile()`.

### The deprecated token-based API

```rust
use toddstack_accounts_server_sdk::friends::{Event, Friends};

let friends = Friends::new(&accounts, &access_token);
let list = friends.list().await?;          // only friends who also use your app
friends.request_by_username("alex").await?;
friends.post_event(&Event {
    kind: String::from("beat_your_score"),
    title: String::from("Alex beat your high score"),
    body: String::from("4,200 points in today's daily"),
    collapse_key: Some(String::from("daily-2026-11-14")),
    ..Event::default()
}).await?;                                  // needs scope friends:events
```

The person can mute your app's events, one kind or all (`friends.mute("daily_completed")`,
`friends.mute(ALL_KINDS)`, `unmute`, `mutes()`), here or in Account Center.

`inbox_with_unread(limit)` returns `Inbox { items, unread: Some(Unread { project, account, total }) }`: `total` is your
badge. An item with `client_id: None` is an account-level item from the service itself ("alex accepted your friend
request"): it shows in every project, is one item read everywhere at once, and is never muted or dropped.
`post_event_result` returns `EventResult { delivered, dropped, counts }` where `post_event` returns only `delivered`.

A block overrides everything (no requests or events either way). Events go to friends' inboxes (`inbox()`),
and will also go out as push notifications once the native apps exist.

## In-app friend requests

Every project can list, send, accept, decline, cancel, remove, block and unblock friends inside its own web and native
apps, not only on accounts.toddgriffin.me. Enable the `axum` feature and mount one router; it forwards each call to
the accounts service, so no browser or app ever sees accounts credentials. Implement `FriendsAuth::player_sub` and the
router acts for that player over the Server API (`accounts.player(sub)`) with your client credentials: no player token
is stored (the config needs its client secret, and the player's `friends` scope applies as usual). The token path
(`FriendsAuth::access_token`, forwarded with `Friends`) still works but is deprecated.

```toml
toddstack-accounts-server-sdk = { version = "0.9.3", features = ["axum"] }
```

```rust
use toddstack_accounts_server_sdk::friends_router::{self, Credential, FriendsAuth};

struct MyAuth { /* your sessions */ }
impl FriendsAuth for MyAuth {
    // Preferred: the caller's `sub` and the credential kind that proved it (the string is ignored: pass an empty
    // one). Cookie writes get the same-origin check. None falls back to `access_token`.
    async fn player_sub(&self, headers: &axum::http::HeaderMap) -> Option<(String, Credential)> { /* ... */ }

    // Deprecated: the caller's accounts access token (refresh it if expired) and the credential that proved it:
    // Credential::Cookie(token) for your session cookie, Credential::Bearer(token) for the native app's
    // `Authorization: Bearer` app token. None for a guest or signed-out caller (they get 401).
    async fn access_token(&self, headers: &axum::http::HeaderMap) -> Option<Credential> { /* ... */ }
    fn public_origin(&self) -> &str { "https://www.boggledygook.com" }
}

let app = app.merge(friends_router::router(accounts.clone(), std::sync::Arc::new(MyAuth { /* .. */ })));
```

| route (all JSON) | does |
|---|---|
| `GET /api/v1/friends` | `{friends: [Person]}` |
| `GET /api/v1/friends/requests` | `{incoming: [Person], outgoing: [Person]}` |
| `POST /api/v1/friends/requests` `{username}` or `{friend_code}` | `{result: requested\|accepted\|already_friends}` |
| `POST /api/v1/friends/requests/{sub}/accept` and `.../decline` | 204 |
| `DELETE /api/v1/friends/requests/{sub}` | cancel a request you sent: 204 |
| `DELETE /api/v1/friends/{sub}` | remove a friend: 204 |
| `GET /api/v1/friends/relationships?subs=a,b` | `{relationships: {sub: friends\|request_sent\|request_received\|blocked\|none}}`, 1 to 100 subs |
| `GET /api/v1/friends/blocks` | `{blocked: [Person], hidden: [sub]}` |
| `POST /api/v1/friends/blocks` `{sub}` | 204 |
| `DELETE /api/v1/friends/blocks/{sub}` | unblock: 204 |

Errors are `{"error": <code>, "message": <text to show the person>}`: `account_required` (401, a guest or signed-out
caller: offer sign-in), `session_expired` (401), `cross_site` (403), `missing_scope` (403), `bad_request` (400),
`refused` (400: not found, blocked, the person's privacy setting, or a limit of 200 pending, 1000 friends; the message
is deliberately vague so a block or setting can't be probed), `not_found` (404), `conflict` (409), `unprocessable` (422), `rate_limited` (429, 20
requests an hour), `accounts_unavailable` (502). A `sub` that isn't 1 to 64 of `A-Za-z0-9_-` is `bad_request`.

Writes authenticated by a cookie must be provably same-origin: `Sec-Fetch-Site: same-origin`, or, without that header,
an `Origin` equal to `public_origin`. `same-site`, `cross-site`, `none`, or neither header, is `cross_site` (403). The
check follows the credential `player_sub` (or `access_token`) reports, not the request's headers, so a `Bearer` header cannot excuse a
cookie. `Credential::Bearer` (native apps) is exempt, as a forged cross-site request cannot carry it. The
routes sit beside, not on top of, a project's `/api/v1/app/*` sign-in routes. Your web UI uses the TypeScript
`FriendsClient`.

## Notifications

Your server sends notifications as your project (client credentials, so `Config::client_secret` must be set):

```rust
use toddstack_accounts_server_sdk::notifications::{Notification, Recipient};

friends.set_time_zone("America/Chicago").await?; // the person's token; from their browser or device, for send_at_local

let mut streak = Notification::new(
    Recipient::User(sub.clone()), // or Recipient::FriendsOf(sub) or Recipient::AllPlayers
    "streak_at_risk",             // 1-40 of a-z 0-9 _, first use creates it; players can mute a type
    "Your streak ends tonight",
);
streak.body = String::from("Play today's daily to keep it.");
streak.url = Some(String::from("https://www.boggledygook.com/daily")); // on a registered redirect or logout origin
streak.send_at_local = Some(String::from("20:00"));                      // on the player's own clock
streak.collapse_key = Some(String::from("streak"));
streak.idempotency_key = Some(String::from("streak-2026-11-15"));        // always set one
streak.sender_sub = Some(sub.clone());                                   // show this person's name and avatar, to friends only
let receipt = accounts.send_notification(&streak).await?; // Receipt { id, status, recipients, counts, result, replayed }

accounts.cancel_notifications("streak", Some(&sub)).await?; // they played: drop the scheduled reminder (user optional)
let later = accounts.notification_status(&receipt.id).await?; // poll a fan-out's counts

// Friends of a batch of your players (1 to 100 `sub`s), with no person's token: a BTreeMap of sub to sorted friend subs.
let friends = accounts.friends_of(&[sub_a, sub_b]).await?;
```

- **Always set `idempotency_key`** (a date or game id). A retry with the same key gets the recorded send back
  (`replayed: true`) and sends nothing; the same key with a different request, or a field the service can't parse, is `Error::Unprocessable` (422).
- There are **no caps and no quiet hours**: every notification is delivered, one per friend who finishes a daily. The
  phone's own Focus handles quiet time. Only a runaway guard (over 100 to one player in an hour) drops the rest, counted
  as `dropped_runaway`.
- `collapse_key` replaces that player's unread item with the same key from your project, and names a scheduled one so
  `cancel_notifications` can remove it. Nothing collapses without a key.
- `sender_sub` (a uuid string) names who the push is from. The service shows that person's name and avatar only to
  their friends, and only when neither side is a review account, is suspended, or has blocked the other; otherwise the
  push goes out plain.
- A send to one `User` is delivered inline: `Status::Done` and `result` (`Delivered`, `Scheduled` or `NotDelivered`). A
  fan-out (`FriendsOf`, `AllPlayers`) answers `Status::Queued`; poll `notification_status` for its `counts`.
  `NotDelivered` folds together muted, left your project and suspended, so a send can't probe players.
- The named `User` or `FriendsOf` player must have signed in to your project: otherwise `Error::NotFound` (404). A rate
  limit (6000 requests an hour, 10 fan-outs an hour) is `Error::RateLimited` (429).

`friends_of` returns the people a `FriendsOf` send would reach: accepted friends who have signed in to your project,
not blocked either way, never review or suspended accounts. It is a read, so it is retried like `notification_status`;
duplicate subs are sent once. Any sub that never signed in to your project fails the whole call with `Error::NotFound`;
0 or over 100 distinct subs is a 400 (`Error::Api`). Don't log the subs.

## Devices (push)

Your server registers a player's phone for push, as your project (client credentials). Native apps never call the
accounts service: the app sends its token to your server (an iOS APNs token as a hex string; an Android FCM
token is already text), and your server registers it for the signed-in player.

```rust
use toddstack_accounts_server_sdk::devices::{Device, Environment, Platform};

accounts.register_device(&Device {
    sub: sub.clone(),                                   // must have signed in to your project
    platform: Platform::Ios,                            // or Platform::Android
    token: token_hex,
    app_id: String::from("me.toddgriffin.boggledygook"), // bundle / package id an admin registered for your project
    environment: Environment::Production,               // or Sandbox (development builds)
}).await?;
accounts.unregister_device(Platform::Ios, &token_hex).await?; // sign-out; fine if it was never registered
```

- One phone, one person: registering a token that belonged to another player moves it. At most 10 devices are kept per
  player. Both calls are idempotent, so a transient failure is retried.
- `Error::Conflict` (409): the token is registered to another project. `Error::NotFound` (404): the player never
  signed in to your project. A malformed token or app id (400) or an unregistered app id (403) is `Error::Api`; a `sub` that isn't a UUID or an unknown platform is
  422 (`Error::Unprocessable`).

## Friend buttons

`relationships` says where the person stands with each person you show, so a button can read the right thing:

```rust
use toddstack_accounts_server_sdk::friends::Relationship;

let found = friends.relationships(&[&alex_sub, &sam_sub]).await?; // any number; sent 100 per call
let label = match friends.relationship(&alex_sub).await? {
    Relationship::Friends => "Friends",
    Relationship::RequestSent => "Requested",       // friends.cancel(sub) takes it back
    Relationship::RequestReceived => "Accept",      // friends.accept(sub)
    Relationship::Blocked => "Unblock",             // the person blocked them
    _ => "Add friend",
};
```

`None` also covers the person themselves, unknown or suspended accounts, people who don't use your app, and
someone who blocked the person, so a block can't be probed. Keep buttons current with the `friend_request.*`
and `friendship.*` webhooks.

## Blocks

A block is silent: nobody is told. Keep the two apart yourself in friends lists, "Playing now" and chat
(not leaderboards) with `blocks()`:

```rust
let blocks = friends.blocks().await?;
// blocks.blocked: people this person blocked, for an Unblock list.
// blocks.hidden: everyone blocked either way; hide them, never say why.
let visible: Vec<_> = playing_now.into_iter().filter(|p| !blocks.hidden.contains(&p.sub)).collect();
```

## Reports

Report a username, picture or behaviour to Todd's review queue. Your server reports for a signed-in player with
`player.report(..)` (their `sub` goes as `reporter_sub`; the deprecated `friends.report` uses their own token); a guest
has no token, so your server reports for them with your client secret:

```rust
use toddstack_accounts_server_sdk::report::{GuestReport, Reason};

player.report(&sub, Reason::Harassment, Some("kept insulting me in chat")).await?;

accounts.report_as_guest(&GuestReport {
    sub,
    reason: Reason::Name,
    note: None,
    guest_id: Some(guest_id),   // your stable guest id: limits and dedupes per guest
    guest_ip: Some(client_ip),  // the verified IP nginx passed on
}).await?;
```

Only accounts can be reported (a guest's id is 404), not yourself (400). Notes are at most `MAX_NOTE_CHARS`
(500). 10 reports per reporter per hour (429). Reporting the same person for the same reason again while
it's open succeeds and changes nothing, so always show the same thanks.

## Retries

This is the canonical retry rule: at most 3 retries (4 attempts), waiting 0.5 s, 1 s and 2 s with +-25% jitter. Reads
from the accounts service (the signing keys, `userinfo`, and every friends `GET`) are retried on an intermittent
failure: a connection error, a timeout, a 5xx, a 408 or a 429. Each attempt has its own deadline (3 s for a read). A
`Retry-After` of up to 5 s is waited out in place of the backoff; a longer one ends the retries. No retry starts after
25 s. A 4xx other than 408 and 429 is an answer and is never retried. After the last retry you get the plain error.

Server API calls (`accounts.player(sub)`, `report_as_player`) retry writes as well as reads, because every one is
idempotent (3 retries, 10 s deadline each for a write, no retry after 55 s).

A notification send is retried too, but only when it carries an `idempotency_key` (same schedule, each attempt with a
10 s deadline): the service replays the recorded send, so a retry cannot notify twice. Without a key it is sent once.
`notification_status` is a read and follows the read rule. A malformed status id is a 400 (`Error::Api`).

Everything else is sent exactly once: the code exchange (a code is single-use), refresh (a refresh that landed but whose
answer was lost has already rotated the token, so a second try would sign the person out), revocation, and every write
(a repeated friend request, block, report or event would be a duplicate).

## Not in this crate

Secrets and session storage stay in your app. HTTP routes do too, except the optional `friends_router` (feature `axum`), which asks your app who the caller is.

## Local mode

Local testing never touches production. `Config::local(client_id, secret, redirect_uri)` points at the accounts
service on your machine (`LOCAL_BASE_URL`, `http://127.0.0.1:8790`); `Config::toddgriffin_at(base_url, ..)` takes any
base. The issuer is `<base>/oidc`, and the JWKS, authorize, token, end-session, API (`/api/v1`) and account links all
derive from it. `http` is accepted only for `127.0.0.1`, `localhost` and `::1`; any other `http` base or issuer is
`Error::Config` (from `toddgriffin_at` and `Accounts::new`).

**Issuer match:** the issuer must exactly match the local server's `ACCOUNTS_BASE_URL`. If you run accounts on `http://localhost:8790` (needed for passkeys), use `toddgriffin_at("http://localhost:8790", ..)`, not the `127.0.0.1` default.

## Added

**0.9.3** (behaviour change, nothing breaks): retries now follow the canonical rule. Was: reads made 3 attempts (2 retries)
after full-jitter backoff from 200 ms capped at 1 s, `Retry-After` cap 2 s, budget 6 s, 408 final, and 429 final on
notification calls. Now: 3 retries (4 attempts), 0.5 s, 1 s, 2 s with +-25% jitter, 408 and 429 retried everywhere,
`Retry-After` cap 5 s, budget 25 s (55 s for keyed sends).

**0.9.2** (additive, nothing breaks): `Accounts::profile_by_sub`, `Accounts::profile_by_username` and `player::PublicProfile`. A blank sub or username is `not_found` locally, with no request.

## Breaking changes

**0.9.0**: added `Accounts::player` (the Server API), `report_as_player`, `revocation` (`Revocations`, the poller,
`Accounts::handle_webhook`) and `EventKind::TokensRevoked`. `verify_access_token` now also refuses a token whose
`fam` is revoked (nothing is revoked until you feed the set). Source-breaking: `AccessClaims` has a new `fam` field and
`EventResult` a new `replayed` field, so code that builds either with a struct literal must add it; `Error` has a new
`Server` variant (`Error` is `non_exhaustive`). New dependency: `jiff` (RFC 3339 times). **Deprecated, removed in
0.10.0:** `Friends` (every method), `Accounts::userinfo` and `FriendsAuth::access_token`, which the router still serves until
you implement the new `FriendsAuth::player_sub` (both trait methods now default to `None`). `Config` no longer prints
the client secret in `Debug` (`"[redacted]"`). `start_revocation_poller` now errors if the first poll fails after
3 quick tries (it used to fail open silently), and `PollerOptions` has a new `startup_backoff` field.

**0.8.0**: `IdClaims` has a new `visibility` field and `Notification` a new `sender_sub` field, so code that builds either with a struct literal must add it (`Notification::new` and token verification are unchanged). Added `Visibility`, `IdClaims::is_review` and `Event::visibility`.

**0.7.0**: the 0.6 `Friends` methods (`accept`, `decline`, `cancel`, `remove`, `block`, `unblock`, `relationships`, `mark_read`, `mute`, `unmute`) now reject an id outside `[A-Za-z0-9_-]{1,64}` with a bad-request error (`400`) before any request is sent, so a decoded `../`, `?` or `#` can't reach another accounts path. Real accounts-service subs always match, so callers passing real subs see no change. Added the `axum` feature and `friends_router`.

**0.6.0**: `Accounts::new` and `Accounts::with_keys` now return `Error::Config` for an `http` issuer that is not
loopback (it was accepted). Added `Config::local`, `Config::toddgriffin_at` and `LOCAL_BASE_URL`.

**0.5.0**: `Error` is now `#[non_exhaustive]` and has a `Conflict` variant, so a 409 from a project call is
`Error::Conflict` (it was `Error::Api { status: 409, .. }`). Add a wildcard arm to exhaustive matches.
