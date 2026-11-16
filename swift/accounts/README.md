# ToddstackAccounts (Swift)

Sign-in for Todd Griffin's native apps (iOS and macOS, SwiftUI or UIKit/AppKit). The Swift twin of `toddstack-accounts`
([Rust](../../rust/toddstack_accounts/README.md), [TypeScript](../../typescript/accounts/README.md)) for apps. Part of
[toddstack](https://github.com/goddtriffin/toddstack).

A native app **never talks to `accounts.toddgriffin.me`**. It signs in through its own project's server, which runs the
accounts web flow for it. Swift 6, iOS 17.4+ / macOS 14.4+, no third-party dependencies.

## Install

Swift Package Manager, from the repository URL (the manifest is `Package.swift` at the repo root):

```swift
.package(url: "https://github.com/goddtriffin/toddstack", from: "0.7.0")  // see Releases
// target dependency:
.product(name: "ToddstackAccounts", package: "toddstack")
```

Tests of your app can also depend on the product `ToddstackAccountsTesting` (mocks and an in-memory store; never link it
into the shipping app target).

**Parity:** this package is the app-side half. Backend features of the Rust and TypeScript packages (token verification,
webhooks, notifications, reports) are called through your project's server, not from the app; a Swift client for
those is a gap until a project needs it.

## What your app needs

- **Associated Domains** entitlement with `applinks:<callback host>`, for example `applinks:www.boggledygook.com`.
  The callback is an https Universal Link, so the host must serve an `apple-app-site-association` file that claims the
  callback path for your app. (`ASWebAuthenticationSession.Callback.https` needs iOS 17.4 / macOS 14.4.)
- Nothing in Info.plist; no custom URL scheme.
- Keychain: the session is stored as a generic password (`AfterFirstUnlockThisDeviceOnly`: not synced, not restored to
  another device). On macOS, sandboxed apps need no extra entitlement; iOS needs none.

## The server contract

Your project's server implements this; the SDK speaks only this.

1. `GET {server}/app/sign-in?code_challenge=<base64url SHA-256>&code_challenge_method=S256&state=<random>[&prompt=login]`: the server runs its
   accounts web flow, then redirects to the app's claimed https callback with `?code=<one-time>&state=<same>`, or
   `?error=<code>&state=<same>`.
2. `POST {server}/api/v1/app/session` with JSON `{"code","code_verifier"}` answers `200 {"token","expires_at","player"}`
   (`expires_at` is RFC 3339 or `null`; `player` is `{"id","username","picture","guest"}`) or `400 {"error":"invalid_grant"}`.
   If the stored session is a **guest** (`player.guest` true), the SDK sends its token as `Authorization: Bearer <token>`:
   the server merges that guest into the signed-in player and revokes the guest token, and the SDK replaces the stored
   token with the new one. A signed-in account's token is never sent here (switching accounts).
3. Authenticated calls carry `Authorization: Bearer <token>`.
4. `POST {server}/api/v1/app/sign-out` (bearer) answers `{"end_session_url": <https url or null>}`. The SDK deletes
   the token, then opens `end_session_url` in the web-auth session (global sign-out of the browser's accounts session).
   The end-session page is expected to finish on the app's callback URL; closing the sheet is fine.
5. `POST {server}/api/v1/app/guest` answers like 2 (a guest session).

## Use

```swift
import ToddstackAccounts

let config = try AccountsConfig(
    serverURL: URL(string: "https://www.boggledygook.com")!,
    callbackURL: URL(string: "https://www.boggledygook.com/app/callback")!,
    clientId: "boggledygook"
)
let accounts = AccountsClient(config: config)

// Launch: reuse the stored session, else start as a guest (zero clicks to start). A Keychain read error
// throws instead of reading as "signed out", so it never replaces an account with a guest.
let session: Session
if let stored = try accounts.currentSession() { session = stored } else { session = try await accounts.startGuest() }

// "Sign in": silent when the browser already has an accounts session. Merges the guest.
do { try await accounts.signIn() } catch AccountsError.cancelled { /* person closed the sheet */ }
try await accounts.signIn(prompt: .login)   // force a fresh sign-in (switch account)

// Calls to your server
var request = URLRequest(url: URL(string: "https://www.boggledygook.com/api/v1/me")!)
request = try accounts.authorized(request)  // adds Bearer; throws .notSignedIn, or .foreignHost for any host but serverURL

// Links to the accounts site carry ?client=<clientId>
let manage: String = accounts.accountURL("/account")

try await accounts.signOut()   // deletes the token even if the network fails
```

## API

| item | what it does |
|---|---|
| `AccountsConfig(serverURL:callbackURL:accountsBaseURL:clientId:)` | validates that server and callback are https; `accountsBaseURL` defaults to `https://accounts.toddgriffin.me` |
| `AccountsClient(config:transport:webAuthenticator:store:random:now:)` | everything below; defaults are `URLSession`, `ASWebAuthenticationSession`, Keychain |
| `signIn(prompt:)` | `.none` (default) or `.login`; PKCE S256, constant-time `state` check, code exchange, stores the session |
| `startGuest()` | guest session |
| `currentSession()` | stored session or nil; an expired one is deleted; a Keychain read error throws |
| `authorized(_:)` | `URLRequest` with the Bearer token, only for the server's scheme, host and port |
| `signOut()` | server sign-out, local delete, global sign-out sheet; throws only on a Keychain failure |
| `accountURL(_:)` | accounts-site link tagged `?client=` (a query is kept, a fragment stays last) |
| `apnsTokenHex(_:)` | the lowercase hex of an APNs device token's `Data`, the form your server registers (see below) |
| `AccountsError` | `config`, `stateMismatch`, `foreignHost`, `signInInProgress`, `authorization(code:)`, `missingCode`, `invalidGrant`, `cancelled`, `notSignedIn`, `api(status:message:)`, `network`, `invalidResponse`, `storage` |
| `SessionStore` | `KeychainSessionStore` (production), `InMemorySessionStore` (tests) |
| `AccountsTransport`, `WebAuthenticator` | the network and the browser sheet; swap them for mocks |

The sign-in session is **not ephemeral**: the browser's accounts cookie is what makes sign-in silent. A guest cannot
customize anything; gate locked options and offer `signIn()`.

## In-app friend requests

`FriendsClient` calls the friends routes your server mounts (the Rust crate's `friends_router`; table in
[its README](../../rust/toddstack_accounts/README.md)), authenticated with the session `AccountsClient` keeps. The app
never sees an accounts token. With no session, or as a guest, a call throws `FriendsError` with code `account_required` (offer `signIn()`).

```swift
let friends = FriendsClient(accounts: accounts)
let requests: FriendRequests = try await friends.requests()   // .incoming, .outgoing: [Person]
do {
    let sent: Sent = try await friends.request(username: "alex") // .requested, .accepted, .alreadyFriends
} catch let error as FriendsError {
    show(error.message)   // already worded for the person; error.code is e.g. "rate_limited", "refused"
}
try await friends.accept(sub); try await friends.decline(sub); try await friends.cancel(sub)
try await friends.remove(sub); try await friends.block(sub); try await friends.unblock(sub)
let people: [Person] = try await friends.list()
let blocks: Blocks = try await friends.blocks()
let states: [String: Relationship] = try await friends.relationships(subs)
```

Also `request(friendCode:)`. Models: `Person` (`Identifiable`), `FriendRequests`, `Blocks`, `Sent`, `Relationship`
(`.unknown` for a state newer than the SDK). Tests seed `MockAccountsTransport` with the request (see `FriendsTests`).

## Push

Your app never calls the accounts service. Ask for notification permission, then send the token to your own server in
`application(_:didRegisterForRemoteNotificationsWithDeviceToken:)`, with the build's environment (`sandbox` for
development builds, `production` for TestFlight and the App Store):

```swift
let token: String = apnsTokenHex(deviceToken)   // POST this to your server with accounts.authorized(_:)
```

Your server then calls `registerDevice` (and `unregisterDevice` on sign-out) in its own `toddstack-accounts`
(Rust or TypeScript); registering the token for another player moves it.

## Testing your app

`ToddstackAccountsTesting` has `MockAccountsTransport` and `MockWebAuthenticator`. Seed responses by argument value
(`seedSend(_:response:)`, `seedAuthenticate(url:callbackURL:response:)`); an unseeded call traps, and each test that seeds
anything ends with `verifyExhausted()`. Inject `random:` to make `state` and the PKCE verifier deterministic.

## Local mode

Local testing never touches production: every base URL is yours to set. `serverURL` and `accountsBaseURL` accept
`http` on loopback (`127.0.0.1`, `localhost`, `::1`) and nowhere else (`AccountsError.config`);
`AccountsConfig.local(serverURL:callbackURL:clientId:)` also points `accountsBaseURL` at
`AccountsConfig.localAccountsBaseURL` (`http://127.0.0.1:8790`). The sign-out URL your server returns follows the same
rule. **Issuer match:** `accountsBaseURL` must exactly match the local server's `ACCOUNTS_BASE_URL`; if accounts runs on `http://localhost:8790` (needed for passkeys), pass that as `accountsBaseURL`, not the `127.0.0.1` default. `callbackURL` is an https Universal Link, or (any mode, local debug builds) a reverse-DNS custom scheme such as
`me.toddgriffin.boggledygook.debug:/app/callback` (RFC 8252; the sheet then uses `ASWebAuthenticationSession.Callback.customScheme`); plain http is rejected.

```swift
let config = try AccountsConfig.local(
    serverURL: URL(string: "http://127.0.0.1:8992")!, callbackURL: callback, clientId: "boggledygook")
```

The library does not touch your app's settings. For cleartext to loopback, the app adds to its Info.plist (for debug
builds):

```xml
<key>NSAppTransportSecurity</key>
<dict><key>NSAllowsLocalNetworking</key><true/></dict>
```

## Develop

From the repo root: `make swift_test` (macOS; uses Xcode's toolchain for swift-testing) and
`DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcodebuild -scheme ToddstackAccounts -destination 'generic/platform=iOS' build`.

## Breaking changes

**0.7.0**: no breaking changes. Added `FriendsClient` and the friends models.

**0.6.0**: `serverURL` and `accountsBaseURL` accept `http` on loopback, with a new config error message for other
URLs. The sign-out URL your server returns may now be loopback `http` (it was https only). `callbackURL` also accepts
a reverse-DNS custom scheme.

## Releases

SwiftPM reads a repo's **plain semver tags** (`1.2.3` or `v1.2.3`) as the package's versions and ignores everything
else. The other packages here use prefixed tags (`webserver-base-vX.Y.Z`, `toddstack-accounts-vX.Y.Z`), which SwiftPM
ignores, so they do not collide. The old `v0.1.0` ... `v0.3.0` tags (from before the rename) do: SwiftPM reads them
as versions 0.1.0 ... 0.3.0 of this package, and they have no `Package.swift`.

Scheme: **bare `X.Y.Z` tags** (no `v`) serve SwiftPM **and** JitPack (the Kotlin SDK, see
[`kotlin/accounts/README.md`](../../kotlin/accounts/README.md)), so the Swift and Kotlin versions are always equal
to the tag. The first was **`0.4.0`** (above the old tags, so it sorts after them). Set `toddstackAccountsVersion`
to the tag you are about to cut; the Rust and TypeScript packages keep their own `toddstack-accounts-vX.Y.Z` tags.
