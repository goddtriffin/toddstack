package me.toddgriffin.toddstack.accounts

import java.net.URI
import java.net.URISyntaxException
import java.net.URLDecoder
import java.net.URLEncoder
import java.time.Instant
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/** Opens the browser. The Android implementation is [AndroidBrowserLauncher]. */
public interface BrowserLauncher {
    /** Opens the sign-in page; the person comes back through [Accounts.handleCallback]. */
    public fun launchSignIn(url: String)

    /** Opens [url] in a plain browser tab (the global sign-out page). */
    public fun open(url: String)
}

/** `prompt=login` makes the server ask for a fresh sign-in even when a browser session exists. */
public enum class Prompt(internal val value: String) { Login("login") }

/**
 * Sign-in for a native app through your project's server. See the README for the server contract.
 *
 * Build one per process and share it. Call [currentSession] once at start-up.
 */
public class Accounts(
    public val config: AccountsConfig,
    private val api: AccountsApi,
    private val store: TokenStore,
    private val clock: () -> Instant = { Instant.now() },
) {
    /** Set by [attach]; tests set it directly. */
    public var launcher: BrowserLauncher? = null

    private val mutex: Mutex = Mutex()
    private val _session: MutableStateFlow<Session?> = MutableStateFlow(null)
    private var waiter: CompletableDeferred<Session>? = null

    /** The session as last loaded or changed; observe it from Compose. Null until [currentSession] has run. */
    public val session: StateFlow<Session?> get() = _session

    /** The stored session, or null when there is none or it has expired (an expired one is deleted). */
    public suspend fun currentSession(): Session? = mutex.withLock { load() }

    /**
     * `Bearer <token>` for the cached session, for an OkHttp interceptor; null when signed out or when [url]
     * is not on your server (same scheme, host and port as `serverUrl`), so the token never goes elsewhere.
     */
    public fun cachedAuthorizationHeader(url: String): String? =
        if (isServerUrl(url)) _session.value?.takeUnless { it.isExpired(clock()) }?.let { "Bearer ${it.token}" } else null

    /** Like [cachedAuthorizationHeader] but reads the stored session. */
    public suspend fun authorizationHeader(url: String): String? =
        if (isServerUrl(url)) currentSession()?.let { "Bearer ${it.token}" } else null

    private fun isServerUrl(url: String): Boolean {
        val target: URI = try { URI(url) } catch (_: URISyntaxException) { return false }
        val own: URI = URI(config.server)
        fun port(u: URI): Int = if (u.port >= 0) u.port else if (u.scheme.equals("http", ignoreCase = true)) 80 else 443
        // the scheme must match the configured server's, and the config only allows https or loopback http
        return target.scheme.equals(own.scheme, ignoreCase = true) &&
            target.host != null && target.host.equals(own.host, ignoreCase = true) &&
            port(target) == port(own)
    }

    /**
     * Signs in through the browser and returns once the callback has been handled. Survives process
     * recreation: the verifier and state are stored, so a callback that arrives after a restart still
     * completes the exchange (the new process's `handleCallback` call does it; this call is gone with the old one).
     */
    public suspend fun signIn(prompt: Prompt? = null): Session {
        val browser: BrowserLauncher = launcher
            ?: throw AccountsException(
                AccountsException.Kind.Config,
                "no browser launcher: call attach(activity) before signIn",
            )
        val pending = Pending(verifier = Pkce.randomToken(), state = Pkce.randomToken())
        val mine = CompletableDeferred<Session>()
        mutex.withLock {
            // A new sign-in supersedes an open one (the person may have closed the browser tab without a callback).
            waiter?.completeExceptionally(
                AccountsException(AccountsException.Kind.Cancelled, "superseded by a newer sign-in"),
            )
            storage { store.put(PENDING_KEY, Json.encodeToString(Pending.serializer(), pending)) }
            waiter = mine
        }
        browser.launchSignIn(signInUrl(pending, prompt))
        try {
            return mine.await()
        } finally {
            mutex.withLock { if (waiter === mine) waiter = null }
        }
    }

    /** The server URL that starts the web flow for [pending]. */
    internal fun signInUrl(pending: Pending, prompt: Prompt?): String {
        val query = StringBuilder()
            .append("code_challenge=").append(encode(Pkce.challenge(pending.verifier)))
            .append("&code_challenge_method=S256")
            .append("&state=").append(encode(pending.state))
        if (prompt != null) query.append("&prompt=").append(prompt.value)
        return "${config.server}/app/sign-in?$query"
    }

    /**
     * Finishes a sign-in from the callback [uri] (`https://.../app/callback?code=..&state=..`). Call it from
     * the Activity that owns the App Link, and from the Auth Tab result. A callback whose state does not match
     * is rejected and leaves the real sign-in pending.
     */
    public suspend fun handleCallback(uri: String): Session = mutex.withLock {
        val query: Map<String, String> = parseCallback(uri)
        val raw: String = storage { store.get(PENDING_KEY) }
            ?: throw AccountsException(AccountsException.Kind.UnexpectedCallback, "no sign-in in progress")
        val pending: Pending = try {
            Json.decodeFromString(Pending.serializer(), raw)
        } catch (e: kotlinx.serialization.SerializationException) {
            throw AccountsException(AccountsException.Kind.Storage, "stored sign-in state is corrupt", cause = e)
        }
        val state: String? = query["state"]
        if (state == null || !Pkce.constantTimeEquals(state, pending.state)) {
            throw AccountsException(AccountsException.Kind.StateMismatch, "callback state does not match")
        }
        storage { store.remove(PENDING_KEY) }
        try {
            query["error"]?.let {
                throw AccountsException(AccountsException.Kind.Authorization, "sign-in failed: $it", error = it)
            }
            val code: String = query["code"]
                ?: throw AccountsException(AccountsException.Kind.MissingCode, "callback had no code")
            // Only a guest token is sent: the server merges that guest into the player and revokes the token.
            val bearer: String? = try {
                load()?.takeIf { it.player.guest }?.token
            } catch (_: AccountsException) {
                // Unreadable session (e.g. restored from backup without its Keystore key): drop it so sign-in repairs it.
                storage { store.remove(SESSION_KEY) }
                _session.value = null
                null
            }
            val fresh: Session = api.exchangeSession(code, pending.verifier, bearer)
            save(fresh)
            waiter?.complete(fresh)
            fresh
        } catch (e: AccountsException) {
            waiter?.completeExceptionally(e)
            throw e
        }
    }

    /** Ends a browser sign-in the person abandoned (they closed the tab). */
    public suspend fun cancelSignIn() {
        mutex.withLock {
            storage { store.remove(PENDING_KEY) }
            waiter?.completeExceptionally(AccountsException(AccountsException.Kind.Cancelled, "sign-in cancelled"))
        }
    }

    /** Creates a guest session (guests customize nothing; locked options upsell sign-up). */
    public suspend fun startGuest(): Session = mutex.withLock {
        val fresh: Session = api.startGuest()
        save(fresh)
        fresh
    }

    /**
     * Signs out everywhere: asks the server, opens the returned end-session page in the browser, and deletes
     * the token locally even when the network fails. Returns true when the end-session page was opened.
     */
    public suspend fun signOut(): Boolean {
        // Read and delete under one lock so a sign-in finishing in between is not wiped without being signed out.
        val token: String? = mutex.withLock {
            val read: String? = try {
                load()?.token
            } catch (_: AccountsException) {
                null // unreadable storage: still deleted below
            }
            storage { store.remove(SESSION_KEY) }
            _session.value = null
            read
        }
        var endSession: String? = null
        try {
            if (token != null) endSession = api.signOut(token).endSessionUrl
        } catch (_: AccountsException) {
            // The token is already deleted: signing out must work offline.
        }
        val url: String = endSession ?: return false
        val browser: BrowserLauncher = launcher ?: return false
        browser.open(url)
        return true
    }

    /**
     * [path] on the accounts site tagged with your client: `accountUrl("/account")` is
     * `https://accounts.toddgriffin.me/account?client=<clientId>`. Every link to the accounts site carries it,
     * so its pages show "Back to <your project>". A query already in [path] is kept; a fragment stays last.
     */
    public fun accountUrl(path: String = "/account"): String {
        val base: String = config.accountsBaseUrl.trimEnd('/')
        val hash: Int = path.indexOf('#')
        val fragment: String = if (hash >= 0) path.substring(hash) else ""
        val bare: String = if (hash >= 0) path.substring(0, hash) else path
        val slash: String = if (bare.startsWith("/")) "" else "/"
        val join: String = if (bare.contains('?')) "&" else "?"
        return "$base$slash$bare${join}client=${encode(config.clientId)}$fragment"
    }

    private suspend fun load(): Session? {
        val raw: String = storage { store.get(SESSION_KEY) } ?: run {
            _session.value = null
            return null
        }
        val stored: Session = try {
            Json.decodeFromString(Session.serializer(), raw)
        } catch (e: kotlinx.serialization.SerializationException) {
            throw AccountsException(AccountsException.Kind.Storage, "stored session is corrupt", cause = e)
        }
        if (stored.isExpired(clock())) {
            storage { store.remove(SESSION_KEY) }
            _session.value = null
            return null
        }
        _session.value = stored
        return stored
    }

    private suspend fun save(fresh: Session) {
        storage { store.put(SESSION_KEY, Json.encodeToString(Session.serializer(), fresh)) }
        _session.value = fresh
    }

    /** A storage failure is an error, never "signed out". */
    private suspend fun <T> storage(block: suspend () -> T): T =
        try {
            block()
        } catch (e: AccountsException) {
            throw e
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            throw AccountsException(AccountsException.Kind.Storage, "token storage failed: ${e.message}", cause = e)
        }

    private fun parseCallback(uri: String): Map<String, String> {
        val parsed: URI = try {
            URI(uri)
        } catch (e: URISyntaxException) {
            throw AccountsException(AccountsException.Kind.UnexpectedCallback, "unreadable callback", cause = e)
        }
        val expected: String = config.callbackUrl.substringBefore('?').trimEnd('/')
        val actual: String = uri.substringBefore('?').substringBefore('#').trimEnd('/')
        if (parsed.scheme != "https" || !actual.equals(expected, ignoreCase = true)) {
            throw AccountsException(AccountsException.Kind.UnexpectedCallback, "not the callback URL: $actual")
        }
        val query: String = parsed.rawQuery ?: return emptyMap()
        return query.split('&').filter { it.isNotEmpty() }.associate {
            val name: String = it.substringBefore('=')
            val value: String = it.substringAfter('=', "")
            URLDecoder.decode(name, "UTF-8") to URLDecoder.decode(value, "UTF-8")
        }
    }

    private fun encode(value: String): String = URLEncoder.encode(value, "UTF-8").replace("+", "%20")

    /** What to remember between opening the browser and the callback. */
    @Serializable
    internal data class Pending(val verifier: String, val state: String)

    private companion object {
        const val SESSION_KEY: String = "session"
        const val PENDING_KEY: String = "pending"
    }
}
