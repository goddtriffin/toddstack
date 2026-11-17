# toddstack-accounts (Kotlin, Android)

Sign-in for native Android apps. The same features as the Rust and TypeScript `toddstack-accounts`, for an app
that signs in **through its project's server and never directly against `accounts.toddgriffin.me`**.

- Coordinates (JitPack, from the repo's bare release tag): `com.github.goddtriffin.toddstack:toddstack-accounts:0.8.0`.
  Maven Central (`me.toddgriffin:toddstack-accounts`) comes later; see below.
- Android library, minSdk 26. Dependencies: coroutines, kotlinx.serialization, androidx.browser (Auth Tab),
  androidx.activity, DataStore. No networking library (it uses `HttpURLConnection`).
- Your app must compile against SDK 36 or later (androidx.browser 1.10 requires it).

## The server contract

Your project's server implements these. The SDK only ever talks to `serverUrl`.

1. `GET {server}/app/sign-in?code_challenge=<base64url SHA-256>&code_challenge_method=S256&state=<random>[&prompt=login]`
   runs the accounts web flow, then redirects to the app's claimed https callback:
   `https://www.boggledygook.com/app/callback?code=<one-time>&state=<same>`, or `?error=<code>&state=<same>`.
2. `POST {server}/api/v1/app/session` with JSON `{"code","code_verifier"}` returns `200
   {"token","expires_at": RFC 3339 or null,"player":{"id","username","picture","guest":bool,"visibility"?}}`, or `400
   {"error":"invalid_grant"}`. When the stored session is a **guest**, the SDK sends its token as
   `Authorization: Bearer <token>`; the server merges that guest into the signed-in player and revokes the
   guest token, and the SDK replaces the stored token with the new one. A signed-in token is never sent here.
3. Authenticated calls carry `Authorization: Bearer <token>`.
4. `POST {server}/api/v1/app/sign-out` (bearer) returns `{"end_session_url": url or null}`. The SDK opens it in
   a browser tab (global sign-out) and deletes the token locally even if the network fails.
5. `POST {server}/api/v1/app/guest` returns the same shape as 2.

## Set up the App Link

The callback must be an https App Link the app claims. In `AndroidManifest.xml`, on the Activity that handles it:

```xml
<activity android:name=".MainActivity" android:exported="true" android:launchMode="singleTop">
    <intent-filter android:autoVerify="true">
        <action android:name="android.intent.action.VIEW" />
        <category android:name="android.intent.category.DEFAULT" />
        <category android:name="android.intent.category.BROWSABLE" />
        <data android:scheme="https" android:host="www.boggledygook.com" android:path="/app/callback" />
    </intent-filter>
</activity>
```

Serve `https://www.boggledygook.com/.well-known/assetlinks.json` with the app's package
(`me.toddgriffin.boggledygook`) and the SHA-256 fingerprint of every signing certificate (debug, release, and
the Play App Signing key). Without it the link opens the browser, not the app. Check it with
`adb shell pm get-app-links me.toddgriffin.boggledygook`.

## Push

Your app never calls the accounts service. Get the FCM token (`FirebaseMessaging`) and send it to your own server
with the player's bearer token; your server registers it with `registerDevice` (`platform: "android"`, your package
id) in its `toddstack-accounts` (Rust or TypeScript) and calls `unregisterDevice` on sign-out. An FCM token is
already text, so unlike iOS there is nothing to encode here.

## Use it

```kotlin
val accounts = Accounts(
    applicationContext,
    AccountsConfig(
        serverUrl = "https://www.boggledygook.com",
        callbackUrl = "https://www.boggledygook.com/app/callback",
        clientId = "boggledygook",
    ),
) // keep one per process

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        accounts.attach(this) // registers the Auth Tab launcher: call it in onCreate
        lifecycleScope.launch {
            accounts.currentSession() // load once at start-up; then observe accounts.session
            intent?.data?.let { accounts.handleCallback(it.toString()) } // Custom Tab fallback (App Link)
        }
    }
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        intent.data?.let { uri -> lifecycleScope.launch { runCatching { accounts.handleCallback(uri.toString()) } } }
    }
}

val session = accounts.signIn()            // suspends until the callback arrives; Prompt.Login forces a fresh sign-in
val guest = accounts.startGuest()          // zero clicks to start
accounts.signOut()                         // opens the end-session page, clears the token even offline
accounts.accountUrl("/account")            // https://accounts.toddgriffin.me/account?client=boggledygook
```

Auth Tab is used when the person's browser supports it (the https redirect returns through the result callback);
otherwise a Custom Tab is used and the App Link reaches your Activity, which calls `handleCallback`. The PKCE
verifier and state are stored, so a callback that arrives after the process was recreated still completes (via
`handleCallback`).

Observe `accounts.session` (a `StateFlow<Session?>`) from Compose. Call `currentSession()` once at start.

### Calling your server

Add the token to requests with an OkHttp interceptor. The header is returned only for https requests to your
server's scheme, host and port, so the token cannot leak to another host:

```kotlin
Interceptor { chain ->
    val request = chain.request()
    val header = accounts.cachedAuthorizationHeader(request.url.toString())
    chain.proceed(if (header == null) request else request.newBuilder().header("Authorization", header).build())
}
```

`authorizationHeader(url)` is the suspending version that reads storage.

## In-app friend requests

`Friends` calls the friends routes your server mounts (the Rust crate's `friends_router`; table in
[its README](../../rust/toddstack_accounts/README.md)), authenticated with the session `Accounts` keeps. The app never
sees an accounts token. With no session, or a guest, a call throws `FriendsException` with code `account_required`
(offer `signIn()`); other refusals carry the server's `code` and a `message` already worded for the person.

```kotlin
val friends = Friends(accounts)                 // real network; Friends(accounts, MockFriendsApi()) in tests
val requests: FriendRequests = friends.requests() // .incoming, .outgoing: List<Person>
try {
    val sent: Sent = friends.requestByUsername("alex") // Requested, Accepted, AlreadyFriends
} catch (e: FriendsException) {
    show(e.message)   // e.code is e.g. "rate_limited", "refused"
}
friends.accept(sub); friends.decline(sub); friends.cancel(sub)
friends.remove(sub); friends.block(sub); friends.unblock(sub)
val people: List<Person> = friends.list()
val blocks: Blocks = friends.blocks()
val states: Map<String, Relationship> = friends.relationships(subs)
```

Also `requestByCode`. Everything is `suspend`. `FriendsApi` is the one-method transport (`UrlConnectionFriendsApi` is the
real one); `MockFriendsApi` in the test fixtures seeds replies per exact request.

### Errors

Everything throws `AccountsException`; switch on `kind`: `StateMismatch`, `Authorization` (see `error`),
`MissingCode`, `InvalidGrant`, `Api`, `Http`, `Config`, `UnexpectedCallback`, `Cancelled`, `Storage` (the token store
failed: this is never treated as signed out). A state mismatch leaves the real sign-in pending.

A new `signIn` supersedes an open one: the old call fails with `Cancelled`. With the Custom Tab fallback nothing
tells the SDK the person closed the tab, so just let them tap sign-in again. (Closing an Auth Tab is reported, and
cancels the sign-in.) The SDK does not cancel on activity resume, because the App Link callback and the resume race.

### Local mode

Local testing never touches production: every base URL is yours to set. `serverUrl` and `accountsBaseUrl` accept
`http` on loopback (`127.0.0.1`, `localhost`, `::1`) and on `10.0.2.2` (the emulator's alias for your machine), and
nowhere else (`AccountsException` of kind `Config`). `AccountsConfig.local(serverUrl, callbackUrl, clientId)` also points
`accountsBaseUrl` at `LOCAL_ACCOUNTS_BASE_URL` (`http://127.0.0.1:8790`). The bearer token follows the configured
server's scheme and origin. **Issuer match:** `accountsBaseUrl` must exactly match the local server's `ACCOUNTS_BASE_URL`; if accounts runs on `http://localhost:8790` (needed for passkeys), pass that as `accountsBaseUrl`, not the `127.0.0.1` default. `10.0.2.2` is the emulator's alias for your machine; on a physical device it is a real LAN address, so use it only from the emulator. `callbackUrl` is an https App Link, or (any mode, local debug builds) a reverse-DNS custom scheme such as
`me.toddgriffin.boggledygook.debug:/app/callback` (RFC 8252; the Auth Tab is launched with that redirect scheme, and a Custom Tab fallback needs an intent filter for it in your debug manifest); plain http is rejected.

```kotlin
AccountsConfig.local("http://10.0.2.2:8992", "https://www.boggledygook.com/app/callback", "boggledygook")
```

The library manifest does not allow cleartext. The app adds a **debug-only** `network_security_config`
(`app/src/debug/res/xml/network_security_config.xml`, referenced from `app/src/debug/AndroidManifest.xml` with
`android:networkSecurityConfig`) that permits cleartext to `10.0.2.2`, `127.0.0.1` and `localhost`:

```xml
<network-security-config>
  <domain-config cleartextTrafficPermitted="true">
    <domain includeSubdomains="false">10.0.2.2</domain>
    <domain includeSubdomains="false">127.0.0.1</domain>
    <domain includeSubdomains="false">localhost</domain>
  </domain-config>
</network-security-config>
```

### Storage and testing

The library manifest declares `INTERNET`. Exclude the token file from Auto Backup: a restored file without its
Keystore key cannot be decrypted (the SDK then drops it and the next sign-in repairs it, but exclude it anyway).
Add to the app's `res/xml/backup_rules.xml` (`android:fullBackupContent`) and `data_extraction_rules.xml`
(inside `<cloud-backup>` and `<device-transfer>`):

```xml
<exclude domain="file" path="datastore/toddstack_accounts.preferences_pb" />
```

(`toddstack_accounts` is the default `KeystoreTokenStore` name.)


Tokens live in DataStore, each value encrypted with an AES-GCM key in the Android Keystore
(`KeystoreTokenStore`). `Session.toString()` never prints the token. In unit tests use `InMemoryTokenStore` and
`MockAccountsApi` from the `testFixtures` (`testImplementation(testFixtures("com.github.goddtriffin.toddstack:toddstack-accounts:0.8.0"))`):
seed a response per exact arguments (`seedExchangeSession(code, verifier, bearer, Result.success(session))`),
inspect `calls`, and end every test with `assertAllSeedsConsumed()`. An unseeded call throws.

## Install (JitPack)

[JitPack](https://jitpack.io) builds public GitHub repos from tags, with no account or key. Add its repository and
pin the exact version (a release tag; never a branch or `-SNAPSHOT`):

```kotlin
// settings.gradle.kts
dependencyResolutionManagement { repositories { google(); mavenCentral(); maven("https://jitpack.io") } }

// app/build.gradle.kts
implementation("com.github.goddtriffin.toddstack:toddstack-accounts:0.8.0")
```

The group is `com.github.goddtriffin.toddstack` (JitPack's group for a module of a multi-module repo), the artifact is
`toddstack-accounts`, and the version is the tag. Your app must use Kotlin 2.4 or later (the library's metadata is 2.4).
The first request for a tag makes JitPack build it, which takes a few minutes; check
`https://jitpack.io/com/github/goddtriffin/toddstack/toddstack-accounts/0.8.0/` or the build log at
<https://jitpack.io/#goddtriffin/toddstack>.

### Local build

From a checkout of this repo, for work in progress: `cd kotlin && ./gradlew :accounts:publishToMavenLocal`, then in
the app `repositories { mavenLocal() }` and `implementation("me.toddgriffin:toddstack-accounts:0.8.0")` (the local
group is `me.toddgriffin`; only JitPack's build uses the `com.github...` group). Run it again after any change.

## Releasing

The Kotlin version is the repo's **bare release tag**, shared with SwiftPM: one tag `X.Y.Z` serves both. To release:

1. Pick the next version. Set `version` in `kotlin/accounts/build.gradle.kts` and `toddstackAccountsVersion` in
   `swift/accounts/Sources/ToddstackAccounts/Version.swift` to it (and bump the Rust and TypeScript
   `toddstack-accounts` too when they changed: they keep their own `toddstack-accounts-vX.Y.Z` tags).
2. `make lint test`, merge to `main`.
3. Tag the merge commit and push the tag: `git tag X.Y.Z && git push origin X.Y.Z`. SwiftPM sees it at once.
4. Warm JitPack: open <https://jitpack.io/#goddtriffin/toddstack>, choose the tag, press Get it, and wait for a green
   build (or `curl -s https://jitpack.io/com/github/goddtriffin/toddstack/toddstack-accounts/X.Y.Z/toddstack-accounts-X.Y.Z.pom`).
   `jitpack.yml` at the repo root runs `publishToMavenLocal` in `kotlin/` on JDK 17.

## Publishing to Maven Central (later, not done)

`maven-publish` and `signing` are configured with sources and javadoc jars and full POM metadata, and no
credentials. Todd must:

1. Create a Sonatype Central account at <https://central.sonatype.com> and register the namespace `me.toddgriffin`.
2. Verify the namespace by adding the TXT record Central shows to the DNS of `toddgriffin.me` (Cloudflare; ask
   Todd per the infra rules, and record it in infra `docs/dns.md`).
3. Generate a GPG key, publish its public key to a keyserver (`keys.openpgp.org`), and store the private key and
   passphrase in 1Password.
4. Generate a Central user token and keep it in 1Password.
5. Publish from a Central-supported plugin or the Central Portal upload (Central no longer accepts the legacy OSSRH
   staging flow): `./gradlew :accounts:publishToMavenLocal` produces the signed artifacts when
   `ORG_GRADLE_PROJECT_signingInMemoryKey` and `ORG_GRADLE_PROJECT_signingInMemoryKeyPassword` are set; upload the
   resulting bundle in the portal. Wiring a publishing plugin with the token is the remaining step.

## Breaking changes

**0.8.0**: no breaking changes. Added `Visibility` (`NORMAL`, `REVIEW`), `Player.visibility` (default `NORMAL`, also for an absent or newer value) and `Player.isReview`. `visibility` is `"review"` for a review account (the accounts service's `toddgriffin_visibility` claim, passed on by your server). Notifications and webhooks are server-side, so `sender_sub` and the `user.updated` visibility live in the Rust and TypeScript SDKs only.

**0.7.0**: no breaking changes. Added `Friends`, `FriendsApi` and the friends models.

**0.6.0**: `accountsBaseUrl` is now validated: it rejects `http` off loopback (it was unchecked). `serverUrl` now
rejects an https URL with no host. Both accept `http` on loopback (`127.0.0.1`, `localhost`, `::1`, `10.0.2.2`).
`callbackUrl` also accepts a reverse-DNS custom scheme.

## Develop

`make lint test` from the repo root also covers `kotlin/` (`./gradlew lint test`), or `cd kotlin && ./gradlew build test` (lint fails on warnings). JDK 17 or later; needs the Android SDK with platform 36.
