package me.toddgriffin.toddstack.accounts

import java.util.Base64
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PkceTest {
    private val verifier: String = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"

    @Test
    fun the_rfc_7636_appendix_b_verifier_gives_the_published_challenge() {
        assertEquals("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", Pkce.challenge(verifier))
    }

    @Test
    fun the_rfc_7636_appendix_b_octets_encode_to_the_published_verifier() {
        val octets: IntArray = intArrayOf(
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186,
            22, 212, 37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        )
        val bytes: ByteArray = ByteArray(octets.size) { octets[it].toByte() }
        assertEquals(verifier, Base64.getUrlEncoder().withoutPadding().encodeToString(bytes))
    }

    @Test
    fun a_random_token_is_43_url_safe_characters_and_never_repeats() {
        val first: String = Pkce.randomToken()
        assertEquals(43, first.length)
        assertTrue(first.all { it.isLetterOrDigit() || it == '-' || it == '_' })
        assertNotEquals(first, Pkce.randomToken())
    }

    @Test
    fun constant_time_equality_matches_equal_strings_only() {
        assertTrue(Pkce.constantTimeEquals("abc", "abc"))
        assertFalse(Pkce.constantTimeEquals("abc", "abd"))
        assertFalse(Pkce.constantTimeEquals("abc", "abcd"))
    }
}
