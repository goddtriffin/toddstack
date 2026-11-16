package me.toddgriffin.toddstack.accounts

import java.net.URLEncoder
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

/** A friend, request or blocked person as your app sees them. [since] is RFC 3339. */
@Serializable
public data class Person(
    val sub: String,
    val username: String,
    val name: String? = null,
    val picture: String? = null,
    val since: String,
)

/** Pending friend requests: [incoming] were sent to the person, [outgoing] by them. */
@Serializable
public data class FriendRequests(val incoming: List<Person>, val outgoing: List<Person>)

/**
 * The person's blocks. [blocked] is for an Unblock list; [hidden] is everyone blocked either way (hide them, but never
 * show it as a block: blocks are silent).
 */
@Serializable
public data class Blocks(val blocked: List<Person>, val hidden: List<String>)

/** What sending a request did. */
@Serializable
public enum class Sent {
    @SerialName("requested") Requested,

    /** They had already asked: you're friends now. */
    @SerialName("accepted") Accepted,

    @SerialName("already_friends") AlreadyFriends,
}

/** Where the person stands with someone: what a friend button should say. */
public enum class Relationship(internal val wire: String) {
    Friends("friends"),

    /** "Requested": take it back with [me.toddgriffin.toddstack.accounts.Friends.cancel]. */
    RequestSent("request_sent"),

    /** "Accept". */
    RequestReceived("request_received"),

    /** "Unblock". */
    Blocked("blocked"),

    /** "Add friend"; also the answer for unknown accounts and someone who blocked the person. */
    None("none"),

    /** A state newer than this SDK. */
    Unknown("unknown"),
    ;

    internal companion object {
        fun fromWire(value: String): Relationship = entries.firstOrNull { it.wire == value } ?: Unknown
    }
}

/**
 * A friends call your server refused. [code] is the server's error code: `account_required` (a guest: offer
 * sign-in), `session_expired`, `cross_site`, `missing_scope`, `bad_request`, `refused` (not found, blocked, a
 * privacy setting or a limit), `not_found`, `conflict`, `unprocessable`, `rate_limited`, `accounts_unavailable`. [message] is meant for the person.
 */
public class FriendsException(
    public val status: Int,
    public val code: String,
    message: String,
) : Exception(message)

/**
 * In-app friend requests: calls the friends routes your project's server mounts (the Rust crate's `friends_router`),
 * authenticated with the session [accounts] keeps. The app never sees an accounts token.
 *
 * With no session the client itself throws [FriendsException] `account_required`; a guest is sent and the server answers the same code.
 */
public class Friends(private val accounts: Accounts, private val api: FriendsApi) {
    /** Real network calls to [accounts]'s server. */
    public constructor(accounts: Accounts) : this(accounts, UrlConnectionFriendsApi(accounts.config))

    /** Friends who also use your app. */
    public suspend fun list(): List<Person> =
        read(ListBody.serializer(), call("GET", "")).friends

    /** Pending requests, incoming and outgoing. */
    public suspend fun requests(): FriendRequests = read(FriendRequests.serializer(), call("GET", "/requests"))

    /** Sends a request by exact username. [Sent.Accepted] means they had already asked. */
    public suspend fun requestByUsername(username: String): Sent = send(buildBody("username", username))

    /** Sends a request by friend code (works even when the person only takes requests with their code). */
    public suspend fun requestByCode(friendCode: String): Sent = send(buildBody("friend_code", friendCode))

    /** Accepts a request the person received. */
    public suspend fun accept(sub: String) { call("POST", "/requests/${segment(sub)}/accept") }

    /** Declines a request the person received. */
    public suspend fun decline(sub: String) { call("POST", "/requests/${segment(sub)}/decline") }

    /** Takes back a request the person sent. */
    public suspend fun cancel(sub: String) { call("DELETE", "/requests/${segment(sub)}") }

    /** Ends a friendship. */
    public suspend fun remove(sub: String) { call("DELETE", "/${segment(sub)}") }

    /** Blocks: ends the friendship and requests, both ways. */
    public suspend fun block(sub: String) { call("POST", "/blocks", buildBody("sub", segment(sub))) }

    /** Undoes [block]. */
    public suspend fun unblock(sub: String) { call("DELETE", "/blocks/${segment(sub)}") }

    /** Who the person blocked, and everyone to keep apart from them. */
    public suspend fun blocks(): Blocks = read(Blocks.serializer(), call("GET", "/blocks"))

    /** Where the person stands with each of [subs] (1 to 100), keyed by sub: what a friend button should say. */
    public suspend fun relationships(subs: List<String>): Map<String, Relationship> =
        read(RelationshipsBody.serializer(), call("GET", "/relationships?subs=${URLEncoder.encode(subs.joinToString(",") { segment(it) }, "UTF-8")}"))
            .relationships.mapValues { Relationship.fromWire(it.value) }

    @Serializable
    private data class ListBody(val friends: List<Person>)

    @Serializable
    private data class SentBody(val result: Sent)

    @Serializable
    private data class RelationshipsBody(val relationships: Map<String, String>)

    @Serializable
    private data class ErrorBody(val error: String? = null, val message: String? = null)

    private suspend fun send(body: String): Sent = read(SentBody.serializer(), call("POST", "/requests", body)).result

    private suspend fun call(method: String, path: String, body: String? = null): String {
        val full = "$BASE$path"
        val authorization: String = accounts.authorizationHeader(accounts.config.server + full)
            ?: throw FriendsException(401, "account_required", "Sign in to use friends.")
        val reply: FriendsReply = api.send(method, full, body, authorization)
        if (reply.status !in 200..299) throw refusal(reply)
        return reply.body
    }

    private fun refusal(reply: FriendsReply): FriendsException {
        val parsed: ErrorBody? = try {
            json.decodeFromString(ErrorBody.serializer(), reply.body)
        } catch (_: SerializationException) {
            null
        }
        return FriendsException(
            reply.status,
            parsed?.error ?: "accounts_unavailable",
            parsed?.message ?: "Friends request failed (${reply.status}).",
        )
    }

    private fun <T> read(serializer: kotlinx.serialization.KSerializer<T>, text: String): T =
        try {
            json.decodeFromString(serializer, text)
        } catch (e: SerializationException) {
            throw AccountsException(AccountsException.Kind.Http, "unreadable friends reply", cause = e)
        }

    private fun buildBody(name: String, value: String): String =
        buildJsonObject { put(name, value) }.toString()

    /**
     * 1 to 64 of `A-Za-z0-9_-`: a real `sub` is a UUID, and anything else (`.`, `/`, `?`, `#`, `%`) could redirect
     * the call to another path on the server.
     */
    private fun segment(value: String): String {
        if (!ID.matches(value)) throw FriendsException(400, "bad_request", "That isn't a valid person id.")
        return value
    }

    private companion object {
        const val BASE: String = "/api/v1/friends"
        val ID: Regex = Regex("^[A-Za-z0-9_-]{1,64}$")
        val json: Json = Json { ignoreUnknownKeys = true }
    }
}
