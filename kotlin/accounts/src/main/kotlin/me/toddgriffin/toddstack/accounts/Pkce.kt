package me.toddgriffin.toddstack.accounts

import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64

/** PKCE (RFC 7636, S256) and state helpers. */
public object Pkce {
    private val random: SecureRandom = SecureRandom()
    private val encoder: Base64.Encoder = Base64.getUrlEncoder().withoutPadding()

    /** 32 random bytes, base64url without padding (43 characters): a verifier or a state. */
    public fun randomToken(): String {
        val bytes = ByteArray(32)
        random.nextBytes(bytes)
        return encoder.encodeToString(bytes)
    }

    /** The S256 challenge for [verifier]: base64url(SHA-256(verifier)). */
    public fun challenge(verifier: String): String =
        encoder.encodeToString(MessageDigest.getInstance("SHA-256").digest(verifier.toByteArray(Charsets.US_ASCII)))

    /** Compares in constant time so a mismatch leaks nothing about where it differs. */
    public fun constantTimeEquals(a: String, b: String): Boolean =
        MessageDigest.isEqual(a.toByteArray(Charsets.UTF_8), b.toByteArray(Charsets.UTF_8))
}
