package io.btcsessions.bitbox

import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.SystemClock
import android.util.Base64
import android.util.Log
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.concurrent.Executors

private const val TAG = "SessionsBitbox"
private const val VENDOR_ID = 0x03eb
private const val PRODUCT_ID = 0x2403

/** One HID report. The BitBox02's endpoints are 64 bytes, without a report ID. */
private const val REPORT = 64

/** A report goes out at once unless the device is gone. */
private const val WRITE_TIMEOUT_MS = 5_000

/** Which USB device to open: the BitBox02 by default; Ledger passes its vendor ID and any product. */
@InvokeArg
class OpenArgs {
    var vendorId: Int = VENDOR_ID
    /** -1: any product of this vendor. */
    var productId: Int = PRODUCT_ID
    /** Named in messages. */
    var label: String = "BitBox02"
}

@InvokeArg
class WriteArgs {
    var data: String = ""
}

@InvokeArg
class ReadArgs {
    var timeoutMs: Int = 10_000
}

/** The open device: the connection plus its claimed HID interface and endpoints. */
private class Open(
    val connection: UsbDeviceConnection,
    val intf: UsbInterface,
    val epIn: UsbEndpoint,
    val epOut: UsbEndpoint,
)

/**
 * A BitBox02 plugged in over USB, for the wallet's Rust code (not the webview).
 *
 * Kotlin does the HID I/O itself, the way the BitBoxApp does on Android:
 * claim interface 0 and move raw 64-byte reports with
 * `UsbDeviceConnection.bulkTransfer` (which works on interrupt endpoints).
 * bitbox-api adds the U2F-HID framing on top. Every transfer runs on one
 * background thread, in order, never on the main thread.
 *
 * `adb logcat -s SessionsBitbox` shows what happened.
 */
@TauriPlugin
class BitboxPlugin(private val activity: Activity) : Plugin(activity) {
    private val io = Executors.newSingleThreadExecutor()
    private val lock = Any()
    private var open: Open? = null

    private val usbManager: UsbManager
        get() = activity.getSystemService(Context.USB_SERVICE) as UsbManager

    /** Named in messages: the device of the last `open`. */
    private var label = "BitBox02"

    @Command
    fun open(invoke: Invoke) {
        try {
            val args = invoke.parseArgs(OpenArgs::class.java)
            label = args.label
            val manager = usbManager
            val device = manager.deviceList.values.firstOrNull {
                it.vendorId == args.vendorId && (args.productId < 0 || it.productId == args.productId)
            }
            if (device == null) {
                Log.d(TAG, "open: no $label among ${manager.deviceList.size} USB device(s)")
                invoke.reject("No $label found", "NOT_FOUND")
                return
            }
            Log.d(TAG, "open: found ${device.deviceName} (${device.productName}), permission=${manager.hasPermission(device)}")
            if (manager.hasPermission(device)) {
                openDevice(manager, device, invoke)
            } else {
                requestPermission(manager, device, invoke)
            }
        } catch (e: Exception) {
            Log.w(TAG, "open failed", e)
            invoke.reject(e.message ?: "USB error", "OPEN_FAILED")
        }
    }

    /** Send `data` as 64-byte reports (the last one zero-padded). */
    @Command
    fun write(invoke: Invoke) {
        val args = invoke.parseArgs(WriteArgs::class.java)
        io.execute {
            try {
                val o = current() ?: throw IllegalStateException("not open")
                val bytes = Base64.decode(args.data, Base64.DEFAULT)
                var at = 0
                while (at < bytes.size) {
                    val report = ByteArray(REPORT)
                    val n = minOf(REPORT, bytes.size - at)
                    System.arraycopy(bytes, at, report, 0, n)
                    val sent = o.connection.bulkTransfer(o.epOut, report, REPORT, WRITE_TIMEOUT_MS)
                    if (sent < 0) {
                        Log.w(TAG, "write: bulkTransfer failed ($sent) at byte $at of ${bytes.size}")
                        invoke.reject("Couldn't write to the $label", "WRITE_FAILED")
                        return@execute
                    }
                    at += n
                }
                Log.d(TAG, "write: ${bytes.size} bytes ${head(bytes)}")
                val ret = JSObject()
                ret.put("written", bytes.size)
                invoke.resolve(ret)
            } catch (e: Exception) {
                Log.w(TAG, "write failed", e)
                invoke.reject(e.message ?: "USB error", "WRITE_FAILED")
            }
        }
    }

    /** One report from the device, exactly as many bytes as arrived. */
    @Command
    fun read(invoke: Invoke) {
        val args = invoke.parseArgs(ReadArgs::class.java)
        io.execute {
            try {
                val o = current() ?: throw IllegalStateException("not open")
                val size = maxOf(REPORT, o.epIn.maxPacketSize)
                val buf = ByteArray(size)
                val timeout = args.timeoutMs.coerceIn(100, 600_000)
                val start = SystemClock.elapsedRealtime()
                val got = o.connection.bulkTransfer(o.epIn, buf, size, timeout)
                val took = SystemClock.elapsedRealtime() - start
                if (got < 0) {
                    // bulkTransfer says -1 for both; the clock tells them apart.
                    if (took >= timeout - 50) {
                        Log.w(TAG, "read: nothing within ${timeout}ms")
                        invoke.reject("The $label didn't answer", "TIMEOUT")
                    } else {
                        Log.w(TAG, "read: bulkTransfer failed ($got) after ${took}ms")
                        invoke.reject("Couldn't read from the $label", "READ_FAILED")
                    }
                    return@execute
                }
                val data = buf.copyOf(got)
                Log.d(TAG, "read: $got bytes in ${took}ms ${head(data)}")
                val ret = JSObject()
                ret.put("data", Base64.encodeToString(data, Base64.NO_WRAP))
                invoke.resolve(ret)
            } catch (e: Exception) {
                Log.w(TAG, "read failed", e)
                invoke.reject(e.message ?: "USB error", "READ_FAILED")
            }
        }
    }

    /** Close at once (not queued): a blocked read then fails instead of waiting out its timeout. */
    @Command
    fun close(invoke: Invoke) {
        synchronized(lock) { closeLocked() }
        invoke.resolve()
    }

    private fun current(): Open? = synchronized(lock) { open }

    /** Shows Android's "allow access to BitBox02" prompt, then opens it. */
    private fun requestPermission(manager: UsbManager, device: UsbDevice, invoke: Invoke) {
        val context = activity.applicationContext
        val action = context.packageName + ".BITBOX_USB_PERMISSION"
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context, intent: Intent) {
                if (intent.action != action) return
                try {
                    context.unregisterReceiver(this)
                } catch (ignored: IllegalArgumentException) {
                    // Already unregistered.
                }
                val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
                Log.d(TAG, "permission: granted=$granted hasPermission=${manager.hasPermission(device)}")
                if (granted || manager.hasPermission(device)) {
                    try {
                        openDevice(manager, device, invoke)
                    } catch (e: Exception) {
                        Log.w(TAG, "open after permission failed", e)
                        invoke.reject(e.message ?: "USB error", "OPEN_FAILED")
                    }
                } else {
                    invoke.reject("USB access was not allowed", "PERMISSION_DENIED")
                }
            }
        }
        ContextCompat.registerReceiver(
            context,
            receiver,
            IntentFilter(action),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        // Explicit (package set) so it may be mutable on Android 14+; mutable
        // because the system fills in EXTRA_PERMISSION_GRANTED (Android 12+).
        val intent = Intent(action).setPackage(context.packageName)
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
        val pending = PendingIntent.getBroadcast(context, 0, intent, flags)
        Log.d(TAG, "permission: asking")
        manager.requestPermission(device, pending)
    }

    /** The BitBox02's HID interface: interface 0, as the BitBoxApp uses, or the first HID one. */
    private fun hidInterface(device: UsbDevice): UsbInterface? {
        if (device.interfaceCount == 0) return null
        val first = device.getInterface(0)
        if (first.interfaceClass == UsbConstants.USB_CLASS_HID) return first
        return (0 until device.interfaceCount)
            .map { device.getInterface(it) }
            .firstOrNull { it.interfaceClass == UsbConstants.USB_CLASS_HID }
    }

    private fun openDevice(manager: UsbManager, device: UsbDevice, invoke: Invoke) {
        synchronized(lock) {
            closeLocked()
            val intf = hidInterface(device)
            if (intf == null) {
                Log.w(TAG, "open: no HID interface among ${device.interfaceCount}")
                invoke.reject("This $label has no HID interface", "OPEN_FAILED")
                return
            }
            var epIn: UsbEndpoint? = null
            var epOut: UsbEndpoint? = null
            for (i in 0 until intf.endpointCount) {
                val ep = intf.getEndpoint(i)
                if (ep.direction == UsbConstants.USB_DIR_IN) {
                    if (epIn == null) epIn = ep
                } else if (epOut == null) {
                    epOut = ep
                }
            }
            if (epIn == null || epOut == null) {
                Log.w(TAG, "open: interface ${intf.id} has ${intf.endpointCount} endpoint(s), need IN and OUT")
                invoke.reject("This $label has no HID endpoints", "OPEN_FAILED")
                return
            }
            val conn = manager.openDevice(device)
            if (conn == null) {
                Log.w(TAG, "open: UsbManager.openDevice returned null")
                invoke.reject("Android wouldn't open the $label", "OPEN_FAILED")
                return
            }
            // force = true detaches the kernel's HID driver if it has the interface.
            if (!conn.claimInterface(intf, true)) {
                Log.w(TAG, "open: couldn't claim interface ${intf.id}")
                conn.close()
                invoke.reject("The $label is in use by another app", "BUSY")
                return
            }
            open = Open(conn, intf, epIn, epOut)
            Log.d(
                TAG,
                "open: interface ${intf.id}, in 0x%02x (%d), out 0x%02x (%d)".format(
                    epIn.address, epIn.maxPacketSize, epOut.address, epOut.maxPacketSize,
                ),
            )
            val ret = JSObject()
            ret.put("product", device.productName ?: label)
            invoke.resolve(ret)
        }
    }

    private fun closeLocked() {
        val o = open ?: return
        open = null
        try {
            o.connection.releaseInterface(o.intf)
        } catch (ignored: Exception) {
            // Unplugged.
        }
        o.connection.close()
        Log.d(TAG, "closed")
    }

    /** The U2F-HID header (channel, command, length): enough to follow the conversation. */
    private fun head(bytes: ByteArray): String =
        bytes.take(7).joinToString("") { "%02x".format(it) }
}
