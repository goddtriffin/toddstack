package me.toddgriffin.toddstack.accounts

/** Everything that can go wrong; switch on [kind]. */
public class AccountsException(
    public val kind: Kind,
    message: String,
    public val error: String? = null,
    public val status: Int? = null,
    cause: Throwable? = null,
) : Exception(message, cause) {

    /** What went wrong. */
    public enum class Kind {
        /** The callback's `state` does not match the pending sign-in: forged or stale. Start over. */
        StateMismatch,

        /** The server sent the person back with an error (`access_denied`, ...); see [error]. */
        Authorization,

        /** The callback had no `code`. */
        MissingCode,

        /** The code exchange was refused (`invalid_grant`: expired or reused). Sign in again. */
        InvalidGrant,

        /** The server refused a call; [status] and [error] are set when known. */
        Api,

        /** The request itself failed (network, timeout, unreadable reply). */
        Http,

        /** Bad configuration. */
        Config,

        /** A callback arrived with no sign-in in progress, or for a different URL. */
        UnexpectedCallback,

        /** The person closed the browser before finishing. */
        Cancelled,

        /** The call needs a signed-in or guest session and there is none. */
        NotSignedIn,

        /** Another sign-in is already open; finish or [Accounts.cancelSignIn] it first. */
        SignInInProgress,

        /** The token store could not be read or written (corrupt value, lost key, disk). Not the same as signed out. */
        Storage,
    }
}
