package me.toddgriffin.toddstack.accounts

/** Arguments of [AccountsApi.exchangeSession]. */
public data class ExchangeSessionArgs(val code: String, val codeVerifier: String, val bearer: String?)

/** Arguments of [AccountsApi.startGuest]. */
public data object StartGuestArgs

/** Arguments of [AccountsApi.signOut]. */
public data class SignOutArgs(val bearer: String)

/** One call the mock received, in order. */
public sealed interface MockCall {
    public data class ExchangeSession(val args: ExchangeSessionArgs) : MockCall
    public data class StartGuest(val args: StartGuestArgs) : MockCall
    public data class SignOut(val args: SignOutArgs) : MockCall
}

/**
 * A mock [AccountsApi]. Seed a response per exact arguments with the `seed...` methods (a [Result], so a
 * failure is seeded as `Result.failure(AccountsException(...))`); calls consume them in FIFO order. A call
 * with no seed throws `IllegalStateException` (`mock_no_response_configured`): it is a bug in the test.
 *
 * Kotlin has no scope-exit hook, so every test that seeds anything must end with [assertAllSeedsConsumed].
 */
public class MockAccountsApi : AccountsApi {
    private class Seeds<A, R> {
        val entries: MutableList<Pair<A, ArrayDeque<Result<R>>>> = mutableListOf()
    }

    private val exchangeSeeds: Seeds<ExchangeSessionArgs, Session> = Seeds()
    private val guestSeeds: Seeds<StartGuestArgs, Session> = Seeds()
    private val signOutSeeds: Seeds<SignOutArgs, SignOutResponse> = Seeds()
    private val callLog: MutableList<MockCall> = java.util.Collections.synchronizedList(mutableListOf())

    /** Every call received so far, in order. */
    public val calls: List<MockCall> get() = synchronized(callLog) { callLog.toList() }

    /** Seeds the response to [AccountsApi.exchangeSession] with these arguments. */
    public fun seedExchangeSession(code: String, codeVerifier: String, bearer: String?, response: Result<Session>) {
        seed(exchangeSeeds, ExchangeSessionArgs(code, codeVerifier, bearer), response)
    }

    /** Seeds the response to [AccountsApi.startGuest]. */
    public fun seedStartGuest(response: Result<Session>) {
        seed(guestSeeds, StartGuestArgs, response)
    }

    /** Seeds the response to [AccountsApi.signOut] with this bearer. */
    public fun seedSignOut(bearer: String, response: Result<SignOutResponse>) {
        seed(signOutSeeds, SignOutArgs(bearer), response)
    }

    override suspend fun exchangeSession(code: String, codeVerifier: String, bearer: String?): Session {
        val args = ExchangeSessionArgs(code, codeVerifier, bearer)
        callLog.add(MockCall.ExchangeSession(args))
        return take("exchangeSession", exchangeSeeds, args).getOrThrow()
    }

    override suspend fun startGuest(): Session {
        callLog.add(MockCall.StartGuest(StartGuestArgs))
        return take("startGuest", guestSeeds, StartGuestArgs).getOrThrow()
    }

    override suspend fun signOut(bearer: String): SignOutResponse {
        val args = SignOutArgs(bearer)
        callLog.add(MockCall.SignOut(args))
        return take("signOut", signOutSeeds, args).getOrThrow()
    }

    /** Fails when a seeded response was never consumed, naming each method, arguments and count left. */
    public fun assertAllSeedsConsumed() {
        val left: List<String> = leftovers("exchangeSession", exchangeSeeds) +
            leftovers("startGuest", guestSeeds) +
            leftovers("signOut", signOutSeeds)
        check(left.isEmpty()) { "mock_unconsumed_responses: ${left.joinToString("; ")}" }
    }

    private fun <A, R> seed(seeds: Seeds<A, R>, args: A, response: Result<R>) {
        synchronized(seeds) {
            val queue: ArrayDeque<Result<R>> =
                seeds.entries.firstOrNull { it.first == args }?.second
                    ?: ArrayDeque<Result<R>>().also { seeds.entries.add(args to it) }
            queue.addLast(response)
        }
    }

    private fun <A, R> take(method: String, seeds: Seeds<A, R>, args: A): Result<R> =
        synchronized(seeds) {
            val queue: ArrayDeque<Result<R>>? = seeds.entries.firstOrNull { it.first == args }?.second
            check(queue != null && queue.isNotEmpty()) {
                "mock_no_response_configured: $method($args) was called with no seeded response"
            }
            queue.removeFirst()
        }

    private fun <A, R> leftovers(method: String, seeds: Seeds<A, R>): List<String> =
        synchronized(seeds) {
            seeds.entries.filter { it.second.isNotEmpty() }.map { "$method(${it.first}) has ${it.second.size} left" }
        }
}
