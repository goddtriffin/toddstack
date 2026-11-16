# toddstack-accounts (TypeScript)

[![JSR](https://jsr.io/badges/@todd/toddstack-accounts)](https://jsr.io/@todd/toddstack-accounts)
[![npm](https://img.shields.io/npm/v/toddstack-accounts)](https://www.npmjs.com/package/toddstack-accounts)

Client for Todd Griffin's Projects accounts (`accounts.toddgriffin.me`): what a project's backend needs to sign people
in, check tokens, verify webhooks, and use friends. The TypeScript half of `toddstack-accounts`, with the same features
as the [Rust crate](../../rust/toddstack_accounts/README.md). Part of
[toddstack](https://github.com/goddtriffin/toddstack).

Web Platform APIs only (`fetch`, WebCrypto): Node 20+, Deno, Bun. No dependencies.

```sh
npx jsr add @todd/toddstack-accounts   # or: deno add jsr:@todd/toddstack-accounts
npm install toddstack-accounts
```

Web apps use the **backend-for-frontend** pattern: your server is the OIDC client and keeps the tokens; the browser only
ever holds your own session cookie. Native apps sign in through the system browser as public clients and send their
access token to your server, which checks it with `accounts.verifyAccessToken`.

## Sign in (two routes)

```ts
import { Accounts, toddgriffin } from "toddstack-accounts"; // or "@todd/toddstack-accounts"

const accounts = new Accounts(toddgriffin(
  "boggledygook",
  process.env.ACCOUNTS_CLIENT_SECRET!, // from your env (1Password)
  "https://www.boggledygook.com/auth/callback", // registered exactly
));

// GET /auth/login?return=/play
const { url, pending } = await accounts.startLogin({
  scopes: ["profile", "offline_access", "friends"],
  returnTo: "/play",
});
// Keep `pending` server-side or in an encrypted HttpOnly cookie, then redirect to `url`.

// GET /auth/callback?code=..&state=..&iss=..
const signedIn = await accounts.finishLogin(pending, new URL(request.url).search);
// signedIn.claims.sub is the person's permanent id: key everything on it.
// Keep signedIn.tokens (refreshToken rotates: always store the new one).
// Redirect to signedIn.returnTo (already checked to be a path on your site).
```

`finishLogin` checks `state` and `iss`, exchanges the code with PKCE, and verifies the ID token (ES256 signature against
the published keys, issuer, audience, expiry, nonce).

Signing keys rotate on a schedule, and a new key is published two days before it signs. The client keeps the key set for
an hour, and a token whose `kid` isn't in it triggers a refetch (at most once a minute, shared by concurrent checks), so
rotation never rejects a valid token.

| Call                                        | What                                                                                                                                                                 |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `startLogin({ prompt, loginHint, maxAge })` | `prompt: "login"` forces a fresh sign-in, `"none"` checks silently, `"create"` opens sign-up                                                                         |
| `accounts.refresh(token)`                   | New tokens; an `AccountsError` with `kind: "token_refused"` means sign in again                                                                                      |
| `accounts.revoke(token)`                    | Revokes a refresh token (and its family); `signOut` does this for you                                                                                                |
| `accounts.verifyAccessToken(token)`         | For your API: checks `typ`, issuer, audience = your client id, expiry                                                                                                |
| `accounts.verifyIdToken(token, nonce?)`     | ID token checks on their own                                                                                                                                         |
| `accounts.userinfo(token)`                  | The profile JSON                                                                                                                                                     |
| `safeReturnTo(path)`                        | Keeps redirects on your site                                                                                                                                         |
| `accountUrl(config, "/account")`            | A link to the accounts site tagged `?client=<your id>`: use it for every link there, so the page shows "Back to <your project>" and Todd sees where people came from |

Errors are `AccountsError` with a `kind`: `state_mismatch`, `authorization` (the person cancelled, or `login_required`
after `prompt: "none"`), `missing_code`, `token_refused`, `invalid_token`, `invalid_webhook`, `api` (with `status`),
`http`, `config`.

## Sign out (one route)

```ts
// GET /logout: end your own session first, then:
const { url, revokeError } = await accounts.signOut(refreshToken, {
  idTokenHint: idToken, // optional, expired is fine
  postLogoutRedirectUri: "/", // a path, or an absolute URL on your redirect URI's origin
  state: undefined, // echoed back as ?state=
});
if (revokeError) console.error(revokeError); // sign-out still goes ahead
// Redirect to `url` to also sign out of accounts.
```

`signOut` revokes the refresh token (pass `null` if you don't keep one) and builds the end-session URL (`endSessionUrl`,
with your `client_id`). Redirecting there signs the person out of **every** project in that browser, after the accounts
service asks "Sign out?". To sign out of your app alone, use `accounts.revoke` and skip the redirect. The post-logout
URL must be registered for your app exactly (ask Todd); an unregistered or off-site one is dropped (`postLogoutTarget`
returns null) and the person stays on the accounts "You're signed out" page.

## Webhooks

```ts
import { verifyWebhook, WEBHOOK_HEADER } from "toddstack-accounts";

const event = await verifyWebhook(webhookSecret, request.headers.get(WEBHOOK_HEADER), await request.text());
switch (event.type) {
  case "user.deleted": /* delete or anonymize event.data.sub's rows */
    break;
  case "user.updated": /* refresh cached username / picture */
    break;
  case "user.signed_out":
  case "user.suspended":
  case "grant.revoked": /* end event.data.sub's sessions */
    break;
  case "friendship.created": /* event.data.sub and event.data.friend_sub are now friends */
    break;
  case "friendship.removed": /* drop friend-only state between them */
    break;
  case "friend_request.created":
  case "friend_request.cancelled":
  case "friend_request.declined": /* event.data.from_sub asked event.data.to_sub, took it back, or was turned down */
    break;
}
```

`event.type` is a `WebhookEventType` (the list is `WEBHOOK_EVENT_TYPES`) or a newer type, which still verifies;
`isKnownWebhookEventType` tells them apart. Friendship and friend request events are sent once per side: each person
gets one with themselves as `sub`, so handle only `sub`'s view. An accepted request sends only `friendship.created`.

Your sessions are your own: accounts can't end them. On `user.signed_out`, `user.suspended` and `grant.revoked`, delete
every session you hold for `event.data.sub`. Their refresh tokens are already revoked.

Verify the raw body before parsing. Signatures older than 5 minutes are refused. Deliveries retry, so ignore repeated
`event.id`s.

## Friends and events

```ts
const friends = accounts.friends(accessToken);
const list = await friends.list(); // only friends who also use your app
await friends.requestByUsername("alex");
await friends.postEvent({
  kind: "beat_your_score",
  title: "Alex beat your high score",
  body: "4,200 points in today's daily",
  collapseKey: "daily-2026-11-14",
}); // needs scope friends:events
```

Also `requests`, `requestByCode`, `accept`, `decline`, `cancel`, `remove`, `block`, `unblock`, `inbox(limit)`,
`markRead`, `mutes`, `mute(kind)` (`ALL_KINDS` mutes everything), `unmute`.

### Friend buttons

`relationships` says where the person stands with each person you show, so a button can read the right thing:

```ts
const found = await friends.relationships([alexSub, samSub]); // any number; sent 100 per call
const label = {
  friends: "Friends",
  request_sent: "Requested", // friends.cancel(sub) takes it back
  request_received: "Accept", // friends.accept(sub)
  blocked: "Unblock", // the person blocked them
  none: "Add friend",
}[await friends.relationship(alexSub)];
```

`none` also covers the person themselves, unknown or suspended accounts, people who don't use your app, and someone who
blocked the person, so a block can't be probed. Keep buttons current with the `friend_request.*` and `friendship.*`
webhooks.

### Blocks

A block is silent: nobody is told. Keep the two apart yourself in friends lists, "Playing now" and chat (not
leaderboards) with `blocks()`:

```ts
const { blocked, hidden } = await friends.blocks();
// blocked: people this person blocked, for an Unblock list.
// hidden: everyone blocked either way; hide them, never say why.
const visible = playingNow.filter((p) => !hidden.includes(p.sub));
```

### Reports

Report a username, picture or behaviour to Todd's review queue. A signed-in person reports with their own token (no
particular scope); a guest has no token, so your server reports for them with your client secret:

```ts
await friends.report(sub, "harassment", "kept insulting me in chat");

await accounts.reportAsGuest({
  sub,
  reason: "name",
  guestId, // your stable guest id: limits and dedupes per guest
  guestIp, // the verified IP nginx passed on
});
```

Only accounts can be reported (a guest's id is 404), not yourself (400). Notes are at most `MAX_NOTE_CHARS` (500). 10
reports per reporter per hour (429). Reporting the same person for the same reason again while it's open succeeds and
changes nothing, so always show the same thanks.

## Retries

Reads from the accounts service (the signing keys, `userinfo`, and every friends `GET`) are retried on an intermittent
failure: a connection error, a timeout, a 5xx or a 429. Three attempts at most, each with its own 3 s deadline, paced by
exponential backoff with full jitter (200 ms, doubling, capped at 1 s). A `Retry-After` of up to 2 s is waited out; a
longer one ends the retries. No retry starts after 6 s. A 4xx is an answer and is never retried.

Everything else is sent exactly once: the code exchange (a code is single-use), refresh (a refresh that landed but whose
answer was lost has already rotated the token, so a second try would sign the person out), revocation, and every write
(a repeated friend request, block, report or event would be a duplicate).

## Tests

`new Accounts(config, { keys, fetch })` pins the signing keys and swaps `fetch`, so tests run with no network.

## Publishing

`deno.jsonc` (JSR) and `package.json` (npm) carry the same version as `version.ts`; a test checks. The npm package is
the `tsc` build of the same sources (`tsconfig.npm.json`, output in `dist/`, not committed).
