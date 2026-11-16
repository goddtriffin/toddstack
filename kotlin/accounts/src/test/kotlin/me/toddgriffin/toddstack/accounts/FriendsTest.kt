package me.toddgriffin.toddstack.accounts

import java.time.Instant
import kotlinx.coroutines.test.runTest
import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class FriendsTest {
    private val config = AccountsConfig(
        serverUrl = "https://www.boggledygook.com/",
        callbackUrl = "https://www.boggledygook.com/app/callback",
        clientId = "boggledygook",
    )
    private val signedIn = Session("tok", null, Player("p1", "todd", null, false))
    private val store = InMemoryTokenStore()
    private val accounts: Accounts =
        Accounts(config, MockAccountsApi(), store) { Instant.parse("2026-11-16T00:00:00Z") }
    private val mock = MockFriendsApi()
    private val friends = Friends(accounts, mock)
    private val alexJson: String =
        """{"sub":"u2","username":"alex","name":null,"picture":null,"since":"2026-11-15T10:00:00Z"}"""
    private val alex = Person("u2", "alex", null, null, "2026-11-15T10:00:00Z")

    private suspend fun givenSignedIn() {
        store.put("session", Json.encodeToString(Session.serializer(), signedIn))
    }

    private fun seed(method: String, path: String, status: Int, body: String, request: String? = null) {
        mock.seedSend(method, "/api/v1/friends$path", request, "Bearer tok", Result.success(FriendsReply(status, body)))
    }

    @Test
    fun list_and_requests_read_the_people() = runTest {
        givenSignedIn()
        seed("GET", "", 200, """{"friends":[$alexJson]}""")
        seed("GET", "/requests", 200, """{"incoming":[$alexJson],"outgoing":[]}""")
        assertEquals(listOf(alex), friends.list())
        assertEquals(FriendRequests(listOf(alex), emptyList()), friends.requests())
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun sending_by_username_and_by_code_returns_the_result() = runTest {
        givenSignedIn()
        seed("POST", "/requests", 200, """{"result":"requested"}""", """{"username":"alex"}""")
        seed("POST", "/requests", 200, """{"result":"already_friends"}""", """{"friend_code":"ABCD2345"}""")
        assertEquals(Sent.Requested, friends.requestByUsername("alex"))
        assertEquals(Sent.AlreadyFriends, friends.requestByCode("ABCD2345"))
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun accept_decline_cancel_remove_block_and_unblock_hit_their_routes() = runTest {
        givenSignedIn()
        seed("POST", "/requests/u2/accept", 204, "")
        seed("POST", "/requests/u3/decline", 204, "")
        seed("DELETE", "/requests/u4", 204, "")
        seed("DELETE", "/u5", 204, "")
        seed("POST", "/blocks", 204, "", """{"sub":"u6"}""")
        seed("DELETE", "/blocks/u7", 204, "")
        friends.accept("u2")
        friends.decline("u3")
        friends.cancel("u4")
        friends.remove("u5")
        friends.block("u6")
        friends.unblock("u7")
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun blocks_and_relationships_read_through_and_a_new_state_is_unknown() = runTest {
        givenSignedIn()
        seed("GET", "/blocks", 200, """{"blocked":[$alexJson],"hidden":["u2"]}""")
        seed("GET", "/relationships?subs=u2%2Cu3", 200, """{"relationships":{"u2":"request_received","u3":"later"}}""")
        assertEquals(Blocks(listOf(alex), listOf("u2")), friends.blocks())
        assertEquals(
            mapOf("u2" to Relationship.RequestReceived, "u3" to Relationship.Unknown),
            friends.relationships(listOf("u2", "u3")),
        )
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun a_server_error_throws_friends_exception_with_its_code_and_message() = runTest {
        givenSignedIn()
        seed("POST", "/requests", 429, """{"error":"rate_limited","message":"Please wait an hour."}""", """{"username":"x"}""")
        seed("GET", "", 502, "<html>")
        val limited = assertThrows(FriendsException::class.java) { kotlinx.coroutines.runBlocking { friends.requestByUsername("x") } }
        assertEquals(Triple(429, "rate_limited", "Please wait an hour."), Triple(limited.status, limited.code, limited.message))
        val down = assertThrows(FriendsException::class.java) { kotlinx.coroutines.runBlocking { friends.list() } }
        assertEquals(Triple(502, "accounts_unavailable", "Friends request failed (502)."), Triple(down.status, down.code, down.message))
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun with_no_session_nothing_is_sent_and_it_reads_as_account_required() = runTest {
        val error = assertThrows(FriendsException::class.java) { kotlinx.coroutines.runBlocking { friends.list() } }
        assertEquals(Pair(401, "account_required"), Pair(error.status, error.code))
        assertEquals(emptyList<SendArgs>(), mock.calls)
    }
}
