package me.toddgriffin.toddstack.accounts

/**
 * The calls to your project's server. [UrlConnectionAccountsApi] is the real one; `MockAccountsApi` (in the
 * test fixtures) is the mock. Failures are thrown as [AccountsException].
 */
public interface AccountsApi {
    /** `POST /api/v1/app/session`; [bearer] is the stored (guest) token, which the server merges and revokes. */
    public suspend fun exchangeSession(code: String, codeVerifier: String, bearer: String?): Session

    /** `POST /api/v1/app/guest`. */
    public suspend fun startGuest(): Session

    /** `POST /api/v1/app/sign-out` with [bearer]. */
    public suspend fun signOut(bearer: String): SignOutResponse
}
