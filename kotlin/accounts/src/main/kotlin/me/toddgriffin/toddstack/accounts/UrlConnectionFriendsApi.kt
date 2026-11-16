package me.toddgriffin.toddstack.accounts

import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** [FriendsApi] over `HttpURLConnection`: no networking library needed. */
public class UrlConnectionFriendsApi internal constructor(
    private val baseUrl: String,
    private val timeoutMillis: Int,
) : FriendsApi {
    /** Talks to [config]'s `serverUrl`. */
    public constructor(config: AccountsConfig, timeoutMillis: Int = 15_000) : this(config.server, timeoutMillis)

    override suspend fun send(method: String, path: String, body: String?, authorization: String): FriendsReply =
        withContext(Dispatchers.IO) {
            val connection: HttpURLConnection = try {
                URL(baseUrl + path).openConnection() as HttpURLConnection
            } catch (e: Exception) {
                throw AccountsException(AccountsException.Kind.Http, "could not open $path", cause = e)
            }
            try {
                connection.requestMethod = method
                // A redirect would replay the Authorization header on another origin.
                connection.instanceFollowRedirects = false
                connection.connectTimeout = timeoutMillis
                connection.readTimeout = timeoutMillis
                connection.setRequestProperty("Accept", "application/json")
                connection.setRequestProperty("Authorization", authorization)
                if (body != null) {
                    connection.doOutput = true
                    connection.setRequestProperty("Content-Type", "application/json")
                    connection.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
                }
                val status: Int = connection.responseCode
                val stream = if (status in 200..299) connection.inputStream else connection.errorStream
                FriendsReply(status, stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() }.orEmpty())
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: Exception) {
                throw AccountsException(AccountsException.Kind.Http, "request to $path failed: ${e.message}", cause = e)
            } finally {
                connection.disconnect()
            }
        }
}
