package io.github.ghkdwjdrk123.notifier

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * 앱 설정 저장소.
 * 기기 토큰은 Android Keystore의 AES 키로 암호화해 저장한다(키는 기기 밖으로 나가지 않음).
 */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("notifier", Context.MODE_PRIVATE)

    /** 맥 서버 주소(예: https://mac.tailnet.ts.net:8443). */
    var serverUrl: String
        get() = sp.getString(KEY_URL, "") ?: ""
        set(v) = sp.edit().putString(KEY_URL, v.trimEnd('/')).apply()

    var deviceName: String
        get() = sp.getString(KEY_NAME, "갤럭시") ?: "갤럭시"
        set(v) = sp.edit().putString(KEY_NAME, v).apply()

    /** 상시 알림을 켜 둘지(사용자가 끄면 부팅 후에도 시작하지 않음). */
    var statusEnabled: Boolean
        get() = sp.getBoolean(KEY_ENABLED, true)
        set(v) = sp.edit().putBoolean(KEY_ENABLED, v).apply()

    var token: String?
        get() = sp.getString(KEY_TOKEN, null)?.let { decrypt(it) }
        set(v) {
            if (v == null) sp.edit().remove(KEY_TOKEN).apply()
            else sp.edit().putString(KEY_TOKEN, encrypt(v)).apply()
        }

    val isPaired: Boolean get() = serverUrl.isNotEmpty() && sp.contains(KEY_TOKEN)

    fun clearPairing() {
        sp.edit().remove(KEY_TOKEN).apply()
    }

    // 서비스 스레드와 화면 스레드가 동시에 처음 불러도 키를 두 번 만들지 않게
    private fun key(): SecretKey = synchronized(KEY_LOCK) {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return@synchronized it.secretKey }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        gen.generateKey()
    }

    private fun encrypt(plain: String): String {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        val out = c.iv + c.doFinal(plain.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(out, Base64.NO_WRAP)
    }

    private fun decrypt(stored: String): String? = try {
        val raw = Base64.decode(stored, Base64.NO_WRAP)
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, raw, 0, 12))
        String(c.doFinal(raw, 12, raw.size - 12), Charsets.UTF_8)
    } catch (e: Exception) {
        null // 키가 바뀌었거나 손상됨: 다시 등록해야 함
    }

    companion object {
        private const val KEY_URL = "server_url"
        private const val KEY_NAME = "device_name"
        private const val KEY_TOKEN = "token_enc"
        private const val KEY_ENABLED = "status_enabled"
        private const val KEY_ALIAS = "notifier-token-key"
        private val KEY_LOCK = Any()
    }
}
