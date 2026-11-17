package me.toddgriffin.toddstack.accounts

import java.time.Instant
import java.time.OffsetDateTime
import java.time.format.DateTimeParseException
import kotlinx.serialization.SerialName
import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder

/**
 * How the service treats an account. Review accounts (the App Store and Play reviewers' logins) are hidden from other
 * people: they never appear to friends and get no friend-facing sender details on notifications.
 */
@Serializable(with = VisibilitySerializer::class)
public enum class Visibility(public val wire: String) {
    /** An ordinary account. The default, and what an absent value means. */
    NORMAL("normal"),

    /** A review account. */
    REVIEW("review"),
    ;

    public companion object {
        /** Reads the wire value; anything but `"review"`, an absent value included, is [NORMAL]. */
        public fun fromWire(value: String?): Visibility = if (value == "review") REVIEW else NORMAL
    }
}

/** Reads `"review"` as [Visibility.REVIEW] and anything else, a newer value included, as [Visibility.NORMAL]. */
internal object VisibilitySerializer : KSerializer<Visibility> {
    override val descriptor: SerialDescriptor = PrimitiveSerialDescriptor("Visibility", PrimitiveKind.STRING)

    override fun deserialize(decoder: Decoder): Visibility = Visibility.fromWire(decoder.decodeString())

    override fun serialize(encoder: Encoder, value: Visibility) {
        encoder.encodeString(value.wire)
    }
}

/**
 * Who the session belongs to. [visibility] is [Visibility.REVIEW] for a review account (the accounts service's
 * `toddgriffin_visibility: "review"` claim, passed on by your project server as `visibility`); it is
 * [Visibility.NORMAL] otherwise, including when your server omits it.
 */
@Serializable
public data class Player(
    val id: String,
    val username: String,
    val picture: String? = null,
    val guest: Boolean,
    val visibility: Visibility = Visibility.NORMAL,
) {
    /** Whether this is a review account. */
    public val isReview: Boolean get() = visibility == Visibility.REVIEW
}

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
