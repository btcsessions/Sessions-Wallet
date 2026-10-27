package io.btcsessions.marmot

import android.app.Activity
import android.util.Log
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import dev.ipf.marmotkit.AppMessageRecordFfi
import dev.ipf.marmotkit.CreateGroupOptionsFfi
import dev.ipf.marmotkit.Marmot
import dev.ipf.marmotkit.MarmotAndroid
import dev.ipf.marmotkit.MarmotKitException
import dev.ipf.marmotkit.MarmotOptions
import dev.ipf.marmotkit.SecretStore
import dev.ipf.marmotkit.marmotDeploymentNeutral
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

private const val TAG = "SessionsMarmot"

/**
 * The account key, in memory only. The wallet hands it over on start (it's
 * derived from the seed, which the wallet keeps); nothing is written to the
 * phone's keystore, so a locked or wiped wallet leaves no key behind.
 */
private class MemorySecrets : SecretStore {
    private val byAccount = ConcurrentHashMap<String, String>()
    private val labels = ConcurrentHashMap<String, String>()

    fun put(accountHex: String, secretHex: String) {
        byAccount[accountHex] = secretHex
    }

    fun clear() {
        byAccount.clear()
        labels.clear()
    }

    override fun hasSecretForLabel(label: String): Boolean =
        labels[label]?.let { byAccount.containsKey(it) } ?: false

    override fun hasSecretForAccountId(accountIdHex: String): Boolean = byAccount.containsKey(accountIdHex)

    override fun writeSecret(label: String, accountIdHex: String, secretKeyHex: String) {
        labels[label] = accountIdHex
        byAccount[accountIdHex] = secretKeyHex
    }

    override fun loadSecret(label: String, accountIdHex: String): String {
        val id = accountIdHex.ifEmpty { labels[label] ?: "" }
        return byAccount[id] ?: throw MarmotKitException.SecretNotFound("no key for this account")
    }

    override fun removeSecret(label: String, accountIdHex: String) {
        // The wallet owns the key: Marmot forgetting an account doesn't erase it.
        labels.remove(label)
    }
}

@InvokeArg
class StartArgs {
    var root: String = ""
    var relays: Array<String> = arrayOf()
    var secretHex: String = ""
    var pubkeyHex: String = ""
}

@InvokeArg
class NameArgs {
    var name: String = ""
}

@InvokeArg
class GroupArgs {
    var groupId: String = ""
}

@InvokeArg
class MembersArgs {
    var groupId: String = ""
    var members: Array<String> = arrayOf()
}

@InvokeArg
class SendArgs {
    var groupId: String = ""
    var kind: Long = 0
    var content: String = ""
}

@InvokeArg
class TextArgs {
    var groupId: String = ""
    var text: String = ""
}

@InvokeArg
class MessagesArgs {
    var groupId: String = ""
    var kinds: Array<Long> = arrayOf()
    var limit: Long = 500
}

/** Group vault messaging: MarmotKit (MLS over Nostr) in its own native library. */
@TauriPlugin
class MarmotPlugin(private val activity: Activity) : Plugin(activity) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val lock = Mutex()
    private val secrets = MemorySecrets()
    private var marmot: Marmot? = null
    private var account: String? = null
    private var root: String? = null

    companion object {
        @Volatile private var initialized = false
    }

    private fun message(e: Throwable): String = when (e) {
        is MarmotKitException -> (e.message ?: e.javaClass.simpleName).removePrefix("details=").removePrefix("message=")
        else -> e.message ?: e.javaClass.simpleName
    }

    /** Run `block` off the main thread and answer the invoke. */
    private fun run(invoke: Invoke, what: String, block: suspend () -> JSObject) {
        scope.launch {
            try {
                invoke.resolve(block())
            } catch (e: Throwable) {
                Log.w(TAG, "$what failed", e)
                invoke.reject(message(e))
            }
        }
    }

    private fun ready(): Pair<Marmot, String> {
        val m = marmot ?: throw IllegalStateException("Group messaging isn't started")
        val a = account ?: throw IllegalStateException("Group messaging isn't signed in")
        return m to a
    }

    private fun nsec(secretHex: String): String = Bech32.encode("nsec", hexToBytes(secretHex))

    @Command
    fun start(invoke: Invoke) {
        val args = invoke.parseArgs(StartArgs::class.java)
        run(invoke, "start") {
            lock.withLock {
                val current = marmot
                if (current != null && account == args.pubkeyHex && root == args.root) {
                    return@withLock JSObject().put("account", args.pubkeyHex).put("running", true)
                }
                if (current != null) {
                    try { current.shutdownAndClose() } catch (e: Throwable) { Log.w(TAG, "close", e) }
                    marmot = null
                    account = null
                }
                if (!initialized) {
                    MarmotAndroid.initialize(activity.applicationContext)
                    initialized = true
                }
                secrets.put(args.pubkeyHex, args.secretHex)
                val relays = args.relays.toList()
                val options = MarmotOptions(
                    relayPolicy = null,
                    cursorPersistence = null,
                    clientName = "Sessions Wallet",
                    secretStore = secrets,
                    attachmentAcquisitionMode = null,
                    moderationReportConfig = null,
                    deployment = marmotDeploymentNeutral(),
                )
                val m = Marmot.newWithConfiguration(args.root, relays, options)
                m.start()
                val known = m.listAccounts().any { it.accountIdHex == args.pubkeyHex }
                if (!known) {
                    m.login(nsec(args.secretHex), relays, relays, relays)
                } else {
                    try { m.signInAccount(args.pubkeyHex) } catch (e: Throwable) { Log.i(TAG, "sign in: ${e.message}") }
                }
                marmot = m
                account = args.pubkeyHex
                root = args.root
                JSObject().put("account", args.pubkeyHex).put("running", true)
            }
        }
    }

    @Command
    fun stop(invoke: Invoke) {
        run(invoke, "stop") {
            lock.withLock {
                marmot?.let { try { it.shutdownAndClose() } catch (e: Throwable) { Log.w(TAG, "close", e) } }
                marmot = null
                account = null
                secrets.clear()
            }
            JSObject()
        }
    }

    @Command
    fun createGroup(invoke: Invoke) {
        val args = invoke.parseArgs(NameArgs::class.java)
        run(invoke, "createGroup") {
            val (m, a) = ready()
            val id = m.createGroupWithOptions(a, args.name, listOf(), CreateGroupOptionsFfi(description = null, initialImage = null, disappearingMessageSecs = 0uL))
            JSObject().put("groupId", id)
        }
    }

    @Command
    fun invite(invoke: Invoke) {
        val args = invoke.parseArgs(MembersArgs::class.java)
        run(invoke, "invite") {
            val (m, a) = ready()
            m.inviteMembers(a, args.groupId, args.members.toList())
            JSObject()
        }
    }

    @Command
    fun remove(invoke: Invoke) {
        val args = invoke.parseArgs(MembersArgs::class.java)
        run(invoke, "remove") {
            val (m, a) = ready()
            m.removeMembers(a, args.groupId, args.members.toList())
            JSObject()
        }
    }

    @Command
    fun accept(invoke: Invoke) {
        val args = invoke.parseArgs(GroupArgs::class.java)
        run(invoke, "accept") {
            val (m, a) = ready()
            m.acceptGroupInvite(a, args.groupId)
            JSObject()
        }
    }

    @Command
    fun decline(invoke: Invoke) {
        val args = invoke.parseArgs(GroupArgs::class.java)
        run(invoke, "decline") {
            val (m, a) = ready()
            m.declineGroupInvite(a, args.groupId)
            JSObject()
        }
    }

    @Command
    fun groups(invoke: Invoke) {
        run(invoke, "groups") {
            val (m, a) = ready()
            val items = JSArray()
            for (row in m.chatList(a, true)) {
                val o = JSObject()
                    .put("groupId", row.groupIdHex)
                    .put("name", row.groupName.ifEmpty { row.title })
                    .put("pending", row.pendingConfirmation)
                    .put("unread", row.unreadCount.toLong())
                if (row.pendingConfirmation) {
                    try {
                        val d = m.groupDetails(a, row.groupIdHex)
                        d.group.welcomerAccountIdHex?.let { o.put("welcomer", it) }
                    } catch (e: Throwable) {
                        Log.i(TAG, "details: ${e.message}")
                    }
                }
                items.put(o)
            }
            JSObject().put("items", items)
        }
    }

    @Command
    fun members(invoke: Invoke) {
        val args = invoke.parseArgs(GroupArgs::class.java)
        run(invoke, "members") {
            val (m, a) = ready()
            val members = JSArray()
            for (x in m.groupMembers(a, args.groupId)) members.put(x.memberIdHex)
            val admins = JSArray()
            try {
                for (x in m.groupDetails(a, args.groupId).group.admins) admins.put(x)
            } catch (e: Throwable) {
                Log.i(TAG, "admins: ${e.message}")
            }
            JSObject().put("members", members).put("admins", admins)
        }
    }

    @Command
    fun send(invoke: Invoke) {
        val args = invoke.parseArgs(SendArgs::class.java)
        run(invoke, "send") {
            val (m, a) = ready()
            m.sendCustomEvent(a, args.groupId, args.kind.toULong(), listOf(), args.content)
            JSObject()
        }
    }

    @Command
    fun sendText(invoke: Invoke) {
        val args = invoke.parseArgs(TextArgs::class.java)
        run(invoke, "sendText") {
            val (m, a) = ready()
            m.sendText(a, args.groupId, args.text)
            JSObject()
        }
    }

    private fun json(r: AppMessageRecordFfi): JSObject = JSObject()
        .put("id", r.messageIdHex)
        .put("sender", r.sender)
        .put("kind", r.kind.toLong())
        .put("content", r.plaintext)
        .put("createdAt", r.recordedAt.toLong())

    @Command
    fun messages(invoke: Invoke) {
        val args = invoke.parseArgs(MessagesArgs::class.java)
        run(invoke, "messages") {
            val (m, a) = ready()
            val kinds = if (args.kinds.isEmpty()) null else args.kinds.map { it.toULong() }
            val list = m.messages(a, args.groupId, args.limit.toUInt(), kinds)
            val items = JSArray()
            list.sortedBy { it.recordedAt }.forEach { items.put(json(it)) }
            JSObject().put("items", items)
        }
    }
}

private fun hexToBytes(hex: String): ByteArray {
    require(hex.length % 2 == 0) { "odd hex" }
    return ByteArray(hex.length / 2) { i -> hex.substring(i * 2, i * 2 + 2).toInt(16).toByte() }
}

/** BIP-173 bech32, enough for an nsec. */
private object Bech32 {
    private const val CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"
    private val GEN = intArrayOf(0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3)

    private fun polymod(values: IntArray): Int {
        var chk = 1
        for (v in values) {
            val b = chk ushr 25
            chk = ((chk and 0x1ffffff) shl 5) xor v
            for (i in 0 until 5) if (((b ushr i) and 1) == 1) chk = chk xor GEN[i]
        }
        return chk
    }

    private fun hrpExpand(hrp: String): IntArray =
        IntArray(hrp.length * 2 + 1) { i ->
            when {
                i < hrp.length -> hrp[i].code ushr 5
                i == hrp.length -> 0
                else -> hrp[i - hrp.length - 1].code and 31
            }
        }

    private fun toWords(data: ByteArray): IntArray {
        val out = ArrayList<Int>()
        var acc = 0
        var bits = 0
        for (b in data) {
            acc = (acc shl 8) or (b.toInt() and 0xff)
            bits += 8
            while (bits >= 5) {
                bits -= 5
                out.add((acc ushr bits) and 31)
            }
        }
        if (bits > 0) out.add((acc shl (5 - bits)) and 31)
        return out.toIntArray()
    }

    fun encode(hrp: String, data: ByteArray): String {
        val words = toWords(data)
        val values = hrpExpand(hrp) + words + IntArray(6)
        val mod = polymod(values) xor 1
        val checksum = IntArray(6) { i -> (mod ushr (5 * (5 - i))) and 31 }
        return hrp + "1" + (words + checksum).joinToString("") { CHARSET[it].toString() }
    }
}
