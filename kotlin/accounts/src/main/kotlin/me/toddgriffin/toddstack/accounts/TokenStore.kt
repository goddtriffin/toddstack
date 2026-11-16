package me.toddgriffin.toddstack.accounts

/** Where the session and the pending sign-in live between launches. [KeystoreTokenStore] is the real one. */
public interface TokenStore {
    /** The value under [key], or null. */
    public suspend fun get(key: String): String?

    /** Stores [value] under [key]. */
    public suspend fun put(key: String, value: String)

    /** Deletes [key]. */
    public suspend fun remove(key: String)
}

/** A [TokenStore] in memory, for tests. */
public class InMemoryTokenStore : TokenStore {
    private val values: MutableMap<String, String> = java.util.concurrent.ConcurrentHashMap()

    override suspend fun get(key: String): String? = values[key]

    override suspend fun put(key: String, value: String) {
        values[key] = value
    }

    override suspend fun remove(key: String) {
        values.remove(key)
    }
}
