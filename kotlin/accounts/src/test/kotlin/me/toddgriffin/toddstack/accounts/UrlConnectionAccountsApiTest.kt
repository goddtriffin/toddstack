package me.toddgriffin.toddstack.accounts

import com.sun.net.httpserver.HttpServer
import java.net.InetSocketAddress
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

class UrlConnectionAccountsApiTest {
    private val server: HttpServer = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
    private val seen: MutableList<Triple<String, String?, String>> = mutableListOf()
    private val api: UrlConnectionAccountsApi by lazy {
        server.start()
        UrlConnectionAccountsApi("http://127.0.0.1:${server.address.port}", 5_000)
    }

    @After
    fun stop() = server.stop(0)

    private fun reply(path: String, status: Int, body: String, location: String? = null) {
        server.createContext(path) { ex ->
            seen.add(Triple(path, ex.requestHeaders.getFirst("Authorization"), ex.requestBody.readBytes().decodeToString()))
            if (location != null) ex.responseHeaders.add("Location", location)
            val bytes: ByteArray = body.toByteArray()
            ex.sendResponseHeaders(status, if (bytes.isEmpty()) -1 else bytes.size.toLong())
            if (bytes.isNotEmpty()) ex.responseBody.use { it.write(bytes) }
            ex.close()
        }
    }

    private val sessionJson: String =
        """{"token":"t","expires_at":null,"player":{"id":"p","username":"u","picture":null,"guest":true},"extra":1}"""

    private suspend fun playerWith(player: String): Player {
        reply("/api/v1/app/session", 200, """{"token":"t","player":$player}""")
        return api.exchangeSession("c", "v", null).player
    }

    @Test
    fun a_review_player_reads_as_review() = runBlocking {
        val player: Player = playerWith("""{"id":"p","username":"u","guest":false,"visibility":"review"}""")
        assertEquals(Visibility.REVIEW, player.visibility)
        assertEquals(true, player.isReview)
    }

    @Test
    fun a_player_without_visibility_or_with_a_newer_value_is_normal() = runBlocking {
        val absent: Player = playerWith("""{"id":"p","username":"u","guest":false}""")
        assertEquals(Visibility.NORMAL, absent.visibility)
        assertEquals(false, absent.isReview)
        assertEquals(Visibility.NORMAL, Visibility.fromWire("newer"))
        assertEquals(Visibility.NORMAL, Visibility.fromWire(null))
    }

    @Test
    fun the_exchange_posts_the_code_and_verifier_with_the_bearer_and_reads_the_session() = runBlocking {
        reply("/api/v1/app/session", 200, sessionJson)
        val session: Session = api.exchangeSession("c", "v", "guest-token")
        assertEquals(Session("t", null, Player("p", "u", null, true)), session)
        assertEquals(
            listOf(Triple("/api/v1/app/session", "Bearer guest-token", """{"code":"c","code_verifier":"v"}""")),
            seen,
        )
    }

    @Test
    fun no_authorization_header_is_sent_without_a_bearer() = runBlocking {
        reply("/api/v1/app/guest", 200, sessionJson)
        api.startGuest()
        assertNull(seen.single().second)
    }

    @Test
    fun a_400_invalid_grant_is_an_invalid_grant_error() {
        reply("/api/v1/app/session", 400, """{"error":"invalid_grant"}""")
        val e = assertThrows(AccountsException::class.java) { runBlocking { api.exchangeSession("c", "v", null) } }
        assertEquals(AccountsException.Kind.InvalidGrant, e.kind)
        assertEquals(400, e.status)
    }

    @Test
    fun a_redirect_is_not_followed_so_the_bearer_never_reaches_another_origin() {
        reply("/api/v1/app/sign-out", 302, "", location = "http://127.0.0.1:1/elsewhere")
        val e = assertThrows(AccountsException::class.java) { runBlocking { api.signOut("t") } }
        assertEquals(AccountsException.Kind.Api, e.kind)
        assertEquals(302, e.status)
    }

    @Test
    fun an_unreadable_reply_is_an_http_error() {
        reply("/api/v1/app/guest", 200, "<html>")
        val e = assertThrows(AccountsException::class.java) { runBlocking { api.startGuest() } }
        assertEquals(AccountsException.Kind.Http, e.kind)
    }
}
