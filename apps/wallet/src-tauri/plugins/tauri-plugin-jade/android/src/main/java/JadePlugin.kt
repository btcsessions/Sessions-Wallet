package io.btcsessions.jade

import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.BluetoothStatusCodes
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.location.LocationManager
import android.os.Build
import android.os.ParcelUuid
import android.util.Base64
import android.util.Log
import androidx.core.content.ContextCompat
import androidx.core.location.LocationManagerCompat
import app.tauri.PermissionState
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

private const val TAG = "SessionsJade"

/** Jade's Nordic-UART-style service. We write RX; Jade indicates on TX. */
private val SERVICE: UUID = UUID.fromString("6e400001-b5a3-f393-e0a9-e50e24dcca9e")
private val RX: UUID = UUID.fromString("6e400002-b5a3-f393-e0a9-e50e24dcca9e")
private val TX: UUID = UUID.fromString("6e400003-b5a3-f393-e0a9-e50e24dcca9e")
private val CCCD: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

/** GATT statuses meaning the link isn't encrypted with a bond Jade knows. */
private val AUTH_STATUSES = setOf(
    BluetoothGatt.GATT_INSUFFICIENT_AUTHENTICATION, // 5
    BluetoothGatt.GATT_INSUFFICIENT_ENCRYPTION, // 15
    137, // GATT_AUTH_FAIL
)

/** Where the last Jade we connected to is remembered (its identity address). */
private const val PREFS = "sessions_jade"
private const val KEY_ADDRESS = "address"

@InvokeArg
class ConnectArgs {
    lateinit var onData: Channel
    var scanMs: Long = 15000
    /** Remove the phone's (stale) pairing with the last Jade first, then pair afresh. */
    var repair: Boolean = false
}

@InvokeArg
class WriteArgs {
    var data: String = ""
}

/** A failure with a code the Rust side maps to plain English (the command is rejected). */
private class Failure(val code: String, message: String) : Exception(message)

/**
 * The link to Jade failed in a way the Rust side decides about (retry, re-pair
 * or give up): `connect` resolves with these facts instead of rejecting.
 * [kind]: "connect" (no link), "dropped" (link went down while setting up),
 * "auth" (Jade refused our encryption keys), "bondLost" (Android dropped the
 * pairing). [status]: the GATT status / HCI reason Android gave.
 */
private class LinkFailure(val kind: String, val status: Int) : Exception("$kind (status $status)")

/** Results of GATT and bonding callbacks, handed to the worker thread. */
private sealed class Ev {
    data class Conn(val status: Int, val state: Int) : Ev()
    data class Mtu(val mtu: Int, val status: Int) : Ev()
    data class Services(val status: Int) : Ev()
    data class DescWrite(val status: Int) : Ev()
    data class CharWrite(val status: Int) : Ev()
    data class Bond(val state: Int) : Ev()
}

/**
 * Talks to a Blockstream Jade over Bluetooth LE for the wallet's Rust code
 * (not the webview). Every GATT operation runs on one worker thread, one at a
 * time, waiting for its callback: Android allows only one in flight.
 *
 * The link stays up between operations (Jade locks itself when it drops);
 * Rust sends keepalive pings and reconnects when needed. Jade uses resolvable
 * private addresses, so a Jade the phone is already paired with is reached
 * by its bonded identity address, never paired again by accident. When the
 * phone's pairing is stale (Jade was reset or forgot it), Rust asks for
 * `connect` with `repair`: the old pairing is removed and Android shows its
 * pairing prompt again.
 *
 * `adb logcat -s SessionsJade` shows what happened.
 */
@SuppressLint("MissingPermission")
@TauriPlugin(
    permissions = [
        Permission(
            strings = ["android.permission.BLUETOOTH_SCAN", "android.permission.BLUETOOTH_CONNECT"],
            alias = "bluetooth",
        ),
        Permission(strings = ["android.permission.ACCESS_FINE_LOCATION"], alias = "location"),
    ],
)
class JadePlugin(private val activity: Activity) : Plugin(activity) {
    private val worker = Executors.newSingleThreadExecutor()
    private val events = LinkedBlockingQueue<Ev>()

    @Volatile private var gatt: BluetoothGatt? = null
    @Volatile private var channel: Channel? = null
    @Volatile private var device: BluetoothDevice? = null
    private var rx: BluetoothGattCharacteristic? = null
    private var chunk = 20

    /** The Jade of the last connect attempt: what `repair` un-pairs. */
    @Volatile private var lastTarget: BluetoothDevice? = null
    /** This attempt started with the phone already paired to the target. */
    private var wasBonded = false
    /** This attempt paired just now. */
    private var freshBond = false
    /** Bond-state changes are expected (pairing or un-pairing in progress). */
    @Volatile private var bondingNow = false
    @Volatile private var foreground = true

    /** Android 12+ asks for "Nearby devices"; older versions for location. */
    private val permissionAlias: String
        get() = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) "bluetooth" else "location"

    @Command
    fun connect(invoke: Invoke) {
        if (getPermissionState(permissionAlias) != PermissionState.GRANTED) {
            requestPermissionForAlias(permissionAlias, invoke, "afterPermission")
            return
        }
        worker.execute { doConnect(invoke) }
    }

    @PermissionCallback
    fun afterPermission(invoke: Invoke) {
        if (getPermissionState(permissionAlias) == PermissionState.GRANTED) {
            worker.execute { doConnect(invoke) }
        } else {
            invoke.reject("Bluetooth access was not allowed", "PERMISSION_DENIED")
        }
    }

    @Command
    fun write(invoke: Invoke) {
        worker.execute {
            try {
                val args = invoke.parseArgs(WriteArgs::class.java)
                val bytes = Base64.decode(args.data, Base64.DEFAULT)
                var at = 0
                while (at < bytes.size) {
                    val end = minOf(at + chunk, bytes.size)
                    writeChunk(bytes.copyOfRange(at, end))
                    at = end
                }
                invoke.resolve()
            } catch (f: Failure) {
                Log.w(TAG, "write: ${f.code} ${f.message}")
                invoke.reject(f.message, f.code)
            } catch (l: LinkFailure) {
                Log.w(TAG, "write: link ${l.message}")
                invoke.reject("Jade disconnected", "DISCONNECTED")
            } catch (e: Exception) {
                Log.w(TAG, "write failed", e)
                invoke.reject(e.message ?: "Bluetooth error", "OTHER")
            }
        }
    }

    @Command
    fun disconnect(invoke: Invoke) {
        worker.execute {
            Log.d(TAG, "disconnect (asked)")
            closeGatt()
            invoke.resolve()
        }
    }

    // --- app in the background --------------------------------------------------

    // The no-argument variants: AppCompatActivity isn't on this module's classpath.
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onStop() {
        foreground = false
        tellForeground()
    }

    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onResume() {
        foreground = true
        tellForeground()
    }

    /** Rust pauses its keepalive pings while the app is in the background. */
    private fun tellForeground() {
        val ch = channel ?: return
        val msg = JSObject()
        msg.put("background", !foreground)
        ch.send(msg)
    }

    // --- connecting -----------------------------------------------------------

    private fun doConnect(invoke: Invoke) {
        val context = activity.applicationContext
        val receiver = bondReceiver()
        ContextCompat.registerReceiver(
            context,
            receiver,
            IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED),
            ContextCompat.RECEIVER_EXPORTED,
        )
        try {
            val args = invoke.parseArgs(ConnectArgs::class.java)
            Log.d(TAG, "connect: repair=${args.repair}")
            closeGatt()
            val manager = activity.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager?
            val adapter = manager?.adapter ?: throw Failure("NO_BLUETOOTH", "This phone has no Bluetooth")
            if (!adapter.isEnabled) throw Failure("BLUETOOTH_OFF", "Bluetooth is off")
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
                val location = activity.getSystemService(Context.LOCATION_SERVICE) as LocationManager?
                if (location != null && !LocationManagerCompat.isLocationEnabled(location)) {
                    throw Failure("LOCATION_OFF", "Location is off")
                }
            }
            if (args.repair) unpair()

            val known = knownJades(adapter)
            val results = scan(adapter, args.scanMs, known.map { it.address }.toSet())
            if (results.isEmpty()) throw Failure("NOT_FOUND", "No Jade found")
            val targets = chooseTargets(results, known, args.repair)
            val progress = JSObject()
            progress.put("connecting", true)
            args.onData.send(progress)

            for ((i, target) in targets.withIndex()) {
                val last = i == targets.lastIndex
                try {
                    val ret = connectTo(target, args.onData, if (last) 2 else 1, if (last) 20_000 else 10_000)
                    invoke.resolve(ret)
                    return
                } catch (l: LinkFailure) {
                    closeGatt()
                    if (last || l.kind != "connect") throw l
                    Log.d(TAG, "connect: paired Jade ${target.address} didn't answer; trying ${targets[i + 1].address}")
                }
            }
        } catch (l: LinkFailure) {
            Log.w(TAG, "connect: ${l.message}, wasBonded=$wasBonded freshBond=$freshBond")
            closeGatt()
            val failure = JSObject()
            failure.put("kind", l.kind)
            failure.put("status", l.status)
            failure.put("bonded", wasBonded)
            failure.put("freshBond", freshBond)
            val ret = JSObject()
            ret.put("failure", failure)
            invoke.resolve(ret)
        } catch (f: Failure) {
            Log.w(TAG, "connect: ${f.code} ${f.message}")
            closeGatt()
            invoke.reject(f.message, f.code)
        } catch (e: Exception) {
            Log.w(TAG, "connect failed", e)
            closeGatt()
            invoke.reject(e.message ?: "Bluetooth error", "OTHER")
        } finally {
            try {
                context.unregisterReceiver(receiver)
            } catch (ignored: IllegalArgumentException) {
                // Already unregistered.
            }
        }
    }

    /** Connect, pair if needed, turn on indications. Returns `{name, mtu}`. */
    private fun connectTo(target: BluetoothDevice, onData: Channel, attempts: Int, timeoutMs: Long): JSObject {
        events.clear()
        device = target
        lastTarget = target
        wasBonded = target.bondState == BluetoothDevice.BOND_BONDED
        freshBond = false
        Log.d(TAG, "connect: ${target.address} (${target.name}), bonded=$wasBonded")
        val g = connectGatt(target, attempts, timeoutMs)

        // Bigger packets: Jade accepts up to 517.
        var mtu = 23
        if (g.requestMtu(517)) {
            val m = poll<Ev.Mtu>(5_000)
            if (m != null && m.status == BluetoothGatt.GATT_SUCCESS) mtu = m.mtu
        }
        chunk = (mtu - 3).coerceIn(20, 512)

        if (!g.discoverServices()) throw Failure("OTHER", "Couldn't read Jade's services")
        val services = await<Ev.Services>(15_000)
        if (services.status != BluetoothGatt.GATT_SUCCESS) {
            throw Failure("OTHER", "Couldn't read Jade's services (${services.status})")
        }
        val service = g.getService(SERVICE) ?: throw Failure("OTHER", "This isn't a Jade")
        val tx = service.getCharacteristic(TX) ?: throw Failure("OTHER", "This isn't a Jade")
        rx = service.getCharacteristic(RX) ?: throw Failure("OTHER", "This isn't a Jade")

        // Jade only talks over an encrypted, bonded link. Android shows its
        // pairing dialog; Jade shows the same number to confirm.
        if (target.bondState != BluetoothDevice.BOND_BONDED) {
            val pairing = JSObject()
            pairing.put("pairing", true)
            onData.send(pairing)
            bond(target)
            freshBond = true
            // Let the link settle on the new keys (Green does the same).
            Thread.sleep(1_500)
        }

        subscribe(g, tx)
        remember(target)
        // From here on, what Jade sends goes to Rust (and a later disconnect).
        channel = onData
        Log.d(TAG, "connected: mtu=$mtu freshBond=$freshBond")

        val ret = JSObject()
        ret.put("name", target.name ?: "Jade")
        ret.put("mtu", mtu)
        return ret
    }

    /** Connect, retrying on failure: Android often fails the first attempt (status 133). */
    private fun connectGatt(target: BluetoothDevice, attempts: Int, timeoutMs: Long): BluetoothGatt {
        var status = -1
        for (attempt in 0 until attempts) {
            val g = target.connectGatt(activity, false, callback, BluetoothDevice.TRANSPORT_LE)
                ?: throw Failure("OTHER", "Couldn't connect to Jade")
            gatt = g
            // A disconnect arrives as an Ev.Conn here; only a dropped pairing throws.
            val conn = poll<Ev.Conn>(timeoutMs)
            if (conn != null && conn.state == BluetoothProfile.STATE_CONNECTED) return g
            status = conn?.status ?: -1
            Log.d(TAG, "connectGatt: attempt ${attempt + 1} failed, status=$status")
            gatt = null
            try {
                g.disconnect()
                g.close()
            } catch (ignored: Exception) {
                // Already gone.
            }
            events.clear()
            if (attempt + 1 < attempts) Thread.sleep(600)
        }
        throw LinkFailure("connect", status)
    }

    /** Paired Jades: the one we used last (if still paired), else any paired device named Jade. */
    private fun knownJades(adapter: BluetoothAdapter): List<BluetoothDevice> {
        val bonded = adapter.bondedDevices.orEmpty()
        val remembered = prefs().getString(KEY_ADDRESS, null)
        val last = bonded.filter { it.address == remembered }
        val named = bonded.filter { it.address != remembered && (it.name ?: "").startsWith("Jade") }
        return last + named
    }

    /**
     * Who to connect to, in order. A scanned Jade the phone is paired with,
     * if Android resolved its address; otherwise the paired Jade by identity
     * address first (Jade advertises from rotating private addresses, which
     * Android doesn't always map back), then the nearest scanned one.
     */
    private fun chooseTargets(
        results: Collection<ScanResult>,
        known: List<BluetoothDevice>,
        repair: Boolean,
    ): List<BluetoothDevice> {
        val scanned = results.sortedByDescending { it.rssi }.map { it.device }
        val knownAddresses = known.map { it.address }.toSet()
        scanned.firstOrNull { it.address in knownAddresses }?.let { return listOf(it) }
        val nearest = scanned.first()
        // known[0] is the Jade used last, if it's still paired.
        if (!repair && known.isNotEmpty()) return listOf(known[0], nearest)
        return listOf(nearest)
    }

    /** Scan for Jade's service; the strongest one seen within a moment of the first. */
    private fun scan(adapter: BluetoothAdapter, timeoutMs: Long, known: Set<String>): Collection<ScanResult> {
        val scanner = adapter.bluetoothLeScanner ?: throw Failure("BLUETOOTH_OFF", "Bluetooth is off")
        val seen = ConcurrentHashMap<String, ScanResult>()
        val failed = AtomicInteger(0)
        val cb = object : ScanCallback() {
            override fun onScanResult(callbackType: Int, result: ScanResult) {
                seen[result.device.address] = result
            }

            override fun onBatchScanResults(results: MutableList<ScanResult>) {
                for (r in results) seen[r.device.address] = r
            }

            override fun onScanFailed(errorCode: Int) {
                failed.set(errorCode)
            }
        }
        val filters = listOf(ScanFilter.Builder().setServiceUuid(ParcelUuid(SERVICE)).build())
        val settings = ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY).build()
        scanner.startScan(filters, settings, cb)
        try {
            val deadline = System.currentTimeMillis() + timeoutMs
            var firstSeen = 0L
            while (System.currentTimeMillis() < deadline && failed.get() == 0) {
                Thread.sleep(200)
                // Our paired Jade: no need to wait for others.
                if (seen.keys.any { it in known }) break
                if (seen.isNotEmpty()) {
                    if (firstSeen == 0L) firstSeen = System.currentTimeMillis()
                    // Give a second Jade nearby a moment to show up, then pick the closest.
                    if (System.currentTimeMillis() - firstSeen > 1_500) break
                }
            }
        } finally {
            try {
                scanner.stopScan(cb)
            } catch (ignored: Exception) {
                // Bluetooth switched off meanwhile.
            }
        }
        Log.d(TAG, "scan: ${seen.size} Jade(s) ${seen.values.joinToString { "${it.device.address}@${it.rssi}" }}, known=$known")
        if (failed.get() != 0 && seen.isEmpty()) throw Failure("OTHER", "Bluetooth scan failed (${failed.get()})")
        return seen.values
    }

    /** Pair: Android shows its dialog, Jade the matching number. */
    private fun bond(target: BluetoothDevice) {
        bondingNow = true
        try {
            if (target.bondState == BluetoothDevice.BOND_BONDED) return
            if (target.bondState != BluetoothDevice.BOND_BONDING && !target.createBond()) {
                throw Failure("PAIRING_FAILED", "Couldn't pair with Jade")
            }
            Log.d(TAG, "bond: pairing with ${target.address}")
            // The user reads and confirms the number on both screens.
            val start = System.currentTimeMillis()
            var sawBonding = false
            while (true) {
                val elapsed = System.currentTimeMillis() - start
                if (elapsed > 120_000) throw Failure("PAIRING_FAILED", "Pairing with Jade timed out")
                // Broadcasts can be missed: also look at the state directly.
                val state = try {
                    poll<Ev.Bond>(1_000)?.state ?: target.bondState
                } catch (l: LinkFailure) {
                    // Jade hangs up when the number is declined or not confirmed in time.
                    throw Failure("PAIRING_FAILED", "Pairing with Jade was declined (${l.status})")
                }
                when (state) {
                    BluetoothDevice.BOND_BONDED -> {
                        Log.d(TAG, "bond: paired")
                        return
                    }
                    BluetoothDevice.BOND_BONDING -> sawBonding = true
                    BluetoothDevice.BOND_NONE -> if (sawBonding || elapsed > 15_000) {
                        throw Failure("PAIRING_FAILED", "Pairing with Jade was declined")
                    }
                }
            }
        } finally {
            bondingNow = false
        }
    }

    /**
     * Remove the phone's pairing with the last Jade (it no longer matches what
     * Jade has), so the next connect pairs afresh. `removeBond` is hidden API,
     * reached by reflection as BLE apps commonly do; if Android refuses, the
     * person has to forget Jade in Bluetooth settings.
     */
    private fun unpair() {
        val target = lastTarget ?: knownJades(bluetoothAdapter() ?: return).firstOrNull() ?: return
        prefs().edit().remove(KEY_ADDRESS).apply()
        if (target.bondState == BluetoothDevice.BOND_NONE) {
            Log.d(TAG, "unpair: ${target.address} isn't paired any more")
            return
        }
        Log.d(TAG, "unpair: removing the stale pairing with ${target.address}")
        device = target
        bondingNow = true
        try {
            val removed = try {
                target.javaClass.getMethod("removeBond").invoke(target) as? Boolean ?: false
            } catch (e: Exception) {
                Log.w(TAG, "unpair: removeBond unavailable", e)
                false
            }
            if (!removed) {
                throw Failure("PAIRING_STALE", "Jade doesn't recognise this phone's pairing")
            }
            val deadline = System.currentTimeMillis() + 5_000
            while (target.bondState != BluetoothDevice.BOND_NONE && System.currentTimeMillis() < deadline) {
                events.poll(200, TimeUnit.MILLISECONDS)
            }
            Log.d(TAG, "unpair: bond state now ${target.bondState}")
            if (target.bondState != BluetoothDevice.BOND_NONE) {
                throw Failure("PAIRING_STALE", "Jade doesn't recognise this phone's pairing")
            }
        } finally {
            bondingNow = false
            device = null
            events.clear()
        }
        Thread.sleep(500)
    }

    /** Turn on indications (Jade's TX) or notifications, with a few retries. */
    private fun subscribe(g: BluetoothGatt, tx: BluetoothGattCharacteristic) {
        if (!g.setCharacteristicNotification(tx, true)) {
            throw Failure("OTHER", "Couldn't listen to Jade")
        }
        val cccd = tx.getDescriptor(CCCD) ?: throw Failure("OTHER", "This isn't a Jade")
        val value = if (tx.properties and BluetoothGattCharacteristic.PROPERTY_INDICATE != 0) {
            BluetoothGattDescriptor.ENABLE_INDICATION_VALUE
        } else {
            BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
        }
        var lastStatus = -1
        var authFailures = 0
        repeat(5) {
            val started = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                g.writeDescriptor(cccd, value) == BluetoothStatusCodes.SUCCESS
            } else {
                legacyWriteDescriptor(g, cccd, value)
            }
            if (started) {
                val ev = await<Ev.DescWrite>(10_000)
                if (ev.status == BluetoothGatt.GATT_SUCCESS) return
                lastStatus = ev.status
                Log.d(TAG, "subscribe: status ${ev.status}")
                // Encryption can lag a moment behind a fresh pairing; an old
                // pairing that still fails twice is one Jade no longer has.
                if (ev.status in AUTH_STATUSES && !freshBond && ++authFailures >= 2) {
                    throw LinkFailure("auth", ev.status)
                }
            }
            Thread.sleep(if (lastStatus in AUTH_STATUSES) 1_000 else 500)
        }
        if (lastStatus in AUTH_STATUSES) throw LinkFailure("auth", lastStatus)
        throw Failure("OTHER", "Couldn't listen to Jade ($lastStatus)")
    }

    // --- writing --------------------------------------------------------------

    private fun writeChunk(bytes: ByteArray) {
        val g = gatt ?: throw Failure("NOT_CONNECTED", "Jade isn't connected")
        val c = rx ?: throw Failure("NOT_CONNECTED", "Jade isn't connected")
        var tries = 0
        while (true) {
            val started = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                g.writeCharacteristic(c, bytes, BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT) ==
                    BluetoothStatusCodes.SUCCESS
            } else {
                legacyWrite(g, c, bytes)
            }
            if (started) break
            // The stack is still busy with the previous operation.
            if (++tries >= 20) throw Failure("OTHER", "Couldn't write to Jade")
            Thread.sleep(50)
        }
        val ev = await<Ev.CharWrite>(15_000)
        if (ev.status in AUTH_STATUSES) {
            throw Failure("PAIRING_STALE", "Jade doesn't recognise this phone's pairing")
        }
        if (ev.status != BluetoothGatt.GATT_SUCCESS) {
            throw Failure("OTHER", "Couldn't write to Jade (${ev.status})")
        }
    }

    // --- callbacks --------------------------------------------------------------

    private val callback = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            if (!ours(g)) return
            Log.d(TAG, "connection state $newState, status $status")
            events.offer(Ev.Conn(status, newState))
            if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                val ch = channel
                channel = null
                if (ch != null) {
                    val msg = JSObject()
                    msg.put("disconnected", true)
                    msg.put("status", status)
                    ch.send(msg)
                }
                // Free Android's GATT client slot (leaking them causes 133s later).
                worker.execute {
                    if (gatt === g) closeGatt()
                }
            }
        }

        override fun onMtuChanged(g: BluetoothGatt, mtu: Int, status: Int) {
            if (ours(g)) events.offer(Ev.Mtu(mtu, status))
        }

        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            if (ours(g)) events.offer(Ev.Services(status))
        }

        override fun onDescriptorWrite(g: BluetoothGatt, descriptor: BluetoothGattDescriptor, status: Int) {
            if (ours(g)) events.offer(Ev.DescWrite(status))
        }

        override fun onCharacteristicWrite(g: BluetoothGatt, characteristic: BluetoothGattCharacteristic, status: Int) {
            if (ours(g)) events.offer(Ev.CharWrite(status))
        }

        // Android 13+.
        override fun onCharacteristicChanged(
            g: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
        ) {
            received(g, characteristic, value)
        }

        // Android 12 and older.
        @Deprecated("Deprecated in Java")
        override fun onCharacteristicChanged(g: BluetoothGatt, characteristic: BluetoothGattCharacteristic) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                val value = legacyValue(characteristic) ?: return
                received(g, characteristic, value)
            }
        }
    }

    private fun received(g: BluetoothGatt, characteristic: BluetoothGattCharacteristic, value: ByteArray) {
        if (!ours(g) || characteristic.uuid != TX || value.isEmpty()) return
        val msg = JSObject()
        msg.put("data", Base64.encodeToString(value, Base64.NO_WRAP))
        channel?.send(msg)
    }

    /** Bond-state changes of the device we're working with, into the event queue. */
    private fun bondReceiver() = object : BroadcastReceiver() {
        override fun onReceive(ctx: Context, intent: Intent) {
            if (intent.action != BluetoothDevice.ACTION_BOND_STATE_CHANGED) return
            @Suppress("DEPRECATION")
            val d: BluetoothDevice? = intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE)
            if (d == null || d.address != device?.address) return
            val state = intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.BOND_NONE)
            Log.d(TAG, "bond state of ${d.address}: $state")
            events.offer(Ev.Bond(state))
        }
    }

    // --- helpers --------------------------------------------------------------

    // GATT calls for Android 12 and older (replaced by value-passing ones in 13).
    @Suppress("DEPRECATION")
    private fun legacyWriteDescriptor(g: BluetoothGatt, d: BluetoothGattDescriptor, value: ByteArray): Boolean {
        d.value = value
        return g.writeDescriptor(d)
    }

    @Suppress("DEPRECATION")
    private fun legacyWrite(g: BluetoothGatt, c: BluetoothGattCharacteristic, bytes: ByteArray): Boolean {
        c.writeType = BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
        c.value = bytes
        return g.writeCharacteristic(c)
    }

    @Suppress("DEPRECATION")
    private fun legacyValue(c: BluetoothGattCharacteristic): ByteArray? = c.value

    private fun bluetoothAdapter(): BluetoothAdapter? =
        (activity.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager?)?.adapter

    private fun prefs() = activity.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    private fun remember(target: BluetoothDevice) {
        if (target.bondState == BluetoothDevice.BOND_BONDED) {
            prefs().edit().putString(KEY_ADDRESS, target.address).apply()
        }
    }

    /** Callbacks can arrive before connectGatt returns, so match on the device. */
    private fun ours(g: BluetoothGatt): Boolean {
        val d = device ?: return false
        return g.device?.address == d.address
    }

    /**
     * The next event of type [T], or null after [ms]. The link dropping, or
     * Android dropping the pairing it started with, fails with [LinkFailure].
     */
    private inline fun <reified T : Ev> poll(ms: Long): T? {
        val deadline = System.currentTimeMillis() + ms
        while (true) {
            val left = deadline - System.currentTimeMillis()
            if (left <= 0) return null
            val ev = events.poll(left, TimeUnit.MILLISECONDS) ?: return null
            if (ev is T) return ev
            if (ev is Ev.Conn && ev.state == BluetoothProfile.STATE_DISCONNECTED) {
                throw LinkFailure("dropped", ev.status)
            }
            if (ev is Ev.Bond && ev.state == BluetoothDevice.BOND_NONE && wasBonded && !bondingNow) {
                throw LinkFailure("bondLost", 0)
            }
            // Anything else is stale: skip it.
        }
    }

    private inline fun <reified T : Ev> await(ms: Long): T =
        poll<T>(ms) ?: throw Failure("DISCONNECTED", "Jade didn't answer")

    private fun closeGatt() {
        val g = gatt
        gatt = null
        channel = null
        rx = null
        device = null
        if (g != null) {
            Log.d(TAG, "closing GATT")
            try {
                g.disconnect()
                g.close()
            } catch (ignored: Exception) {
                // Already gone.
            }
        }
        events.clear()
    }
}
