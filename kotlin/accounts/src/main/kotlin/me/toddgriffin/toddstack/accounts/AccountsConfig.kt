package me.toddgriffin.toddstack.accounts

/** The accounts site every project shares. */
public const val DEFAULT_ACCOUNTS_BASE_URL: String = "https://accounts.toddgriffin.me"

/**
 * Settings for one app.
 *
 * @property serverUrl your project's server (`https://www.boggledygook.com`); the app only ever talks to it.
 * @property callbackUrl the https App Link the server redirects to after sign-in
 *   (`https://www.boggledygook.com/app/callback`).
 * @property accountsBaseUrl the accounts site, used only for links such as [Accounts.accountUrl].
 * @property clientId your project's client id; tags every link to the accounts site.
 */
public data class AccountsConfig(
    val serverUrl: String,
    val callbackUrl: String,
    val clientId: String,
    val accountsBaseUrl: String = DEFAULT_ACCOUNTS_BASE_URL,
) {
    init {
        if (!serverUrl.startsWith("https://")) {
            throw AccountsException(AccountsException.Kind.Config, "serverUrl must be https, got $serverUrl")
        }
        if (!callbackUrl.startsWith("https://")) {
            throw AccountsException(
                AccountsException.Kind.Config,
                "callbackUrl must be an https App Link, got $callbackUrl",
            )
        }
        if (clientId.isBlank()) {
            throw AccountsException(AccountsException.Kind.Config, "clientId must not be blank")
        }
    }

    internal val server: String get() = serverUrl.trimEnd('/')
}
