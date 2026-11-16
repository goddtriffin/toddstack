package me.toddgriffin.toddstack.accounts

/** The accounts site every project shares. */
public const val DEFAULT_ACCOUNTS_BASE_URL: String = "https://accounts.toddgriffin.me"

/** The accounts site running locally. Local testing never touches production: see [AccountsConfig.local]. */
public const val LOCAL_ACCOUNTS_BASE_URL: String = "http://127.0.0.1:8790"

private val LOOPBACK_HOSTS: Set<String> = setOf("127.0.0.1", "localhost", "[::1]", "::1", "10.0.2.2")

/**
 * Whether [url] is https with a host, or http on a loopback host: `127.0.0.1`, `localhost`, `::1`, or `10.0.2.2`
 * (the Android emulator's alias for the development machine; on a physical device it is an ordinary LAN address, so
 * use it only from the emulator). Nothing else may go over the wire unencrypted.
 */
internal fun isAllowedBaseUrl(url: String): Boolean {
    val uri: java.net.URI = try { java.net.URI(url) } catch (_: java.net.URISyntaxException) { return false }
    val host: String = uri.host?.lowercase() ?: return false
    return when (uri.scheme?.lowercase()) {
        "https" -> true
        "http" -> host in LOOPBACK_HOSTS
        else -> false
    }
}

/**
 * Whether [url] is an https URL with a host, or a reverse-DNS private-use scheme (RFC 8252: the scheme contains a dot,
 * e.g. `me.toddgriffin.boggledygook.debug:/app/callback`). Plain http is never a callback.
 */
internal fun isAllowedCallbackUrl(url: String): Boolean {
    val uri: java.net.URI = try { java.net.URI(url) } catch (_: java.net.URISyntaxException) { return false }
    val scheme: String = uri.scheme?.lowercase() ?: return false
    return if (scheme == "https") !uri.host.isNullOrEmpty() else scheme != "http" && '.' in scheme
}

/**
 * Settings for one app.
 *
 * @property serverUrl your project's server (`https://www.boggledygook.com`); the app only ever talks to it. For
 *   local testing, an http URL on loopback (`http://10.0.2.2:8992` from the emulator): see [local].
 * @property callbackUrl where the server redirects after sign-in: an https App Link
 *   (`https://www.boggledygook.com/app/callback`), or a reverse-DNS custom scheme for local debug builds
 *   (`me.toddgriffin.boggledygook.debug:/app/callback`).
 * @property accountsBaseUrl the accounts site, used only for links such as [Accounts.accountUrl]. https, or http on
 *   loopback (`127.0.0.1`, `localhost`, `::1`, `10.0.2.2`).
 * @property clientId your project's client id; tags every link to the accounts site.
 */
public data class AccountsConfig(
    val serverUrl: String,
    val callbackUrl: String,
    val clientId: String,
    val accountsBaseUrl: String = DEFAULT_ACCOUNTS_BASE_URL,
) {
    init {
        if (!isAllowedBaseUrl(serverUrl)) {
            throw AccountsException(
                AccountsException.Kind.Config,
                "serverUrl must be https (http only on loopback), got $serverUrl",
            )
        }
        if (!isAllowedBaseUrl(accountsBaseUrl)) {
            throw AccountsException(
                AccountsException.Kind.Config,
                "accountsBaseUrl must be https (http only on loopback), got $accountsBaseUrl",
            )
        }
        if (!isAllowedCallbackUrl(callbackUrl)) {
            throw AccountsException(
                AccountsException.Kind.Config,
                "callbackUrl must be an https App Link or a reverse-DNS custom scheme, got $callbackUrl",
            )
        }
        if (clientId.isBlank()) {
            throw AccountsException(AccountsException.Kind.Config, "clientId must not be blank")
        }
    }

    public companion object {
        /**
         * A config for local testing: your project's server on your machine (`http://10.0.2.2:8992` from the
         * emulator) and the accounts site at [LOCAL_ACCOUNTS_BASE_URL]; [callbackUrl] is typically a debug custom scheme.
         */
        public fun local(serverUrl: String, callbackUrl: String, clientId: String): AccountsConfig =
            AccountsConfig(serverUrl, callbackUrl, clientId, LOCAL_ACCOUNTS_BASE_URL)
    }

    internal val server: String get() = serverUrl.trimEnd('/')
}
