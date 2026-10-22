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

/** Jade's Nordic-UART-style service. We write RX; Jade indicates on TX. */
private val SERVICE: UUID = UUID.fromString("6e400001-b5a3-f393-e0a9-e50e24dcca9e")
private val RX: UUID = UUID.fromString("6e400002-b5a3-f393-e0a9-e50e24dcca9e")
private val TX: UUID = UUID.fromString("6e400003-b5a3-f393-e0a9-e50e24dcca9e")
private val CCCD: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

/** GATT statuses meaning the link isn't (or is no longer) encrypted with a bond Jade knows. */
private val AUTH_STATUSES = setOf(
    BluetoothGatt.GATT_INSUFFICIENT_AUTHENTICATION,
    BluetoothGatt.GATT_INSUFFICIENT_ENCRYPTION,
    137, // GATT_AUTH_FAIL
)

@InvokeArg
class ConnectArgs {
    lateinit var onData: Channel
    var scanMs: Long = 15000
}

@InvokeArg
class WriteArgs {
    var data: String = ""
}

/** A failure with a code the Rust side maps to plain English. */
private class Failure(val code: String, message: String) : Exception(message)

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
                invoke.reject(f.message, f.code)
            } catch (e: Exception) {
                invoke.reject(e.message ?: "Bluetooth error", "OTHER")
            }
        }
    }

    @Command
    fun disconnect(invoke: Invoke) {
        worker.execute {
            closeGatt()
            invoke.resolve()
        }
    }

    // --- connecting -----------------------------------------------------------

    private fun doConnect(invoke: Invoke) {
        try {
            val args = invoke.parseArgs(ConnectArgs::class.java)
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
            val found = scan(adapter, args.scanMs) ?: throw Failure("NOT_FOUND", "No Jade found")
            val progress = JSObject()
            progress.put("connecting", true)
            args.onData.send(progress)

            events.clear()
            device = found
            val g = connectGatt(found)

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
                throw Failure("OTHER", "Couldn't read Jade's services")
            }
            val service = g.getService(SERVICE) ?: throw Failure("OTHER", "This isn't a Jade")
            val tx = service.getCharacteristic(TX) ?: throw Failure("OTHER", "This isn't a Jade")
            rx = service.getCharacteristic(RX) ?: throw Failure("OTHER", "This isn't a Jade")

            // Jade only talks over an encrypted, bonded link. Android shows its
            // pairing dialog; Jade shows the same number to confirm.
            if (found.bondState != BluetoothDevice.BOND_BONDED) {
                bond(found)
                // Let the link settle on the new keys (Green does the same).
                Thread.sleep(1_500)
            }

            subscribe(g, tx)
            // From here on, what Jade sends goes to Rust (and a later disconnect).
            channel = args.onData

            val ret = JSObject()
            ret.put("name", found.name ?: "Jade")
            ret.put("mtu", mtu)
            invoke.resolve(ret)
        } catch (f: Failure) {
            closeGatt()
            invoke.reject(f.message, f.code)
        } catch (e: Exception) {
            closeGatt()
            invoke.reject(e.message ?: "Bluetooth error", "OTHER")
        }
    }

    /** Connect, retrying once: Android often fails the first attempt (status 133). */
    private fun connectGatt(target: BluetoothDevice): BluetoothGatt {
        repeat(2) { attempt ->
            val g = target.connectGatt(activity, false, callback, BluetoothDevice.TRANSPORT_LE)
                ?: throw Failure("OTHER", "Couldn't connect to Jade")
            gatt = g
            val conn = poll<Ev.Conn>(20_000)
            if (conn != null && conn.state == BluetoothProfile.STATE_CONNECTED) return g
            gatt = null
            try {
                g.disconnect()
                g.close()
            } catch (ignored: Exception) {
                // Already gone.
            }
            events.clear()
            if (attempt == 0) Thread.sleep(600)
        }
        throw Failure("DISCONNECTED", "Couldn't connect to Jade")
    }

    /** Scan for Jade's service; the strongest one seen within a moment of the first. */
    private fun scan(adapter: BluetoothAdapter, timeoutMs: Long): BluetoothDevice? {
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
        if (failed.get() != 0 && seen.isEmpty()) throw Failure("OTHER", "Bluetooth scan failed (${failed.get()})")
        return seen.values.maxByOrNull { it.rssi }?.device
    }

    private fun bond(target: BluetoothDevice) {
        val context = activity.applicationContext
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context, intent: Intent) {
                if (intent.action != BluetoothDevice.ACTION_BOND_STATE_CHANGED) return
                @Suppress("DEPRECATION")
                val d: BluetoothDevice? = intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE)
                if (d?.address != target.address) return
                events.offer(Ev.Bond(intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.BOND_NONE)))
            }
        }
        ContextCompat.registerReceiver(
            context,
            receiver,
            IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED),
            ContextCompat.RECEIVER_EXPORTED,
        )
        try {
            if (target.bondState == BluetoothDevice.BOND_BONDED) return
            if (target.bondState != BluetoothDevice.BOND_BONDING && !target.createBond()) {
                throw Failure("PAIRING_FAILED", "Couldn't pair with Jade")
            }
            // The user reads and confirms the number on both screens.
            val start = System.currentTimeMillis()
            var sawBonding = false
            while (true) {
                val elapsed = System.currentTimeMillis() - start
                if (elapsed > 120_000) throw Failure("PAIRING_FAILED", "Pairing with Jade timed out")
                // Broadcasts can be missed: also look at the state directly.
                val state = poll<Ev.Bond>(1_000)?.state ?: target.bondState
                when (state) {
                    BluetoothDevice.BOND_BONDED -> return
                    BluetoothDevice.BOND_BONDING -> sawBonding = true
                    BluetoothDevice.BOND_NONE -> if (sawBonding || elapsed > 15_000) {
                        throw Failure("PAIRING_FAILED", "Pairing with Jade was declined")
                    }
                }
            }
        } finally {
            try {
                context.unregisterReceiver(receiver)
            } catch (ignored: IllegalArgumentException) {
                // Already unregistered.
            }
        }
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
                if (ev.status in AUTH_STATUSES) {
                    throw Failure("PAIRING_STALE", "Jade doesn't recognise this phone's pairing")
                }
            }
            Thread.sleep(500)
        }
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
            events.offer(Ev.Conn(status, newState))
            if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                val ch = channel
                channel = null
                if (ch != null) {
                    val msg = JSObject()
                    msg.put("disconnected", true)
                    ch.send(msg)
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

    /** Callbacks can arrive before connectGatt returns, so match on the device. */
    private fun ours(g: BluetoothGatt): Boolean {
        val d = device ?: return false
        return g.device?.address == d.address
    }

    /** The next event of type [T], or null after [ms]. A disconnect fails. */
    private inline fun <reified T : Ev> poll(ms: Long): T? {
        val deadline = System.currentTimeMillis() + ms
        while (true) {
            val left = deadline - System.currentTimeMillis()
            if (left <= 0) return null
            val ev = events.poll(left, TimeUnit.MILLISECONDS) ?: return null
            if (ev is T) return ev
            if (ev is Ev.Conn && ev.state == BluetoothProfile.STATE_DISCONNECTED) {
                // HCI 0x05 / 0x06: authentication failure, PIN or key missing.
                if (ev.status in AUTH_STATUSES || ev.status == 6) {
                    throw Failure("PAIRING_STALE", "Jade doesn't recognise this phone's pairing")
                }
                throw Failure("DISCONNECTED", "Jade disconnected")
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
