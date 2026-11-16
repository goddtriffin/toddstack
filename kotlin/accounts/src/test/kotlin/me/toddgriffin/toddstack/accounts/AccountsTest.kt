package me.toddgriffin.toddstack.accounts

import java.net.URLDecoder
import java.time.Instant
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class AccountsTest {
    private val config = AccountsConfig(
        serverUrl = "https://www.boggledygook.com/",
        callbackUrl = "https://www.boggledygook.com/app/callback",
        clientId = "boggledygook",
    )
    private val callback: String = "https://www.boggledygook.com/app/callback"
    private val verifier: String = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
    private val state: String = "state-123"
    private val player = Player("p1", "todd", "https://img/p1.png", false)
    private val signedIn = Session("signed-in-token", "2030-01-01T00:00:00Z", player)
    private val guest = Session("guest-token", null, Player("g1", "guest-1", null, true))

    private class RecordingLauncher : BrowserLauncher {
        val signIns: MutableList<String> = mutableListOf()
        val opened: MutableList<String> = mutableListOf()
        override fun launchSignIn(url: String) { signIns.add(url) }
        override fun open(url: String) { opened.add(url) }
    }

    private val mock = MockAccountsApi()
    private val store = InMemoryTokenStore()
    private val launcher = RecordingLauncher()
    private val now: Instant = Instant.parse("2026-11-16T00:00:00Z")
    private val accounts: Accounts = Accounts(config, mock, store) { now }.also { it.launcher = launcher }

    private suspend fun givenPending() {
        store.put("pending", """{"verifier":"$verifier","state":"$state"}""")
    }

    private suspend fun givenSession(session: Session) {
        store.put("session", Json.encodeToString(Session.serializer(), session))
    }

    @Test
    fun a_valid_callback_exchanges_the_code_with_the_verifier_and_stores_the_session() = runTest {
        givenPending()
        mock.seedExchangeSession("one-time", verifier, null, Result.success(signedIn))
        assertEquals(signedIn, accounts.handleCallback("$callback?code=one-time&state=$state"))
        assertEquals(signedIn, accounts.currentSession())
        assertEquals(signedIn, accounts.session.value)
        assertEquals("Bearer signed-in-token", accounts.authorizationHeader("https://www.boggledygook.com/api/v1/x"))
        assertNull(store.get("pending"))
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun a_state_mismatch_is_rejected_and_nothing_is_exchanged() = runTest {
        givenPending()
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?code=c&state=forged") }
        }
        assertEquals(AccountsException.Kind.StateMismatch, e.kind)
        assertEquals(emptyList<MockCall>(), mock.calls)
        assertEquals("""{"verifier":"$verifier","state":"$state"}""", store.get("pending"))
    }

    @Test
    fun a_callback_without_state_is_a_mismatch() = runTest {
        givenPending()
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?code=c") }
        }
        assertEquals(AccountsException.Kind.StateMismatch, e.kind)
    }

    @Test
    fun a_callback_with_no_sign_in_in_progress_is_unexpected() = runTest {
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?code=c&state=$state") }
        }
        assertEquals(AccountsException.Kind.UnexpectedCallback, e.kind)
    }

    @Test
    fun a_callback_for_another_url_is_unexpected() = runTest {
        givenPending()
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("https://evil.example/app/callback?code=c&state=$state") }
        }
        assertEquals(AccountsException.Kind.UnexpectedCallback, e.kind)
    }

    @Test
    fun an_error_callback_surfaces_the_code_and_does_not_exchange() = runTest {
        givenPending()
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?error=access_denied&state=$state") }
        }
        assertEquals(AccountsException.Kind.Authorization, e.kind)
        assertEquals("access_denied", e.error)
        assertEquals(emptyList<MockCall>(), mock.calls)
        assertNull(store.get("pending"))
    }

    @Test
    fun a_callback_without_a_code_is_missing_code() = runTest {
        givenPending()
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?state=$state") }
        }
        assertEquals(AccountsException.Kind.MissingCode, e.kind)
    }

    @Test
    fun an_invalid_grant_is_reported_and_stores_no_session() = runTest {
        givenPending()
        mock.seedExchangeSession(
            "used", verifier, null,
            Result.failure(AccountsException(AccountsException.Kind.InvalidGrant, "invalid_grant", "invalid_grant", 400)),
        )
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?code=used&state=$state") }
        }
        assertEquals(AccountsException.Kind.InvalidGrant, e.kind)
        assertNull(accounts.currentSession())
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun the_stored_guest_token_is_sent_on_the_exchange_and_replaced_by_the_new_token() = runTest {
        givenPending()
        givenSession(guest)
        mock.seedExchangeSession("c", verifier, "guest-token", Result.success(signedIn))
        accounts.handleCallback("$callback?code=c&state=$state")
        assertEquals(signedIn, accounts.currentSession())
        assertEquals(
            listOf<MockCall>(MockCall.ExchangeSession(ExchangeSessionArgs("c", verifier, "guest-token"))),
            mock.calls,
        )
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun a_signed_in_token_is_never_sent_on_the_exchange() = runTest {
        givenPending()
        givenSession(signedIn)
        mock.seedExchangeSession("c", verifier, null, Result.success(signedIn))
        accounts.handleCallback("$callback?code=c&state=$state")
        assertEquals(
            listOf<MockCall>(MockCall.ExchangeSession(ExchangeSessionArgs("c", verifier, null))),
            mock.calls,
        )
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun no_bearer_is_sent_on_the_exchange_when_nothing_is_stored() = runTest {
        givenPending()
        mock.seedExchangeSession("c", verifier, null, Result.success(signedIn))
        accounts.handleCallback("$callback?code=c&state=$state")
        assertEquals(
            listOf<MockCall>(MockCall.ExchangeSession(ExchangeSessionArgs("c", verifier, null))),
            mock.calls,
        )
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun sign_in_opens_the_server_url_with_the_challenge_and_state_and_resumes_on_the_callback() = runTest {
        val job = async(StandardTestDispatcher(testScheduler)) { accounts.signIn(Prompt.Login) }
        runCurrent()
        val url: String = launcher.signIns.single()
        val params: Map<String, String> = url.substringAfter('?').split('&').associate {
            URLDecoder.decode(it.substringBefore('='), "UTF-8") to URLDecoder.decode(it.substringAfter('='), "UTF-8")
        }
        assertEquals("https://www.boggledygook.com/app/sign-in", url.substringBefore('?'))
        assertEquals(setOf("code_challenge", "code_challenge_method", "state", "prompt"), params.keys)
        assertEquals("login", params["prompt"])
        assertEquals("S256", params["code_challenge_method"])
        val pending = Json.parseToJsonElement(store.get("pending")!!).let { it as kotlinx.serialization.json.JsonObject }
        val storedVerifier: String = (pending["verifier"] as kotlinx.serialization.json.JsonPrimitive).content
        val storedState: String = (pending["state"] as kotlinx.serialization.json.JsonPrimitive).content
        assertEquals(Pkce.challenge(storedVerifier), params["code_challenge"])
        assertEquals(storedState, params["state"])

        mock.seedExchangeSession("c", storedVerifier, null, Result.success(signedIn))
        accounts.handleCallback("$callback?code=c&state=$storedState")
        assertEquals(signedIn, job.await())
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun a_sign_in_without_a_launcher_is_a_config_error() = runTest {
        val bare = Accounts(config, mock, store)
        val e = assertThrows(AccountsException::class.java) { kotlinx.coroutines.runBlocking { bare.signIn() } }
        assertEquals(AccountsException.Kind.Config, e.kind)
    }

    @Test
    fun a_guest_session_is_stored() = runTest {
        mock.seedStartGuest(Result.success(guest))
        assertEquals(guest, accounts.startGuest())
        assertEquals(guest, accounts.currentSession())
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun sign_out_clears_the_token_and_opens_the_end_session_page() = runTest {
        givenSession(signedIn)
        mock.seedSignOut("signed-in-token", Result.success(SignOutResponse("https://accounts.toddgriffin.me/oidc/session/end")))
        assertEquals(true, accounts.signOut())
        assertNull(accounts.currentSession())
        assertNull(accounts.cachedAuthorizationHeader("https://www.boggledygook.com/api/v1/x"))
        assertEquals(listOf("https://accounts.toddgriffin.me/oidc/session/end"), launcher.opened)
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun sign_out_clears_the_token_even_when_the_network_fails() = runTest {
        givenSession(signedIn)
        mock.seedSignOut("signed-in-token", Result.failure(AccountsException(AccountsException.Kind.Http, "offline")))
        assertEquals(false, accounts.signOut())
        assertNull(store.get("session"))
        assertNull(accounts.currentSession())
        assertEquals(emptyList<String>(), launcher.opened)
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun sign_out_with_a_null_end_session_url_opens_nothing_and_still_clears() = runTest {
        givenSession(guest)
        mock.seedSignOut("guest-token", Result.success(SignOutResponse(null)))
        assertEquals(false, accounts.signOut())
        assertNull(store.get("session"))
        assertEquals(emptyList<String>(), launcher.opened)
    }

    @Test
    fun an_expired_session_is_deleted_and_reads_as_signed_out() = runTest {
        givenSession(signedIn.copy(expiresAt = "2026-11-15T23:59:59+00:00"))
        assertNull(accounts.currentSession())
        assertNull(store.get("session"))
    }

    @Test
    fun a_session_with_a_future_expiry_with_an_offset_is_valid() = runTest {
        givenSession(signedIn.copy(expiresAt = "2026-11-16T01:00:00+00:30"))
        assertEquals("signed-in-token", accounts.currentSession()?.token)
    }

    @Test
    fun account_urls_carry_the_client_and_keep_query_and_fragment_in_place() {
        assertEquals("https://accounts.toddgriffin.me/account?client=boggledygook", accounts.accountUrl())
        assertEquals("https://accounts.toddgriffin.me/account/security?client=boggledygook", accounts.accountUrl("account/security"))
        assertEquals("https://accounts.toddgriffin.me/friends?tab=1&client=boggledygook#top", accounts.accountUrl("/friends?tab=1#top"))
    }

    @Test
    fun the_accounts_base_url_can_be_overridden_and_a_trailing_slash_is_dropped() {
        val other = Accounts(config.copy(accountsBaseUrl = "https://accounts.test/", clientId = "a b"), mock, store)
        assertEquals("https://accounts.test/account?client=a%20b", other.accountUrl())
    }

    @Test
    fun a_config_with_a_non_https_callback_is_rejected() {
        val e = assertThrows(AccountsException::class.java) { config.copy(callbackUrl = "boggledygook://callback") }
        assertEquals(AccountsException.Kind.Config, e.kind)
    }

    @Test
    fun the_bearer_is_added_only_for_https_requests_to_the_server_origin() = runTest {
        givenSession(signedIn)
        accounts.currentSession()
        val expected = "Bearer signed-in-token"
        assertEquals(expected, accounts.cachedAuthorizationHeader("https://www.boggledygook.com/api/v1/app/me"))
        assertEquals(expected, accounts.cachedAuthorizationHeader("https://WWW.boggledygook.com:443/x"))
        assertEquals(expected, accounts.authorizationHeader("https://www.boggledygook.com/x"))
        assertNull(accounts.cachedAuthorizationHeader("http://www.boggledygook.com/x"))
        assertNull(accounts.cachedAuthorizationHeader("https://www.boggledygook.com:8443/x"))
        assertNull(accounts.cachedAuthorizationHeader("https://boggledygook.com/x"))
        assertNull(accounts.cachedAuthorizationHeader("https://www.boggledygook.com.evil.example/x"))
        assertNull(accounts.cachedAuthorizationHeader("https://evil.example/www.boggledygook.com"))
        assertNull(accounts.authorizationHeader("https://accounts.toddgriffin.me/account"))
        assertNull(accounts.cachedAuthorizationHeader("not a url"))
    }

    private class FailingStore(private val failure: Exception) : TokenStore {
        override suspend fun get(key: String): String? = throw failure
        override suspend fun put(key: String, value: String): Unit = throw failure
        override suspend fun remove(key: String): Unit = throw failure
    }

    @Test
    fun a_storage_read_error_is_thrown_and_not_treated_as_signed_out() = runTest {
        val broken = Accounts(config, mock, FailingStore(java.io.IOException("disk")))
        val e = assertThrows(AccountsException::class.java) { kotlinx.coroutines.runBlocking { broken.currentSession() } }
        assertEquals(AccountsException.Kind.Storage, e.kind)
    }

    @Test
    fun a_corrupt_stored_session_is_a_storage_error_and_is_kept() = runTest {
        store.put("session", "{not json")
        val e = assertThrows(AccountsException::class.java) { kotlinx.coroutines.runBlocking { accounts.currentSession() } }
        assertEquals(AccountsException.Kind.Storage, e.kind)
        assertEquals("{not json", store.get("session"))
    }

    @Test
    fun a_new_sign_in_supersedes_an_abandoned_one_which_fails_with_cancelled() = runTest {
        val first = async(StandardTestDispatcher(testScheduler)) { runCatching { accounts.signIn() } }
        runCurrent()
        val second = async(StandardTestDispatcher(testScheduler)) { runCatching { accounts.signIn() } }
        runCurrent()
        assertEquals(2, launcher.signIns.size)
        assertEquals(AccountsException.Kind.Cancelled, (first.await().exceptionOrNull() as AccountsException).kind)
        accounts.cancelSignIn()
        assertEquals(AccountsException.Kind.Cancelled, (second.await().exceptionOrNull() as AccountsException).kind)
    }

    @Test
    fun a_corrupt_pending_sign_in_is_a_storage_error() = runTest {
        store.put("pending", "{not json")
        val e = assertThrows(AccountsException::class.java) {
            kotlinx.coroutines.runBlocking { accounts.handleCallback("$callback?code=c&state=$state") }
        }
        assertEquals(AccountsException.Kind.Storage, e.kind)
    }

    @Test
    fun an_unreadable_stored_session_is_dropped_so_sign_in_repairs_it() = runTest {
        givenPending()
        store.put("session", "{not json")
        mock.seedExchangeSession("c", verifier, null, Result.success(signedIn))
        assertEquals(signedIn, accounts.handleCallback("$callback?code=c&state=$state"))
        assertEquals(signedIn, accounts.currentSession())
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun a_cached_session_that_has_since_expired_gets_no_bearer() = runTest {
        val clock = java.util.concurrent.atomic.AtomicReference(now)
        val ticking = Accounts(config, mock, store) { clock.get() }
        givenSession(signedIn.copy(expiresAt = "2026-11-16T00:01:00Z"))
        ticking.currentSession()
        assertEquals("Bearer signed-in-token", ticking.cachedAuthorizationHeader("https://www.boggledygook.com/x"))
        clock.set(now.plusSeconds(120))
        assertNull(ticking.cachedAuthorizationHeader("https://www.boggledygook.com/x"))
    }

    @Test
    fun a_session_never_prints_its_token() {
        assertEquals(false, signedIn.toString().contains("signed-in-token"))
        assertEquals(true, signedIn.toString().contains("<redacted>"))
    }
}
