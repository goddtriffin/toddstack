package me.toddgriffin.toddstack.accounts

/** Arguments of [FriendsApi.send]. */
public data class SendArgs(val method: String, val path: String, val body: String?, val authorization: String)

/**
 * A mock [FriendsApi]. Seed a reply per exact arguments with [seedSend] (a [Result], so a failed request is
 * `Result.failure(AccountsException(...))`); calls consume them in FIFO order. A call with no seed throws
 * `IllegalStateException` (`mock_no_response_configured`): it is a bug in the test.
 *
 * Kotlin has no scope-exit hook, so every test that seeds anything must end with [assertAllSeedsConsumed].
 */
public class MockFriendsApi : FriendsApi {
    private val seeds: MutableList<Pair<SendArgs, ArrayDeque<Result<FriendsReply>>>> = mutableListOf()
    private val callLog: MutableList<SendArgs> = java.util.Collections.synchronizedList(mutableListOf())

    /** Every call received so far, in order. */
    public val calls: List<SendArgs> get() = synchronized(callLog) { callLog.toList() }

    /** Seeds the reply to [FriendsApi.send] with these arguments. */
    public fun seedSend(
        method: String,
        path: String,
        body: String?,
        authorization: String,
        response: Result<FriendsReply>,
    ) {
        val args = SendArgs(method, path, body, authorization)
        synchronized(seeds) {
            val queue: ArrayDeque<Result<FriendsReply>> =
                seeds.firstOrNull { it.first == args }?.second
                    ?: ArrayDeque<Result<FriendsReply>>().also { seeds.add(args to it) }
            queue.addLast(response)
        }
    }

    override suspend fun send(method: String, path: String, body: String?, authorization: String): FriendsReply {
        val args = SendArgs(method, path, body, authorization)
        callLog.add(args)
        return synchronized(seeds) {
            val queue: ArrayDeque<Result<FriendsReply>>? = seeds.firstOrNull { it.first == args }?.second
            check(queue != null && queue.isNotEmpty()) {
                "mock_no_response_configured: send($args) was called with no seeded response"
            }
            queue.removeFirst()
        }.getOrThrow()
    }

    /** Fails when a seeded reply was never consumed, naming each call's arguments and count left. */
    public fun assertAllSeedsConsumed() {
        val left: List<String> = synchronized(seeds) {
            seeds.filter { it.second.isNotEmpty() }.map { "send(${it.first}) has ${it.second.size} left" }
        }
        check(left.isEmpty()) { "mock_unconsumed_responses: ${left.joinToString("; ")}" }
    }
}
