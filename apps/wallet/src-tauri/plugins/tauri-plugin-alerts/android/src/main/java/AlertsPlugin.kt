package io.btcsessions.alerts

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import android.webkit.WebView
import androidx.activity.result.ActivityResult
import androidx.core.app.NotificationManagerCompat
import app.tauri.PermissionState
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

private const val TAG = "SessionsAlerts"

/**
 * The parts of payment alerts that need the app's screen: the notification
 * permission prompt, the battery-optimisation prompt, and knowing whether the
 * screen is showing. Posting and the watch service are in [Alerts].
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = "notifications")
    ]
)
class AlertsPlugin(private val activity: Activity) : Plugin(activity) {
    private val ctx: Context get() = activity.applicationContext

    override fun load(webView: WebView) {
        Alerts.foreground = true
        Alerts.channels(ctx)
    }

    override fun onResume() {
        Alerts.foreground = true
    }

    override fun onPause() {
        Alerts.foreground = false
    }

    override fun onDestroy() {
        Alerts.foreground = false
    }

    private fun notifications(): String {
        if (NotificationManagerCompat.from(ctx).areNotificationsEnabled()) return "granted"
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return "denied"
        return when (getPermissionState("notifications")) {
            PermissionState.GRANTED -> "denied" // allowed, but switched off in settings
            PermissionState.DENIED -> "denied"
            else -> "prompt"
        }
    }

    private fun unrestricted(): Boolean {
        val pm = ctx.getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return false
        return pm.isIgnoringBatteryOptimizations(ctx.packageName)
    }

    private fun status(): JSObject {
        val o = JSObject()
        o.put("supported", true)
        o.put("notifications", notifications())
        o.put("watching", WatchService.running)
        o.put("unrestricted", unrestricted())
        return o
    }

    @Command
    fun status(invoke: Invoke) {
        invoke.resolve(status())
    }

    @Command
    fun requestNotifications(invoke: Invoke) {
        when (notifications()) {
            "granted" -> invoke.resolve(status())
            "prompt" -> requestPermissionForAlias("notifications", invoke, "notificationsAnswered")
            else -> {
                // Refused before, or switched off: only system settings can change it.
                try {
                    val i = Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, ctx.packageName)
                    startActivityForResult(invoke, i, "settingsClosed")
                } catch (e: Exception) {
                    Log.w(TAG, "notification settings: ${e.message}")
                    invoke.resolve(status())
                }
            }
        }
    }

    @PermissionCallback
    private fun notificationsAnswered(invoke: Invoke) {
        invoke.resolve(status())
    }

    @ActivityCallback
    private fun settingsClosed(invoke: Invoke, @Suppress("UNUSED_PARAMETER") result: ActivityResult) {
        invoke.resolve(status())
    }

    /** "Allow Sessions to always run in the background?" (a system dialog). */
    @Command
    fun requestUnrestricted(invoke: Invoke) {
        if (unrestricted()) {
            invoke.resolve(status())
            return
        }
        try {
            @Suppress("BatteryLife")
            val i = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
                .setData(Uri.parse("package:${ctx.packageName}"))
            startActivityForResult(invoke, i, "settingsClosed")
        } catch (e: Exception) {
            try {
                startActivityForResult(
                    invoke, Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS), "settingsClosed"
                )
            } catch (e2: Exception) {
                Log.w(TAG, "battery settings: ${e2.message}")
                invoke.resolve(status())
            }
        }
    }
}
