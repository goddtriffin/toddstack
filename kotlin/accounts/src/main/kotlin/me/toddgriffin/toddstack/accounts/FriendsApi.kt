package me.toddgriffin.toddstack.accounts

/** A reply from your server: the HTTP status and the raw body. */
public data class FriendsReply(val status: Int, val body: String)

/**
 * The one call [Friends] makes to your project's server. [UrlConnectionFriendsApi] is the real one;
 * `MockFriendsApi` (in the test fixtures) is the mock. A 4xx or 5xx is a normal reply; throw
 * [AccountsException] (`Http`) only when the request itself failed.
 */
public interface FriendsApi {
    /** [path] is under the server (`/api/v1/friends/...`); [authorization] is the full `Bearer <token>` header value. */
    public suspend fun send(method: String, path: String, body: String?, authorization: String): FriendsReply
}
