package me.toddgriffin.toddstack.accounts

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class MockAccountsApiTest {
    private val first: Session = Session("t1", null, Player("p1", "one", null, false))
    private val second: Session = Session("t2", null, Player("p2", "two", null, false))

    @Test
    fun a_seeded_exchange_returns_the_response_and_records_the_call() = runTest {
        val mock = MockAccountsApi()
        mock.seedExchangeSession("c", "v", "guest", Result.success(first))
        assertEquals(first, mock.exchangeSession("c", "v", "guest"))
        assertEquals(listOf<MockCall>(MockCall.ExchangeSession(ExchangeSessionArgs("c", "v", "guest"))), mock.calls)
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun an_unseeded_exchange_panics() = runTest {
        val mock = MockAccountsApi()
        val e = assertThrows(IllegalStateException::class.java) {
            kotlinx.coroutines.runBlocking { mock.exchangeSession("c", "v", null) }
        }
        assertTrue(e.message!!.startsWith("mock_no_response_configured: exchangeSession"))
    }

    @Test
    fun exchange_responses_for_the_same_args_come_back_in_fifo_order() = runTest {
        val mock = MockAccountsApi()
        mock.seedExchangeSession("c", "v", null, Result.success(first))
        mock.seedExchangeSession("c", "v", null, Result.success(second))
        assertEquals(first, mock.exchangeSession("c", "v", null))
        assertEquals(second, mock.exchangeSession("c", "v", null))
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun a_seeded_guest_returns_the_response_and_records_the_call() = runTest {
        val mock = MockAccountsApi()
        mock.seedStartGuest(Result.success(first))
        assertEquals(first, mock.startGuest())
        assertEquals(listOf<MockCall>(MockCall.StartGuest(StartGuestArgs)), mock.calls)
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun an_unseeded_guest_panics() {
        assertThrows(IllegalStateException::class.java) {
            kotlinx.coroutines.runBlocking { MockAccountsApi().startGuest() }
        }
    }

    @Test
    fun guest_responses_come_back_in_fifo_order() = runTest {
        val mock = MockAccountsApi()
        mock.seedStartGuest(Result.success(first))
        mock.seedStartGuest(Result.success(second))
        assertEquals(first, mock.startGuest())
        assertEquals(second, mock.startGuest())
    }

    @Test
    fun a_seeded_sign_out_returns_the_response_and_records_the_call() = runTest {
        val mock = MockAccountsApi()
        mock.seedSignOut("t", Result.success(SignOutResponse("https://x/end")))
        assertEquals(SignOutResponse("https://x/end"), mock.signOut("t"))
        assertEquals(listOf<MockCall>(MockCall.SignOut(SignOutArgs("t"))), mock.calls)
        mock.assertAllSeedsConsumed()
    }

    @Test
    fun an_unseeded_sign_out_panics() {
        assertThrows(IllegalStateException::class.java) {
            kotlinx.coroutines.runBlocking { MockAccountsApi().signOut("t") }
        }
    }

    @Test
    fun sign_out_responses_come_back_in_fifo_order() = runTest {
        val mock = MockAccountsApi()
        mock.seedSignOut("t", Result.success(SignOutResponse("a")))
        mock.seedSignOut("t", Result.success(SignOutResponse(null)))
        assertEquals("a", mock.signOut("t").endSessionUrl)
        assertEquals(null, mock.signOut("t").endSessionUrl)
    }

    @Test
    fun a_seeded_failure_is_thrown() = runTest {
        val mock = MockAccountsApi()
        mock.seedStartGuest(Result.failure(AccountsException(AccountsException.Kind.Http, "down")))
        val e = assertThrows(AccountsException::class.java) { kotlinx.coroutines.runBlocking { mock.startGuest() } }
        assertEquals(AccountsException.Kind.Http, e.kind)
    }

    @Test
    fun an_unconsumed_response_fails_the_exhaustiveness_check() {
        val mock = MockAccountsApi()
        mock.seedStartGuest(Result.success(first))
        val e = assertThrows(IllegalStateException::class.java) { mock.assertAllSeedsConsumed() }
        assertEquals("mock_unconsumed_responses: startGuest(StartGuestArgs) has 1 left", e.message)
    }
}
