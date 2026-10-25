package io.btcsessions.alerts

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat

private const val TAG = "SessionsAlerts"

const val CHANNEL_PAYMENTS = "payments"
const val CHANNEL_WATCH = "watch"

/**
 * Called from Rust through JNI with the application context, so it works
 * whether or not the app's screen exists. Keep the method names and
 * signatures in step with `src/lib.rs`.
 */
object Alerts {
    /** The app's screen is showing (set by [AlertsPlugin]'s lifecycle hooks). */
    @Volatile
    @JvmStatic
    var foreground: Boolean = false

    @JvmStatic
    fun inForeground(): Boolean = foreground

    fun channels(ctx: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_PAYMENTS, "Payments", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "When a payment arrives or confirms"
                enableVibration(true)
            }
        )
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_WATCH, "Watching for payments", NotificationManager.IMPORTANCE_MIN).apply {
                description = "Shown while the wallet watches for payments with the app closed"
                setShowBadge(false)
            }
        )
    }

    /** Opens the app (or brings it back) when a notification is tapped. */
    fun openApp(ctx: Context, request: Int): PendingIntent? {
        val launch = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName) ?: return null
        launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        return PendingIntent.getActivity(
            ctx, request, launch, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
    }

    /**
     * A payment notification, unless the app is on screen. Same [key] replaces
     * the earlier one. The lock screen shows [publicTitle] (no amounts).
     */
    @JvmStatic
    fun post(ctx: Context, key: String, title: String, body: String, publicTitle: String): Boolean {
        if (foreground) return false
        return try {
            if (!NotificationManagerCompat.from(ctx).areNotificationsEnabled()) return false
            channels(ctx)
            val tap = openApp(ctx, key.hashCode())
            val public = NotificationCompat.Builder(ctx, CHANNEL_PAYMENTS)
                .setSmallIcon(R.drawable.ic_stat_payment)
                .setContentTitle(publicTitle)
                .setContentIntent(tap)
                .build()
            val n = NotificationCompat.Builder(ctx, CHANNEL_PAYMENTS)
                .setSmallIcon(R.drawable.ic_stat_payment)
                .setContentTitle(title)
                .setContentText(body)
                .setCategory(NotificationCompat.CATEGORY_STATUS)
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
                .setPublicVersion(public)
                .setAutoCancel(true)
                .setContentIntent(tap)
                .build()
            NotificationManagerCompat.from(ctx).notify(key, 1, n)
            true
        } catch (e: SecurityException) {
            Log.w(TAG, "post: no permission")
            false
        } catch (e: Exception) {
            Log.w(TAG, "post failed", e)
            false
        }
    }

    @JvmStatic
    fun startWatch(ctx: Context) {
        try {
            val intent = Intent(ctx, WatchService::class.java)
            ContextCompat.startForegroundService(ctx, intent)
        } catch (e: Exception) {
            // ForegroundServiceStartNotAllowedException when in the background:
            // the next time the app is opened tries again.
            Log.w(TAG, "startWatch failed: ${e.javaClass.simpleName}: ${e.message}")
        }
    }

    @JvmStatic
    fun stopWatch(ctx: Context) {
        try {
            ctx.stopService(Intent(ctx, WatchService::class.java))
        } catch (e: Exception) {
            Log.w(TAG, "stopWatch failed", e)
        }
    }
}
