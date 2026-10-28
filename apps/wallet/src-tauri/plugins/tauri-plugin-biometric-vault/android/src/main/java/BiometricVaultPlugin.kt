package io.btcsessions.biometric

import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.AtomicFile
import android.util.Base64
import android.util.Log
import android.view.WindowManager
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_WEAK
import androidx.biometric.BiometricPrompt
import androidx.fragment.app.FragmentActivity
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONObject
import java.io.File
import java.security.KeyStore
import java.security.UnrecoverableKeyException
import java.util.Arrays
import java.util.concurrent.Executor
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

// Never log the passcode, its ciphertext or the IV.
private const val TAG = "SessionsBiometric"
private const val KEYSTORE = "AndroidKeyStore"

/** Every key we make starts with this; a new one per "turn on". */
private const val KEY_PREFIX = "io.btcsessions.wallet.biometric-passcode."
private const val TRANSFORMATION = "AES/GCM/NoPadding"
private const val GCM_TAG_BITS = 128

/** `{ alias, iv, ct }` (base64), in the app's private no-backup directory. */
private const val SEALED_FILE = "biometric-passcode.json"

/** Present after biometric unlock was switched off because biometrics changed. */
private const val INVALIDATED_FILE = "biometric-invalidated"

private const val MSG_INVALIDATED =
    "Your fingerprints or face changed. Enter your passcode, then turn biometric unlock on again in Settings."

@InvokeArg
class EnableArgs {
    var passcode: String = ""
}

@InvokeArg
class SecureArgs {
    var on: Boolean = false
}

@InvokeArg
class HexArgs {
    var data: String = ""
}

/**
 * The device key that binds the wallet vault to this phone: no biometric,
 * never leaves the Keystore (StrongBox where there is one). Not under
 * [KEY_PREFIX], so turning biometric unlock off never deletes it.
 */
private const val DEVICE_KEY = "io.btcsessions.wallet.vault-device"

private fun toHex(b: ByteArray): String = b.joinToString("") { "%02x".format(it) }

private fun fromHex(s: String): ByteArray {
    require(s.length % 2 == 0)
    return ByteArray(s.length / 2) { i -> s.substring(i * 2, i * 2 + 2).toInt(16).toByte() }
}

/** Rejected with a code the Rust side maps (see `BiometricError::from_code`). */
private class Failure(val code: String, message: String) : Exception(message)

private class Sealed(val alias: String, val iv: ByteArray, val ct: ByteArray)

private fun b64(bytes: ByteArray): String = Base64.encodeToString(bytes, Base64.NO_WRAP)

private fun unb64(s: String): ByteArray = Base64.decode(s, Base64.NO_WRAP)

@TauriPlugin
class BiometricVaultPlugin(private val activity: Activity) : Plugin(activity) {
    private val main = Handler(Looper.getMainLooper())
    private val mainExecutor = Executor { main.post(it) }

    /** A prompt is showing. Main thread only. */
    private var prompting = false

    private val dir: File get() = activity.noBackupFilesDir
    private val sealedFile: AtomicFile get() = AtomicFile(File(dir, SEALED_FILE))
    private val invalidatedFile: File get() = File(dir, INVALIDATED_FILE)

    // ---- Commands -------------------------------------------------------

    @Command
    fun status(invoke: Invoke) {
        main.post {
            try {
                invoke.resolve(statusObject())
            } catch (e: Exception) {
                Log.w(TAG, "status failed: ${e.javaClass.simpleName}")
                invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
            }
        }
    }

    /** Seal the passcode (already checked by Rust) under a new biometric-bound key. */
    @Command
    fun enable(invoke: Invoke) {
        val args = try {
            invoke.parseArgs(EnableArgs::class.java)
        } catch (e: Exception) {
            invoke.reject("bad arguments", "OTHER")
            return
        }
        main.post {
            var alias: String? = null
            try {
                val fragmentActivity = promptHost()
                unavailableReason(kind())?.let { throw Failure("UNAVAILABLE", it) }
                val newAlias = KEY_PREFIX + System.currentTimeMillis()
                alias = newAlias
                val key = createKey(newAlias)
                val cipher = Cipher.getInstance(TRANSFORMATION)
                cipher.init(Cipher.ENCRYPT_MODE, key)
                val iv = cipher.iv
                val word = when (kind()) {
                    "fingerprint" -> "fingerprint unlock"
                    "face" -> "face unlock"
                    else -> "fingerprint or face unlock"
                }
                val info = BiometricPrompt.PromptInfo.Builder()
                    .setTitle("Turn on $word")
                    .setSubtitle("Use it instead of your passcode")
                    .setNegativeButtonText("Cancel")
                    .setAllowedAuthenticators(BIOMETRIC_STRONG)
                    .build()
                prompt(fragmentActivity, info, cipher, invoke, onFail = { deleteKey(newAlias) }) { c ->
                    val plain = args.passcode.toByteArray(Charsets.UTF_8)
                    try {
                        val ct = c.doFinal(plain)
                        writeSealed(Sealed(newAlias, iv, ct))
                    } finally {
                        Arrays.fill(plain, 0)
                    }
                    deleteKeys(except = newAlias)
                    invalidatedFile.delete()
                    invoke.resolve(JSObject())
                }
            } catch (f: Failure) {
                alias?.let { deleteKey(it) }
                reject(invoke, f)
            } catch (e: Exception) {
                alias?.let { deleteKey(it) }
                Log.w(TAG, "enable failed: ${e.javaClass.simpleName}")
                invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
            }
        }
    }

    /** Delete every key and the stored passcode; acknowledges `invalidated`. */
    @Command
    fun disable(invoke: Invoke) {
        main.post {
            try {
                wipe()
                invalidatedFile.delete()
                invoke.resolve(JSObject())
            } catch (e: Exception) {
                Log.w(TAG, "disable failed: ${e.javaClass.simpleName}")
                invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
            }
        }
    }

    /** Ask for a fingerprint / face and decrypt the passcode with it. */
    @Command
    fun unlock(invoke: Invoke) {
        main.post {
            try {
                val fragmentActivity = promptHost()
                val sealed = readSealed()
                if (sealed == null) {
                    if (invalidatedFile.exists()) {
                        // Reported once here; status shows it until then.
                        invalidatedFile.delete()
                        throw Failure("INVALIDATED", MSG_INVALIDATED)
                    }
                    throw Failure("NOT_ENABLED", "not enabled")
                }
                val cipher = try {
                    decryptCipher(sealed)
                } catch (e: Exception) {
                    if (isInvalidation(e)) {
                        wipe()
                        throw Failure("INVALIDATED", MSG_INVALIDATED)
                    }
                    throw e
                }
                unavailableReason(kind())?.let { throw Failure("UNAVAILABLE", it) }
                val info = BiometricPrompt.PromptInfo.Builder()
                    .setTitle("Unlock Sessions Wallet")
                    .setNegativeButtonText("Use passcode")
                    .setAllowedAuthenticators(BIOMETRIC_STRONG)
                    // Face: unlock as soon as it matches, no extra tap.
                    .setConfirmationRequired(false)
                    .build()
                prompt(fragmentActivity, info, cipher, invoke, onFail = {}) { c ->
                    val plain = try {
                        c.doFinal(sealed.ct)
                    } catch (e: AEADBadTagException) {
                        // Tampered or corrupt: start over.
                        wipe()
                        throw Failure("OTHER", "the stored passcode can't be read; turn biometric unlock on again")
                    }
                    try {
                        val ret = JSObject()
                        ret.put("passcode", String(plain, Charsets.UTF_8))
                        invoke.resolve(ret)
                    } finally {
                        Arrays.fill(plain, 0)
                    }
                }
            } catch (f: Failure) {
                reject(invoke, f)
            } catch (e: Exception) {
                Log.w(TAG, "unlock failed: ${e.javaClass.simpleName}")
                invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
            }
        }
    }

    // ---- Prompt ---------------------------------------------------------

    private fun promptHost(): FragmentActivity {
        val fa = activity as? FragmentActivity
            ?: throw Failure("UNAVAILABLE", "Biometric unlock isn't available in this app build")
        if (prompting) throw Failure("BUSY", "busy")
        // BiometricPrompt silently does nothing once the activity saved its
        // state (app in the background): the call would never return.
        if (fa.isFinishing || fa.supportFragmentManager.isStateSaved) {
            throw Failure("CANCELLED", "the app isn't in the foreground")
        }
        return fa
    }

    /**
     * Show the prompt bound to [cipher]; [onSuccess] gets the authorised
     * cipher. [onFail] runs on any error or cancel before the invoke is
     * rejected. Main thread.
     */
    private fun prompt(
        host: FragmentActivity,
        info: BiometricPrompt.PromptInfo,
        cipher: Cipher,
        invoke: Invoke,
        onFail: () -> Unit,
        onSuccess: (Cipher) -> Unit,
    ) {
        val callback = object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                prompting = false
                try {
                    val c = result.cryptoObject?.cipher
                        ?: throw Failure("OTHER", "no cipher after authentication")
                    onSuccess(c)
                } catch (f: Failure) {
                    onFail()
                    reject(invoke, f)
                } catch (e: Exception) {
                    onFail()
                    if (isInvalidation(e)) {
                        wipe()
                        reject(invoke, Failure("INVALIDATED", MSG_INVALIDATED))
                    } else {
                        Log.w(TAG, "after authentication: ${e.javaClass.simpleName}")
                        invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
                    }
                }
            }

            override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                prompting = false
                onFail()
                reject(invoke, promptFailure(errorCode, errString.toString()))
            }

            // onAuthenticationFailed (not recognised): the prompt says so and
            // keeps listening; lockout comes as an error.
        }
        prompting = true
        try {
            BiometricPrompt(host, mainExecutor, callback)
                .authenticate(info, BiometricPrompt.CryptoObject(cipher))
        } catch (e: Exception) {
            prompting = false
            throw e
        }
    }

    private fun promptFailure(code: Int, message: String): Failure = when (code) {
        BiometricPrompt.ERROR_NEGATIVE_BUTTON,
        BiometricPrompt.ERROR_USER_CANCELED,
        BiometricPrompt.ERROR_CANCELED,
        BiometricPrompt.ERROR_TIMEOUT -> Failure("CANCELLED", message)
        BiometricPrompt.ERROR_LOCKOUT -> Failure(
            "LOCKOUT",
            "Too many attempts. Try again in 30 seconds, or use your passcode.",
        )
        BiometricPrompt.ERROR_LOCKOUT_PERMANENT -> Failure(
            "LOCKOUT",
            "Too many attempts. Unlock your phone with its PIN, pattern or password to use your fingerprint or face again. Use your passcode for now.",
        )
        BiometricPrompt.ERROR_NO_BIOMETRICS,
        BiometricPrompt.ERROR_HW_NOT_PRESENT,
        BiometricPrompt.ERROR_HW_UNAVAILABLE,
        BiometricPrompt.ERROR_NO_DEVICE_CREDENTIAL,
        BiometricPrompt.ERROR_SECURITY_UPDATE_REQUIRED ->
            Failure("UNAVAILABLE", unavailableReason(kind()) ?: message)
        else -> Failure("OTHER", message)
    }

    private fun reject(invoke: Invoke, f: Failure) {
        Log.i(TAG, "rejected: ${f.code}")
        invoke.reject(f.message, f.code)
    }

    // ---- Device ---------------------------------------------------------

    /** Best guess from the phone's sensors: "fingerprint", "face" or "biometric". */
    private fun kind(): String {
        val pm = activity.packageManager
        val fingerprint = pm.hasSystemFeature(PackageManager.FEATURE_FINGERPRINT)
        val face = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && pm.hasSystemFeature(PackageManager.FEATURE_FACE)
        val iris = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && pm.hasSystemFeature(PackageManager.FEATURE_IRIS)
        return when {
            fingerprint && !face && !iris -> "fingerprint"
            face && !fingerprint && !iris -> "face"
            else -> "biometric"
        }
    }

    /** Null when a strong (Class 3) biometric is ready to use. */
    private fun unavailableReason(kind: String): String? {
        val bm = BiometricManager.from(activity)
        return when (bm.canAuthenticate(BIOMETRIC_STRONG)) {
            BiometricManager.BIOMETRIC_SUCCESS -> null
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED ->
                if (bm.canAuthenticate(BIOMETRIC_WEAK) == BiometricManager.BIOMETRIC_SUCCESS) {
                    // E.g. camera face unlock on many Samsung phones.
                    "This phone's face unlock isn't secure enough to protect a wallet. Add a fingerprint in your phone's Settings to use biometric unlock."
                } else when (kind) {
                    "face" -> "Set up face unlock in your phone's Settings first."
                    else -> "Add a fingerprint in your phone's Settings first."
                }
            BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE ->
                if (bm.canAuthenticate(BIOMETRIC_WEAK) != BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE) {
                    "This phone's face unlock isn't secure enough to protect a wallet, and it has no fingerprint sensor."
                } else {
                    "This phone has no fingerprint sensor or secure face unlock."
                }
            BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE ->
                "The fingerprint or face sensor isn't available right now. Try again later."
            BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED ->
                "Your phone needs a security update before apps can use its fingerprint or face unlock."
            BiometricManager.BIOMETRIC_ERROR_UNSUPPORTED ->
                "This phone's Android version doesn't let apps use fingerprint or face unlock."
            else -> "Couldn't check this phone's fingerprint or face unlock."
        }
    }

    private fun statusObject(): JSObject {
        val kind = kind()
        val reason = unavailableReason(kind)
        var enabled = false
        val sealed = readSealed()
        if (sealed != null) {
            try {
                // Init needs no authentication, but fails once the key is
                // invalidated (biometrics added or removed, lock screen off).
                decryptCipher(sealed)
                enabled = true
                // (Not while "turn on" is prompting: its new key isn't stored yet.)
                if (!prompting) deleteKeys(except = sealed.alias)
            } catch (e: Exception) {
                if (isInvalidation(e)) {
                    Log.i(TAG, "key invalidated: ${e.javaClass.simpleName}")
                    wipe()
                    try {
                        invalidatedFile.createNewFile()
                    } catch (io: Exception) {
                        Log.w(TAG, "couldn't record invalidation: ${io.javaClass.simpleName}")
                    }
                } else {
                    Log.w(TAG, "status: key check failed: ${e.javaClass.simpleName}")
                }
            }
        } else {
            // A key left behind (an interrupted "turn on").
            if (!prompting) deleteKeys(except = null)
        }
        val ret = JSObject()
        ret.put("available", reason == null)
        ret.put("enabled", enabled)
        ret.put("invalidated", !enabled && invalidatedFile.exists())
        ret.put("kind", kind)
        if (reason != null) ret.put("reason", reason)
        return ret
    }

    // ---- Secret screens ---------------------------------------------------

    /**
     * While a secret is on screen (recovery phrase, connection secrets,
     * ecash): no screenshots, recordings or casting (FLAG_SECURE), and taps
     * are ignored while another app draws over this one (tapjacking).
     */
    @Command
    fun setSecure(invoke: Invoke) {
        val args = invoke.parseArgs(SecureArgs::class.java)
        main.post {
            try {
                val w = activity.window
                if (args.on) w.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
                else w.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
                w.decorView.filterTouchesWhenObscured = args.on
                invoke.resolve(JSObject())
            } catch (e: Exception) {
                invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
            }
        }
    }

    // ---- Device key (vault binding) -------------------------------------

    /** `{ data: hex }` → `{ data: hex(iv || ciphertext) }`. */
    @Command
    fun deviceWrap(invoke: Invoke) {
        val args = invoke.parseArgs(HexArgs::class.java)
        try {
            val plain = fromHex(args.data)
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, deviceKey())
            val out = cipher.iv + cipher.doFinal(plain)
            Arrays.fill(plain, 0)
            invoke.resolve(JSObject().put("data", toHex(out)))
        } catch (e: Exception) {
            Log.w(TAG, "device wrap failed: ${e.javaClass.simpleName}")
            invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
        }
    }

    /** `{ data: hex(iv || ciphertext) }` → `{ data: hex }`. */
    @Command
    fun deviceUnwrap(invoke: Invoke) {
        val args = invoke.parseArgs(HexArgs::class.java)
        try {
            val sealed = fromHex(args.data)
            val key = keyStore().getKey(DEVICE_KEY, null) as? SecretKey
                ?: throw KeyPermanentlyInvalidatedException("device key missing")
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, sealed, 0, 12))
            val plain = cipher.doFinal(sealed, 12, sealed.size - 12)
            invoke.resolve(JSObject().put("data", toHex(plain)))
            Arrays.fill(plain, 0)
        } catch (e: Exception) {
            Log.w(TAG, "device unwrap failed: ${e.javaClass.simpleName}")
            invoke.reject(e.message ?: e.javaClass.simpleName, "OTHER")
        }
    }

    private fun deviceKey(): SecretKey {
        (keyStore().getKey(DEVICE_KEY, null) as? SecretKey)?.let { return it }
        fun spec(strongBox: Boolean): KeyGenParameterSpec {
            val b = KeyGenParameterSpec.Builder(
                DEVICE_KEY,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
            if (strongBox && Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) b.setIsStrongBoxBacked(true)
            return b.build()
        }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        return try {
            val strongBox = Build.VERSION.SDK_INT >= Build.VERSION_CODES.P &&
                activity.packageManager.hasSystemFeature(PackageManager.FEATURE_STRONGBOX_KEYSTORE)
            generator.init(spec(strongBox))
            generator.generateKey()
        } catch (e: Exception) {
            // StrongBox listed but unusable (some phones): the TEE instead.
            Log.i(TAG, "StrongBox unavailable (${e.javaClass.simpleName}), using the TEE")
            generator.init(spec(strongBox = false))
            generator.generateKey()
        }
    }

    // ---- Keys and storage -----------------------------------------------

    private fun keyStore(): KeyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }

    private fun createKey(alias: String): SecretKey {
        val builder = KeyGenParameterSpec.Builder(
            alias,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setUserAuthenticationRequired(true)
            // Adding (or removing) a fingerprint or face destroys the key.
            .setInvalidatedByBiometricEnrollment(true)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Every use needs a strong biometric; the device PIN won't do.
            builder.setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
        } else {
            @Suppress("DEPRECATION")
            builder.setUserAuthenticationValidityDurationSeconds(-1)
        }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(builder.build())
        return generator.generateKey()
    }

    /** Throws KeyPermanentlyInvalidatedException once biometrics changed. */
    private fun decryptCipher(sealed: Sealed): Cipher {
        val key = keyStore().getKey(sealed.alias, null) as? SecretKey
            ?: throw KeyPermanentlyInvalidatedException("key missing")
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, sealed.iv))
        return cipher
    }

    private fun isInvalidation(e: Exception): Boolean =
        e is KeyPermanentlyInvalidatedException || e is UnrecoverableKeyException

    private fun deleteKey(alias: String) {
        try {
            keyStore().deleteEntry(alias)
        } catch (e: Exception) {
            Log.w(TAG, "couldn't delete key: ${e.javaClass.simpleName}")
        }
    }

    /** Delete our keys, all but [except]. */
    private fun deleteKeys(except: String?) {
        try {
            val ks = keyStore()
            for (alias in ks.aliases().toList()) {
                if (alias.startsWith(KEY_PREFIX) && alias != except) ks.deleteEntry(alias)
            }
        } catch (e: Exception) {
            Log.w(TAG, "couldn't clean up keys: ${e.javaClass.simpleName}")
        }
    }

    /** Forget everything: every key and the stored passcode. */
    private fun wipe() {
        sealedFile.delete()
        deleteKeys(except = null)
    }

    private fun readSealed(): Sealed? {
        val file = sealedFile
        if (!file.baseFile.exists()) return null
        return try {
            val json = JSONObject(String(file.readFully(), Charsets.UTF_8))
            Sealed(json.getString("alias"), unb64(json.getString("iv")), unb64(json.getString("ct")))
        } catch (e: Exception) {
            Log.w(TAG, "stored passcode unreadable: ${e.javaClass.simpleName}")
            wipe()
            null
        }
    }

    private fun writeSealed(sealed: Sealed) {
        val json = JSONObject()
            .put("v", 1)
            .put("alias", sealed.alias)
            .put("iv", b64(sealed.iv))
            .put("ct", b64(sealed.ct))
            .toString()
            .toByteArray(Charsets.UTF_8)
        val file = sealedFile
        val out = file.startWrite()
        try {
            out.write(json)
            file.finishWrite(out)
        } catch (e: Exception) {
            file.failWrite(out)
            throw e
        }
    }
}
