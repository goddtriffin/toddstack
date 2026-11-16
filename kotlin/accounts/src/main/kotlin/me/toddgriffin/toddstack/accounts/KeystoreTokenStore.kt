package me.toddgriffin.toddstack.accounts

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import kotlinx.coroutines.flow.first

/**
 * A [TokenStore] that keeps values in DataStore, each encrypted with an AES-GCM key held in the Android
 * Keystore. The key never leaves secure hardware where the device has it. A value that cannot be decrypted
 * (key lost after a restore) throws [AccountsException] of kind `Storage`: it is not the same as signed out.
 */
public class KeystoreTokenStore(
    context: Context,
    name: String = "toddstack_accounts",
) : TokenStore {
    private val alias: String = "toddstack_accounts_$name"
    private val dataStore: DataStore<Preferences> = sharedStore(context.applicationContext, name)

    override suspend fun get(key: String): String? {
        val stored: String = dataStore.data.first()[stringPreferencesKey(key)] ?: return null
        return try {
            decrypt(stored)
        } catch (e: java.security.GeneralSecurityException) {
            throw AccountsException(AccountsException.Kind.Storage, "stored value cannot be decrypted", cause = e)
        } catch (e: IllegalArgumentException) {
            throw AccountsException(AccountsException.Kind.Storage, "stored value is corrupt", cause = e)
        }
    }

    override suspend fun put(key: String, value: String) {
        val sealed: String = encrypt(value)
        dataStore.edit { it[stringPreferencesKey(key)] = sealed }
    }

    override suspend fun remove(key: String) {
        dataStore.edit { it.remove(stringPreferencesKey(key)) }
    }

    private fun key(): SecretKey {
        val keyStore: KeyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(alias, null) as? SecretKey)?.let { return it }
        val generator: KeyGenerator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }

    private fun encrypt(plain: String): String {
        val cipher: Cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val sealed: ByteArray = cipher.doFinal(plain.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(cipher.iv + sealed, Base64.NO_WRAP)
    }

    private fun decrypt(stored: String): String {
        val bytes: ByteArray = Base64.decode(stored, Base64.NO_WRAP)
        val cipher: Cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, bytes, 0, IV_BYTES))
        return String(cipher.doFinal(bytes, IV_BYTES, bytes.size - IV_BYTES), Charsets.UTF_8)
    }

    private companion object {
        const val ANDROID_KEYSTORE: String = "AndroidKeyStore"
        const val TRANSFORMATION: String = "AES/GCM/NoPadding"
        const val IV_BYTES: Int = 12
        const val TAG_BITS: Int = 128

        // One DataStore per file per process, or DataStore throws.
        private val stores: MutableMap<String, DataStore<Preferences>> = mutableMapOf()

        @Synchronized
        fun sharedStore(context: Context, name: String): DataStore<Preferences> =
            stores.getOrPut(name) {
                PreferenceDataStoreFactory.create {
                    File(context.filesDir, "datastore/$name.preferences_pb")
                }
            }
    }
}
