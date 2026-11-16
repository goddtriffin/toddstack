package me.toddgriffin.toddstack.accounts

import android.net.Uri
import androidx.activity.ComponentActivity
import androidx.activity.result.ActivityResultLauncher
import android.content.Intent
import androidx.browser.auth.AuthTabIntent
import androidx.browser.customtabs.CustomTabsClient
import androidx.browser.customtabs.CustomTabsIntent
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/**
 * Opens the sign-in page in an Auth Tab when the person's browser supports it (the https redirect comes back
 * through the result callback), and in a Custom Tab otherwise (the redirect arrives as an App Link, which your
 * callback Activity passes to [Accounts.handleCallback]).
 *
 * Create it in `onCreate`, before the Activity is started: it registers an activity result launcher.
 */
public class AndroidBrowserLauncher(
    private val activity: ComponentActivity,
    private val accounts: Accounts,
) : BrowserLauncher {
    private val scope: CoroutineScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private val authTab: ActivityResultLauncher<Intent> =
        AuthTabIntent.registerActivityResultLauncher(activity) { result ->
            scope.launch {
                val uri: Uri? = result.resultUri
                if (result.resultCode == AuthTabIntent.RESULT_OK && uri != null) {
                    try {
                        accounts.handleCallback(uri.toString())
                    } catch (_: AccountsException) {
                        // signIn() already received the failure.
                    }
                } else {
                    accounts.cancelSignIn()
                }
            }
        }

    override fun launchSignIn(url: String) {
        val callback: Uri = Uri.parse(accounts.config.callbackUrl)
        val host: String? = callback.host
        val path: String? = callback.path
        if (host != null && path != null && authTabSupported()) {
            AuthTabIntent.Builder().build().launch(authTab, Uri.parse(url), host, path)
        } else {
            CustomTabsIntent.Builder().build().launchUrl(activity, Uri.parse(url))
        }
    }

    override fun open(url: String) {
        CustomTabsIntent.Builder().build().launchUrl(activity, Uri.parse(url))
    }

    private fun authTabSupported(): Boolean {
        val provider: String = CustomTabsClient.getPackageName(activity, null) ?: return false
        return CustomTabsClient.isAuthTabSupported(activity, provider)
    }
}

/** Connects this [Accounts] to [activity]; call it in `onCreate`, before `super.onStart`. */
public fun Accounts.attach(activity: ComponentActivity) {
    launcher = AndroidBrowserLauncher(activity, this)
}

/** An [Accounts] with the real network and the Keystore-backed store. */
public fun Accounts(context: android.content.Context, config: AccountsConfig): Accounts =
    Accounts(config, UrlConnectionAccountsApi(config), KeystoreTokenStore(context))
