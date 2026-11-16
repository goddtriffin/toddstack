package me.toddgriffin.toddstack.accounts

import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/** [AccountsApi] over `HttpURLConnection`: no networking library needed. */
public class UrlConnectionAccountsApi(
    private val config: AccountsConfig,
    private val timeoutMillis: Int = 15_000,
) : AccountsApi {

    @Serializable
    private data class ExchangeBody(val code: String, val code_verifier: String)

    @Serializable
    private data class ErrorBody(val error: String? = null)

    override suspend fun exchangeSession(code: String, codeVerifier: String, bearer: String?): Session {
        val body: String = json.encodeToString(ExchangeBody.serializer(), ExchangeBody(code, codeVerifier))
        return post("/api/v1/app/session", body, bearer, Session.serializer())
    }

    override suspend fun startGuest(): Session = post("/api/v1/app/guest", null, null, Session.serializer())

    override suspend fun signOut(bearer: String): SignOutResponse =
        post("/api/v1/app/sign-out", null, bearer, SignOutResponse.serializer())

    private suspend fun <T> post(
        path: String,
        body: String?,
        bearer: String?,
        serializer: kotlinx.serialization.KSerializer<T>,
    ): T = withContext(Dispatchers.IO) {
        val connection: HttpURLConnection = try {
            URL(config.server + path).openConnection() as HttpURLConnection
        } catch (e: IOException) {
            throw AccountsException(AccountsException.Kind.Http, "could not open $path", cause = e)
        }
        try {
            connection.requestMethod = "POST"
            connection.connectTimeout = timeoutMillis
            connection.readTimeout = timeoutMillis
            connection.setRequestProperty("Accept", "application/json")
            if (bearer != null) connection.setRequestProperty("Authorization", "Bearer $bearer")
            if (body != null) {
                connection.doOutput = true
                connection.setRequestProperty("Content-Type", "application/json")
                connection.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
            }
            val status: Int = connection.responseCode
            val stream = if (status in 200..299) connection.inputStream else connection.errorStream
            val text: String = stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() }.orEmpty()
            if (status in 200..299) {
                json.decodeFromString(serializer, text)
            } else {
                throw refusal(status, text)
            }
        } catch (e: IOException) {
            throw AccountsException(AccountsException.Kind.Http, "request to $path failed: ${e.message}", cause = e)
        } catch (e: SerializationException) {
            throw AccountsException(AccountsException.Kind.Http, "unreadable reply from $path", cause = e)
        } finally {
            connection.disconnect()
        }
    }

    private fun refusal(status: Int, text: String): AccountsException {
        val error: String? = try {
            json.decodeFromString(ErrorBody.serializer(), text).error
        } catch (_: SerializationException) {
            null
        }
        val kind: AccountsException.Kind =
            if (status == 400 && error == "invalid_grant") AccountsException.Kind.InvalidGrant else AccountsException.Kind.Api
        return AccountsException(kind, "server answered $status${error?.let { ": $it" }.orEmpty()}", error, status)
    }

    private companion object {
        val json: Json = Json { ignoreUnknownKeys = true }
    }
}
