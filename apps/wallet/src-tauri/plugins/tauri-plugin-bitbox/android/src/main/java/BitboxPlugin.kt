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
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

private const val VENDOR_ID = 0x03eb
private const val PRODUCT_ID = 0x2403

/**
 * Opens a BitBox02 plugged in over USB and hands its usbfs file descriptor to
 * Rust, which does the HID transfers. Called from Rust only, not the webview.
 */
@TauriPlugin
class BitboxPlugin(private val activity: Activity) : Plugin(activity) {
    private val lock = Any()
    private var connection: UsbDeviceConnection? = null
    private var claimed: UsbInterface? = null

    private val usbManager: UsbManager
        get() = activity.getSystemService(Context.USB_SERVICE) as UsbManager

    @Command
    fun open(invoke: Invoke) {
        try {
            val manager = usbManager
            val device = manager.deviceList.values.firstOrNull {
                it.vendorId == VENDOR_ID && it.productId == PRODUCT_ID
            }
            if (device == null) {
                invoke.reject("No BitBox02 found", "NOT_FOUND")
                return
            }
            if (manager.hasPermission(device)) {
                openDevice(manager, device, invoke)
            } else {
                requestPermission(manager, device, invoke)
            }
        } catch (e: Exception) {
            invoke.reject(e.message ?: "USB error", "OPEN_FAILED")
        }
    }

    @Command
    fun close(invoke: Invoke) {
        synchronized(lock) { closeLocked() }
        invoke.resolve()
    }

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
                if (granted && manager.hasPermission(device)) {
                    try {
                        openDevice(manager, device, invoke)
                    } catch (e: Exception) {
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
        manager.requestPermission(device, pending)
    }

    /** The BitBox02's HID interface: interface 0, or the first HID one. */
    private fun hidInterface(device: UsbDevice): UsbInterface? {
        val all = (0 until device.interfaceCount).map { device.getInterface(it) }
        val hid = all.filter { it.interfaceClass == UsbConstants.USB_CLASS_HID }
        return hid.firstOrNull { it.id == 0 } ?: hid.firstOrNull()
    }

    private fun openDevice(manager: UsbManager, device: UsbDevice, invoke: Invoke) {
        synchronized(lock) {
            closeLocked()
            val intf = hidInterface(device)
            if (intf == null) {
                invoke.reject("This BitBox02 has no HID interface", "OPEN_FAILED")
                return
            }
            var epIn: UsbEndpoint? = null
            var epOut: UsbEndpoint? = null
            for (i in 0 until intf.endpointCount) {
                val ep = intf.getEndpoint(i)
                if (ep.type != UsbConstants.USB_ENDPOINT_XFER_INT) continue
                if (ep.direction == UsbConstants.USB_DIR_IN) epIn = ep else epOut = ep
            }
            if (epIn == null || epOut == null) {
                invoke.reject("This BitBox02 has no HID endpoints", "OPEN_FAILED")
                return
            }
            val conn = manager.openDevice(device)
            if (conn == null) {
                invoke.reject("Couldn't open the BitBox02", "OPEN_FAILED")
                return
            }
            // force = true detaches the kernel's HID driver if it has the interface.
            if (!conn.claimInterface(intf, true)) {
                conn.close()
                invoke.reject("The BitBox02 is in use by another app", "BUSY")
                return
            }
            connection = conn
            claimed = intf
            val ret = JSObject()
            ret.put("fd", conn.fileDescriptor)
            ret.put("interface", intf.id)
            ret.put("endpointIn", epIn.address)
            ret.put("endpointOut", epOut.address)
            invoke.resolve(ret)
        }
    }

    private fun closeLocked() {
        val conn = connection
        val intf = claimed
        if (conn != null) {
            if (intf != null) conn.releaseInterface(intf)
            conn.close()
        }
        connection = null
        claimed = null
    }
}
