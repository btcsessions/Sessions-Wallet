package io.btcsessions.passkey

import android.app.Activity
import android.os.Build
import android.os.CancellationSignal
import android.util.Base64
import android.util.Log
import androidx.credentials.CreateCredentialResponse
import androidx.credentials.CreatePublicKeyCredentialRequest
import androidx.credentials.CreatePublicKeyCredentialResponse
import androidx.credentials.CredentialManager
import androidx.credentials.CredentialManagerCallback
import androidx.credentials.GetCredentialRequest
import androidx.credentials.GetCredentialResponse
import androidx.credentials.GetPublicKeyCredentialOption
import androidx.credentials.PublicKeyCredential
import androidx.credentials.exceptions.CreateCredentialCancellationException
import androidx.credentials.exceptions.CreateCredentialException
import androidx.credentials.exceptions.CreateCredentialInterruptedException
import androidx.credentials.exceptions.CreateCredentialProviderConfigurationException
import androidx.credentials.exceptions.CreateCredentialUnsupportedException
import androidx.credentials.exceptions.GetCredentialCancellationException
import androidx.credentials.exceptions.GetCredentialException
import androidx.credentials.exceptions.GetCredentialInterruptedException
import androidx.credentials.exceptions.GetCredentialProviderConfigurationException
import androidx.credentials.exceptions.GetCredentialUnsupportedException
import androidx.credentials.exceptions.NoCredentialException
import androidx.credentials.exceptions.domerrors.NotAllowedError
import androidx.credentials.exceptions.domerrors.SecurityError
import androidx.credentials.exceptions.publickeycredential.CreatePublicKeyCredentialDomException
import androidx.credentials.exceptions.publickeycredential.GetPublicKeyCredentialDomException
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.json.JSONObject
import java.security.SecureRandom
import java.util.concurrent.Executors

private const val TAG = "SessionsPasskey"

/** Credential Manager handles passkeys from Android 9 (via Google Play services). */
private const val MIN_SDK = Build.VERSION_CODES.P

@InvokeArg
class CreateArgs {
    var rpId: String = ""
    var rpName: String = ""
    /** WebAuthn user handle, base64url. */
    var userId: String = ""
    var userName: String = ""
    /** PRF `eval.first`, base64url. */
    var prfSalt: String = ""
}

@InvokeArg
class GetArgs {
    var rpId: String = ""
    var prfSalt: String = ""
    /** Credential ids (base64url) to limit the choice to; empty = any. */
    var allowCredentials: List<String>? = null
}

/** Rejected with a code the Rust side maps to plain English. */
private class Failure(val code: String, message: String) : Exception(message)

private fun b64url(bytes: ByteArray): String =
    Base64.encodeToString(bytes, Base64.URL_SAFE or Base64.NO_WRAP or Base64.NO_PADDING)

private fun challenge(): String {
    // No server checks the assertion: only the PRF output matters. A fresh
    // random challenge keeps every ceremony well-formed.
    val bytes = ByteArray(32)
    SecureRandom().nextBytes(bytes)
    return b64url(bytes)
}

private fun prfExtension(salt: String): JSONObject =
    JSONObject().put("prf", JSONObject().put("eval", JSONObject().put("first", salt)))

/** `clientExtensionResults.prf` of a registration or assertion response. */
private fun prfOf(responseJson: String): Pair<String, JSONObject?> {
    val json = JSONObject(responseJson)
    val id = json.optString("rawId").ifEmpty { json.optString("id") }
    val prf = json.optJSONObject("clientExtensionResults")?.optJSONObject("prf")
    return Pair(id, prf)
}

private fun firstResult(prf: JSONObject?): String? =
    prf?.optJSONObject("results")?.optString("first")?.takeIf { it.isNotEmpty() }

@TauriPlugin
class PasskeyPlugin(private val activity: Activity) : Plugin(activity) {
    private val manager: CredentialManager by lazy { CredentialManager.create(activity) }
    private val executor = Executors.newSingleThreadExecutor()

    @Command
    fun isSupported(invoke: Invoke) {
        val ret = JSObject()
        ret.put("supported", Build.VERSION.SDK_INT >= MIN_SDK)
        ret.put("sdk", Build.VERSION.SDK_INT)
        invoke.resolve(ret)
    }

    /**
     * Register a discoverable passkey and get its PRF output. Providers that
     * don't evaluate PRF while creating get a second prompt (`get`) for it.
     */
    @Command
    fun create(invoke: Invoke) {
        try {
            if (Build.VERSION.SDK_INT < MIN_SDK) throw Failure("NOT_SUPPORTED", "Android ${Build.VERSION.SDK_INT}")
            val args = invoke.parseArgs(CreateArgs::class.java)
            val options = JSONObject()
                .put("challenge", challenge())
                .put("rp", JSONObject().put("id", args.rpId).put("name", args.rpName))
                .put(
                    "user",
                    JSONObject().put("id", args.userId).put("name", args.userName).put("displayName", args.userName),
                )
                .put(
                    "pubKeyCredParams",
                    JSONArray()
                        .put(JSONObject().put("type", "public-key").put("alg", -7))
                        .put(JSONObject().put("type", "public-key").put("alg", -257)),
                )
                .put("timeout", 180000)
                .put("attestation", "none")
                .put(
                    "authenticatorSelection",
                    JSONObject()
                        .put("residentKey", "required")
                        .put("requireResidentKey", true)
                        .put("userVerification", "required"),
                )
                .put("extensions", prfExtension(args.prfSalt))
            val request = CreatePublicKeyCredentialRequest(options.toString())
            manager.createCredentialAsync(
                activity,
                request,
                CancellationSignal(),
                executor,
                object : CredentialManagerCallback<CreateCredentialResponse, CreateCredentialException> {
                    override fun onResult(result: CreateCredentialResponse) {
                        try {
                            val response = result as? CreatePublicKeyCredentialResponse
                                ?: throw Failure("OTHER", "unexpected credential type")
                            val (id, prf) = prfOf(response.registrationResponseJson)
                            if (id.isEmpty()) throw Failure("OTHER", "no credential id")
                            val first = firstResult(prf)
                            Log.i(TAG, "create: ok, PRF ${if (first != null) "returned at creation" else "not returned; signing in"}, prf=${prf?.keys()?.asSequence()?.toList()}")
                            when {
                                first != null -> resolve(invoke, id, first)
                                // The provider says outright it has no PRF.
                                prf != null && prf.has("enabled") && !prf.optBoolean("enabled") ->
                                    throw Failure("PRF_UNSUPPORTED", "PRF not enabled")
                                // Enabled (or not said): evaluate it with a sign-in.
                                else -> signIn(invoke, args.rpId, args.prfSalt, listOf(id))
                            }
                        } catch (f: Failure) {
                            reject(invoke, f)
                        } catch (e: Exception) {
                            Log.w(TAG, "create: bad response", e)
                            invoke.reject(e.message ?: "passkey error", "OTHER")
                        }
                    }

                    override fun onError(e: CreateCredentialException) {
                        Log.w(TAG, "create: ${e.type}: ${e.errorMessage}")
                        reject(invoke, createFailure(e))
                    }
                },
            )
        } catch (f: Failure) {
            reject(invoke, f)
        } catch (e: Exception) {
            Log.w(TAG, "create failed", e)
            invoke.reject(e.message ?: "passkey error", "OTHER")
        }
    }

    /** Sign in with a passkey (the user picks one) and get its PRF output. */
    @Command
    fun get(invoke: Invoke) {
        try {
            if (Build.VERSION.SDK_INT < MIN_SDK) throw Failure("NOT_SUPPORTED", "Android ${Build.VERSION.SDK_INT}")
            val args = invoke.parseArgs(GetArgs::class.java)
            signIn(invoke, args.rpId, args.prfSalt, args.allowCredentials ?: emptyList())
        } catch (f: Failure) {
            reject(invoke, f)
        } catch (e: Exception) {
            Log.w(TAG, "get failed", e)
            invoke.reject(e.message ?: "passkey error", "OTHER")
        }
    }

    private fun signIn(invoke: Invoke, rpId: String, prfSalt: String, allow: List<String>) {
        val options = JSONObject()
            .put("challenge", challenge())
            .put("rpId", rpId)
            .put("timeout", 180000)
            .put("userVerification", "required")
            .put("extensions", prfExtension(prfSalt))
        if (allow.isNotEmpty()) {
            val list = JSONArray()
            for (id in allow) list.put(JSONObject().put("type", "public-key").put("id", id))
            options.put("allowCredentials", list)
        }
        val request = GetCredentialRequest(listOf(GetPublicKeyCredentialOption(options.toString())))
        manager.getCredentialAsync(
            activity,
            request,
            CancellationSignal(),
            executor,
            object : CredentialManagerCallback<GetCredentialResponse, GetCredentialException> {
                override fun onResult(result: GetCredentialResponse) {
                    try {
                        val credential = result.credential as? PublicKeyCredential
                            ?: throw Failure("OTHER", "unexpected credential type")
                        val (id, prf) = prfOf(credential.authenticationResponseJson)
                        Log.i(TAG, "get: ok, PRF ${if (prf != null) "present" else "missing"}")
                        val first = firstResult(prf)
                            ?: throw Failure("PRF_UNSUPPORTED", "no PRF result")
                        resolve(invoke, id, first)
                    } catch (f: Failure) {
                        reject(invoke, f)
                    } catch (e: Exception) {
                        Log.w(TAG, "get: bad response", e)
                        invoke.reject(e.message ?: "passkey error", "OTHER")
                    }
                }

                override fun onError(e: GetCredentialException) {
                    Log.w(TAG, "get: ${e.type}: ${e.errorMessage}")
                    reject(invoke, getFailure(e))
                }
            },
        )
    }

    private fun resolve(invoke: Invoke, credentialId: String, prf: String) {
        val ret = JSObject()
        ret.put("credentialId", credentialId)
        ret.put("prf", prf)
        invoke.resolve(ret)
    }

    private fun reject(invoke: Invoke, f: Failure) {
        Log.w(TAG, "${f.code}: ${f.message}")
        invoke.reject(f.message, f.code)
    }

    private fun createFailure(e: CreateCredentialException): Failure {
        val msg = e.errorMessage?.toString() ?: e.type
        return when (e) {
            is CreateCredentialCancellationException -> Failure("CANCELLED", msg)
            is CreateCredentialInterruptedException -> Failure("OTHER", "interrupted, try again")
            is CreateCredentialProviderConfigurationException -> Failure("NOT_SUPPORTED", msg)
            is CreateCredentialUnsupportedException -> Failure("NOT_SUPPORTED", msg)
            is CreatePublicKeyCredentialDomException -> when (e.domError) {
                is SecurityError -> Failure("NOT_TRUSTED", msg)
                is NotAllowedError -> Failure("CANCELLED", msg)
                else -> Failure("OTHER", msg)
            }
            else -> Failure("OTHER", msg)
        }
    }

    private fun getFailure(e: GetCredentialException): Failure {
        val msg = e.errorMessage?.toString() ?: e.type
        return when (e) {
            is GetCredentialCancellationException -> Failure("CANCELLED", msg)
            is GetCredentialInterruptedException -> Failure("OTHER", "interrupted, try again")
            is NoCredentialException -> Failure("NO_CREDENTIAL", msg)
            is GetCredentialProviderConfigurationException -> Failure("NOT_SUPPORTED", msg)
            is GetCredentialUnsupportedException -> Failure("NOT_SUPPORTED", msg)
            is GetPublicKeyCredentialDomException -> when (e.domError) {
                is SecurityError -> Failure("NOT_TRUSTED", msg)
                is NotAllowedError -> Failure("CANCELLED", msg)
                else -> Failure("OTHER", msg)
            }
            else -> Failure("OTHER", msg)
        }
    }
}
