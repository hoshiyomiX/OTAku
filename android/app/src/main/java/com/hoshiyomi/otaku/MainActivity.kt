package com.hoshiyomi.otaku

import android.Manifest
import androidx.core.splashscreen.SplashScreen.Companion.installSplashScreen
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.Looper
import android.os.PowerManager
import android.provider.DocumentsContract
import android.provider.Settings
import android.util.Log
import android.view.View
import android.widget.ArrayAdapter
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.app.AppCompatDelegate
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.isActive
import kotlinx.coroutines.delay
import kotlinx.coroutines.Job
import java.lang.ref.WeakReference
import java.io.File
import java.io.FileOutputStream
import androidx.core.content.edit
import android.text.SpannableString
import android.text.style.ForegroundColorSpan
import android.app.NotificationManager
import android.app.Activity
import android.app.PendingIntent
import androidx.core.app.NotificationCompat
import com.hoshiyomi.otaku.service.OTAService

/**
 * MainActivity — OTAku Android.
 *
 * Single-purpose: Build partition images (.img) into a flashable OTA ZIP.
 *
 * Flow:
 *   1. Select partition images (dd.img, odm.img, dlkm.img, etc.)
 *   2. Choose compression algorithm
 *   3. Select output directory
 *   4. Tap "Build" to generate flashable OTA ZIP
 *
 * Native backend: Rust (libotaku_native.so) — no Python dependency.
 */
class MainActivity : AppCompatActivity() {

    // ═══════════════════════════════════════════════════════════════
    //  State
    // ═══════════════════════════════════════════════════════════════

    private var selectedCompression: String = "gzip"
    private var selectedCompressionLevel: Int = 0  // 0 = default (best)
    @Volatile
    private var isExecuting = false
    companion object {
        // Application-scoped coroutine scope for long-running build operations.
        // Survives Activity destruction when the user minimizes the app.
        // Uses Dispatchers.Default (CPU-bound) instead of Main.immediate to
        // prevent UI thread blocking during progress callbacks and notification
        // updates — critical for floating window mode where UI thread is shared
        // between smaller rendering area and progress updates.
        private val buildScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

        // Partition image list — moved to companion so it survives Activity recreation
        // (theme switch via AppCompatDelegate.setDefaultNightMode). configChanges
        // includes uiMode, so the Activity is NOT auto-recreated — cycleTheme()
        // and onConfigurationChanged handle this by calling recreate() explicitly.
        // Previously this was an instance member, so switching theme would clear
        // the list and force the user to re-pick.
        @Volatile
        private var imageFiles: MutableList<Pair<String, String>> = mutableListOf() // (name, path)

        // Active image-loading coroutine job — tracked so it can be cancelled
        // when the user clicks "Remove All" or removes a specific partition.
        // Without this, clicking "Remove All" mid-copy would leave the copy
        // running in the background; when it finishes, it would re-add the
        // partition to imageFiles, causing the "loading chaos" bug where:
        //   1. User picks vendor.img → copy starts (coroutine A)
        //   2. User clicks Remove All → imageFiles.clear(), but coroutine A still running
        //   3. User picks vendor.img again → copy starts (coroutine B)
        //   4. Both coroutines write to the SAME destFile (inputDir/vendor.img)
        //      → file corruption, mixed sizes in the log
        //   5. Both coroutines finish → "Loaded vendor" appears twice with different sizes
        @Volatile
        private var imageLoadingJob: kotlinx.coroutines.Job? = null

        // Log panel expand/collapse state — survives Activity recreation
        // (theme switch, configuration changes). Default: expanded.
        @Volatile
        var isLogExpanded: Boolean = true

        // Drag tracking for pull/push log toggle (not persisted — resets on recreation)
        @Volatile
        var lastLogDragStartX: Float = 0f
        @Volatile
        var lastLogDragStartY: Float = 0f

        // Whether a build is currently running (survives Activity recreation)
        @Volatile var isBuilding = false
            private set

        // Weak reference to the current Activity for safe UI updates from coroutine
        @Volatile private var activityRef: WeakReference<MainActivity>? = null

        // WakeLock (survives Activity recreation)
        @Volatile private var wakeLock: PowerManager.WakeLock? = null

        // Latest output path (survives Activity recreation)
        @Volatile private var lastOutputPath: String = ""

        // Track last progress message to avoid spamming the log with duplicates
        @Volatile private var lastProgressMessage: String = ""

        // Track last progress percent to avoid logging on every chunk
        @Volatile private var lastProgressPercent: Int = -1

        // Track last notification progress bar percent for dedup
        @Volatile private var lastNotifPercent: Int = -1

        // Persisted log text (survives Activity recreation)
        // Truncated to MAX_LOG_BYTES to prevent OOM during long builds in
        // floating window mode (rapid onResume/onPause cycles cause full
        // StringBuffer.toString() copies — unbounded growth = OOM crash).
        @Volatile private var savedLogText: StringBuffer = StringBuffer()
        private const val MAX_LOG_BYTES = 51200  // 50 KB — ~500 lines of progress output
        private fun appendToSavedLog(line: String) {
            savedLogText.append(line)
            if (savedLogText.length > MAX_LOG_BYTES) {
                savedLogText.delete(0, savedLogText.length - MAX_LOG_BYTES)
            }
        }

        // Heartbeat: last time a progress update was received (epoch millis)
        @Volatile private var lastProgressTime: Long = 0L
        // Threshold: if no progress for this long (ms), process is assumed dead
        private const val DEAD_PROCESS_THRESHOLD_MS = 120_000L  // 2 minutes

        // Per-partition split progress bar state
        @Volatile private var partitionCount: Int = 0
        @Volatile private var partitionProgress: IntArray = IntArray(0)
        @Volatile private var currentPartitionIndex: Int = -1

        // IMPL-011: Thread-safe snapshot of partitionProgress contents.
        // @Volatile ensures visibility of the array REFERENCE, but NOT the
        // array CONTENTS. Without synchronization, copyOf() called from
        // onConfigurationChanged/onResume (UI thread) can observe a
        // partially-written array while the build coroutine (Dispatchers.Default)
        // is updating elements. The lock is on the companion object itself
        // (this), which is safe because:
        //   - Kotlin object synchronization is reentrant (same thread can
        //     re-enter without deadlock)
        //   - The critical section is tiny (just copyOf), so lock contention
        //     is negligible even at 500ms poll intervals
        //   - Only the snapshot reader needs the lock; the writer (onProgress
        //     callback) also takes the lock to ensure atomic visibility
        private val progressLock = Any()
        fun snapshotPartitionProgress(): IntArray = synchronized(progressLock) {
            partitionProgress.copyOf()
        }
        fun updatePartitionProgress(index: Int, value: Int) = synchronized(progressLock) {
            if (index in partitionProgress.indices) partitionProgress[index] = value
        }
        fun markAllProgressComplete() = synchronized(progressLock) {
            for (i in partitionProgress.indices) partitionProgress[i] = 100
        }
        @Volatile private var partitionNames: List<String> = emptyList()
        // Device-supported partition names (from nativeScanDevicePartitions).
        // Populated once on app start, used to check user-picked .img files.
        // If a filename (minus .img) does not match any name in this list,
        // the app logs a WARNING but still loads the file (hard refusal was
        // demoted in Task 12 — the scan list is a static known-list whose
        // false negatives must not block valid OEM-specific partitions).
        @Volatile private var deviceSupportedPartitions: List<String> = emptyList()

        // Notification management (survives Activity recreation)
        // Use the same NOTIFICATION_ID as OTAService so progress updates modify
        // the foreground service notification in-place.
        private val NOTIFICATION_ID = com.hoshiyomi.otaku.service.OTAService.NOTIFICATION_ID
        @Volatile private var appContext: Context? = null

        // Cached dependency check result (updated at init, used for pre-build validation)
        @Volatile var cachedDepCheck: NativeBridge.DepCheckResult? = null
            private set

        // Cold start flag: false on fresh process, true after first Activity creation.
        // Used to clear session-only input fields (device, custom filename) on cold start.
        @Volatile var wasProcessAlive = false

        // Native initialization flag: true after initializeNative() has run once.
        // Prevents duplicate "Initializing OTAku…" log lines on Activity recreation.
        @Volatile var nativeInitialized = false

        // Partition scan flag: true after the device partition scan has run once.
        // Prevents duplicate "Device supports N partitions…" + "Supported: …"
        // log lines on Activity recreation (e.g., theme toggle via cycleTheme()).
        // Mirrors the nativeInitialized guard above.
        @Volatile var partitionScanDone = false

        // Suppress repeated "Build in progress (returned from background)" log.
        // Only log once per continuous build session, not on every Activity recreation.
        @Volatile private var resumedWhileBuildingLogged = false

        // IMPL-006 (theme-switch night mode fix): Key used by AppCompatDelegate
        // to save/restore per-instance night mode in savedInstanceState.
        // When cycleTheme() calls recreate(), the saved state contains the OLD
        // night mode (e.g., MODE_NIGHT_YES from dark mode). If not stripped,
        // super.onCreate() restores this old mode, overriding the
        // setDefaultNightMode() call in applyTheme(). This causes mixed
        // light/dark colors (some elements from old mode, some from new mode)
        // because the delegate and Resources disagree on the night mode.
        // Key source: AppCompatDelegateImpl.KEY_LOCAL_NIGHT_MODE (1.6.1).
        private const val KEY_APPCOMPAT_LOCAL_NIGHT_MODE =
            "android:appcompat:local_night_mode"

        // IMPL-004 (theme toggle fix): Flag to prevent double recreate().
        // When cycleTheme() calls applyTheme() + recreate(), the delegate may
        // also trigger onConfigurationChanged() → recreate(). This flag tells
        // onConfigurationChanged() to skip its own recreate() because
        // cycleTheme() already handled it. Cleared in onCreate() of the new
        // Activity instance as a safety net.
        @Volatile var themeSwitchInProgress = false

        // IMPL-008: Track last uiMode for theme-change detection (survives
        // Activity recreation). Previously an instance var — this caused a bug
        // where lastUiMode reset to 0 on every Activity recreation, causing
        // the first system dark/light toggle after recreation to be silently
        // missed (condition "lastUiMode != 0" always fails on first change).
        @Volatile var lastUiMode: Int = 0

        /** MD3-FIX IMPL-003: Cached notification accent — set by MainActivity
         *  onCreate() from the FINAL activity theme (night variant + DynamicColors
         *  overlay applied) so notifications match the active palette. Null until
         *  the first Activity creation; falls back to resolving colorPrimary from
         *  the application context theme (Suisei base palette). */
        @Volatile private var notificationAccentColor: Int? = null

        /** MD3-FIX IMPL-003: Resolve the notification accent color — cached
         *  Activity-theme value first (dynamic-aware), application-theme
         *  fallback (Suisei Blue base). Never throws; null = unresolvable. */
        private fun resolveNotificationAccent(ctx: Context): Int? {
            notificationAccentColor?.let { return it }
            return try {
                val tv = android.util.TypedValue()
                if (ctx.theme.resolveAttribute(
                        com.google.android.material.R.attr.colorPrimary, tv, true
                    )
                ) {
                    if (tv.resourceId != 0) {
                        ContextCompat.getColor(ctx, tv.resourceId)
                    } else {
                        tv.data
                    }
                } else {
                    null
                }
            } catch (_: Exception) { null }
        }

        /** Show ongoing progress notification with determinate progress bar. */
        fun showProgressNotification(message: String, percent: Int) {
            val ctx = appContext ?: return
            try {
                val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                val intent = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
                    flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
                } ?: return
                val pi = PendingIntent.getActivity(
                    ctx, 0, intent,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                )
                val builder = NotificationCompat.Builder(ctx, OTAkuApp.CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.ic_media_play)
                    .setContentTitle("OTAku")
                    .setContentText(message)
                    .setProgress(100, percent.coerceIn(0, 100), percent == 0)
                    .setOngoing(true)
                    .setSilent(true)
                    .setContentIntent(pi)
                    .setPriority(NotificationCompat.PRIORITY_LOW)
                // MD3-FIX IMPL-003: brand the notification with the active
                // palette's primary (Material You on 12+, Suisei Blue below).
                resolveNotificationAccent(ctx)?.let { builder.setColor(it) }
                nm.notify(NOTIFICATION_ID, builder.build())
            } catch (_: Exception) { /* notification is non-critical */ }
        }

        /** Show completion/failure notification (auto-dismissable). */
        fun showCompletionNotification(success: Boolean, message: String) {
            val ctx = appContext ?: return
            try {
                val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                val intent = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
                    flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
                } ?: return
                val pi = PendingIntent.getActivity(
                    ctx, 0, intent,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                )
                val builder = NotificationCompat.Builder(ctx, OTAkuApp.CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.ic_media_play)
                    .setContentTitle(if (success) "Build Complete" else "Build Failed")
                    .setContentText(message)
                    .setOngoing(false)
                    .setAutoCancel(true)
                    .setContentIntent(pi)
                    .setPriority(NotificationCompat.PRIORITY_DEFAULT)
                // MD3-FIX IMPL-003: brand the notification with the active
                // palette's primary (Material You on 12+, Suisei Blue below).
                resolveNotificationAccent(ctx)?.let { builder.setColor(it) }
                nm.notify(NOTIFICATION_ID, builder.build())
            } catch (_: Exception) { /* notification is non-critical */ }
        }

        /** Cancel the build notification. */
        fun cancelBuildNotification() {
            try {
                appContext?.let {
                    (it.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
                        .cancel(NOTIFICATION_ID)
                }
            } catch (_: Exception) { /* notification is non-critical */ }
            appContext = null
        }

        // ═══════════════════════════════════════════════════════════════
        //  Build result tracking (survives Activity recreation)
        // ═══════════════════════════════════════════════════════════════

        /** Most recent build result — captured when build completes, displayed when Activity is alive. */
        @Volatile private var lastBuildResult: OTAResult? = null

        /** Whether lastBuildResult has been displayed to the user (via UI reset). */
        @Volatile private var buildResultDisplayed: Boolean = true

        /**
         * Always-on build completion handler — runs regardless of Activity state.
         *
         * This fixes the "screen-off → 15 min later → UI stuck" bug where the
         * build completes in background but handleBuildResult() was never called
         * because activityRef was null.
         *
         * Always runs (companion-level, uses appContext):
         *   - Sets isBuilding = false
         *   - Fires completion notification (success or failure)
         *   - Marks all partition progress bars as 100%
         *   - Appends final log line to savedLogText
         *   - Stops the foreground service
         *
         * Conditionally runs (if Activity is alive):
         *   - setUIExecuting(false) — hides progress bars + re-enables inputs
         *   - Sets buildResultDisplayed = true
         *
         * If Activity is dead/null (user backgrounded the app), the UI reset is
         * deferred — onResume() checks lastBuildResult and displays it.
         */
        fun recordBuildResult(result: OTAResult) {
            lastBuildResult = result
            buildResultDisplayed = false
            isBuilding = false

            // Always show completion notification (uses appContext, works in background)
            if (result.success) {
                val duration = if (result.durationMs < 60000) "${result.durationMs / 1000}s"
                    else "${result.durationMs / 60000}m ${result.durationMs % 60000 / 1000}s"
                showProgressNotification("Build complete!", 100)
                showCompletionNotification(true, "Finished in $duration")
                appendToSavedLog("\n═══ Build complete ═══\n")
            } else {
                showCompletionNotification(false, result.error ?: "Unknown error")
                appendToSavedLog("\n[ERROR] ${result.error ?: "Unknown error"}\n")
            }

            // Mark all partition progress as complete (companion state)
            // AUDIT-F2: use the locked helper (IMPL-011) instead of raw array
            // writes — direct writes here bypassed progressLock and raced
            // with concurrent onProgress writes from Dispatchers.Default.
            markAllProgressComplete()

            // Stop foreground service (uses appContext, works in background)
            try {
                appContext?.let { OTAService.stop(it) }
            } catch (_: Exception) {}

            // Conditionally update UI if Activity is alive
            val current = activityRef?.get()
            if (current != null && !current.isFinishing && !current.isDestroyed) {
                current.runOnUiThread {
                    current.isExecuting = false
                    current.setUIExecuting(false)
                    buildResultDisplayed = true
                }
            }
        }
    }

    // App-internal directories
    private lateinit var inputDir: File
    private lateinit var outputDir: File

    // SharedPreferences for persisting user settings
    private val prefs by lazy { getSharedPreferences("otaku", Context.MODE_PRIVATE) }

    // ═══════════════════════════════════════════════════════════════
    //  Activity Result Launchers
    // ═══════════════════════════════════════════════════════════════

    private val outputDirChooser = registerForActivityResult(
        ActivityResultContracts.OpenDocumentTree()
    ) { uri: Uri? ->
        uri?.let { handleOutputDirSelected(it) }
    }

    private val imageFileChooser = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK) {
            val clip = result.data?.clipData
            val uris = mutableListOf<Uri>()
            if (clip != null) {
                for (i in 0 until clip.itemCount) {
                    uris.add(clip.getItemAt(i).uri)
                }
            } else {
                result.data?.data?.let { uris.add(it) }
            }
            if (uris.isNotEmpty()) handleImageFilesSelected(uris)
        }
    }

    // Payload.bin picker (prototype) — single document, octet-stream + any
    // (payload.bin has no dedicated MIME type; OTA ZIPs are handled by the
    // user extracting payload.bin out first)
    private val payloadFileChooser = registerForActivityResult(
        ActivityResultContracts.OpenDocument()
    ) { uri: Uri? ->
        if (uri != null) handlePayloadSelected(uri)
    }

    private val permissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) { permissions ->
        val allGranted = permissions.entries.all { it.value }
        if (!allGranted) {
            showLog("Some permissions were denied. File access may be limited.", LogLevel.WARN)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            if (!Environment.isExternalStorageManager()) {
                promptManageStorage()
                // Battery prompt deferred to onResume after user returns from storage settings
                pendingBatteryPrompt = true
                return@registerForActivityResult
            }
        }
        // All storage permissions resolved — now check battery optimization
        checkBatteryOptimizationAtStartup()
    }

    // ═══════════════════════════════════════════════════════════════
    //  Lifecycle
    // ═══════════════════════════════════════════════════════════════

    override fun onCreate(savedInstanceState: Bundle?) {
        // Install splash screen — must be called BEFORE super.onCreate().
        // Shows the OTAku icon (ic_launcher_foreground) on black background
        // at app launch, then transitions smoothly to the main theme.
        val splashScreen = installSplashScreen()

        // IMPL-003 (theme switch speed): Skip splash screen on Activity recreation.
        // When cycleTheme() calls recreate(), savedInstanceState is non-null.
        // The splash would add 2 seconds of unnecessary delay — the user just
        // wants to see the new theme, not stare at the splash again.
        // On cold start (savedInstanceState == null), show splash for 2 seconds.
        val isRecreation = savedInstanceState != null
        if (!isRecreation) {
            var keepSplash = true
            splashScreen.setKeepOnScreenCondition { keepSplash }
            lifecycleScope.launch {
                delay(2000L) // 2 seconds
                keepSplash = false
            }
        }

        // ═══════════════════════════════════════════════════════════
        // THEME INITIALIZATION ORDER (critical — DO NOT reorder)
        // ═══════════════════════════════════════════════════════════
        // 0. (IMPL-006) ALWAYS strip "android:appcompat:local_night_mode"
        //    from savedInstanceState — prevents AppCompatDelegate from restoring
        //    the OLD night mode which would override setDefaultNightMode().
        // 1. applyTheme() BEFORE super.onCreate() — sets the default
        //    night mode (light/dark/system) via AppCompatDelegate.
        //    This is a STATIC call that configures the delegate before
        //    it's created in super.onCreate().
        // 2. super.onCreate() — creates the AppCompatDelegate, which
        //    reads the night mode from step 1 and APPLIES it to the
        //    Activity's Resources. Only AFTER this call does the
        //    Activity's context reflect the correct night mode.
        // 3. setTheme() AFTER super.onCreate() — sets the base theme
        //    (Theme.OTAku.Suisei). This MUST happen after super.onCreate()
        //    because setTheme() resolves night qualifiers from Resources.
        //    Before super.onCreate(), Resources reflect the PREVIOUS
        //    Activity's night mode → wrong theme variant → mixed colors.
        // 4. applyDynamicColorsOverlay() AFTER setTheme() — applies
        //    Material You DynamicColors overlay on the correct base.
        // 5. syncWindowToTheme() — fixes Window attributes that were
        //    set from stale night mode during Activity.attach().
        // 6. setContentView() — inflates views with the correct theme.
        //
        // BUGFIX: Previously, applyDynamicColorsOverlay() was called BEFORE
        // super.onCreate(). This caused DynamicColors to see stale
        // night mode configuration, resulting in:
        //   - Auto mode (system dark): white background, no accent
        //   - Light mode: inverted dark-on-white colors
        //   - Only explicit Dark mode worked correctly
        // ═══════════════════════════════════════════════════════════
        // IMPL-006 (theme-switch night mode fix): ALWAYS strip
        // AppCompatDelegate's saved night mode from savedInstanceState on
        // ANY Activity recreation. Without this, super.onCreate() restores the
        // OLD night mode (e.g., MODE_NIGHT_YES from dark mode), which overrides
        // the setDefaultNightMode() call in applyTheme(). The result is a mixed
        // state where the delegate thinks it's in dark mode but the framework
        // Resources are in light mode — causing "separuh dark, separuh light"
        // (half dark, half light) visual corruption.
        //
        // Bug pattern (user-observed):
        //   dark → auto  = BUG (partial colors)
        //   auto (bugged) → light = BUG persists
        //   light/dark → dark = NORMAL (dark masks the mismatch)
        //   auto → light → dark = NORMAL (no prior dark→auto transition)
        //
        // Root cause: AppCompatDelegateImpl.onSaveInstanceState() saves
        // localNightMode. AppCompatDelegateImpl.onCreate() restores it,
        // taking priority over setDefaultNightMode(). When the previous
        // Activity was in MODE_NIGHT_YES and cycleTheme() switches to
        // MODE_NIGHT_FOLLOW_SYSTEM, the restored MODE_NIGHT_YES overrides
        // the new FOLLOW_SYSTEM, creating the mixed state.
        //
        // IMPL-006 NOTE: We ALWAYS strip (not just when themeSwitchInProgress)
        // because onConfigurationChanged() clears themeSwitchInProgress
        // BEFORE the new Activity's onCreate() runs (race condition).
        // Sequence: cycleTheme() sets flag → onConfigurationChanged() clears
        // flag → recreate() → onCreate() sees flag=false → no stripping → BUG.
        // Unconditional stripping is safe because applyTheme() always calls
        // setDefaultNightMode() with the correct value from preferences.
        val cleanedState = if (savedInstanceState != null) {
            Bundle(savedInstanceState).apply {
                remove(KEY_APPCOMPAT_LOCAL_NIGHT_MODE)
            }
        } else {
            savedInstanceState
        }
        applyTheme()

        super.onCreate(cleanedState)

        // IMPL-008: Set the base theme AFTER super.onCreate() so
        // setTheme() resolves night qualifiers from the NOW-CORRECT
        // Resources (AppCompatDelegate has applied the night mode).
        // Previously, setTheme() was called before super.onCreate(),
        // which caused it to resolve against stale Resources (from the
        // PREVIOUS Activity's night mode), resulting in the wrong theme
        // variant (dark variant when light was expected, or vice versa).
        // This was the root cause of the "separuh dark, separuh light"
        // mixed-colors bug when switching from dark → auto → light.
        // The Window stale-color issue is handled by syncWindowToTheme().
        setTheme(R.style.Theme_OTAku_Suisei)

        // Apply DynamicColors AFTER setTheme() + super.onCreate() so
        // night mode is finalized AND the base theme is correct.
        applyDynamicColorsOverlay()

        // Force-sync the Window's appearance to the current theme.
        // The Resources at attach() time may have had stale night mode, so
        // the Window was themed with the wrong variant. This re-reads
        // window attributes from the now-correct theme and applies them.
        syncWindowToTheme()

        // MD3-FIX IMPL-003: Cache the notification accent from the FINAL
        // activity theme (night variant + DynamicColors overlay applied).
        // Companion-level notifications reuse this even after the Activity
        // is destroyed, so they always match the active palette — Material
        // You on API 31+, Suisei Blue brand accent otherwise.
        notificationAccentColor =
            resolveThemeColorAttr(com.google.android.material.R.attr.colorPrimary)

        setContentView(R.layout.activity_main)
        // IMPL-013: Eagerly cache all view references after inflation.
        // Eliminates the lazy-if-null check in setUIExecuting() which
        // runs on every progress update during builds.
        cacheViews()

        // Initialize lastUiMode from the current configuration so that the FIRST
        // system dark mode change is detected by onConfigurationChanged().
        // Without this, lastUiMode starts at 0 and the condition
        // "lastUiMode != 0" always fails on the first config change,
        // causing the first system dark/light toggle to be silently missed.
        lastUiMode = resources.configuration.uiMode and android.content.res.Configuration.UI_MODE_NIGHT_MASK
        // IMPL-004 safety net: clear themeSwitchInProgress in onCreate() of
        // the new Activity instance. If cycleTheme() set the flag but
        // onConfigurationChanged() never ran to clear it (e.g., the config
        // change was batched with the recreation), this ensures the flag
        // doesn't stay true and suppress a future legitimate recreate().
        themeSwitchInProgress = false

        inputDir = File(filesDir, "input").also { it.mkdirs() }
        outputDir = File("/storage/emulated/0/OTAku").also { it.mkdirs() }

        // Cold start detection: clear session input fields on fresh process start
        if (!wasProcessAlive) {
            prefs.edit {
                remove("device")
                remove("pref_custom_filename")
            }
            wasProcessAlive = true
        }

        initializeNative()

        // Scan device for supported partition names (no root — uses getprop).
        // The result populates deviceSupportedPartitions, which is used to
        // validate user-picked .img files: filenames that don't match any
        // partition name in this list will be refused with a warning.
        // Run async because getprop spawns a subshell (~50-100ms).
        //
        // BUG-FIX (log re-print on theme toggle): gate this block with
        // partitionScanDone to prevent duplicate "Device supports…" log lines
        // when Activity is recreated (e.g., cycleTheme() → recreate()).
        // The companion object deviceSupportedPartitions already holds the
        // cached result from the first scan — re-scanning + re-logging would
        // duplicate the log entries on every theme switch.
        if (!partitionScanDone) {
            partitionScanDone = true
            lifecycleScope.launch(Dispatchers.IO) {
                val result = NativeBridge.scanDevicePartitions()
                withContext(Dispatchers.Main) {
                    if (result.success) {
                        deviceSupportedPartitions = result.partitions
                        // Build summary string with explicit parens to avoid Kotlin
                        // if-expression + string-concat precedence bug (the old code
                        // used `str + if (...) a else b + if (...) c else d + e`
                        // which parsed as `str + if (...) a else (b + if (...) c else (d + e))`
                        // — only the dynamic branch was printed, slot/Android were dropped).
                        val partitionType = if (result.dynamicPartitions) "dynamic" else "static GPT"
                        val slotInfo = if (result.slotSuffix.isNotEmpty()) ", A/B slot=${result.slotSuffix}" else ""
                        showLog("Device supports ${result.partitions.size} partitions ($partitionType$slotInfo, Android ${result.androidVersion})")
                        // Print the actual detected partition list so user can verify
                        // which partitions are supported before picking .img files.
                        showLog("  Supported: ${result.partitions.joinToString(", ")}")
                    } else {
                        // Fallback: permissive mode (accept all .img files, no validation)
                        deviceSupportedPartitions = emptyList()
                        showLog("Partition scan failed: ${result.error}", LogLevel.WARN)
                        showLog("Validation disabled — all .img files will be accepted.", LogLevel.WARN)
                    }
                }
            }
        }

        setupCompressionSelector()
        setupButtons()
        setupToolbar()
        setupDeviceMetaFields()
        setupOutputField()
        setupCustomFilenameField()
        // Dynamic color toggle REMOVED (AUDIT-DC) — color source is pure
        // auto-detect: applyDynamicColorsOverlay() applies Material You on
        // API 31+ via SuiseiColors.isDynamicColorAvailable, no user pref.
        setupBackPressedHandler()  // BUG-H07: OnBackPressedDispatcher
        updateOutputPreview()  // Show default filename preview immediately

        requestStoragePermissions()
        // T17/T19: initial Build FAB gate state (always visible, enabled-gated)
        updateBuildFab()
        handleIncomingIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIncomingIntent(intent)
    }

    // ═══════════════════════════════════════════════════════════════
    //  Initialization
    // ═══════════════════════════════════════════════════════════════

    private fun initializeNative() {
        // Skip if native was already initialized in this process (e.g. Activity was
        // recreated after minimize+reopen). The companion savedLogText already has
        // the initialization messages — re-logging would duplicate them.
        if (nativeInitialized) return
        nativeInitialized = true

        lifecycleScope.launch {
            showLog("Initializing OTAku...", LogLevel.INFO)

            // Check native (Rust) backend
            if (NativeBridge.isLoaded) {
                // AUDIT-T9 (cosmetic #4): keep every JNI call off the Main
                // thread for a uniform discipline, even though these two are
                // trivial in-memory calls (version string + static JSON).
                val (nativeVersion, depCheck) = withContext(Dispatchers.IO) {
                    NativeBridge.getVersion() to NativeBridge.checkDeps()
                }
                showLog("Native backend: $nativeVersion", LogLevel.INFO)
                val available = depCheck.available.joinToString(", ")
                showLog("Native compression: $available", LogLevel.INFO)
                cachedDepCheck = depCheck
                if (depCheck.allOk) {
                    showLog("OTAku ready")
                } else {
                    showLog("Some compression algorithms unavailable", LogLevel.WARN)
                }
            } else {
                showLog("Native backend not loaded: ${NativeBridge.loadError}", LogLevel.ERROR)
                showLog("Possible causes:")
                showLog("  - APK installed from an old build (before v3.0)")
                showLog("  - App installed but native libs extraction failed")
                showLog("  - Try: Uninstall > Re-download latest APK > Install")
            }
        }
    }

    /**
     * Build the 4-line initialization banner that appears when OTAku starts.
     * Used by the Clear Log button to preserve these context lines instead
     * of wiping the entire log. The banner is reconstructed from the live
     * NativeBridge state so the version + compression list stays accurate
     * even if the user clears logs after a native reload.
     *
     * Threading: getVersion()/checkDeps() here are trivial in-memory JNI
     * calls (checkDeps prefers the cached DepCheckResult), so running them
     * on the caller's Main thread is safe by design — documented rather
     * than wrapped in Dispatchers.IO to keep the Clear Log handler
     * synchronous.
     *
     * Format:
     *   Initializing OTAku...
     *   Native backend: otaku-native 1.0.0 (rust)
     *   Native compression: zstd, xz, bzip2, gzip, lz4
     *   OTAku ready
     *
     * If native failed to load, the error variant is returned instead.
     */
    private fun buildInitBanner(): String {
        val sb = StringBuilder()
        sb.append("Initializing OTAku...\n")
        if (NativeBridge.isLoaded) {
            val nativeVersion = NativeBridge.getVersion()
            sb.append("Native backend: $nativeVersion\n")
            val depCheck = cachedDepCheck ?: NativeBridge.checkDeps()
            val available = depCheck.available.joinToString(", ")
            sb.append("Native compression: $available\n")
            if (depCheck.allOk) {
                sb.append("OTAku ready\n")
            } else {
                sb.append("Some compression algorithms unavailable\n")
            }
        } else {
            sb.append("Native backend not loaded: ${NativeBridge.loadError}\n")
            sb.append("Possible causes:\n")
            sb.append("  - APK installed from an old build (before v3.0)\n")
            sb.append("  - App installed but native libs extraction failed\n")
            sb.append("  - Try: Uninstall > Re-download latest APK > Install\n")
        }
        return sb.toString()
    }

    private fun setupToolbar() {
        val toolbar = findViewById<com.google.android.material.appbar.MaterialToolbar>(R.id.toolbar)
        setSupportActionBar(toolbar)
        supportActionBar?.title = getString(R.string.app_name)
        supportActionBar?.subtitle = "v${BuildConfig.VERSION_NAME}"
    }

    // ═══════════════════════════════════════════════════════════════
    //  Theme Management
    // ═══════════════════════════════════════════════════════════════

    /** Apply theme. Default: follow system; user can override to Light/Dark. */
    private fun applyTheme() {
        val themeMode = prefs.getString("pref_theme_mode", "system") ?: "system"
        when (themeMode) {
            "light" -> AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_NO)
            "dark" -> AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_YES)
            else -> AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_FOLLOW_SYSTEM)
        }
    }

    /**
     * Apply Material You dynamic color overlay based on device capability
     * and user preference.
     *
     * IMPL-007: This method ONLY applies the DynamicColors overlay. The base
     * theme (Theme.OTAku.Suisei) is set AFTER super.onCreate() in
     * onCreate() (IMPL-008) so that Resources have the correct night mode
     * before setTheme() resolves theme qualifiers. Previously, setTheme()
     * was called BEFORE super.onCreate(), which caused it to resolve
     * against stale Resources (from the PREVIOUS Activity's night mode),
     * resulting in the wrong theme variant and the "separuh dark, separuh
     * light" mixed-colors bug. The Window stale-color issue is handled
     * by syncWindowToTheme().
     *
     * Must be called AFTER super.onCreate() so night mode is finalized,
     * but BEFORE setContentView() so theme attributes resolve correctly
     * during view inflation.
     */
    private fun applyDynamicColorsOverlay() {
        // AUDIT-DC: pure auto-detect — no user preference. Material You
        // dynamic color is applied whenever the device supports it (API 31+);
        // older devices keep the Suisei Blue base theme untouched.
        if (SuiseiColors.isDynamicColorAvailable) {
            try {
                com.google.android.material.color.DynamicColors
                    .applyToActivityIfAvailable(this)
            } catch (e: Throwable) {
                android.util.Log.e("OTAku", "DynamicColors.applyToActivityIfAvailable() " +
                    "threw: ${e.message}")
            }
        }
    }

    /**
     * IMPL-007: Force-sync the Window's appearance to the current theme.
     *
     * When recreate() is called (e.g., theme switch via cycleTheme()),
     * the new Activity's Window is created in Activity.attach() BEFORE
     * onCreate(). At attach() time, the Resources configuration may
     * still reflect the PREVIOUS Activity's night mode. The Window's
     * background and system bar colors are set once during Window
     * creation and are NOT retroactively updated by later setTheme().
     *
     * This method re-reads window-related attributes from the NOW-CORRECT
     * theme (after super.onCreate() applied the right night mode) and
     * explicitly applies them to the Window, ensuring the Window
     * background, status bar, and navigation bar match the content views.
     */
    private fun syncWindowToTheme() {
        try {
            val window = this.window ?: return

            // Re-read windowBackground from the current theme and apply it.
            val bgAttrs = intArrayOf(android.R.attr.windowBackground)
            val bgTa = obtainStyledAttributes(bgAttrs)
            val background = bgTa.getDrawable(0)
            bgTa.recycle()
            if (background != null) {
                window.setBackgroundDrawable(background)
            }

            // Re-read status bar and navigation bar colors.
            val sysAttrs = intArrayOf(
                android.R.attr.statusBarColor,
                android.R.attr.navigationBarColor
            )
            val sysTa = obtainStyledAttributes(sysAttrs)
            window.statusBarColor = sysTa.getColor(0, android.graphics.Color.TRANSPARENT)
            window.navigationBarColor = sysTa.getColor(1, android.graphics.Color.TRANSPARENT)
            sysTa.recycle()

            // Re-read and apply light status bar / navigation bar flags.
            val lightAttrs = intArrayOf(
                android.R.attr.windowLightStatusBar,
                android.R.attr.windowLightNavigationBar
            )
            val lightTa = obtainStyledAttributes(lightAttrs)
            val lightStatusBar = lightTa.getBoolean(0, false)
            val lightNavigationBar = lightTa.getBoolean(1, false)
            lightTa.recycle()

            // Apply light-bar flags via WindowInsetsController (API 30+)
            // or legacy systemUiVisibility flags (API 26-29).
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                val controller = window.decorView.windowInsetsController
                if (controller != null) {
                    if (lightStatusBar) {
                        controller.setSystemBarsAppearance(
                            android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS,
                            android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                        )
                    } else {
                        controller.setSystemBarsAppearance(
                            0,
                            android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                        )
                    }
                    if (lightNavigationBar) {
                        controller.setSystemBarsAppearance(
                            android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS,
                            android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                        )
                    } else {
                        controller.setSystemBarsAppearance(
                            0,
                            android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                        )
                    }
                }
            } else {
                @Suppress("DEPRECATION")
                var flags = window.decorView.systemUiVisibility
                @Suppress("DEPRECATION")
                val lightStatusBarFlag = android.view.View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                @Suppress("DEPRECATION")
                val lightNavBarFlag = android.view.View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR

                flags = if (lightStatusBar) flags or lightStatusBarFlag
                        else flags and lightStatusBarFlag.inv()
                flags = if (lightNavigationBar) flags or lightNavBarFlag
                        else flags and lightNavBarFlag.inv()

                window.decorView.systemUiVisibility = flags
            }
        } catch (e: Throwable) {
            android.util.Log.e("OTAku", "syncWindowToTheme() failed: ${e.message}")
        }
    }


    /**
     * Re-render partition progress bars from companion state.
     *
     * IMPL-008: Extracted from 3 duplicate code sites (onConfigurationChanged,
     * onResume build-in-progress, onResume build-complete) to reduce code
     * duplication and ensure consistent rendering logic.
     *
     * @param forceComplete If true, render all bars at 100% regardless of
     *   actual progress (used for build completion display).
     */
    private fun renderPartitionProgress(forceComplete: Boolean = false) {
        if (partitionCount <= 0) return
        val barRow = findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
            ?.findViewWithTag<android.widget.LinearLayout>("bar_row")
        if (barRow != null) {
            // AUDIT-F2: read through the locked snapshot (IMPL-011) — a raw
            // indexed read could observe a partially-written array while the
            // build coroutine (Dispatchers.Default) is updating elements.
            val snapshot = snapshotPartitionProgress()
            for (i in 0 until partitionCount) {
                val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                if (bar != null) {
                    bar.isIndeterminate = false
                    bar.progress = if (forceComplete) 100 else snapshot[i]
                }
            }
        }
    }
    override fun onCreateOptionsMenu(menu: android.view.Menu?): Boolean {
        menuInflater.inflate(R.menu.toolbar_menu, menu)
        updateThemeIcon(menu)
        return true
    }

    override fun onPrepareOptionsMenu(menu: android.view.Menu): Boolean {
        updateThemeIcon(menu)
        return super.onPrepareOptionsMenu(menu)
    }

    override fun onOptionsItemSelected(item: android.view.MenuItem): Boolean {
        return when (item.itemId) {
            R.id.action_toggle_theme -> {
                cycleTheme()
                true
            }
            R.id.action_build_ota -> {
                onBuildClicked()
                true
            }
            R.id.action_inspect_payload -> {
                launchPayloadPicker()
                true
            }
            R.id.action_build_payload -> {
                onBuildPayloadClicked()
                true
            }
            else -> super.onOptionsItemSelected(item)
        }
    }

    /** Cycle theme: System -> Light -> Dark -> System
     *
     * IMPL-004/IMPL-005 (theme toggle fix): Sets themeSwitchInProgress
     * flag before applyTheme() + recreate() to prevent double recreation.
     *
     * Without the flag, the sequence is:
     *   1. applyTheme() → setDefaultNightMode() → may trigger onConfigurationChanged()
     *   2. onConfigurationChanged() detects uiMode change → calls recreate()
     *   3. cycleTheme() also calls recreate()
     *   = double recreate → visual flicker, broken colors
     *
     * With the flag:
     *   1. themeSwitchInProgress = true
     *   2. applyTheme() → setDefaultNightMode()
     *   3. onConfigurationChanged() sees flag → skips recreate()
     *   4. cycleTheme() calls recreate() — single, clean recreation
     *   5. onCreate() of new instance clears flag
     *
     * Because configChanges includes uiMode, AppCompatDelegate.setDefaultNightMode
     * does NOT trigger automatic Activity recreation — the theme would only update
     * the delegate internal state but the views would stay in the old mode.
     * Explicit recreate() forces a full layout pass with the new night mode.
     * Companion object state (imageFiles, savedLogText, etc.) survives recreation,
     * so no data is lost.
     */
    private fun cycleTheme() {
        val current = prefs.getString("pref_theme_mode", "system") ?: "system"
        val next = when (current) {
            "system" -> "light"
            "light" -> "dark"
            else -> "system"
        }
        prefs.edit { putString("pref_theme_mode", next) }
        themeSwitchInProgress = true
        applyTheme()
        recreate()
        // IMPL-004 (smooth theme transition): Apply a crossfade animation
        // so the theme switch feels instant rather than a jarring cut.
        // overridePendingTransition must be called immediately after recreate().
        // 0 = no exit animation (old activity fades instantly),
        // android.R.anim.fade_in = new activity fades in smoothly.
        @Suppress("DEPRECATION")
        overridePendingTransition(android.R.anim.fade_in, 0)
    }

    /**
     * Update the theme toggle menu icon to reflect current mode.
     *
     * Three icon states (down from five — the previous sun+badge and
     * moon+badge composites were visually busy at 24dp and the badge was
     * barely visible; consolidated into a single brightness_auto icon):
     *   - "light"  → sun (Material Symbols: light_mode)
     *   - "dark"   → moon (Material Symbols: dark_mode)
     *   - "system" → sun-gear with "A" (Material Symbols: brightness_auto)
     *
     * All three icons use ?attr/colorOnSurface as fillColor, so they
     * automatically adapt to the current theme (light icon on dark bg,
     * dark icon on light bg). No runtime tinting needed.
     */
    private fun updateThemeIcon(menu: android.view.Menu?) {
        val item = menu?.findItem(R.id.action_toggle_theme) ?: return
        val mode = prefs.getString("pref_theme_mode", "system") ?: "system"
        item.setIcon(when (mode) {
            "light" -> R.drawable.ic_theme_light
            "dark" -> R.drawable.ic_theme_dark
            else -> R.drawable.ic_theme_auto
        })
    }


    private fun setupCustomFilenameField() {
        val editFilename = findViewById<com.google.android.material.textfield.TextInputEditText>(R.id.editTextCustomFilename)
        // Restore persisted custom filename (or keep empty for auto)
        editFilename?.setText(prefs.getString("pref_custom_filename", ""))

        // Listen for changes and update preview
        editFilename?.addTextChangedListener(object : android.text.TextWatcher {
            override fun afterTextChanged(s: android.text.Editable?) {
                val text = s?.toString()?.trim() ?: ""
                prefs.edit { putString("pref_custom_filename", text) }
                updateOutputPreview()
            }
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
        })
    }

    private fun setupDeviceMetaFields() {
        val editDevice = findViewById<com.google.android.material.textfield.TextInputEditText>(R.id.editTextDevice)

        // Restore persisted value (or keep empty for default)
        editDevice?.setText(prefs.getString("device", ""))

        // Auto-detect button: detect device codename from vendor partition props
        // (spoof-resistant — uses ro.product.vendor.device + ro.product.board
        // from getprop and /vendor/build.prop, NOT Build.PRODUCT which is
        // easily overridden by Magisk/GSI/LineageOS).
        // If vendor.device and board differ, both are filled comma-separated —
        // matches the flasher script's comma-separated TARGET_DEVICE format.
        findViewById<View>(R.id.buttonAutoDetect)?.setOnClickListener {
            showLog("Detecting device codename (vendor partition props)...")
            lifecycleScope.launch(Dispatchers.IO) {
                val result = NativeBridge.detectDeviceCodename()
                withContext(Dispatchers.Main) {
                    if (result.success && result.codename.isNotEmpty()) {
                        editDevice?.setText(result.codename)
                        prefs.edit { putString("device", result.codename) }
                        updateOutputPreview()
                        updateBuildFab()
                        showLog("Auto-detected device: ${result.codename}")
                        if (result.vendorDevice.isNotEmpty() && result.board.isNotEmpty()
                            && result.vendorDevice != result.board) {
                            showLog("  vendor.device='${result.vendorDevice}', board='${result.board}' — using both (comma-separated)", LogLevel.INFO)
                        }
                    } else {
                        val errMsg = result.error ?: "all vendor sources returned empty"
                        showLog("Auto-detect failed: $errMsg", LogLevel.ERROR)
                        showLog("This can happen if /vendor is not mounted or the device uses non-standard props.", LogLevel.WARN)
                        showLog("Enter your device codename manually.", LogLevel.WARN)
                    }
                }
            }
        }

        // Listen for changes in device codename — persist it + refresh the
        // output filename preview (build gating lives in onBuildClicked()).
        editDevice?.addTextChangedListener(object : android.text.TextWatcher {
            override fun afterTextChanged(s: android.text.Editable?) {
                val text = s?.toString()?.trim() ?: ""
                prefs.edit { putString("device", text) }
                updateOutputPreview()
                updateBuildFab()
            }
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
        })
    }

    private fun setupOutputField() {
        val editOutput = findViewById<android.widget.EditText>(R.id.editTextOutput)

        // Restore persisted output directory, or default to /storage/emulated/0/OTAku
        val savedDir = prefs.getString("output_dir", null)
        if (savedDir != null) {
            outputDirPath = savedDir
            editOutput?.setText(savedDir)
        } else {
            editOutput?.setText(outputDir.absolutePath)
            outputDirPath = outputDir.absolutePath
        }

        // Listen for manual edits in the output path field
        editOutput?.addTextChangedListener(object : android.text.TextWatcher {
            override fun afterTextChanged(s: android.text.Editable?) {
                val text = s?.toString()?.trim()
                if (!text.isNullOrEmpty() && text != outputDir.absolutePath) {
                    outputDirPath = text
                    prefs.edit { putString("output_dir", text) }
                    updateOutputPreview()
                }
            }
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
        })
    }

    private fun setupCompressionSelector() {
        // T19: AppCompatSpinner upgraded to the M3 ExposedDropdownMenu pattern
        // (TextInputLayout + MaterialAutoCompleteTextView — same as the payload
        // build dialog since T14). The id still resolves to the dropdown text
        // field. displayLabels map 1:1 onto OTABridge.COMPRESSION_ALGORITHMS
        // indices, and inputType=none in the XML means the only reachable
        // values are the adapter items — no free-text validation needed.
        val dropdown = findViewById<android.widget.AutoCompleteTextView>(R.id.spinnerCompression)
        // Ordered by compression ratio: best → fastest (matches OTABridge.COMPRESSION_ALGORITHMS)
        val displayLabels = listOf(
            "zstd — supreme (~35%)",
            "xz — ultra (~45%)",
            "bzip2 — high (~50%)",
            "gzip — standard (~60%)",
            "lz4 — fast (~70%)"
        )
        dropdown?.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, displayLabels))

        // Restore persisted compression selection (survives Activity recreation from
        // theme switch, config change, etc). Without this, recreate() resets the
        // selector and selectedCompression to the default "gzip". setText(.., false)
        // never fires the item-click listener, so restoring is silent by design.
        val savedCompression = prefs.getString("pref_compression", "gzip") ?: "gzip"
        var restoreIdx = OTABridge.COMPRESSION_ALGORITHMS.indexOf(savedCompression)
        if (restoreIdx < 0) restoreIdx = OTABridge.COMPRESSION_ALGORITHMS.indexOf("gzip")
        if (restoreIdx >= 0 && restoreIdx < displayLabels.size) {
            dropdown?.setText(displayLabels[restoreIdx], false)
            selectedCompression = OTABridge.COMPRESSION_ALGORITHMS[restoreIdx]
        }

        dropdown?.setOnItemClickListener { _, _, position, _ ->
            selectedCompression = OTABridge.COMPRESSION_ALGORITHMS[position]
            prefs.edit { putString("pref_compression", selectedCompression) }
            updateCompressionLevelSpinner()
            updateOutputPreview()
        }

        // Initialize compression level selector
        setupCompressionLevelSpinner()
    }

    // Compression level ranges per algorithm (matches Rust native backend LEVEL_RANGES)
    // Ordered by compression ratio: best → fastest
    private val COMPRESSION_LEVELS: Map<String, Pair<Int, Int>> = mapOf(
        "zstd" to Pair(1, 22),     // zstd: levels 1-22, default 3
        "xz" to Pair(0, 9),       // stdlib lzma: levels 0-9, default 6
        "bzip2" to Pair(1, 9),    // stdlib bzip2: levels 1-9, default 9
        "gzip" to Pair(1, 9),     // stdlib gzip: levels 1-9, default 6
        "lz4" to Pair(1, 12)      // lz4_flex frame: levels 1-12, default 4
    )

    // Default compression level per algorithm (single source of truth for UI labels)
    private val DEFAULT_COMPRESSION_LEVELS: Map<String, Int> = mapOf(
        "zstd" to 3,
        "xz" to 6,
        "bzip2" to 9,
        "gzip" to 6,
        "lz4" to 4
    )

    private fun setupCompressionLevelSpinner() {
        // T19: M3 ExposedDropdownMenu — item clicks are the ONLY way to change
        // the level (inputType=none; setText(.., false) never fires the
        // listener), so the ghost onItemSelected events of the old Spinner
        // (adapter swap auto-firing position 0) are gone by construction.
        val dropdown = findViewById<android.widget.AutoCompleteTextView>(R.id.spinnerCompressionLevel)

        dropdown?.setOnItemClickListener { _, _, position, _ ->
            val items = getCurrentLevelItems()
            selectedCompressionLevel = if (position < items.size) items[position] else 0
            prefs.edit { putInt("pref_compression_level", selectedCompressionLevel) }
        }
        updateCompressionLevelSpinner()
    }

    private fun getCurrentLevelItems(): List<Int> {
        val range = COMPRESSION_LEVELS[selectedCompression] ?: (0 to 0)
        val (min, max) = range
        return if (min == 0 && max == 0) {
            listOf(0)  // Unknown algorithm → just show "Default"
        } else {
            // BUG FIX: Previously, `listOf(0) + (min..max)` produced duplicate zeros
            // for algorithms where min=0 (xz: 0-9). The list would be
            // [0, 0, 1, 2, ...] with two "Default" labels. Now we filter out 0 from
            // the numeric range so the sentinel 0 (meaning "Default") is unique.
            listOf(0) + (min..max).filter { it != 0 }.toList()
        }
    }

    private fun updateCompressionLevelSpinner() {
        // T19: M3 ExposedDropdownMenu — rebuild the adapter for the newly
        // selected algorithm, then reset the field to "Default". The label
        // scheme is unchanged from the Spinner era: sentinel 0 renders as
        // "Default (<algo default>)", explicit levels render as the number.
        val dropdown = findViewById<android.widget.AutoCompleteTextView>(R.id.spinnerCompressionLevel) ?: return
        val items = getCurrentLevelItems()
        val defaultLevel = DEFAULT_COMPRESSION_LEVELS[selectedCompression] ?: 0
        val labels = items.map { if (it == 0) "Default ($defaultLevel)" else "$it" }
        dropdown.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, labels))
        // Reset selection to "Default" (labels is never empty — sentinel 0
        // always present, see getCurrentLevelItems).
        dropdown.setText(labels[0], false)
        selectedCompressionLevel = 0
    }

    private fun setupButtons() {
        findViewById<View>(R.id.buttonAddImages).setOnClickListener {
            // Launch document picker filtered to .img files only
            // EXTRA_MIME_TYPES narrows selection — non-.img files are grayed out
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "*/*"
                putExtra(Intent.EXTRA_MIME_TYPES, arrayOf("application/octet-stream"))
                putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
            }
            imageFileChooser.launch(intent)
        }

        findViewById<View>(R.id.buttonBrowseOutput).setOnClickListener {
            outputDirChooser.launch(null)
        }

        findViewById<View>(R.id.buttonRemoveAll)?.setOnClickListener {
            // Cancel any in-flight image-loading coroutine FIRST.
            // Without this, the copy keeps running in the background and
            // re-adds the partition to imageFiles when it finishes —
            // causing the "loading chaos" bug (duplicate log entries,
            // concurrent writes to the same destFile, mixed sizes).
            imageLoadingJob?.cancel()
            imageLoadingJob = null

            imageFiles.clear()
            copyPendingRemovals()
            // Also clean up any orphaned .part temp files from interrupted copies
            inputDir.listFiles()?.forEach { file ->
                if (file.name.endsWith(".part")) file.delete()
            }
            updateImageListUI()
            updateOutputPreview()
            showLog("All images removed.")
        }

        findViewById<View>(R.id.buttonCopyLog).setOnClickListener {
            copyLogToClipboard()
        }

        findViewById<View>(R.id.buttonClearLog).setOnClickListener {
            // Clear user-generated log content but PRESERVE the initialization
            // banner (4 lines: Initializing, Native backend, Native compression,
            // OTAku ready). The init banner identifies the app version + available
            // compression algorithms — useful context the user shouldn't lose
            // when clearing build logs.
            //
            // Strategy: rebuild savedLogText from the cached init messages,
            // then refresh the TextView to match.
            val initBanner = buildInitBanner()
            savedLogText.setLength(0)
            appendToSavedLog(initBanner)
            // BUG-H02 fix: Null-safe access to textViewLog
            findViewById<android.widget.TextView>(R.id.textViewLog)?.text = initBanner
        }

        // ════════════════════════════════════════════════════════════
        //  Log panel — CONTAINER MORPH (single surface, M3-style)
        // ════════════════════════════════════════════════════════════
        //  Redesign: replaces the old two-view mini-pill ↔ logCard
        //  alpha crossfade, which read as a "ghost swap" — position,
        //  size, color and elevation all teleported at once. ONE
        //  MaterialCardView now morphs continuously between:
        //    COLLAPSED pill : width=pillW, height=pillH, radius=pillH/2,
        //                    content = arrow + "LOGS" label only
        //    EXPANDED card  : width=MATCH_PARENT, height=overlay share
        //                    radius=16dp, full log content
        //  Spatial continuity is preserved (M3 container-transform
        //  intent): the pill IS the collapsed card — same fill
        //  (colorPrimaryContainer), same stroke, same elevation; only
        //  geometry + content alpha animate.
        //
        //  OVERLAY (T16): the card is a TRUE floating surface now — a
        //  direct child of the root CoordinatorLayout with
        //  gravity=bottom, not in the LinearLayout flow. It floats
        //  above the full-height settings scroll; the scroll's bottom
        //  padding mirrors the card height so the last row stays
        //  reachable (updateSettingsBottomPadding).
        //
        //  Gesture semantics (vertical, unchanged):
        //    Drag UP   → expand   (1:1 finger tracking)
        //    Drag DOWN → collapse
        //    Tap       → toggle
        //    Release   → SpringAnimation settle, velocity-continuous
        //                with the finger (M3 Expressive motion feel)
        // ════════════════════════════════════════════════════════════
        val logCard = findViewById<com.google.android.material.card.MaterialCardView>(R.id.logCard)
        val logHeader = findViewById<View>(R.id.logHeaderBar)
        val toggleBtn = findViewById<android.widget.ImageView>(R.id.buttonToggleLog)
        toggleBtn?.contentDescription = getString(R.string.cd_toggle_log_panel)  // P0-fix C-04
        val logDivider = findViewById<View>(R.id.logDivider)
        val logScrollView = findViewById<androidx.core.widget.NestedScrollView>(R.id.scrollViewLog)
        val buttonCopyLog = findViewById<View>(R.id.buttonCopyLog)
        val buttonClearLog = findViewById<View>(R.id.buttonClearLog)
        val scrollViewSettings = findViewById<androidx.core.widget.NestedScrollView>(R.id.scrollViewSettings)
        val parentLayout = logCard?.parent as? androidx.coordinatorlayout.widget.CoordinatorLayout
        val density = resources.displayMetrics.density
        val CARD_RADIUS_PX = 16f * density

        // ── Morph state ──
        // morphProgress: 0f = collapsed pill, 1f = expanded card.
        // Values slightly outside [0,1] occur transiently during spring
        // motion; applyMorph clamps height overshoot to +6% and
        // hard-clamps everything below 0 (the pill must never shrink
        // under its content height).
        var morphProgress = if (isLogExpanded) 1f else 0f
        var pillWidth = 0
        var pillHeight = 0
        var fullCardWidth = 0
        var fullCardHeight = 0
        var springAnim: androidx.dynamicanimation.animation.SpringAnimation? = null

        /**
         * Measure the collapsed-pill geometry:
         *   pillHeight = card wrap height with content (scroll + divider)
         *                and Copy/Clear buttons hidden
         *   pillWidth  = card wrap width in the same hidden state
         * Uses manual measure() passes (works before first layout),
         * then restores previous visibility + LayoutParams — same
         * technique as the previous header-measure helper.
         */
        fun measurePillGeometry() {
            logCard?.let { card ->
                val svWas = logScrollView?.visibility ?: View.VISIBLE
                val dvWas = logDivider?.visibility ?: View.VISIBLE
                val cpWas = buttonCopyLog?.visibility ?: View.VISIBLE
                val clWas = buttonClearLog?.visibility ?: View.VISIBLE
                logScrollView?.visibility = View.GONE
                logDivider?.visibility = View.GONE
                buttonCopyLog?.visibility = View.GONE
                buttonClearLog?.visibility = View.GONE
                val lp = card.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams
                val prevW = lp?.width
                val prevH = lp?.height
                if (lp != null) {
                    lp.width = android.view.ViewGroup.LayoutParams.WRAP_CONTENT
                    lp.height = android.view.ViewGroup.LayoutParams.WRAP_CONTENT
                }
                card.measure(
                    android.view.View.MeasureSpec.makeMeasureSpec(0, android.view.View.MeasureSpec.UNSPECIFIED),
                    android.view.View.MeasureSpec.makeMeasureSpec(0, android.view.View.MeasureSpec.UNSPECIFIED)
                )
                if (card.measuredHeight > 0) pillHeight = card.measuredHeight
                if (card.measuredWidth > 0) pillWidth = card.measuredWidth
                if (lp != null) {
                    if (prevW != null) lp.width = prevW
                    if (prevH != null) lp.height = prevH
                }
                logScrollView?.visibility = svWas
                logDivider?.visibility = dvWas
                buttonCopyLog?.visibility = cpWas
                buttonClearLog?.visibility = clWas
            }
            if (pillHeight <= 0) pillHeight = (56 * density).toInt()  // fallback — 56dp M3 pill/FAB parity (T18)
            if (pillWidth <= 0) pillWidth = (160 * density).toInt()   // fallback
        }

        /**
         * Measure the expanded-card target geometry from the parent:
         *   fullCardWidth  = what match_parent resolves to (parent width
         *                    minus padding minus the card's own margins)
         *   fullCardHeight = weight-based share, assuming the card back
         *                    at weight=1 alongside its siblings
         * Independent of the card's CURRENT LayoutParams (works during
         * mid-morph explicit-size states) — the same invariant the
         * previous expanded-height helper guaranteed.
         */
        fun measureFullGeometry() {
            val parent = parentLayout ?: return
            val card = logCard ?: return
            if (parent.height <= 0 || parent.width <= 0) return
            val lp = card.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams ?: return
            fullCardWidth = parent.width - parent.paddingLeft - parent.paddingRight - lp.leftMargin - lp.rightMargin
            // Overlay share: parity with the old 50/50 LinearLayout weight
            // split — the scroll area (settings view = coordinator minus
            // appbar) is the reference; the card covers its lower half.
            val scrollArea = scrollViewSettings?.height?.takeIf { it > 0 }
                ?: (parent.height - parent.paddingTop - parent.paddingBottom)
            val available = scrollArea - lp.topMargin - lp.bottomMargin
            val share = (available * 0.5f).toInt()
            fullCardHeight = share.coerceAtLeast(pillHeight)
        }

        /**
         * Apply one morph frame at `progress` (0 = pill, 1 = card).
         * Every visual property derives from the single progress value,
         * so all frames are internally consistent — no timing drift
         * between size, radius, rotation and fades.
         */
        fun applyMorph(progress: Float) {
            val card = logCard ?: return
            val lp = card.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams ?: return
            val cp = progress.coerceIn(0f, 1.06f)  // small spring overshoot allowed (height only)
            val vis = progress.coerceIn(0f, 1f)    // clamped for fades + radius
            // Geometry — explicit size; gravity keeps the card pinned
            // to the bottom through every intermediate frame
            val h = (pillHeight + (fullCardHeight - pillHeight) * cp).toInt()
            val w = (pillWidth + (fullCardWidth - pillWidth) * cp).toInt()
            lp.width = w
            lp.height = h
            card.layoutParams = lp
            // Corner radius: fully-round pill → 16dp card
            card.radius = (pillHeight / 2f) + (CARD_RADIUS_PX - pillHeight / 2f) * vis
            // Arrow rotates continuously 0° (expand_more) → 180° (expand_less look)
            toggleBtn?.rotation = vis * 180f
            // Log content fades in fast — fully visible by 50% progress
            val contentAlpha = ((vis - 0.2f) / 0.3f).coerceIn(0f, 1f)
            logDivider?.alpha = contentAlpha
            logScrollView?.alpha = contentAlpha
            val showContent = vis > 0.19f
            if (showContent) {
                if (logDivider?.visibility != View.VISIBLE) logDivider?.visibility = View.VISIBLE
                if (logScrollView?.visibility != View.VISIBLE) logScrollView?.visibility = View.VISIBLE
            } else {
                if (logDivider?.visibility != View.GONE) logDivider?.visibility = View.GONE
                if (logScrollView?.visibility != View.GONE) logScrollView?.visibility = View.GONE
            }
            // Copy/Clear buttons fade in last — fully visible by 80%
            val btnAlpha = ((vis - 0.55f) / 0.25f).coerceIn(0f, 1f)
            buttonCopyLog?.alpha = btnAlpha
            buttonClearLog?.alpha = btnAlpha
            val showButtons = vis > 0.54f
            if (showButtons) {
                if (buttonCopyLog?.visibility != View.VISIBLE) buttonCopyLog?.visibility = View.VISIBLE
                if (buttonClearLog?.visibility != View.VISIBLE) buttonClearLog?.visibility = View.VISIBLE
            } else {
                if (buttonCopyLog?.visibility != View.GONE) buttonCopyLog?.visibility = View.GONE
                if (buttonClearLog?.visibility != View.GONE) buttonClearLog?.visibility = View.GONE
            }
        }

        /**
         * Overlay-aware reachability (T16): the log card is a true
         * floating overlay (CoordinatorLayout gravity-bottom), so its
         * current height must be mirrored into the settings scroll's
         * bottom padding (+ card bottom margin) — otherwise the last
         * settings row would scroll into dead space hidden under the
         * card. clipToPadding=false on scrollViewSettings lets content
         * glide under the floating card instead of hard-clipping.
         */
        fun updateSettingsBottomPadding(cardHeightPx: Int) {
            val sv = scrollViewSettings ?: return
            val lp = logCard?.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams
            // Floor at the floating Build FAB zone (56dp FAB + 16dp
            // margin): whichever overlay is taller wins (expanded card
            // vs the FAB row at bottom-end).
            sv.setPadding(sv.paddingLeft, sv.paddingTop, sv.paddingRight,
                (cardHeightPx + (lp?.bottomMargin ?: 0)).coerceAtLeast((72 * density).toInt()))
        }

        /** Snap to the canonical EXPANDED params (no animation). */
        fun applyExpandedCanonical() {
            val card = logCard ?: return
            val lp = card.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams ?: return
            lp.width = android.view.ViewGroup.LayoutParams.MATCH_PARENT
            lp.height = fullCardHeight.coerceAtLeast(pillHeight)
            lp.gravity = android.view.Gravity.BOTTOM
            card.layoutParams = lp
            card.radius = CARD_RADIUS_PX
            toggleBtn?.rotation = 180f
            logDivider?.visibility = View.VISIBLE
            logDivider?.alpha = 1f
            logScrollView?.visibility = View.VISIBLE
            logScrollView?.alpha = 1f
            buttonCopyLog?.visibility = View.VISIBLE
            buttonCopyLog?.alpha = 1f
            buttonClearLog?.visibility = View.VISIBLE
            buttonClearLog?.alpha = 1f
            morphProgress = 1f
            updateSettingsBottomPadding(fullCardHeight)
        }

        /** Snap to the canonical COLLAPSED (pill) params (no animation). */
        fun applyPillCanonical() {
            val card = logCard ?: return
            val lp = card.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams ?: return
            lp.width = pillWidth
            lp.height = pillHeight
            lp.gravity = android.view.Gravity.BOTTOM
            card.layoutParams = lp
            card.radius = pillHeight / 2f
            toggleBtn?.rotation = 0f
            logDivider?.visibility = View.GONE
            logDivider?.alpha = 0f
            logScrollView?.visibility = View.GONE
            logScrollView?.alpha = 0f
            buttonCopyLog?.visibility = View.GONE
            buttonCopyLog?.alpha = 0f
            buttonClearLog?.visibility = View.GONE
            buttonClearLog?.alpha = 0f
            morphProgress = 0f
            updateSettingsBottomPadding(pillHeight)
        }

        fun finalizeState(expanded: Boolean) {
            if (expanded) applyExpandedCanonical() else applyPillCanonical()
            isLogExpanded = expanded
        }

        /**
         * Spring-settle to a target state (0 = pill, 1 = expanded).
         * Release velocity carries over from the drag (px/s → progress/s)
         * so the surface continues the finger's motion — no visual seam
         * between drag and settle. dampingRatio 0.85 / stiffness 450 =
         * snappy with a subtle settle (M3 Expressive intent, no jelly).
         */
        fun springTo(target: Float, velocityPxSec: Float = 0f) {
            val range = (fullCardHeight - pillHeight).coerceAtLeast(1)
            springAnim?.cancel()
            val holder = androidx.dynamicanimation.animation.FloatValueHolder(morphProgress)
            val anim = androidx.dynamicanimation.animation.SpringAnimation(holder)
            anim.spring = androidx.dynamicanimation.animation.SpringForce(target)
                .setDampingRatio(0.85f)
                .setStiffness(450f)
            anim.setStartVelocity(velocityPxSec / range)
            anim.minimumVisibleChange = 1f / range
            anim.addUpdateListener { _, value, _ ->
                morphProgress = value
                applyMorph(value)
            }
            anim.addEndListener { _, canceled, _, _ ->
                if (!canceled) finalizeState(target > 0.5f)
            }
            anim.start()
            springAnim = anim
        }

        // ── Gesture — unified header/pill touch handler ──
        // The header bar IS the pill surface when collapsed — one handler
        // covers both states (previously the mini pill needed its own).
        val DRAG_TOUCH_SLOP = 6f
        val TAP_THRESHOLD = 16f
        val FLING_THRESHOLD = 500f
        var dragStartY = 0f
        var dragStartX = 0f
        var dragStartProgress = 0f
        var wasDragging = false
        var lastMoveTime = 0L
        var lastMoveY = 0f
        var dragVelocity = 0f  // px/sec, + = expand direction (up)

        // T18 — the gesture handler is a SHARED surface: attached to both
        // the header bar and the arrow icon. The arrow previously relied on
        // touch fall-through (isClickable=false), which still left icon-area
        // taps and drags dead — an XML clickable/focusable/ripple child can
        // blur the dispatch chain. An explicit handler on the child makes
        // tap AND drag deterministic on both surfaces; the handler reads
        // event.rawX/rawY (screen-absolute), so one closure serves both.
        val logGestureHandler = android.view.View.OnTouchListener { _, event ->
            when (event.actionMasked) {
                android.view.MotionEvent.ACTION_DOWN -> {
                    springAnim?.cancel()
                    dragStartY = event.rawY
                    dragStartX = event.rawX
                    wasDragging = false
                    lastMoveTime = android.os.SystemClock.uptimeMillis()
                    lastMoveY = event.rawY
                    dragVelocity = 0f
                    // Refresh the expand target for this gesture (parent
                    // size may have changed since the last interaction).
                    measureFullGeometry()
                    // Pin the CURRENT geometry to explicit sizes so the
                    // morph math is stable during drag. If we were in the
                    // canonical expanded state (MATCH_PARENT + explicit
                    // overlay height), pinning to the actual laid-out
                    // size = zero jump.
                    logCard?.let { card ->
                        val clp = card.layoutParams as? androidx.coordinatorlayout.widget.CoordinatorLayout.LayoutParams
                        if (clp != null) {
                            clp.width = card.width
                            clp.height = card.height
                            card.layoutParams = clp
                        }
                    }
                    val range = (fullCardHeight - pillHeight).coerceAtLeast(1)
                    morphProgress = ((logCard?.height ?: pillHeight) - pillHeight).toFloat() / range
                    dragStartProgress = morphProgress.coerceIn(0f, 1f)
                    true
                }
                android.view.MotionEvent.ACTION_MOVE -> {
                    val dy = event.rawY - dragStartY
                    val dx = event.rawX - dragStartX
                    val absDy = Math.abs(dy)
                    val absDx = Math.abs(dx)
                    if (absDy > absDx && absDy > DRAG_TOUCH_SLOP) {
                        wasDragging = true
                        // Velocity (px/sec, + = expand): smoothed 60/40 EMA,
                        // inverted so up = positive — same as before.
                        val now = android.os.SystemClock.uptimeMillis()
                        val dt = (now - lastMoveTime).coerceAtLeast(1L)
                        val instVel = ((event.rawY - lastMoveY) / dt * 1000f)
                        dragVelocity = if (dragVelocity == 0f) {
                            -instVel
                        } else {
                            (dragVelocity * 0.6f + (-instVel) * 0.4f)
                        }
                        lastMoveTime = now
                        lastMoveY = event.rawY
                        // 1:1 finger tracking: up = positive = expand
                        val effectiveDy = -dy
                        val range = (fullCardHeight - pillHeight).coerceAtLeast(1)
                        morphProgress = (dragStartProgress + effectiveDy / range).coerceIn(0f, 1f)
                        applyMorph(morphProgress)
                    }
                    true
                }
                android.view.MotionEvent.ACTION_UP -> {
                    val dy = event.rawY - dragStartY
                    val dx = event.rawX - dragStartX
                    if (wasDragging) {
                        // Fling-aware target (same thresholds as before)
                        val shouldExpand = when {
                            dragVelocity > FLING_THRESHOLD -> true    // fling up → expand
                            dragVelocity < -FLING_THRESHOLD -> false   // fling down → collapse
                            else -> morphProgress > 0.5f               // passive: nearest state
                        }
                        springTo(if (shouldExpand) 1f else 0f, dragVelocity)
                    } else if (Math.abs(dy) < TAP_THRESHOLD && Math.abs(dx) < TAP_THRESHOLD) {
                        // Tap → toggle
                        springTo(if (morphProgress > 0.5f) 0f else 1f)
                    }
                    wasDragging = false
                    true
                }
                android.view.MotionEvent.ACTION_CANCEL -> {
                    // AUDIT-F3: the system cancelled the gesture (parent
                    // intercepted, accessibility action, multi-window
                    // switches…). Previously this fell through to
                    // `else -> false`, leaving the card frozen mid-morph
                    // with explicit geometry, no running animation, and
                    // wasDragging stuck true until the next touch.
                    // Settle toward the nearest state so the surface is
                    // never left stranded (same fling-aware logic as UP).
                    if (wasDragging) {
                        val shouldExpand = when {
                            dragVelocity > FLING_THRESHOLD -> true
                            dragVelocity < -FLING_THRESHOLD -> false
                            else -> morphProgress > 0.5f
                        }
                        springTo(if (shouldExpand) 1f else 0f, dragVelocity)
                    } else {
                        springTo(if (morphProgress > 0.5f) 1f else 0f)
                    }
                    wasDragging = false
                    true
                }
                else -> false
            }
        }
        logHeader?.setOnTouchListener(logGestureHandler)
        toggleBtn?.setOnTouchListener(logGestureHandler)

        // Keyboard/accessibility path: activation via performClick()
        // (TalkBack double-tap, keyboard focus + Enter) routes here even
        // though the touch handler consumes touch events.
        logHeader?.setOnClickListener {
            springTo(if (morphProgress > 0.5f) 0f else 1f)
        }

        // T18 — arrow as full gesture + a11y surface: the shared handler
        // above covers physical taps and drags on the icon (both toggle
        // states); clickable stays TRUE so TalkBack/keyboard can focus
        // and activate it. performClick routes to the listener below —
        // physical taps never double-fire because the touch handler
        // consumes them first.
        toggleBtn?.isClickable = true
        toggleBtn?.setOnClickListener {
            springTo(if (morphProgress > 0.5f) 0f else 1f)
        }

        // ── Init (no animation) — survives Activity recreation ──
        measurePillGeometry()
        // Provisional expanded height until first layout measures the
        // real overlay share (post{} refines via measureFullGeometry) —
        // the expanded height is explicit in overlay mode, unlike the
        // old weight-based split which resolved at layout time.
        if (fullCardHeight <= 0) fullCardHeight = (resources.displayMetrics.heightPixels * 0.45f).toInt()
        if (isLogExpanded) applyExpandedCanonical() else applyPillCanonical()
        logCard?.post {
            // After first layout: refine geometry with real parent sizes
            // (re-measuring the pill also picks up final font metrics).
            measurePillGeometry()
            measureFullGeometry()
            // Absorb refined geometry in BOTH states — the expanded
            // height is explicit now (overlay share), so it must be
            // re-derived once the parent is actually laid out.
            if (morphProgress <= 0f) applyPillCanonical() else applyExpandedCanonical()
        }
        // Prevent parent NestedScrollView from stealing scroll events inside the log panel
        logScrollView?.setOnTouchListener { v, _ ->
            v.parent?.requestDisallowInterceptTouchEvent(true)
            false
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Permission handling
    // ═══════════════════════════════════════════════════════════════

    private fun requestStoragePermissions() {
        val permissionsToRequest = mutableListOf<String>()

        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.READ_EXTERNAL_STORAGE)
                != PackageManager.PERMISSION_GRANTED
            ) permissionsToRequest.add(Manifest.permission.READ_EXTERNAL_STORAGE)
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.WRITE_EXTERNAL_STORAGE)
                != PackageManager.PERMISSION_GRANTED
            ) permissionsToRequest.add(Manifest.permission.WRITE_EXTERNAL_STORAGE)
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED
            ) permissionsToRequest.add(Manifest.permission.POST_NOTIFICATIONS)
        }

        if (permissionsToRequest.isNotEmpty()) {
            permissionLauncher.launch(permissionsToRequest.toTypedArray())
        } else {
            // All runtime permissions already granted — check manage storage + battery
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && !Environment.isExternalStorageManager()) {
                promptManageStorage()
                pendingBatteryPrompt = true
            } else {
                // All storage permissions already resolved — check battery optimization
                checkBatteryOptimizationAtStartup()
            }
        }
    }

    /** Check and prompt battery optimization at startup (after storage permissions are resolved). */
    private fun checkBatteryOptimizationAtStartup() {
        if (prefs.getBoolean("pref_battery_prompted", false)) return
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        if (pm.isIgnoringBatteryOptimizations(packageName)) {
            prefs.edit { putBoolean("pref_battery_prompted", true) }
            return
        }
        promptBatteryOptimization()
    }

    private fun promptManageStorage() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            if (!Environment.isExternalStorageManager()) {
                MaterialAlertDialogBuilder(this)
                    .setTitle("Storage Permission Required")
                    .setMessage(
                        "OTAku needs full file access to read partition images " +
                        "and save the output ZIP.\n\n" +
                        "Please grant \"All files access\" on the next screen."
                    )
                    .setPositiveButton("Grant Access") { _, _ ->
                        val intent = Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION).apply {
                            data = Uri.parse("package:$packageName")
                        }
                        startActivity(intent)
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  File handling
    // ═══════════════════════════════════════════════════════════════

    private fun handleOutputDirSelected(uri: Uri) {
        // Take persistable URI permission so we can read/write after reboot
        try {
            contentResolver.takePersistableUriPermission(
                uri, Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            )
        } catch (_: SecurityException) { }

        // Resolve SAF tree URI to a real filesystem path
        val resolvedPath = resolveTreeUriToPath(uri) ?: uri.toString()
        outputDirPath = resolvedPath
        prefs.edit { putString("output_dir", resolvedPath) }
        runOnUiThread {
            // BUG-H02 fix: Use null-safe operator to prevent NPE if view not found
            findViewById<android.widget.EditText>(R.id.editTextOutput)
                ?.setText(resolvedPath)
            showLog("Output directory: $resolvedPath")
            updateOutputPreview()
        }
    }

    /**
     * Resolve a SAF tree URI to a filesystem path.
     * treeDocId is typically "primary:<path>" or "XXXX-XXXX:<path>".
     */
    private fun resolveTreeUriToPath(uri: Uri): String? {
        return try {
            val treeDocId = DocumentsContract.getTreeDocumentId(uri)
            val split = treeDocId.split(":", limit = 2)
            if (split.size == 2) {
                val volume = split[0]
                val path = split[1]
                val storageRoot = when (volume) {
                    "primary" -> "/storage/emulated/0"
                    else -> "/storage/$volume"
                }
                "$storageRoot/$path"
            } else {
                uri.lastPathSegment?.let { "/storage/emulated/0/$it" }
            }
        } catch (_: Exception) {
            uri.lastPathSegment
        }
    }

    /**
     * Resolve a SAF document URI (single file, not tree) to a real filesystem path.
     *
     * This is the key to avoiding the copy-to-inputDir step. When the user
     * picks a .img file via SAF, the URI is typically:
     *   content://com.android.externalstorage.documents/document/primary%3ADownload%2Fboot.img
     *
     * We can parse the document ID ("primary:Download/boot.img") to recover
     * the real filesystem path ("/storage/emulated/0/Download/boot.img").
     *
     * If the file is on external SD card, the volume is the card's UUID
     * (e.g. "1A2B-3C4D"), and the path is "/storage/1A2B-3C4D/...".
     *
     * Returns null if:
     *   - The URI scheme is not "content" (e.g. already a file:// URI)
     *   - The document ID can't be parsed (virtual document, cloud provider)
     *   - The resolved path doesn't exist or isn't readable
     *
     * When null is returned, the caller should fall back to copying the file
     * via ContentResolver.openInputStream() — this handles cloud providers
     * (Google Drive, etc.) and other virtual documents that don't have a
     * real filesystem path.
     */
    private fun resolveUriToFilePath(uri: Uri): String? {
        // Only content:// URIs from the Documents provider can be resolved.
        // file:// URIs already have the path.
        if (uri.scheme == "file") {
            val path = uri.path
            return if (path != null && java.io.File(path).canRead()) path else null
        }
        if (uri.scheme != "content") return null

        return try {
            val docId = DocumentsContract.getDocumentId(uri)
            val split = docId.split(":", limit = 2)
            if (split.size != 2) return null

            val volume = split[0]
            val relativePath = split[1]

            val storageRoot = when (volume) {
                "primary" -> "/storage/emulated/0"
                else -> "/storage/$volume"
            }
            val fullPath = "$storageRoot/$relativePath"

            // Verify the file exists and is readable by our app.
            // MANAGE_EXTERNAL_STORAGE grants broad access, but some paths
            // (e.g. /data/data/other.app/) are still off-limits.
            val file = java.io.File(fullPath)
            if (file.exists() && file.canRead()) {
                fullPath
            } else {
                null
            }
        } catch (_: Exception) {
            null
        }
    }

    private var outputDirPath: String? = null

    // IMPL-008: Cached view references — avoids repeated findViewById() calls
    // in setUIExecuting() which runs on every progress update during builds.
    // findViewById() is O(n) view hierarchy traversal; caching eliminates jank.
    private var cachedBtnAddImages: View? = null
    private var cachedBtnRemoveAll: View? = null
    private var cachedBtnBrowseOutput: View? = null
    // T19: compression selectors are M3 ExposedDropdownMenus now — cache the
    // TextInputLayout wrappers. setEnabled on the layout recursively disables
    // the inner MaterialAutoCompleteTextView AND greys the outlined box
    // (verified against material 1.12.0 source: setEnabled → recursiveSetEnabled).
    private var cachedLayoutCompression: com.google.android.material.textfield.TextInputLayout? = null
    private var cachedLayoutCompressionLevel: com.google.android.material.textfield.TextInputLayout? = null
    private var cachedEditDevice: View? = null
    private var cachedBtnAutoDetect: View? = null
    private var cachedEditFilename: View? = null
    private var cachedProgressContainer: android.widget.LinearLayout? = null
    private var cachedFabBuild: com.google.android.material.floatingactionbutton.ExtendedFloatingActionButton? = null
    // IMPL-012: Cached bar_row reference — avoids findViewWithTag() on every
    // onProgress callback (500ms poll interval × multi-minute build = thousands
    // of calls). Resolved lazily in the onProgress UI update block.
    private var cachedBarRow: android.widget.LinearLayout? = null

    // IMPL-013: Eagerly resolve all frequently-accessed view references.
    // Called once after setContentView() in onCreate(). Previously, these
    // were resolved lazily in setUIExecuting() via an if-null check on
    // every call — but setUIExecuting() runs on every progress update
    // during builds (every 500ms for minutes), so the null check was
    // redundant overhead after the first call. Eager resolution is
    // cleaner and avoids the null-check branching on the hot path.
    private fun cacheViews() {
        cachedBtnAddImages = findViewById(R.id.buttonAddImages)
        cachedBtnRemoveAll = findViewById(R.id.buttonRemoveAll)
        cachedBtnBrowseOutput = findViewById(R.id.buttonBrowseOutput)
        cachedLayoutCompression = findViewById(R.id.layoutCompression)
        cachedLayoutCompressionLevel = findViewById(R.id.layoutCompressionLevel)
        cachedEditDevice = findViewById(R.id.editTextDevice)
        cachedBtnAutoDetect = findViewById(R.id.buttonAutoDetect)
        cachedEditFilename = findViewById(R.id.editTextCustomFilename)
        cachedProgressContainer = findViewById(R.id.progressBarContainer)
        // Also cache log views (previously done in a separate cacheLogViews())
        cachedLogView = findViewById(R.id.textViewLog)
        cachedScrollView = findViewById(R.id.scrollViewLog)
        // T17 — floating Build FAB (bottom-end, across the log pill);
        // T19 — permanently visible, availability gated by isEnabled.
        cachedFabBuild = findViewById(R.id.fabBuild)
        cachedFabBuild?.setOnClickListener { onBuildClicked() }
    }

    private fun handleImageFilesSelected(uris: List<Uri>) {
        // Cancel any previous in-flight image-loading coroutine.
        // This prevents the "loading chaos" bug where:
        //   1. User picks vendor.img → copy starts (coroutine A)
        //   2. User clicks Remove All → imageFiles.clear(), but A still running
        //   3. User picks vendor.img again → copy starts (coroutine B)
        //   4. Both write to the same destFile → corruption + duplicate log entries
        // By cancelling the previous job, only the latest picker selection runs.
        imageLoadingJob?.cancel()

        imageLoadingJob = lifecycleScope.launch {
            // Show "loading" state immediately so the user knows the picker
            // action was registered. Without this, the user sees no feedback
            // until each partition finishes copying — for large partitions
            // (e.g. 2GB system.img), this delay can be 5-15+ seconds.
            val totalToProcess = uris.size
            var processedCount = 0
            if (totalToProcess > 0) {
                showLog("Loading $totalToProcess partition image(s)…", LogLevel.INFO)

                // Add IMMEDIATE placeholder rows for all .img files in the
                // selection, BEFORE copying starts. This gives instant visual
                // feedback — the user sees "Loading <name>…" rows in the
                // partition list the moment they close the picker, rather
                // than waiting for the first copy to complete.
                //
                // Each placeholder uses a sentinel path ("loading:<name>")
                // that updateImageListUI() recognizes and renders with a
                // "Loading…" label instead of the file size. Once the copy
                // finishes, we replace the sentinel with the real path.
                val placeholders = mutableListOf<Pair<String, String>>()
                for (uri in uris) {
                    val fileName = getFileName(uri) ?: continue
                    if (!fileName.lowercase().endsWith(".img")) continue
                    val partitionName = fileName.removeSuffix(".img").removeSuffix(".IMG")
                    if (imageFiles.any { it.first == partitionName }) continue
                    placeholders.add(partitionName to "loading:$partitionName")
                }
                if (placeholders.isNotEmpty()) {
                    imageFiles.addAll(placeholders)
                    runOnUiThread {
                        updateImageListUI()
                        updateOutputPreview()
                    }
                }
            }

            for (uri in uris) {
                // Check for cancellation before processing each URI.
                if (!isActive) {
                    showLog("Loading cancelled.", LogLevel.WARN)
                    return@launch
                }

                val fileName = getFileName(uri) ?: continue

                // Only accept .img files — reject all others
                if (!fileName.lowercase().endsWith(".img")) {
                    showLog("Skipped: $fileName — only .img files are supported", LogLevel.WARN)
                    continue
                }

                // Partition name = filename without .img extension
                val partitionName = fileName.removeSuffix(".img")
                    .removeSuffix(".IMG")

                // ── Whitelist check on input partitions (DEMOTED to warning) ──
                // If deviceSupportedPartitions is populated (scan succeeded) and
                // the partition name is NOT in the list, WARN but still load the
                // file. This was a hard refusal until Task 12 — demoted because
                // the scan list is a static known-list filtered by getprop:
                // OEM-specific partitions (e.g. some dynamic my_*/odm variants)
                // are false negatives that must not block valid files from
                // loading. Safety is preserved by two later gates:
                //   1. The flasher script validates targets at recovery time
                //      (resolve_target + validate_target on /dev/block/by-name).
                //   2. The user sees the warning in the log before flashing.
                //
                // If deviceSupportedPartitions is empty (scan failed or not yet
                // completed), skip the check entirely (permissive mode).
                if (deviceSupportedPartitions.isNotEmpty() &&
                    partitionName !in deviceSupportedPartitions) {
                    showLog("[!] '$partitionName' is not in this device's scanned partition list — " +
                            "double-check the target partition name before flashing.", LogLevel.WARN)
                    showLog("  Scanned partitions: " + deviceSupportedPartitions.joinToString(", "),
                            LogLevel.WARN)
                    // Fall through — the file loads normally (warn, don't refuse).
                }

                // Skip if already added (real file, not placeholder).
                val placeholderStillPresent = imageFiles.any {
                    it.first == partitionName && it.second.startsWith("loading:")
                }
                if (imageFiles.any { it.first == partitionName && !it.second.startsWith("loading:") }) {
                    showLog("$partitionName already added, skipping.", LogLevel.WARN)
                    imageFiles.removeAll { it.first == partitionName && it.second.startsWith("loading:") }
                    runOnUiThread { updateImageListUI() }
                    continue
                }
                if (!placeholderStillPresent) {
                    showLog("Skipped $partitionName — removed before processing.", LogLevel.WARN)
                    continue
                }

                // ── Try to resolve the SAF URI to a real file path (NO COPY) ──
                // This is the fast path: if the file is on accessible storage
                // (internal shared storage, external SD card), we can read it
                // directly from its original location. No copy = no storage
                // doubling, no loading delay.
                //
                // Falls back to copying via ContentResolver.openInputStream()
                // only if the URI is a virtual document (cloud provider, etc.)
                // that doesn't have a real filesystem path.
                val resolvedPath = resolveUriToFilePath(uri)

                if (resolvedPath != null) {
                    // ── Fast path: use the file in-place (NO COPY) ──
                    val file = java.io.File(resolvedPath)
                    val sizeStr = formatFileSize(file.length())
                    showLog("Linked $partitionName ($sizeStr) — in-place, no copy", LogLevel.SUCCESS)

                    // Replace placeholder with real path
                    val placeholderIdx = imageFiles.indexOfFirst {
                        it.first == partitionName && it.second.startsWith("loading:")
                    }
                    if (placeholderIdx >= 0) {
                        imageFiles[placeholderIdx] = partitionName to resolvedPath
                    } else {
                        // Placeholder was removed during resolution — don't re-add
                        showLog("$partitionName resolved but was removed — skipping.", LogLevel.WARN)
                        continue
                    }
                    processedCount++

                    runOnUiThread {
                        updateImageListUI()
                        updateOutputPreview()
                    }
                } else {
                    // ── Slow path: copy via ContentResolver (cloud/virtual docs) ──
                    showLog("Loading $partitionName … (copying — source not directly accessible)", LogLevel.INFO)

                    val destFile = File(inputDir, fileName)
                    val tempFile = File(inputDir, "$fileName.part")
                    tempFile.delete()

                    val copyStartTime = System.currentTimeMillis()
                    try {
                        copyUriToFile(uri, tempFile)
                    } catch (e: kotlinx.coroutines.CancellationException) {
                        tempFile.delete()
                        showLog("Loading cancelled.", LogLevel.WARN)
                        throw e
                    } catch (e: Exception) {
                        // AUDIT-F4: any non-cancellation failure here
                        // (provider died, stream broken, disk full,
                        // SecurityException on the stream…) previously
                        // propagated out of the lifecycleScope coroutine
                        // and CRASHED the whole app. Handle it like the
                        // rename-failure path: clean up, drop the
                        // placeholder, log, and keep loading the rest.
                        tempFile.delete()
                        destFile.delete()
                        showLog("Failed to load $partitionName: ${e.message ?: e.javaClass.simpleName}", LogLevel.ERROR)
                        imageFiles.removeAll { it.first == partitionName && it.second.startsWith("loading:") }
                        runOnUiThread { updateImageListUI() }
                        continue
                    }
                    val copyDurationMs = System.currentTimeMillis() - copyStartTime

                    destFile.delete()
                    val renamed = tempFile.renameTo(destFile)
                    if (!renamed) {
                        showLog("Failed to finalize $partitionName (rename failed)", LogLevel.ERROR)
                        tempFile.delete()
                        imageFiles.removeAll { it.first == partitionName && it.second.startsWith("loading:") }
                        runOnUiThread { updateImageListUI() }
                        continue
                    }

                    val sizeAfter = destFile.length()
                    val sizeStr = if (sizeAfter > 0) formatFileSize(sizeAfter) else "size unknown"
                    val speedStr = if (copyDurationMs > 0 && sizeAfter > 0) {
                        val mbPerSec = (sizeAfter / 1024.0 / 1024.0) / (copyDurationMs / 1000.0)
                        String.format("%.1f MB/s", mbPerSec)
                    } else null

                    val placeholderIdx = imageFiles.indexOfFirst {
                        it.first == partitionName && it.second.startsWith("loading:")
                    }
                    if (placeholderIdx >= 0) {
                        imageFiles[placeholderIdx] = partitionName to destFile.absolutePath
                    } else {
                        showLog("$partitionName copy completed but was removed — cleaning up.", LogLevel.WARN)
                        destFile.delete()
                        continue
                    }
                    processedCount++

                    val loadedMsg = buildString {
                        append("Loaded $partitionName ($sizeStr)")
                        if (speedStr != null) append(" — $speedStr")
                        if (totalToProcess > 1) append("  [$processedCount/$totalToProcess]")
                    }
                    showLog(loadedMsg, LogLevel.SUCCESS)

                    runOnUiThread {
                        updateImageListUI()
                        updateOutputPreview()
                    }
                }
            }

            runOnUiThread {
                updateImageListUI()
                updateOutputPreview()
            }
        }
    }

    private fun handleIncomingIntent(intent: Intent) {
        // Accept .img files shared/opened from another app
        when (intent.action) {
            Intent.ACTION_VIEW -> {
                intent.data?.let { uri ->
                    handleImageFilesSelected(listOf(uri))
                }
            }
            Intent.ACTION_SEND -> {
                (intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM))?.let { uri ->
                    handleImageFilesSelected(listOf(uri))
                }
            }
        }
    }

    private suspend fun copyUriToFile(uri: Uri, destFile: File) {
        withContext(Dispatchers.IO) {
            contentResolver.openInputStream(uri)?.use { input ->
                FileOutputStream(destFile).use { output ->
                    input.copyTo(output)
                }
            }
        }
    }

    private fun getFileName(uri: Uri): String? {
        var fileName: String? = null
        contentResolver.query(uri, null, null, null, null)?.use { cursor ->
            val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
            if (cursor.moveToFirst() && nameIndex >= 0) {
                fileName = cursor.getString(nameIndex)
            }
        }
        return fileName ?: uri.lastPathSegment
    }

    // ═══════════════════════════════════════════════════════════════
    //  Payload.bin inspect + extract (prototype)
    //  Reads an AOSP OTA payload.bin and extracts partition images.
    // ═══════════════════════════════════════════════════════════════

    /** Open the SAF picker for a payload.bin file. */
    private fun launchPayloadPicker() {
        if (isExecuting) {
            Toast.makeText(this, R.string.payload_busy, Toast.LENGTH_SHORT).show()
            return
        }
        try {
            payloadFileChooser.launch(arrayOf("application/octet-stream", "*/*"))
        } catch (e: Exception) {
            showLog("[!] Cannot open file picker: ${e.message}", LogLevel.ERROR)
        }
    }

    /**
     * Handle a user-picked payload.bin: resolve its path (in-place fast
     * path, cache-copy slow path — same strategy as image files), inspect
     * it via the native backend, print the partition table, then offer
     * extraction of all partitions.
     */
    private fun handlePayloadSelected(uri: Uri) {
        if (isExecuting) return
        val fileName = getFileName(uri) ?: "payload.bin"
        showLog("[*] Payload selected: $fileName", LogLevel.INFO)

        buildScope.launch {
            // ── Fast path: real filesystem path (NO COPY) ──
            var path = resolveUriToFilePath(uri)

            // ── Slow path: virtual/cloud document — copy to cache first ──
            if (path == null) {
                showLog("[*] Source not directly accessible — copying to cache …", LogLevel.INFO)
                val dest = File(inputDir, fileName)
                val temp = File(inputDir, "$fileName.part")
                temp.delete()
                try {
                    copyUriToFile(uri, temp)
                    if (!temp.renameTo(dest)) {
                        temp.delete()
                        dest.delete()
                        showLog("[!] Cache copy failed — cannot inspect this document", LogLevel.ERROR)
                        return@launch
                    }
                    path = dest.absolutePath
                } catch (e: Exception) {
                    temp.delete()
                    showLog("[!] Cache copy failed: ${e.message}", LogLevel.ERROR)
                    return@launch
                }
            }

            // Kotlin flow analysis won't smart-cast the `var` (assigned
            // inside a nested try) — elvis-bail keeps this a non-null
            // String for both uses below without a `!!`.
            val payloadPath = path ?: return@launch
            val result = OTABridge.inspectPayload(payloadPath) { line ->
                showLog(line, LogLevel.PLAIN)
            }
            if (result == null || !result.success) return@launch

            // ── Partition table ──
            showLog("    ── ${result.partitions.size} partitions ──", LogLevel.PLAIN)
            result.partitions.forEach { p ->
                val sizeStr = if (p.sizeBytes > 0) formatFileSize(p.sizeBytes) else "size ?"
                showLog("    • ${p.name}  ($sizeStr, ${p.opCount} ops)", LogLevel.PLAIN)
            }

            runOnUiThread { showExtractDialog(payloadPath, result) }
        }
    }

    /** Base directory for extracted partition images (reuses output dir). */
    private fun payloadExtractDir(): String {
        val base = outputDirPath ?: outputDir.absolutePath
        return "$base/payload_extracted"
    }

    /** Ask the user whether to extract every partition from the payload. */
    private fun showExtractDialog(payloadPath: String, result: NativeBridge.PayloadInspectResult) {
        val totalBytes = result.partitions.sumOf { it.sizeBytes }
        val totalStr = if (totalBytes > 0) formatFileSize(totalBytes) else "?"
        MaterialAlertDialogBuilder(this)
            .setTitle(getString(R.string.payload_extract_title))
            .setMessage(
                getString(
                    R.string.payload_extract_message,
                    result.partitions.size,
                    totalStr,
                    payloadExtractDir()
                )
            )
            .setPositiveButton(getString(R.string.payload_extract_all)) { _, _ ->
                extractAllPayloadPartitions(payloadPath, result)
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    /**
     * Extract every partition sequentially on buildScope. Partition names
     * from the manifest are sanitized before being used as filenames —
     * a malicious payload must not be able to write outside the extract
     * directory via crafted partition_name values (path traversal).
     *
     * Long-run protection (Task 12): multi-GB system.img decompression can
     * take minutes — the OTAService foreground notification (with its
     * PARTIAL_WAKE_LOCK) plus a belt-and-suspenders companion WakeLock keep
     * the CPU alive under Doze, exactly like the DD build path. Progress is
     * reported through the per-partition `.progress` sidecar (Rust writes,
     * OTABridge polls) and surfaces in the split progress bars, the log,
     * and the foreground notification.
     */
    private fun extractAllPayloadPartitions(
        payloadPath: String,
        result: NativeBridge.PayloadInspectResult
    ) {
        // T23: mutual exclusion across all three long-running operations
        // (DD build / payload build / payload extract) — guarded here, at
        // the operation entry, so the inspect→dialog→extract path is covered
        // even though the toolbar menu itself is never disabled.
        if (isBuilding) {
            showLog("Operation already in progress. Please wait.", LogLevel.WARN)
            return
        }
        setUIExecuting(true)
        isExecuting = true
        // T23: extract used to set ONLY the instance flag — a mid-extract
        // recreate left the companion isBuilding=false, so onResume took the
        // "build finished" branch: live notification canceled, controls
        // re-enabled mid-run, FAB clickable → a concurrent second build could
        // corrupt shared companion state. Mirror startBuild's full pattern.
        isBuilding = true
        lastProgressTime = System.currentTimeMillis()  // heartbeat for dead-process detection
        resumedWhileBuildingLogged = false
        appContext = applicationContext
        val names = result.partitions.map { it.name }
        partitionNames = names
        lastProgressMessage = ""
        lastNotifPercent = -1
        showProgressNotification("Extracting payload…", 0)

        // Foreground service — process priority + service-side WakeLock.
        OTAService.start(applicationContext, "Extracting payload…")

        buildScope.launch {
            try {
                // Belt-and-suspenders WakeLock (same pattern as startBuild):
                // the service holds one, this one survives even if the
                // service is killed and restarted by the OS.
                val act = activityRef?.get()
                if (act != null) {
                    val pm = act.applicationContext.getSystemService(Context.POWER_SERVICE) as PowerManager
                    wakeLock = pm.newWakeLock(
                        PowerManager.PARTIAL_WAKE_LOCK,
                        "OTAku::ExtractWakeLock"
                    ).apply {
                        setReferenceCounted(false)
                        acquire(3 * 60 * 60 * 1000L) // 3 hours — enough for any extraction batch
                    }
                }

                // Split progress bars — one bar per manifest partition.
                runOnUiThread { setupSplitProgressBar(names) }

                val dir = File(payloadExtractDir())
                dir.mkdirs()
                val startMs = System.currentTimeMillis()
                var ok = 0
                var failed = 0
                var totalExtracted = 0L
                val total = result.partitions.size

                result.partitions.forEachIndexed { idx, p ->
                    // Sanitize: manifest-controlled name → safe filename component
                    val safeName = p.name.replace(Regex("[^a-zA-Z0-9_.\\-]"), "_")
                    if (safeName != p.name) {
                        showLog("[!] Partition name '${p.name}' sanitized → '$safeName'", LogLevel.WARN)
                    }
                    val outFile = File(dir, "$safeName.img")
                    val r = OTABridge.extractPayloadPartition(
                        payloadPath, safeName, outFile.absolutePath,
                        current = idx + 1,
                        total = total,
                        onProgress = { progress ->
                            // Per-partition bar + mark previous bars complete
                            if (partitionCount > 0) {
                                val pIdx = progress.current - 1
                                if (pIdx in 0 until partitionCount) {
                                    updatePartitionProgress(pIdx, progress.partitionPercent)
                                    synchronized(progressLock) {
                                        for (j in 0 until pIdx) {
                                            if (partitionProgress[j] < 100) partitionProgress[j] = 100
                                        }
                                    }
                                }
                            }

                            // Foreground notification — "Extracting system (2/7) — 43%"
                            val notifMsg = if (progress.partitionPercent in 1..99) {
                                "${progress.message} (${progress.current}/${progress.total}) — ${progress.partitionPercent}%"
                            } else {
                                "${progress.message} (${progress.current}/${progress.total})"
                            }
                            if (notifMsg != lastProgressMessage || progress.percent != lastNotifPercent) {
                                lastProgressMessage = notifMsg
                                lastNotifPercent = progress.percent
                                showProgressNotification(notifMsg, progress.percent)
                            }

                            // Log line on percent change (persist always — K3 pattern)
                            if (progress.partitionPercent != lastProgressPercent) {
                                lastProgressPercent = progress.partitionPercent
                                val logMsg = if (progress.partitionPercent in 1..99) {
                                    "${progress.message} ${progress.partitionPercent}%"
                                } else {
                                    progress.message
                                }
                                val line = if (logMsg.endsWith("\n")) logMsg else "$logMsg\n"
                                appendToSavedLog(line)
                                val current = activityRef?.get()
                                if (current != null && !current.isFinishing && !current.isDestroyed) {
                                    current.runOnUiThread { current.appendLogLineUI(line, LogLevel.PLAIN) }
                                }
                            }

                            // Live bar re-render
                            val current = activityRef?.get()
                            if (current != null && !current.isFinishing && !current.isDestroyed) {
                                current.runOnUiThread {
                                    val container = current.findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
                                    val barRow = container?.findViewWithTag("bar_row") as? android.widget.LinearLayout
                                    if (barRow != null && barRow.childCount == partitionCount) {
                                        val snapshot = snapshotPartitionProgress()
                                        for (i in 0 until partitionCount) {
                                            val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                                            bar?.let {
                                                it.isIndeterminate = false
                                                it.progress = snapshot[i]
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    ) { line -> showLog(line, LogLevel.PLAIN) }
                    if (r.success) {
                        ok++
                        totalExtracted += r.fileSize
                        if (partitionCount > 0 && idx in 0 until partitionCount) {
                            updatePartitionProgress(idx, 100)
                        }
                    } else {
                        failed++
                    }
                }

                val durMs = System.currentTimeMillis() - startMs
                markAllProgressComplete()
                if (failed == 0) {
                    showLog(
                        "═══ Payload extraction done — $ok partitions, " +
                            "${formatFileSize(totalExtracted)}, ${durMs} ms ═══",
                        LogLevel.SUCCESS
                    )
                } else {
                    showLog(
                        "═══ Payload extraction finished with errors — $ok ok / $failed failed " +
                            "(${formatFileSize(totalExtracted)}, ${durMs} ms) ═══",
                        LogLevel.WARN
                    )
                }
            } finally {
                // Release WakeLock + stop the foreground service on every exit
                // path (mirrors the DD build's finally discipline).
                try { wakeLock?.release() } catch (_: Exception) {}
                wakeLock = null
                try { OTAService.stop(appContext ?: applicationContext) } catch (_: Exception) {}
                // T23: isBuilding is companion state — clear it HERE so a
                // mid-extract recreate reconnects (or finishes cleanly) after
                // the operation ends, whatever instance is alive.
                isBuilding = false
                lastProgressMessage = ""
                lastNotifPercent = -1
                lastProgressPercent = -1
                val current = activityRef?.get()
                if (current != null && !current.isFinishing && !current.isDestroyed) {
                    // T23: reset the CURRENT instance's flag (the bare
                    // assignment used to write the launching instance's field,
                    // leaving a post-recreate instance stuck isExecuting=true).
                    current.isExecuting = false
                    current.setUIExecuting(false)
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Payload.bin build + verify (prototype)
    //  Packs the loaded partition images into an AOSP payload.bin and
    //  self-verifies the result (header + manifest re-read).
    // ═══════════════════════════════════════════════════════════════

    /**
     * Menu entry: validate inputs, then let the user pick the payload
     * build parameters — compression algorithm (pre-selected to whatever
     * the DD build selector currently uses), manifest block size, and
     * payload minor version. Block size / minor version were previously
     * hardcoded to the Rust defaults (4096 / 0); they are exposed here so
     * advanced users can match a target updater's expectations.
     */
    private fun onBuildPayloadClicked() {
        // T23: guard on the COMPANION flag — isExecuting is instance state
        // reset by recreate(), so a mid-build theme switch would briefly
        // leave this entry unguarded.
        if (isBuilding) {
            Toast.makeText(this, R.string.payload_busy, Toast.LENGTH_SHORT).show()
            return
        }
        if (!NativeBridge.isLoaded) {
            showLog("Native backend not available: ${NativeBridge.loadError}", LogLevel.ERROR)
            return
        }
        if (imageFiles.isEmpty()) {
            showLog(getString(R.string.payload_no_images), LogLevel.ERROR)
            return
        }
        // Don't allow a payload build while a copy is still in flight —
        // same placeholder guard as the DD build button.
        if (imageFiles.any { it.second.startsWith("loading:") }) {
            showLog("Wait for partition images to finish loading.", LogLevel.WARN)
            return
        }

        val algorithms = OTABridge.COMPRESSION_ALGORITHMS.toTypedArray()
        val checkedIdx = algorithms.indexOf(selectedCompression).coerceAtLeast(0)

        // Fixed-value dropdowns — every AutoCompleteTextView is
        // inputType=none, so the only reachable values are the ones below;
        // no free-text validation needed. Index 0 of each array is the
        // previous hardcoded behavior (4096 / 0), labeled "(default)".
        // Rust still guards the degenerate cases (block_size <= 0 → 4096,
        // minor_version < 0 → 0) at the JNI boundary.
        val blockSizeValues = intArrayOf(4096, 2048, 1024, 512, 8192, 16384, 65536)
        val minorVersionValues = intArrayOf(0, 1, 2, 3, 4)
        val blockSizeLabels = blockSizeValues.indices.map { i ->
            if (i == 0) "${blockSizeValues[i]} (default)" else "${blockSizeValues[i]}"
        }
        val minorVersionLabels = minorVersionValues.indices.map { i ->
            if (i == 0) "${minorVersionValues[i]} (default)" else "${minorVersionValues[i]}"
        }

        val dialogView = layoutInflater.inflate(R.layout.dialog_build_payload, null)
        val dropdownCompression = dialogView.findViewById<android.widget.AutoCompleteTextView>(R.id.dropdownPayloadCompression)
        val dropdownBlockSize = dialogView.findViewById<android.widget.AutoCompleteTextView>(R.id.dropdownPayloadBlockSize)
        val dropdownMinorVersion = dialogView.findViewById<android.widget.AutoCompleteTextView>(R.id.dropdownPayloadMinorVersion)

        var chosenCompression = algorithms[checkedIdx]
        var chosenBlockSize = blockSizeValues[0]
        var chosenMinorVersion = minorVersionValues[0]

        dropdownCompression?.let { dd ->
            dd.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, algorithms))
            dd.setText(algorithms[checkedIdx], false)
            dd.setOnItemClickListener { _, _, position, _ -> chosenCompression = algorithms[position] }
        }
        dropdownBlockSize?.let { dd ->
            dd.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, blockSizeLabels))
            dd.setText(blockSizeLabels[0], false)
            dd.setOnItemClickListener { _, _, position, _ -> chosenBlockSize = blockSizeValues[position] }
        }
        dropdownMinorVersion?.let { dd ->
            dd.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, minorVersionLabels))
            dd.setText(minorVersionLabels[0], false)
            dd.setOnItemClickListener { _, _, position, _ -> chosenMinorVersion = minorVersionValues[position] }
        }

        MaterialAlertDialogBuilder(this)
            .setTitle(getString(R.string.payload_build_title))
            .setView(dialogView)
            .setPositiveButton(android.R.string.ok) { _, _ ->
                buildPayloadBin(imageFiles.toMap(), chosenCompression, chosenBlockSize, chosenMinorVersion)
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    /**
     * Build payload.bin from the loaded images on buildScope, then
     * self-verify the output. Long-run protection mirrors the DD build:
     * OTAService foreground + WakeLock. Progress mirrors the DD build and
     * the payload extract path: Rust writes the per-partition .progress
     * sidecar, OTABridge polls it, and it surfaces in the split progress
     * bars, the log, and the foreground notification.
     */
    private fun buildPayloadBin(
        images: Map<String, String>,
        compression: String,
        blockSize: Int,
        minorVersion: Int
    ) {
        // AUDIT-F5 pattern: capture on the UI thread BEFORE buildScope.
        val level = selectedCompressionLevel
        val effectiveLevel = if (level > 0) level
            else OTABridge.COMPRESS_LEVELS[compression]?.third ?: 0

        val outDir = outputDirPath ?: outputDir.absolutePath
        File(outDir).mkdirs()
        // Auto-unique output: payload.bin, payload-1.bin, … (never silently
        // overwrite a previous build — or the payload the user just inspected).
        var out = File(outDir, "payload.bin")
        var n = 1
        while (out.exists()) {
            out = File(outDir, "payload-$n.bin")
            n++
        }
        val outPath = out.absolutePath

        // Rust processes partitions in alphabetical order (deterministic
        // manifest) — the split bars must match that order.
        val sortedNames = images.keys.sorted()
        partitionNames = sortedNames

        // T23: the dialog positive-click can race a build started while the
        // dialog was open — guard at the operation entry, not just the menu.
        if (isBuilding) {
            showLog("Operation already in progress. Please wait.", LogLevel.WARN)
            return
        }
        setUIExecuting(true)
        isExecuting = true
        // T23: payload build set only the instance flag (same desync as the
        // extract path — see the extract-entry comment). Mirror startBuild.
        isBuilding = true
        lastProgressTime = System.currentTimeMillis()  // heartbeat for dead-process detection
        resumedWhileBuildingLogged = false
        appContext = applicationContext
        lastProgressMessage = ""
        lastNotifPercent = -1
        showProgressNotification("Building payload…", 0)
        OTAService.start(applicationContext, "Building payload…")

        buildScope.launch {
            try {
                // Belt-and-suspenders WakeLock — same discipline as the DD
                // build and the payload extract paths.
                val act = activityRef?.get()
                if (act != null) {
                    val pm = act.applicationContext.getSystemService(Context.POWER_SERVICE) as PowerManager
                    wakeLock = pm.newWakeLock(
                        PowerManager.PARTIAL_WAKE_LOCK,
                        "OTAku::PayloadWakeLock"
                    ).apply {
                        setReferenceCounted(false)
                        acquire(3 * 60 * 60 * 1000L) // 3 hours — enough for any compression job
                    }
                }

                // Split progress bars — one bar per partition, in Rust's
                // alphabetical processing order (same setup as the extract
                // batch loop).
                runOnUiThread { setupSplitProgressBar(sortedNames) }

                showLog(
                    "[*] Building payload.bin — ${images.size} partitions, " +
                        "compression=$compression, level=$effectiveLevel, " +
                        "block_size=$blockSize, minor_version=$minorVersion, output=$outPath",
                    LogLevel.INFO
                )

                val result = OTABridge.writePayload(
                    images = images,
                    compression = compression,
                    level = level,
                    outputPath = outPath,
                    blockSize = blockSize,
                    minorVersion = minorVersion,
                    onProgress = { progress ->
                        // Per-partition bar + mark previous bars complete
                        // (same consumer as the extract batch loop).
                        if (partitionCount > 0) {
                            val pIdx = progress.current - 1
                            if (pIdx in 0 until partitionCount) {
                                updatePartitionProgress(pIdx, progress.partitionPercent)
                                synchronized(progressLock) {
                                    for (j in 0 until pIdx) {
                                        if (partitionProgress[j] < 100) partitionProgress[j] = 100
                                    }
                                }
                            }
                        }

                        // Foreground notification — "Compressing system (2/7) — 43%"
                        val notifMsg = if (progress.partitionPercent in 1..99) {
                            "${progress.message} (${progress.current}/${progress.total}) — ${progress.partitionPercent}%"
                        } else {
                            "${progress.message} (${progress.current}/${progress.total})"
                        }
                        if (notifMsg != lastProgressMessage || progress.percent != lastNotifPercent) {
                            lastProgressMessage = notifMsg
                            lastNotifPercent = progress.percent
                            showProgressNotification(notifMsg, progress.percent)
                        }

                        // Log line on percent change (persist always — K3 pattern)
                        if (progress.partitionPercent != lastProgressPercent) {
                            lastProgressPercent = progress.partitionPercent
                            val logMsg = if (progress.partitionPercent in 1..99) {
                                "${progress.message} ${progress.partitionPercent}%"
                            } else {
                                progress.message
                            }
                            val line = if (logMsg.endsWith("\n")) logMsg else "$logMsg\n"
                            appendToSavedLog(line)
                            val current = activityRef?.get()
                            if (current != null && !current.isFinishing && !current.isDestroyed) {
                                current.runOnUiThread { current.appendLogLineUI(line, LogLevel.PLAIN) }
                            }
                        }

                        // Live bar re-render
                        val current = activityRef?.get()
                        if (current != null && !current.isFinishing && !current.isDestroyed) {
                            current.runOnUiThread {
                                val container = current.findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
                                val barRow = container?.findViewWithTag("bar_row") as? android.widget.LinearLayout
                                if (barRow != null && barRow.childCount == partitionCount) {
                                    val snapshot = snapshotPartitionProgress()
                                    for (i in 0 until partitionCount) {
                                        val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                                        bar?.let {
                                            it.isIndeterminate = false
                                            it.progress = snapshot[i]
                                        }
                                    }
                                }
                            }
                        }
                    },
                    onOutputLine = { line -> showLog(line, LogLevel.PLAIN) }
                )

                // All partitions processed — saturate the bars (success or
                // error; the log lines that follow tell the story).
                markAllProgressComplete()
                runOnUiThread { renderPartitionProgress(true) }

                if (result.success) {
                    showLog(
                        "[+] payload.bin written: ${result.outputPath ?: outPath} " +
                            "(${formatFileSize(result.fileSize)}, ${result.durationMs} ms)",
                        LogLevel.SUCCESS
                    )
                    result.partitions.forEach { s ->
                        showLog(
                            "    • ${s.name}: ${formatFileSize(s.originalSize)} → " +
                                "${formatFileSize(s.compressedSize)} " +
                                "(${(s.ratio * 100).toInt()}% ${s.algorithm})",
                            LogLevel.PLAIN
                        )
                    }

                    // Self-verify — cheap (header + manifest re-read only)
                    // and catches truncation/corruption from storage issues.
                    val verify = OTABridge.verifyPayload(outPath) { line ->
                        showLog(line, LogLevel.PLAIN)
                    }
                    if (verify.success) {
                        showLog("[+] Self-verification passed", LogLevel.SUCCESS)
                    } else {
                        showLog("[!] Self-verification FAILED: ${verify.error}", LogLevel.ERROR)
                    }
                } else {
                    showLog("[!] payload.bin build failed: ${result.error}", LogLevel.ERROR)
                }
            } finally {
                try { wakeLock?.release() } catch (_: Exception) {}
                wakeLock = null
                try { OTAService.stop(appContext ?: applicationContext) } catch (_: Exception) {}
                // T23: companion flag cleared at operation end (same as the
                // extract path) — keeps onResume reconnect honest.
                isBuilding = false
                lastProgressMessage = ""
                lastNotifPercent = -1
                lastProgressPercent = -1
                val current = activityRef?.get()
                if (current != null && !current.isFinishing && !current.isDestroyed) {
                    // T23: reset the CURRENT instance's flag (was a bare
                    // assignment to the launching instance's field).
                    current.isExecuting = false
                    current.setUIExecuting(false)
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Execution — Build to OTA ZIP
    // ═══════════════════════════════════════════════════════════════

    // BUG-H07 fix: Use OnBackPressedDispatcher instead of deprecated onBackPressed().
    // onBackPressed() is deprecated since API 33 (Android 13) — on API 33+ devices
    // it may not be called, so the "Build in Progress" dialog protection would be
    // silently bypassed, allowing the user to back out during a build without warning.
    // OnBackPressedDispatcher works on all API levels and supports predictive back.
    private var backPressedCallback: androidx.activity.OnBackPressedCallback? = null

    private fun setupBackPressedHandler() {
        backPressedCallback?.remove()
        val callback = object : androidx.activity.OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                if (isBuilding) {
                    MaterialAlertDialogBuilder(this@MainActivity)
                        .setTitle("Build in Progress")
                        .setMessage("The build operation is running in the background " +
                            "and will continue even if you leave the app.")
                        .setPositiveButton("Stay", null)
                        .show()
                } else {
                    // Not building — allow back navigation
                    isEnabled = false
                    onBackPressedDispatcher.onBackPressed()
                }
            }
        }
        onBackPressedDispatcher.addCallback(this, callback)
        backPressedCallback = callback
    }

    private fun onBuildClicked() {
        if (isBuilding) {
            showLog("Operation already in progress. Please wait.", LogLevel.WARN)
            return
        }

        if (!NativeBridge.isLoaded) {
            showLog("Native backend not available: ${NativeBridge.loadError}", LogLevel.ERROR)
            showLog("Restart the app to retry initialization.", LogLevel.WARN)
            return
        }

        // Require device codename — it is mandatory for the flasher script's
        // device verification step in custom recovery (TWRP/OrangeFox).
        val editDevice = findViewById<com.google.android.material.textfield.TextInputEditText>(R.id.editTextDevice)
        val device = editDevice?.text?.toString()?.trim() ?: ""
        if (device.isEmpty()) {
            showLog("Device codename is required for recovery verification.", LogLevel.ERROR)
            showLog("Enter your device codename or tap Auto-Detect, then retry.", LogLevel.WARN)
            // Focus the device field and shake to draw attention
            editDevice?.requestFocus()
            return
        }

        // Pre-build dependency check: validate selected compression is available.
        // Uses cached result from initialization to avoid blocking the UI.
        val depCheck = cachedDepCheck
        if (depCheck != null && selectedCompression !in depCheck.available) {
            showLog("Cannot start build: compression '$selectedCompression' is not available.", LogLevel.ERROR)
            showLog("  Available: ${depCheck.available.joinToString(", ")}", LogLevel.INFO)
            return
        }

        startBuild(device)
    }

    /**
     * Prompt the user to grant OEM unrestricted battery optimization.
     * This prevents Android from killing the app during long builds.
     * Shown once at startup after storage permissions are resolved.
     */
    private fun promptBatteryOptimization() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        if (pm.isIgnoringBatteryOptimizations(packageName)) {
            // Already whitelisted — skip the prompt entirely
            prefs.edit { putBoolean("pref_battery_prompted", true) }
            return
        }

        MaterialAlertDialogBuilder(this)
            .setTitle("Prevent App from Being Killed")
            .setMessage(
                "Android may kill OTAku during long builds to save battery, " +
                "causing the build to fail silently.\n\n" +
                "Granting \"Unrestricted\" battery usage prevents this and " +
                "ensures your flashable ZIP builds reliably.\n\n" +
                "On the next screen, select \"Don't optimize\" or \"Unrestricted\" " +
                "for OTAku."
            )
            .setPositiveButton("Grant Unrestricted") { _, _ ->
                prefs.edit { putBoolean("pref_battery_prompted", true) }
                try {
                    val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                        data = Uri.parse("package:$packageName")
                    }
                    startActivity(intent)
                } catch (_: Exception) {
                    // Fallback: open app-specific battery settings
                    try {
                        val intent = Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                        startActivity(intent)
                    } catch (_: Exception) {
                        showLog("Could not open battery settings automatically.", LogLevel.WARN)
                        showLog("Go to: Settings > Apps > OTAku > Battery > Unrestricted", LogLevel.WARN)
                    }
                }
            }
            .setNegativeButton("Skip for Now") { _, _ ->
                prefs.edit { putBoolean("pref_battery_prompted", true) }
                showLog("Battery optimization not granted — build may be killed on long runs.", LogLevel.WARN)
            }
            .setCancelable(false)
            .show()
    }

    // Flag: battery optimization prompt should be shown after storage permissions resolve
    private var pendingBatteryPrompt = false

    /** Core build logic — separated from onBuildClicked for battery prompt flow. */
    private fun startBuild(device: String) {
        val images = imageFiles.toMap()
        prefs.edit { putString("device", device) }
        val deviceValue = device.ifEmpty { "generic" }

        val outDir = outputDirPath ?: outputDir.absolutePath
        File(outDir).mkdirs()

        val customName = prefs.getString("pref_custom_filename", "")?.trim()
        val outputFileName = if (!customName.isNullOrEmpty()) {
            if (customName.lowercase().endsWith(".zip")) customName else "$customName.zip"
        } else {
            OTABridge.buildOutputFileName(deviceValue)
        }
        val outPath = File(outDir, outputFileName).absolutePath

        // AUDIT-F5: capture ROM name + Maker HERE, on the UI thread
        // (startBuild is called from onBuildClicked), BEFORE entering
        // buildScope (Dispatchers.Default). The old code read these view
        // properties inside the buildScope coroutine — a background thread
        // — while its comment claimed a withContext(Dispatchers.Main)
        // wrapper that never existed. Reading TextView text off the UI
        // thread risks CalledFromWrongThreadException / stale values.
        val romName = findViewById<com.google.android.material.textfield.TextInputEditText>(R.id.editTextRomName)
            ?.text?.toString()?.trim() ?: ""
        val maker = findViewById<com.google.android.material.textfield.TextInputEditText>(R.id.editTextMaker)
            ?.text?.toString()?.trim() ?: ""

        // Store state in companion object (survives Activity recreation)
        lastOutputPath = outPath
        lastProgressMessage = ""
        lastProgressPercent = -1
        lastNotifPercent = -1
        lastProgressTime = System.currentTimeMillis()  // Start heartbeat
        resumedWhileBuildingLogged = false  // Reset: new build session
        isBuilding = true
        isExecuting = true
        appContext = applicationContext
        setUIExecuting(true)
        val sortedNames = images.keys.sorted()
        partitionNames = sortedNames
        setupSplitProgressBar(sortedNames)
        showProgressNotification("Preparing…", 0)

        // Start foreground service — gives the process "foreground priority"
        // which prevents Doze/App Standby from killing it during long builds.
        // WakeLock alone is insufficient; only a foreground service guarantees survival.
        OTAService.start(applicationContext)

        // Execute in application-scoped scope (survives Activity destruction)
        buildScope.launch {
            try {
                // Acquire WakeLock with application context as a secondary safeguard.
                // The foreground service also holds a WakeLock — this is belt-and-suspenders.
                val act = activityRef?.get()
                if (act != null) {
                    val pm = act.applicationContext.getSystemService(Context.POWER_SERVICE) as PowerManager
                    wakeLock = pm.newWakeLock(
                        PowerManager.PARTIAL_WAKE_LOCK,
                        "OTAku::BuildWakeLock"
                    ).apply {
                        setReferenceCounted(false)
                        acquire(3 * 60 * 60 * 1000L)  // 3 hours — enough for any compression job
                    }
                }

                // Start heartbeat coroutine — fallback that shows elapsed time
                // when no progress sidecar file is available yet (first few seconds)
                // Uses buildScope (not standalone CoroutineScope) to prevent orphan.
                val buildStartTime = System.currentTimeMillis()
                val heartbeatJob = buildScope.launch {
                    delay(15_000) // Wait 15s before activating heartbeat fallback
                    while (isActive) {
                        // Only show heartbeat if no real progress has arrived
                        if (lastProgressMessage.isEmpty()) {
                            val elapsed = (System.currentTimeMillis() - buildStartTime) / 1000
                            val minutes = elapsed / 60
                            val seconds = elapsed % 60
                            val elapsedStr = if (minutes > 0) "${minutes}m ${seconds}s" else "${seconds}s"
                            showProgressNotification("Compressing… ($elapsedStr elapsed)", 0)
                        }
                        delay(10_000) // 10 seconds between heartbeat updates
                    }
                }

                try {
                    // romName/maker were captured on the UI thread in
                    // startBuild() before this coroutine launched
                    // (AUDIT-F5) — no view access from this background
                    // dispatcher.
                    val result = OTABridge.dd(
                        images = images,
                        device = deviceValue,
                        compression = selectedCompression,
                        level = selectedCompressionLevel,
                        outputPath = outPath,
                        romName = romName,
                        maker = maker,
                        onProgress = { progress ->
                            // Cancel heartbeat — real progress is arriving from file polling
                            heartbeatJob.cancel()

                            // Update heartbeat timestamp (survives Activity recreation)
                            lastProgressTime = System.currentTimeMillis()

                            // Build notification message with per-partition info
                            // For compression: "Compressing boot (1/3) — 45%"
                            // For phases: "Writing ZIP file — 97%"
                            val notifMsg = if (progress.current > 0 && progress.total > 0 &&
                                progress.partitionPercent in 1..99) {
                                "${progress.message} (${progress.current}/${progress.total}) — ${progress.partitionPercent}%"
                            } else {
                                "${progress.message} — ${progress.percent}%"
                            }

                            // Map Rust's internal progress range (0-97%) to notification
                            // progress bar range (0-100%) so the user sees 0→100% completion:
                            //   Rust 0-94% (compression)  → notification 0-90%
                            //   Rust 95% (scripts)        → notification 92%
                            //   Rust 97% (writing ZIP)    → notification 95%
                            //   Build complete            → notification 100%
                            val notifPercent = when {
                                progress.percent >= 97 -> 95   // Writing ZIP
                                progress.percent >= 95 -> 92   // Building scripts
                                else -> (progress.percent * 90.0 / 94.0).toInt().coerceIn(0, 90)
                            }

                            // Always update notification when percent or message changes
                            if (notifMsg != lastProgressMessage || notifPercent != lastNotifPercent) {
                                lastProgressMessage = notifMsg
                                lastNotifPercent = notifPercent
                                showProgressNotification(notifMsg, notifPercent)
                            }

                            // Update split progress bars (per-partition).
                            // Use progress.current (1-based) for partition index — reliable
                            // unlike message parsing which broke when message contained "%".
                            // Use progress.partitionPercent for per-partition bar fill (0-100).
                            if (partitionCount > 0) {
                                val pIdx = progress.current - 1  // 0-based index
                                when {
                                    progress.message.contains("Building flasher") ||
                                    progress.message.contains("Writing ZIP") -> {
                                        // Post-partition steps: mark all bars complete
                                        markAllProgressComplete()
                                        currentPartitionIndex = partitionCount - 1
                                    }
                                    pIdx in 0 until partitionCount -> {
                                        // Use partitionPercent for per-partition bar fill
                                        updatePartitionProgress(pIdx, progress.partitionPercent)
                                        currentPartitionIndex = pIdx
                                        // Mark all previous partitions as complete
                                        // Mark all previous partitions as complete (thread-safe)
                                        synchronized(progressLock) {
                                            for (j in 0 until pIdx) {
                                                if (partitionProgress[j] < 100) partitionProgress[j] = 100
                                            }
                                        }
                                    }
                                }
                            }

                            // Fix K3: ALWAYS persist per-partition progress log line (regardless of Activity state).
                            // Previously this was inside the activityRef gate, so log lines were lost when
                            // the app was backgrounded during compression.
                            val percentChanged = progress.partitionPercent != lastProgressPercent
                            val pendingLogLine: String? = if (percentChanged) {
                                lastProgressPercent = progress.partitionPercent
                                val logMsg = if (progress.partitionPercent in 1..99) {
                                    "${progress.message} ${progress.partitionPercent}%"
                                } else {
                                    progress.message
                                }
                                val line = if (logMsg.endsWith("\n")) logMsg else "$logMsg\n"
                                appendToSavedLog(line)  // always persist
                                line
                            } else {
                                null
                            }

                            // Update UI progress bars and log (only if Activity is alive)
                            val current = activityRef?.get()
                            if (current != null && !current.isFinishing && !current.isDestroyed) {
                                current.runOnUiThread {
                                    // IMPL-012: Use cached bar_row reference instead of
                                    // findViewById + findViewWithTag on every callback.
                                    // Fallback to resolve + cache if reference is stale
                                    // (e.g., after Activity recreation).
                                    if (current.cachedBarRow == null || current.cachedBarRow?.parent == null) {
                                        val container = current.findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
                                        current.cachedBarRow = container?.findViewWithTag("bar_row")
                                    }
                                    val barRow = current.cachedBarRow
                                    if (barRow != null && barRow.childCount == partitionCount) {
                                        // AUDIT-F2: read through the locked snapshot
                                        // (IMPL-011) — raw indexed reads here raced with
                                        // onProgress writes from Dispatchers.Default.
                                        val snapshot = snapshotPartitionProgress()
                                        for (i in 0 until partitionCount) {
                                            val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                                            bar?.let {
                                                it.isIndeterminate = false
                                                it.progress = snapshot[i]
                                            }
                                        }
                                    }
                                }
                                // UI-only log append if percent changed (persist already done above)
                                // Use ?.let to get non-null smart cast inside the lambda
                                // (Kotlin doesn't smart-cast String? to String inside lambdas)
                                pendingLogLine?.let { line ->
                                    current.runOnUiThread {
                                        current.appendLogLineUI(line, LogLevel.PLAIN)
                                    }
                                }
                            }
                        },
                        onOutputLine = { line ->
                            // Fix K1: ALWAYS persist to companion buffer (survives Activity recreation).
                            // Previously this was inside the activityRef gate, so log lines were lost
                            // when the app was backgrounded during the build.
                            val logLine = if (line.endsWith("\n")) line else "$line\n"
                            appendToSavedLog(logLine)

                            // UI update only if Activity is alive (bypass showLog to avoid double-persist)
                            val current = activityRef?.get()
                            if (current != null && !current.isFinishing && !current.isDestroyed) {
                                current.runOnUiThread {
                                    current.appendLogLineUI(logLine, LogLevel.PLAIN)
                                }
                            }
                        }
                    )

                    heartbeatJob.cancel()

                    // Fix K2: Record build result — always runs (companion-level), handles notification,
                    // log persistence, partition progress 100%, and conditional UI reset.
                    // Previously handleBuildResult() was gated by activityRef, so when the app was
                    // backgrounded, no completion notification fired and UI stayed stuck.
                    recordBuildResult(result)
                } catch (e: Exception) {
                    heartbeatJob.cancel()
                    throw e
                }
            } catch (e: kotlinx.coroutines.CancellationException) {
                recordBuildResult(OTAResult.error("Build cancelled"))
                throw e  // Don't swallow coroutine cancellation
            } catch (e: Exception) {
                recordBuildResult(OTAResult.error("Build failed: ${e.message ?: "Unknown exception"}"))
            } finally {
                // Release WakeLock
                try { wakeLock?.release() } catch (_: Exception) {}
                wakeLock = null
                isBuilding = false

                // Stop foreground service — build is no longer running.
                // The service's stopForeground(STOP_FOREGROUND_DETACH) keeps the
                // completion notification visible until the user dismisses it.
                try { OTAService.stop(appContext ?: applicationContext) } catch (_: Exception) {}

                val current = activityRef?.get()
                if (current != null && !current.isFinishing && !current.isDestroyed) {
                    current.isExecuting = false
                    current.setUIExecuting(false)
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════
    //  UI Updates
    // ═══════════════════════════════════════════════════════════════

    /**
     * T19 — floating Build FAB availability (enabled-gated). The FAB sits at
     * bottom-end, across the collapsed log pill (bottom-start), and is
     * PERMANENTLY part of the layout: it renders disabled until the build
     * inputs are ready — at least one partition image loaded (none still in
     * the "loading:" placeholder state), a device codename present, and no
     * native operation executing. The M3 disabled-FAB colors come
     * automatically from the style's state-enabled color selectors — no
     * custom alpha/color hacks.
     *
     * DESIGN CHANGE (fatal-bug fix, 3rd attempt): T17/T18 drove the FAB via
     * visibility toggling (show()/hide(), then hard setVisibility against an
     * identity alpha/scale baseline) — and on the target device the
     * GONE→VISIBLE flip made AFTER the first layout pass never rendered.
     * The FAB appeared only after a theme-switch recreate(), i.e. exactly
     * when visibility was set BEFORE the first layout of a fresh view tree.
     * Two very different implementations failed the same way; meanwhile the
     * pre-T14 FAB of this app — always visible, gated by isEnabled — worked
     * for years. The proven pattern wins: the view never leaves the layout,
     * so "invisible" is impossible by construction; readiness is expressed
     * as the enabled state. Taps while unready are additionally guarded in
     * onBuildClicked() (busy/inputs checks) — defense in depth.
     *
     * T22 postscript: the "never rendered after the first layout pass"
     * reading above was a misdiagnosis — the real culprit was cache
     * starvation: onPause (IMPL-008) nulls cachedEditDevice, onResume never
     * re-resolved it, so every FAB update after a SAF picker trip read
     * device="" and froze the FAB state. Fixed in T22 by re-running
     * cacheViews() + updateBuildFab() at the end of onResume and making
     * this gate self-heal null caches.
     */
    private fun updateBuildFab() {
        // T22 self-heal: a readiness gate must never silently no-op on a null
        // cache — the old `?: return` froze whatever enabled state the FAB last
        // had (the 3rd-generation "grayed-out until theme switch" freeze). If a
        // cache is null, re-resolve it from the live view tree first; the null
        // path below can then only trigger when the view is genuinely absent.
        if (cachedFabBuild == null) {
            val fab = findViewById<com.google.android.material.floatingactionbutton.ExtendedFloatingActionButton>(R.id.fabBuild)
            if (fab != null) {
                fab.setOnClickListener { onBuildClicked() }
                cachedFabBuild = fab
            }
        }
        if (cachedEditDevice == null) {
            cachedEditDevice = findViewById(R.id.editTextDevice)
        }
        val fab = cachedFabBuild ?: return
        val device = (cachedEditDevice as? android.widget.EditText)?.text?.toString()?.trim() ?: ""
        val anyLoading = imageFiles.any { it.second.startsWith("loading:") }
        val canBuild = imageFiles.isNotEmpty() && !anyLoading && device.isNotEmpty() && !isExecuting
        fab.isEnabled = canBuild
    }

    private fun updateImageListUI() {
        val container = findViewById<android.widget.LinearLayout>(R.id.containerImageList)
        val removeButton = findViewById<View>(R.id.buttonRemoveAll)
        // T17: every imageFiles mutation funnels through here — refresh
        // the floating Build FAB alongside the list UI.
        updateBuildFab()

        container?.removeAllViews()

        if (imageFiles.isEmpty()) {
            val emptyText = android.widget.TextView(this).apply {
                text = getString(R.string.hint_no_images)
                textSize = 13f
                setTextColor(android.util.TypedValue().let { tv ->
                    context.theme.resolveAttribute(android.R.attr.textColorSecondary, tv, true)
                    tv.data
                })
                typeface = android.graphics.Typeface.MONOSPACE
            }
            container?.addView(emptyText)
            removeButton?.visibility = View.GONE
        } else {
            val sorted = imageFiles.sortedBy { it.first }
            sorted.forEachIndexed { idx, (name, path) ->
                val file = java.io.File(path)

                val row = android.widget.LinearLayout(this).apply {
                    orientation = android.widget.LinearLayout.HORIZONTAL
                    gravity = android.view.Gravity.CENTER_VERTICAL
                    // BUG-M08 fix: Use dpToPx for density-independent padding
                    setPadding(dpToPx(8), dpToPx(4), dpToPx(4), dpToPx(4))
                }

                val label = android.widget.TextView(this).apply {
                    // Check if this is a placeholder (path starts with "loading:")
                    // — render "Loading…" instead of file size for instant feedback.
                    val isLoading = path.startsWith("loading:")
                    text = if (isLoading) {
                        "  ${idx + 1}. $name  (Loading…)"
                    } else {
                        // For no-copy resolved paths, file.length() may return 0 if
                        // the file is on a path that java.io.File can't stat (even
                        // though Rust can read it via MANAGE_EXTERNAL_STORAGE).
                        // In that case, show "—" instead of "0 B" to avoid confusion.
                        val size = file.length()
                        val sizeStr = if (size > 0) formatFileSize(size) else "—"
                        "  ${idx + 1}. $name  ($sizeStr)"
                    }
                    textSize = 13f
                    // MD3-FIX IMPL-002: loading entries use the theme accent
                    // (?attr/colorPrimary) resolved at runtime so "Loading…" text
                    // follows the active palette (Suisei Blue default, Material
                    // You on 12+) — the old hardcoded teal (#80CBC4) clashed with
                    // the Suisei theme. Loaded entries keep the explicit neutral
                    // resource (P1-fix C-T-03: night-qualified, always visible).
                    if (isLoading) {
                        setTextColor(
                            resolveThemeColorAttr(com.google.android.material.R.attr.colorPrimary)
                                ?: androidx.core.content.ContextCompat.getColor(
                                    this@MainActivity, R.color.partition_text_loading
                                )
                        )
                    } else {
                        setTextColor(
                            androidx.core.content.ContextCompat.getColor(
                                this@MainActivity, R.color.partition_text
                            )
                        )
                    }
                    // Italicize loading entries to visually distinguish them
                    if (isLoading) {
                        typeface = android.graphics.Typeface.create(
                            android.graphics.Typeface.MONOSPACE,
                            android.graphics.Typeface.ITALIC
                        )
                    } else {
                        typeface = android.graphics.Typeface.MONOSPACE
                    }
                    layoutParams = android.widget.LinearLayout.LayoutParams(0, android.widget.LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
                }

                val removeBtn = com.google.android.material.button.MaterialButton(this).apply {
                    // Use official Material Icons Round "close" (X) icon instead
                    // of the previous text "x" which looked unprofessional.
                    // Icon is self-theming via ?attr/colorOnSurface fillColor;
                    // we override the tint to colorError so the delete action
                    // is visually distinct from regular UI elements.
                    icon = androidx.core.content.ContextCompat.getDrawable(
                        this@MainActivity, R.drawable.ic_close
                    )
                    // MaterialButton.setIconSize() expects Int (pixels), not Float.
                    // dpToPx() already returns Int — no .toFloat() needed.
                    // (Previous .toFloat() caused CI build failure:
                    // "Type mismatch: inferred type is Float but Int was expected")
                    iconSize = dpToPx(18)
                    text = null  // icon-only button
                    contentDescription = this@MainActivity.getString(R.string.cd_remove_partition, name)  // P0-fix C-05: was hardcoded "Remove $name partition"
                    insetTop = 0
                    insetBottom = 0
                    minimumWidth = 0
                    minWidth = 0
                    setPadding(0, 0, 0, 0)
                    background = null
                    // Tint icon with colorError (red) so the delete action is
                    // visually distinct. colorError resolves correctly across
                    // all themes (default teal, Suisei Blue, Material You).
                    iconTint = android.content.res.ColorStateList.valueOf(
                        androidx.core.content.ContextCompat.getColor(
                            this@MainActivity, R.color.status_error
                        )
                    )
                    // SIZE-AUDIT S2: explicit 48×48dp — M3 minimum touch target.
                    // Previously wrap_content with an 18dp icon and 8dp
                    // horizontal padding rendered ~34×40dp, below the 48dp
                    // accessibility floor (same defect class as P0-fix A-02
                    // on buttonToggleLog). Fixed size + zero padding lets the
                    // default content gravity center the icon in the 48dp box;
                    // the visual icon stays 18dp so rows remain compact.
                    layoutParams = android.widget.LinearLayout.LayoutParams(
                        dpToPx(48),
                        dpToPx(48)
                    )
                    setOnClickListener {
                        imageFiles.removeAll { it.first == name && it.second == path }
                        copyPendingRemovals()
                        updateImageListUI()
                        updateOutputPreview()
                        showLog("Removed: $name")
                    }
                }

                row.addView(label)
                row.addView(removeBtn)
                container?.addView(row)
            }
            removeButton?.visibility = View.VISIBLE
        }

        // Show/hide empty state hint
        val emptyHint = findViewById<View>(R.id.textEmptyHint)
        emptyHint?.visibility = if (imageFiles.isEmpty()) View.VISIBLE else View.GONE
    }

    private fun updateOutputPreview() {

        // Use custom filename if set, otherwise auto-generate from device name
        val customName = prefs.getString("pref_custom_filename", "")?.trim()
        val fileName = if (!customName.isNullOrEmpty()) {
            // Ensure .zip extension
            if (customName.lowercase().endsWith(".zip")) customName else "$customName.zip"
        } else {
            val device = prefs.getString("device", "")?.trim().orEmpty()
            OTABridge.buildOutputFileName(device.ifEmpty { "generic" })
        }

        // Show preview in dedicated TextView
        findViewById<android.widget.TextView>(R.id.textPreviewFilename)?.text = fileName
    }

    private fun copyPendingRemovals() {
        // Cleanup inputDir for copied images + orphaned .part temp files.
        //
        // IMPORTANT: Only delete files INSIDE inputDir. Never delete files
        // outside inputDir — those are the user's original files at their
        // original location (used in-place when resolveUriToFilePath succeeded).
        // Previously this function would delete any .img file that wasn't in
        // imageFiles, which would have deleted the user's originals if they
        // had been resolved to a real path instead of copied.
        val inputDirPath = inputDir.absolutePath
        inputDir.listFiles()?.forEach { file ->
            val isOrphanedImg = file.name.endsWith(".img") &&
                !imageFiles.any { it.second == file.absolutePath }
            val isOrphanedPart = file.name.endsWith(".part")
            if (isOrphanedImg || isOrphanedPart) {
                file.delete()
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Lifecycle
    // ═══════════════════════════════════════════════════════════════

    override fun onResume() {
        super.onResume()
        cacheLogViews()  // Cache log views for efficient appendLogLineUI
        activityRef = WeakReference(this)
        // T23 — moved cacheViews() to the TOP of onResume (T22 placed it at
        // the tail): the isBuilding branches below call setUIExecuting(),
        // which toggles ten controls through these caches — with them still
        // null every toggle no-op'ed, so after a "build finished while
        // backgrounded" resume the controls stayed disabled (only the FAB
        // self-healed). Populated first, every branch now applies its state
        // for real. cacheViews() is idempotent (findViewById re-assignment +
        // FAB listener re-set, no TextWatchers).
        cacheViews()
        // Restore persisted log text on Activity recreation
        // Always restore from the companion buffer to ensure logs survive
        // minimize/reopen and Activity recreation (fix: logs clearing on warm resume)
        if (savedLogText.isNotEmpty()) {
            val textView = findViewById<android.widget.TextView>(R.id.textViewLog)
            if (textView != null) {
                textView.text = savedLogText.toString()
            }
        }
        // Show battery optimization prompt after user returns from storage settings
        if (pendingBatteryPrompt) {
            pendingBatteryPrompt = false
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && !Environment.isExternalStorageManager()) {
                // Storage still not granted — defer again
                pendingBatteryPrompt = true
            } else {
                checkBatteryOptimizationAtStartup()
            }
        }
        // Check if build process is actually alive
        if (isBuilding) {
            val elapsed = System.currentTimeMillis() - lastProgressTime
            if (lastProgressTime > 0 && elapsed > DEAD_PROCESS_THRESHOLD_MS) {
                // No progress for > 2 minutes — process was killed by OS
                isBuilding = false
                isExecuting = false
                // Stop foreground service if still running (shouldn't be, but safety net)
                try { OTAService.stop(applicationContext) } catch (_: Exception) {}
                cancelBuildNotification()
                showLog("\nBuild was interrupted — process killed (idle timeout).", LogLevel.ERROR)
                showLog("The device may have entered Doze mode and killed the background process.", LogLevel.WARN)
                // Offer to open battery optimization settings directly
                MaterialAlertDialogBuilder(this)
                    .setTitle("Build Killed by System")
                    .setMessage(
                        "Android killed OTAku to save battery during the build.\n\n" +
                        "To prevent this, grant \"Unrestricted\" battery usage for OTAku.\n\n" +
                        "Go to: Settings > Apps > OTAku > Battery > Unrestricted"
                    )
                    .setPositiveButton("Open Settings") { _, _ ->
                        try {
                            val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                                data = Uri.parse("package:$packageName")
                            }
                            startActivity(intent)
                        } catch (_: Exception) {
                            try {
                                startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
                            } catch (_: Exception) {
                                showLog("Could not open battery settings automatically.", LogLevel.WARN)
                            }
                        }
                    }
                    .setNegativeButton("Dismiss", null)
                    .show()
                setUIExecuting(false)
            } else {
                // Process still alive — reconnect UI
                isExecuting = true
                setUIExecuting(true)
                // Only log "returned from background" once per continuous build session,
                // not on every Activity recreation (which can happen multiple times
                // when the user switches between apps rapidly).
                if (!resumedWhileBuildingLogged) {
                    resumedWhileBuildingLogged = true
                    showLog("Build in progress (returned from background).")
                }
                // Re-sync notification with current progress state
                if (lastProgressMessage.isNotEmpty() && lastNotifPercent >= 0) {
                    showProgressNotification(lastProgressMessage, lastNotifPercent)
                }
                // Re-create split progress bars with current state
                if (partitionCount > 0) {
                    val savedProgress = snapshotPartitionProgress()
                    val savedIndex = currentPartitionIndex
                    setupSplitProgressBar(partitionNames)
                    synchronized(progressLock) { savedProgress.copyInto(partitionProgress) }
                    currentPartitionIndex = savedIndex
                    renderPartitionProgress()
                }
            }
        } else {
            // Build finished while app was in background.
            // Fix K4: previously only canceled the notification — didn't reset UI or display
            // the missed completion event. Now we check lastBuildResult and display it.
            cancelBuildNotification()
            isExecuting = false
            setUIExecuting(false)

            // If we missed the completion event (build finished while backgrounded),
            // display it now: mark progress bars 100%, re-render, show completion notification.
            if (lastBuildResult != null && !buildResultDisplayed) {
                buildResultDisplayed = true
                val result = lastBuildResult!!

                // Mark all partition progress as complete
                markAllProgressComplete()

                // Re-render progress bars at 100% if visible
                renderPartitionProgress(forceComplete = true)

                // Re-show completion notification (uses appContext, safe to call here)
                if (result.success) {
                    val duration = if (result.durationMs < 60000) "${result.durationMs / 1000}s"
                        else "${result.durationMs / 60000}m ${result.durationMs % 60000 / 1000}s"
                    showCompletionNotification(true, "Finished in $duration")
                } else {
                    showCompletionNotification(false, result.error ?: "Unknown error")
                }
            }
        }

        // T23: final Build FAB re-sync — the isBuilding branches above may
        // have corrected isExecuting/isBuilding, and updateBuildFab is the
        // single gate that must reflect the corrected state.
        updateBuildFab()
    }



    override fun onConfigurationChanged(newConfig: android.content.res.Configuration) {
        super.onConfigurationChanged(newConfig)
        // Handle floating window resize / multi-window mode without Activity
        // recreation. Companion object state (imageFiles, savedLogText, etc.)
        // survives recreation, so the previous concern about memory spikes
        // from savedLogText.toString() no longer applies — data is in the
        // companion object, not the instance Bundle.

        // IMPL-004/IMPL-005 (theme toggle fix): Detect uiMode change with
        // double-recreate prevention and explicit-override awareness.
        //
        // Three cases:
        //   1. cycleTheme() just called applyTheme()+recreate() — our flag
        //      is set → skip this onConfigurationChanged's recreate() to
        //      avoid double recreation (visual flicker, broken colors).
        //   2. System dark mode changed AND app follows system ("system"
        //      mode) → recreate() needed to apply the new mode.
        //   3. System dark mode changed BUT app has explicit light/dark
        //      override → skip recreate(). The delegate's
        //      setDefaultNightMode() override is authoritative; the
        //      system uiMode change doesn't affect visual output, and
        //      an unnecessary recreate() just causes flicker.
        val newUiMode = newConfig.uiMode and android.content.res.Configuration.UI_MODE_NIGHT_MASK
        if (lastUiMode != 0 && newUiMode != lastUiMode) {
            if (themeSwitchInProgress) {
                // Case 1: cycleTheme() already called recreate() — skip
                themeSwitchInProgress = false
                lastUiMode = newUiMode
                return
            }
            val themeMode = prefs.getString("pref_theme_mode", "system") ?: "system"
            if (themeMode != "system") {
                // Case 3: explicit override — system change doesn't affect us
                lastUiMode = newUiMode
                return
            }
            // Case 2: system mode change while following system → recreate
            // Safety net: re-apply theme before recreate to ensure
            // AppCompatDelegate state matches the current preference.
            applyTheme()
            recreate()
            @Suppress("DEPRECATION")
            overridePendingTransition(android.R.anim.fade_in, 0)
            return  // recreate() handles everything — skip layout patching
        }
        lastUiMode = newUiMode

        // Re-layout progress bars if a build is in progress.
        // IMPL-001 (UI-001 fix): Save progress state before setupSplitProgressBar
        // because it resets partitionProgress to IntArray(count) (all zeros).
        // Without this, a config change (e.g. floating window resize) during
        // an active build would reset all progress bars to 0%, making it
        // appear as if the build restarted from scratch.
        if (isBuilding && imageFiles.isNotEmpty()) {
            val savedProgress = snapshotPartitionProgress()
            val savedIndex = currentPartitionIndex
            setupSplitProgressBar(imageFiles.map { it.first })
            synchronized(progressLock) { savedProgress.copyInto(partitionProgress) }
            currentPartitionIndex = savedIndex
            // Re-render progress bars with restored values
            renderPartitionProgress()
        }
        // Scroll log to bottom (layout may have shifted)
        // IMPL-003 (BUG-02 fix): Use NestedScrollView type instead of ScrollView
        val scrollView = findViewById<androidx.core.widget.NestedScrollView>(R.id.scrollViewLog)
        scrollView?.post {
            val child = scrollView.getChildAt(0)
            if (child != null) {
                val target = child.bottom - scrollView.height
                scrollView.smoothScrollTo(0, if (target > 0) target else 0)
            }
        }
    }

    override fun onPause() {
        super.onPause()
        activityRef = null
        // IMPL-008: Invalidate cached view references — views may be
        // detached/invalid after pause; re-resolve on next onResume.
        cachedBtnAddImages = null
        cachedBtnRemoveAll = null
        cachedBtnBrowseOutput = null
        cachedLayoutCompression = null
        cachedLayoutCompressionLevel = null
        cachedEditDevice = null
        cachedBtnAutoDetect = null
        cachedEditFilename = null
        cachedProgressContainer = null
        cachedBarRow = null
    }

    override fun onDestroy() {
        super.onDestroy()
    }

    private fun setupSplitProgressBar(names: List<String>) {
        val count = names.size
        partitionCount = count
        partitionProgress = IntArray(count)
        currentPartitionIndex = -1
        val container = findViewById<android.widget.LinearLayout>(R.id.progressBarContainer) ?: return
        container.removeAllViews()
        container.orientation = android.widget.LinearLayout.VERTICAL
        container.visibility = View.VISIBLE

        // Horizontal row for progress bars
        val barRow = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.HORIZONTAL
            tag = "bar_row"
            layoutParams = android.widget.LinearLayout.LayoutParams(
                android.widget.LinearLayout.LayoutParams.MATCH_PARENT,
                android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
            )
        }

        // Horizontal row for partition name labels
        val labelRow = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.HORIZONTAL
            layoutParams = android.widget.LinearLayout.LayoutParams(
                android.widget.LinearLayout.LayoutParams.MATCH_PARENT,
                android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
            ).apply {
                topMargin = dpToPx(4)
            }
        }

        // Resolve theme-aware track color (works in both light and dark mode)
        val trackTv = android.util.TypedValue()
        theme.resolveAttribute(com.google.android.material.R.attr.colorSurfaceVariant, trackTv, true)
        val trackColor = ContextCompat.getColor(this@MainActivity, trackTv.resourceId)

        // Resolve theme-aware indicator color (primary color for active progress)
        val indicatorTv = android.util.TypedValue()
        theme.resolveAttribute(com.google.android.material.R.attr.colorPrimary, indicatorTv, true)
        val indicatorColor = ContextCompat.getColor(this@MainActivity, indicatorTv.resourceId)

        for (i in 0 until count) {
            val name = names.getOrElse(i) { "" }
            val isLast = (i == count - 1)
            val gap = if (!isLast) dpToPx(4) else 0

            // Progress bar for this partition — theme-aware colors + animation
            val bar = com.google.android.material.progressindicator.LinearProgressIndicator(this).apply {
                layoutParams = android.widget.LinearLayout.LayoutParams(0, android.widget.LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply {
                    marginEnd = gap
                }
                isIndeterminate = false
                progress = 0
                setTrackColor(trackColor)
                setIndicatorColor(indicatorColor)
            }
            barRow.addView(bar)

            // Partition name label — use theme attribute for dark mode support
            val tv = android.util.TypedValue()
            theme.resolveAttribute(android.R.attr.textColorSecondary, tv, true)
            val labelColor = ContextCompat.getColor(this@MainActivity, tv.resourceId)

            val label = android.widget.TextView(this).apply {
                text = name
                textSize = 10f
                setTextColor(labelColor)
                gravity = android.view.Gravity.CENTER
                layoutParams = android.widget.LinearLayout.LayoutParams(0, android.widget.LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply {
                    marginEnd = gap
                }
                maxLines = 1
                ellipsize = android.text.TextUtils.TruncateAt.END
            }
            labelRow.addView(label)
        }

        container.addView(barRow)
        container.addView(labelRow)
    }

    /**
     * MD3-FIX IMPL-001: Resolve a theme color attribute (e.g. colorPrimary)
     * to a concrete ARGB int from the ACTIVITY theme — which already includes
     * the night-mode variant AND the DynamicColors overlay when active. This
     * is what makes runtime-colored views follow the active palette:
     *   - API 31+ + dynamic color ON  → Material You system palette
     *   - API 26-30 / toggle OFF      → Suisei Blue brand palette (default)
     *
     * @return the resolved color, or null if the attribute can't be resolved
     *         (callers keep their own sensible fallback).
     */
    private fun resolveThemeColorAttr(attrResId: Int): Int? {
        return try {
            val typedValue = android.util.TypedValue()
            if (!theme.resolveAttribute(attrResId, typedValue, true)) {
                return null
            }
            if (typedValue.resourceId != 0) {
                androidx.core.content.ContextCompat.getColor(this, typedValue.resourceId)
            } else {
                typedValue.data
            }
        } catch (e: Exception) {
            android.util.Log.w("OTAku", "resolveThemeColorAttr failed: ${e.message}")
            null
        }
    }

    private fun dpToPx(dp: Int): Int = (dp * resources.displayMetrics.density).toInt()

    /**
     * Update UI to reflect build execution state.
     *
     * IMPL-008: Uses cached view references instead of repeated findViewById()
     * calls. findViewById() traverses the view hierarchy (O(n)) on every call;
     * during builds with rapid progress updates, this caused measurable main
     * thread jank. Cached references are populated in onCreate()/onResume().
     */
    private fun setUIExecuting(executing: Boolean) {
        runOnUiThread {
            // IMPL-013: Cached views are eagerly resolved in cacheViews()
            // (called from onCreate after setContentView). No lazy resolution needed.
            if (executing) {
                cachedProgressContainer?.visibility = View.VISIBLE
            } else {
                cachedProgressContainer?.visibility = View.GONE
                cachedProgressContainer?.removeAllViews()
                partitionCount = 0
                partitionProgress = IntArray(0)
                currentPartitionIndex = -1
                partitionNames = emptyList()
            }
            cachedBtnAddImages?.isEnabled = !executing
            cachedBtnRemoveAll?.isEnabled = !executing
            // Disable all input controls during build to prevent user from
            // changing settings that have no effect on the running build
            // but would mislead them into thinking they do.
            cachedBtnBrowseOutput?.isEnabled = !executing
            // T19: disable the ExposedDropdownMenu TextInputLayout wrappers —
            // recursive disable covers the inner dropdown fields and greys
            // the outlined boxes (M3 disabled state).
            cachedLayoutCompression?.isEnabled = !executing
            cachedLayoutCompressionLevel?.isEnabled = !executing
            cachedEditDevice?.isEnabled = !executing
            cachedBtnAutoDetect?.isEnabled = !executing
            cachedEditFilename?.isEnabled = !executing
            // T19: FAB disables while executing, re-enables when inputs
            // are still valid (enabled-gated — see updateBuildFab).
            updateBuildFab()
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Log Level System
    // ═══════════════════════════════════════════════════════════════

    // MD3-FIX IMPL-001: log level colors resolve from the ACTIVE theme's
    // color tokens (not static color resources) so log lines follow the
    // active palette — Suisei Blue brand accent (API 26-30 or toggle OFF)
    // or Material You dynamic color (API 31+). The old @color/log_* values
    // were teal-specific and clashed with the default Suisei theme.
    //   DEBUG   → textColorSecondary (neutral, muted)
    //   INFO    → colorPrimary        (theme accent)
    //   WARN    → colorTertiary       (warm accent slot)
    //   ERROR   → colorError          (semantic red)
    //   SUCCESS → colorSecondary      (muted secondary slot)
    enum class LogLevel(val tag: String, val themeAttr: Int, val priority: Int) {
        DEBUG("DBG ", android.R.attr.textColorSecondary, android.util.Log.VERBOSE),
        INFO("INFO", com.google.android.material.R.attr.colorPrimary, android.util.Log.INFO),
        WARN("WARN", com.google.android.material.R.attr.colorTertiary, android.util.Log.WARN),
        ERROR("ERR ", com.google.android.material.R.attr.colorError, android.util.Log.ERROR),
        SUCCESS("OK  ", com.google.android.material.R.attr.colorSecondary, android.util.Log.INFO),
        PLAIN("", 0, android.util.Log.DEBUG),
    }

    /** BUG-M15 fix: Use DateTimeFormatter instead of SimpleDateFormat.
     * SimpleDateFormat is NOT thread-safe — concurrent calls from UI + IO coroutines
     * can produce corrupted timestamps. DateTimeFormatter is immutable and thread-safe. */
    private val logTimeFormat = java.time.format.DateTimeFormatter.ofPattern("HH:mm:ss", java.util.Locale.US)

    /** Cached log TextView — avoids findViewById per log line. */
    private var cachedLogView: android.widget.TextView? = null
    /** Cached log NestedScrollView — avoids findViewById per scroll. */
    private var cachedScrollView: androidx.core.widget.NestedScrollView? = null
    /** Last scroll-to-bottom timestamp — throttle to avoid jank during rapid builds. */
    private var lastScrollTime: Long = 0L

    /** Resolve and cache log view references. Call once in onResume or after layout. */
    private fun cacheLogViews() {
        cachedLogView = findViewById(R.id.textViewLog)
        cachedScrollView = findViewById(R.id.scrollViewLog)
    }

    /**
     * UI-only log append — does NOT persist to savedLogText.
     * Caller must persist separately (via savedLogText.append or showLog).
     * Must be called on the UI thread (wrap in runOnUiThread).
     *
     * Optimizations over the original implementation:
     *   - Cached SimpleDateFormat (companion val) instead of per-call allocation
     *   - Cached TextView/ScrollView instead of per-call findViewById
     *   - Throttled auto-scroll (every ~100ms) to avoid jank during rapid builds
     */
    private fun appendLogLineUI(line: String, level: LogLevel = LogLevel.PLAIN) {
        val textView = cachedLogView ?: return

        if (level == LogLevel.PLAIN) {
            textView.append(line)
        } else {
            val timestamp = logTimeFormat.format(java.time.LocalTime.now())
            val prefix = "[$timestamp] [${level.tag}] "
            val colored = SpannableString("$prefix$line")
            try {
                // MD3-FIX IMPL-001: color the prefix with the ACTIVE theme's
                // token for this level (see LogLevel) — resolved through the
                // activity theme, so DynamicColors (API 31+) and the night
                // variant are both honored. Falls back to plain text.
                resolveThemeColorAttr(level.themeAttr)?.let { c ->
                    colored.setSpan(
                        ForegroundColorSpan(c),
                        0, prefix.length, SpannableString.SPAN_EXCLUSIVE_EXCLUSIVE
                    )
                }
            } catch (_: Exception) { /* fallback to plain */ }
            textView.append(colored)
        }

        // Throttled scroll-to-bottom — only scroll every 100ms to avoid jank
        // during rapid build output (e.g. compression progress at 500ms intervals)
        val now = System.currentTimeMillis()
        if (now - lastScrollTime >= 100L) {
            lastScrollTime = now
            val scrollView = cachedScrollView ?: return
            scrollView.post {
                val child = scrollView.getChildAt(0)
                if (child != null) {
                    val target = child.bottom - scrollView.height
                    scrollView.smoothScrollTo(0, if (target > 0) target else 0)
                }
            }
        }
    }

    private fun showLog(text: String, level: LogLevel = LogLevel.INFO) {
        val line = if (text.endsWith("\n")) text else "$text\n"
        // Persist to companion object (survives Activity recreation)
        appendToSavedLog(line)

        // Mirror to Logcat for diagnostics (visible in `adb logcat -s OTAku`)
        when (level) {
            LogLevel.ERROR   -> Log.e("OTAku", text)
            LogLevel.WARN    -> Log.w("OTAku", text)
            LogLevel.INFO    -> Log.i("OTAku", text)
            LogLevel.DEBUG   -> Log.d("OTAku", text)
            LogLevel.SUCCESS -> Log.i("OTAku", "\u2713 $text")
            LogLevel.PLAIN   -> Log.d("OTAku", text)
        }

        // Skip handler posting if already on UI thread
        if (Looper.myLooper() == Looper.getMainLooper()) {
            appendLogLineUI(line, level)
        } else {
            runOnUiThread { appendLogLineUI(line, level) }
        }
    }

    private fun copyLogToClipboard() {
        val logText = findViewById<android.widget.TextView>(R.id.textViewLog)?.text?.toString()
        if (logText.isNullOrBlank()) {
            Toast.makeText(this, "Log is empty", Toast.LENGTH_SHORT).show()
            return
        }
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val clip = ClipData.newPlainText("OTAku Log", logText)
        clipboard.setPrimaryClip(clip)
        Toast.makeText(this, getString(R.string.log_copied), Toast.LENGTH_SHORT).show()
    }

    // ═══════════════════════════════════════════════════════════════
    //  Utilities
    // ═══════════════════════════════════════════════════════════════

    private fun formatFileSize(bytes: Long): String {
        return when {
            bytes < 1024 -> "$bytes B"
            bytes < 1024 * 1024 -> String.format("%.1f KB", bytes / 1024.0)
            bytes < 1024 * 1024 * 1024 -> String.format("%.1f MB", bytes / (1024.0 * 1024))
            else -> String.format("%.2f GB", bytes / (1024.0 * 1024 * 1024))
        }
    }
}
