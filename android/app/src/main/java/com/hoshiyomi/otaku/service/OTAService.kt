package com.hoshiyomi.otaku.service

import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import com.hoshiyomi.otaku.OTAkuApp

/**
 * OTAService — Foreground service that keeps long native operations alive during Doze.
 *
 * Android can kill background coroutines under Doze/App Standby, even when
 * the app holds a WakeLock and is whitelisted for battery optimization.
 * A foreground service gives the process "foreground priority" which prevents
 * the OS from killing it during long compression/extraction operations.
 *
 * Architecture:
 *   - This service is a lightweight "lifecycle protector" — it does NOT run
 *     the work itself. The coroutine continues in MainActivity.buildScope
 *     (DD ZIP build, payload.bin extraction, payload.bin write — the label
 *     passed to start() only customizes the notification text).
 *   - onStartCommand() calls startForeground() to elevate process priority.
 *   - The service holds a PARTIAL_WAKE_LOCK (3 hours) to prevent CPU sleep —
 *     this is the WakeLock that covers long payload extractions (multi-GB
 *     system.img decompression) as well as builds.
 *   - MainActivity updates the foreground notification directly via NotificationManager
 *     (same NOTIFICATION_ID = 1001), which updates the service's foreground notification.
 *   - The service stops itself when told to via ACTION_STOP_BUILD or when the
 *     operation completes (called by MainActivity).
 *
 * Why not run the work IN the service?
 *   - The work uses a companion-object coroutine scope (buildScope) that survives
 *     Activity recreation. Moving it here would require major refactoring of progress
 *     tracking, split progress bars, log text, etc.
 *   - The service's only job is to keep the process alive — separating concerns.
 */
class OTAService : Service() {

    companion object {
        private const val TAG = "OTAService"
        const val NOTIFICATION_ID = 1001

        // Intent actions. Kept the START_BUILD name for the generic "protect
        // a long native operation" action (build, extract, payload write) —
        // renaming would churn every call site for zero behavior change.
        const val ACTION_START_BUILD = "com.hoshiyomi.otaku.ACTION_START_BUILD"
        const val ACTION_STOP_BUILD = "com.hoshiyomi.otaku.ACTION_STOP_BUILD"

        // Notification text override ("Preparing build…" / "Extracting payload…" /
        // "Building payload…"). Read in onStartCommand.
        const val EXTRA_LABEL = "com.hoshiyomi.otaku.EXTRA_LABEL"

        /** Start the foreground service for long-operation protection. */
        fun start(context: Context, label: String = "Preparing build…") {
            val intent = Intent(context, OTAService::class.java).apply {
                action = ACTION_START_BUILD
                putExtra(EXTRA_LABEL, label)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        /** Stop the foreground service after the operation completes. */
        fun stop(context: Context) {
            val intent = Intent(context, OTAService::class.java).apply {
                action = ACTION_STOP_BUILD
            }
            context.startService(intent)
        }
    }

    private var wakeLock: PowerManager.WakeLock? = null
    private lateinit var notificationManager: NotificationManager

    override fun onCreate() {
        super.onCreate()
        notificationManager = getSystemService(NotificationManager::class.java)
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Label from the launching call site — "Preparing build…" default
        // keeps the original build behavior; extraction/payload-write call
        // sites pass their own label so the user can tell operations apart.
        val label = intent?.getStringExtra(EXTRA_LABEL) ?: "Preparing build…"
        when (intent?.action) {
            ACTION_STOP_BUILD -> {
                stopBuild()
                return START_NOT_STICKY
            }
            ACTION_START_BUILD -> {
                // Start foreground immediately — this is what prevents Doze from killing us
                startForegroundNotification(label)
                acquireWakeLock()
                return START_NOT_STICKY
            }
            else -> {
                // No action specified (e.g. service restart) — default label
                startForegroundNotification(label)
                acquireWakeLock()
                return START_NOT_STICKY
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Foreground notification — process lifecycle protection
    // ═══════════════════════════════════════════════════════════════

    private fun startForegroundNotification(text: String) {
        val notification = buildNotification(text)
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Exception) {
            Log.w(TAG, "Foreground type failed: ${e.message}, falling back to plain")
            try {
                startForeground(NOTIFICATION_ID, notification)
            } catch (e2: Exception) {
                Log.e(TAG, "startForeground also failed: ${e2.message}")
            }
        }
    }

    private fun buildNotification(
        text: String,
        percent: Int = 0
    ): android.app.Notification {
        // ContentIntent: tapping the notification opens the app
        val launchIntent = packageManager.getLaunchIntentForPackage(packageName)?.apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val pi = launchIntent?.let {
            PendingIntent.getActivity(this, 0, it,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        }
        return NotificationCompat.Builder(this, OTAkuApp.CHANNEL_ID)
            .setContentTitle("OTAku")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setProgress(100, percent.coerceIn(0, 100), percent == 0)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setOngoing(true)  // Non-swipeable while service is in foreground
            .setSilent(true)   // No sound for status updates
            .apply { pi?.let { setContentIntent(it) } }
            .build()
    }

    // ═══════════════════════════════════════════════════════════════
    //  WakeLock — prevent CPU sleep during heavy I/O
    // ═══════════════════════════════════════════════════════════════

    private fun acquireWakeLock() {
        if (wakeLock?.isHeld == true) return  // Already acquired
        val powerManager = getSystemService(PowerManager::class.java)
        wakeLock = powerManager.newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK,
            "OTAku::BuildWakeLock"
        ).apply {
            setReferenceCounted(false)
            acquire(3 * 60 * 60 * 1000L) // 3 hours — enough for any compression job
        }
    }

    private fun releaseWakeLock() {
        try {
            wakeLock?.release()
        } catch (_: Exception) { /* already released */ }
        wakeLock = null
    }

    // ═══════════════════════════════════════════════════════════════
    //  Lifecycle — stop build and clean up
    // ═══════════════════════════════════════════════════════════════

    private fun stopBuild() {
        releaseWakeLock()
        // Detach notification from foreground service so completion notification
        // (posted by MainActivity.showCompletionNotification) stays visible.
        // STOP_FOREGROUND_DETACH keeps the notification visible but no longer
        // tied to the service lifecycle.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            stopForeground(STOP_FOREGROUND_DETACH)
        } else {
            @Suppress("DEPRECATION")
            stopForeground(false)
        }
        stopSelf()
    }

    override fun onDestroy() {
        releaseWakeLock()
        // Do NOT cancel the notification here — the completion notification
        // (posted by MainActivity) should remain visible until the user dismisses it.
        // Only cancel if no completion notification was posted (e.g. process killed).
        super.onDestroy()
    }
}
