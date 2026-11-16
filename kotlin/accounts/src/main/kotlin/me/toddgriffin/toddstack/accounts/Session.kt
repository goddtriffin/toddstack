package me.toddgriffin.toddstack.accounts

import java.time.Instant
import java.time.OffsetDateTime
import java.time.format.DateTimeParseException
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** Who the session belongs to. */
@Serializable
public data class Player(
    val id: String,
    val username: String,
    val picture: String? = null,
    val guest: Boolean,
)

/** A bearer token and its player. [expiresAt] is RFC 3339, or null when it does not expire. */
@Serializable
public data class Session(
    val token: String,
    @SerialName("expires_at") val expiresAt: String? = null,
    val player: Player,
) {
    /** Never prints the token. */
    override fun toString(): String = "Session(token=<redacted>, expiresAt=$expiresAt, player=$player)"

    /** True once [expiresAt] has passed at [now]; an unreadable value counts as expired. */
    public fun isExpired(now: Instant): Boolean {
        val raw: String = expiresAt ?: return false
        return try {
            !OffsetDateTime.parse(raw).toInstant().isAfter(now)
        } catch (_: DateTimeParseException) {
            true
        }
    }
}

/** The reply to `POST /api/v1/app/sign-out`. */
@Serializable
public data class SignOutResponse(
    @SerialName("end_session_url") val endSessionUrl: String? = null,
)
