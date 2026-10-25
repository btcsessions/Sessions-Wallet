package io.btcsessions.alerts

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat

private const val TAG = "SessionsAlerts"
private const val ACTION_STOP = "io.btcsessions.alerts.STOP"
private const val ID = 7301

/**
 * Keeps the app's process alive after its screen is closed, so the wallet's
 * Rust side keeps its chain-server and Lightning connections and can post
 * payment notifications. It does no work of its own.
 */
class WatchService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            running = false
            ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        Alerts.channels(this)
        val stop = PendingIntent.getService(
            this, 1, Intent(this, WatchService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val n = NotificationCompat.Builder(this, CHANNEL_WATCH)
            .setSmallIcon(R.drawable.ic_stat_payment)
            .setContentTitle("Watching for payments")
            .setContentText("You'll get an alert when a payment arrives")
            .setPriority(NotificationCompat.PRIORITY_MIN)
            .setOngoing(true)
            .setShowWhen(false)
            .setContentIntent(Alerts.openApp(this, 0))
            .addAction(0, "Stop", stop)
            .build()
        try {
            val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
            } else 0
            ServiceCompat.startForeground(this, ID, n, type)
            running = true
        } catch (e: Exception) {
            Log.w(TAG, "startForeground failed: ${e.javaClass.simpleName}: ${e.message}")
            stopSelf()
        }
        // Not sticky: after the system kills the process the wallet is locked
        // anyway, so a restarted service would watch nothing.
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        running = false
        super.onDestroy()
    }

    companion object {
        @Volatile
        @JvmStatic
        var running: Boolean = false

        fun isRunning(@Suppress("UNUSED_PARAMETER") ctx: Context) = running
    }
}
