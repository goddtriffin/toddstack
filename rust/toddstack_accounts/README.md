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

## Sign out (one route)

```rust
use toddstack_accounts::logout;

// GET /logout: end your own session first, then:
let signed_out = accounts.sign_out(refresh_token.as_deref(), &logout::Options {
    id_token_hint: Some(&id_token),               // optional, expired is fine
    post_logout_redirect_uri: Some("/"),          // a path, or an absolute URL on your redirect URI's origin
    state: None,                                  // echoed back as ?state=
}).await;
if let Some(error) = signed_out.revoke_error { /* log it; sign-out still goes ahead */ }
// Redirect to signed_out.url to also sign out of accounts.
```

`sign_out` revokes the refresh token (pass `None` if you don't keep one) and builds the end-session URL
(`logout::end_session_url`, with your `client_id`). Redirecting there signs the person out of **every** project in that
browser, after the accounts service asks "Sign out?". To sign out of your app alone, use `accounts.revoke` and skip
the redirect. The post-logout URL must be registered for your app exactly (ask Todd); an unregistered or off-site one
is dropped and the person stays on the accounts "You're signed out" page.

## Webhooks

```rust
use toddstack_accounts::webhook::{self, EventKind};

let event = webhook::verify(webhook_secret, signature_header, &raw_body, now_unix)?;
match event.kind {
    EventKind::UserDeleted => { /* delete or anonymize event.sub()'s rows */ }
    EventKind::UserUpdated => { /* refresh cached username / picture */ }
    EventKind::UserSignedOut | EventKind::UserSuspended | EventKind::GrantRevoked => { /* end event.sub()'s sessions */ }
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

Verify the raw body before parsing. Signatures older than 5 minutes are refused. Deliveries retry, so ignore
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

A block overrides everything (no requests, no events either way). Events go to friends' inboxes (`inbox()`),
and will also go out as push notifications once the native apps exist.

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

## Not in this crate

Secrets, session storage and HTTP routes stay in your app, so the crate works with any framework.
