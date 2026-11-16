# toddstack-accounts

Client for Todd Griffin's Projects accounts (`accounts.toddgriffin.me`): what a project's backend needs to sign
people in, check tokens, verify webhooks, and use friends. Part of
[toddstack](https://github.com/goddtriffin/toddstack).

Web apps use the **backend-for-frontend** pattern: your Rust backend is the OIDC client and keeps the tokens;
the browser only ever holds your own session cookie. Native apps sign in through the system browser
(`ASWebAuthenticationSession`, Custom Tabs) as public clients and send their access token to your backend,
which checks it with [`Accounts::verify_access_token`].

## Sign in (two routes)

```rust
use toddstack_accounts::{Accounts, Config, login};

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
use toddstack_accounts::logout;

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
- Register a post-logout redirect URI for your client, exactly (Todd runs
  `accounts-service admin client update ... --logout <URI>` or uses the admin UI). The service skips its "Sign out?" confirm
  page only when the ID token hint is valid and the redirect URI is registered (or absent). Otherwise the person sees
  the confirm page.
- Handle `session.signed_out` and `user.signed_out` (see Webhooks).

## Webhooks

```rust
use toddstack_accounts::webhook::{self, EventKind};

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

## Friends and events

```rust
use toddstack_accounts::friends::{Event, Friends};

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

## Notifications

Your server sends notifications as your project (client credentials, so `Config::client_secret` must be set):

```rust
use toddstack_accounts::notifications::{Notification, Recipient};

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
let receipt = accounts.send_notification(&streak).await?; // Receipt { id, status, recipients, counts, result, replayed }

accounts.cancel_notifications("streak", Some(&sub)).await?; // they played: drop the scheduled reminder (user optional)
let later = accounts.notification_status(&receipt.id).await?; // poll a fan-out's counts
```

- **Always set `idempotency_key`** (a date or game id). A retry with the same key gets the recorded send back
  (`replayed: true`) and sends nothing; the same key with a different request, or a field the service can't parse, is `Error::Unprocessable` (422).
- There are **no caps and no quiet hours**: every notification is delivered, one per friend who finishes a daily. The
  phone's own Focus handles quiet time. Only a runaway guard (over 100 to one player in an hour) drops the rest, counted
  as `dropped_runaway`.
- `collapse_key` replaces that player's unread item with the same key from your project, and names a scheduled one so
  `cancel_notifications` can remove it. Nothing collapses without a key.
- A send to one `User` is delivered inline: `Status::Done` and `result` (`Delivered`, `Scheduled` or `NotDelivered`). A
  fan-out (`FriendsOf`, `AllPlayers`) answers `Status::Queued`; poll `notification_status` for its `counts`.
  `NotDelivered` folds together muted, left your project and suspended, so a send can't probe players.
- The named `User` or `FriendsOf` player must have signed in to your project: otherwise `Error::NotFound` (404). A rate
  limit (6000 requests an hour, 10 fan-outs an hour) is `Error::RateLimited` (429).

## Devices (push)

Your server registers a player's phone for push, as your project (client credentials). Native apps never call the
accounts service: the app sends its token to your server (Swift's `apnsTokenHex` makes the hex string; an Android FCM
token is already text), and your server registers it for the signed-in player.

```rust
use toddstack_accounts::devices::{Device, Environment, Platform};

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
use toddstack_accounts::friends::Relationship;

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

Report a username, picture or behaviour to Todd's review queue. A signed-in person reports with their own
token (no particular scope); a guest has no token, so your server reports for them with your client secret:

```rust
use toddstack_accounts::report::{GuestReport, Reason};

friends.report(&sub, Reason::Harassment, Some("kept insulting me in chat")).await?;

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

Reads from the accounts service (the signing keys, `userinfo`, and every friends `GET`) are retried on an intermittent
failure: a connection error, a timeout, a 5xx or a 429. Three attempts at most, each with its own 3 s deadline, paced by
exponential backoff with full jitter (200 ms, doubling, capped at 1 s). A `Retry-After` of up to 2 s is waited out; a
longer one ends the retries. No retry starts after 6 s. A 4xx is an answer and is never retried.

A notification send is retried too, but only when it carries an `idempotency_key` (two more attempts, each with a 10 s
deadline, no retry after 15 s): the service replays the recorded send, so a retry cannot notify twice. Without a key it
is sent once. A 429 is never retried here (nor on `notification_status`): the service's limits are hourly and send no `Retry-After`, so another try would fail too. A malformed status id is a 400 (`Error::Api`).

Everything else is sent exactly once: the code exchange (a code is single-use), refresh (a refresh that landed but whose
answer was lost has already rotated the token, so a second try would sign the person out), revocation, and every write
(a repeated friend request, block, report or event would be a duplicate).

## Not in this crate

Secrets, session storage and HTTP routes stay in your app, so the crate works with any framework.

## Breaking changes

**0.5.0**: `Error` is now `#[non_exhaustive]` and has a `Conflict` variant, so a 409 from a project call is
`Error::Conflict` (it was `Error::Api { status: 409, .. }`). Add a wildcard arm to exhaustive matches.
