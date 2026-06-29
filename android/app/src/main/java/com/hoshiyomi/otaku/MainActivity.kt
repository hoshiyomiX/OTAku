package com.hoshiyomi.otaku

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.PowerManager
import android.provider.DocumentsContract
import android.provider.Settings
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
    private var isExecuting = false
    companion object {
        // Application-scoped coroutine scope for long-running build operations.
        // Survives Activity destruction when the user minimizes the app.
        private val buildScope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)

        // Partition image list — moved to companion so it survives Activity recreation
        // (theme switch via AppCompatDelegate.setDefaultNightMode triggers recreation
        // since configChanges doesn't include uiMode). Previously this was an instance
        // member, so switching theme would clear the list and force the user to re-pick.
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
        @Volatile private var savedLogText: StringBuffer = StringBuffer()

        // Heartbeat: last time a progress update was received (epoch millis)
        @Volatile private var lastProgressTime: Long = 0L
        // Threshold: if no progress for this long (ms), process is assumed dead
        private const val DEAD_PROCESS_THRESHOLD_MS = 120_000L  // 2 minutes

        // Per-partition split progress bar state
        @Volatile private var partitionCount: Int = 0
        @Volatile private var partitionProgress: IntArray = IntArray(0)
        @Volatile private var currentPartitionIndex: Int = -1
        @Volatile private var partitionNames: List<String> = emptyList()

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

        // Suppress repeated "Build in progress (returned from background)" log.
        // Only log once per continuous build session, not on every Activity recreation.
        @Volatile private var resumedWhileBuildingLogged = false

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
                val notification = NotificationCompat.Builder(ctx, OTAkuApp.CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.ic_media_play)
                    .setContentTitle("OTAku")
                    .setContentText(message)
                    .setProgress(100, percent.coerceIn(0, 100), percent == 0)
                    .setOngoing(true)
                    .setSilent(true)
                    .setContentIntent(pi)
                    .setPriority(NotificationCompat.PRIORITY_LOW)
                    .build()
                nm.notify(NOTIFICATION_ID, notification)
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
                val notification = NotificationCompat.Builder(ctx, OTAkuApp.CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.ic_media_play)
                    .setContentTitle(if (success) "Build Complete" else "Build Failed")
                    .setContentText(message)
                    .setOngoing(false)
                    .setAutoCancel(true)
                    .setContentIntent(pi)
                    .setPriority(NotificationCompat.PRIORITY_DEFAULT)
                    .build()
                nm.notify(NOTIFICATION_ID, notification)
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
         *   - setUIExecuting(false) — resets build button text + hides progress bars
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
                savedLogText.append("\n═══ Build complete ═══\n")
            } else {
                showCompletionNotification(false, result.error ?: "Unknown error")
                savedLogText.append("\n[ERROR] ${result.error ?: "Unknown error"}\n")
            }

            // Mark all partition progress as complete (companion state)
            for (i in 0 until partitionCount) {
                partitionProgress[i] = 100
            }

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

    private val removeImageConfirm = registerForActivityResult(
        ActivityResultContracts.CreateDocument("application/octet-stream")
    ) { /* Not used — placeholder for future file save */ }

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
        // Apply theme BEFORE setContentView.
        // applyDynamicTheme() decides between:
        //   - Theme.OTAku (default teal, with Material You overrides on API 31+)
        //   - Theme.OTAku.Suisei (Suisei Blue fallback on API 26-30 or user-disabled)
        // applyTheme() (light/dark/system night mode) must run BEFORE setTheme()
        // because AppCompatDelegate.setDefaultNightMode triggers Activity recreation.
        applyTheme()
        applyDynamicTheme()
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

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
        setupCompressionSelector()
        setupButtons()
        setupToolbar()
        setupDeviceMetaFields()
        setupOutputField()
        setupCustomFilenameField()
        setupThemeToggle()
        updateOutputPreview()  // Show default filename preview immediately
        updateBuildButtonState()  // Disable Build button until partitions are added

        requestStoragePermissions()
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
                val nativeVersion = NativeBridge.getVersion()
                showLog("Native backend: $nativeVersion", LogLevel.INFO)
                val depCheck = NativeBridge.checkDeps()
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
     * Apply dynamic color theme based on device capability and user preference.
     *
     * Decision tree:
     *   - API 31+ (Android 12, Material You available) AND user hasn't disabled
     *     dynamic color → use Theme.OTAku (default teal, which Material You
     *     will override at runtime via DynamicColors.applyToActivityIfAvailable)
     *   - API 26-30 (Material You unavailable) OR user disabled dynamic color
     *     → use Theme.OTAku.Suisei (Suisei Blue fallback palette #00B0F0)
     *
     * Must be called BEFORE setContentView() so the theme attributes are
     * resolved correctly during view inflation.
     *
     * Side effect: on API 31+, also calls DynamicColors.applyToActivityIfAvailable()
     * to enable Material You wallpaper-derived colors for Material3 components.
     * This is a no-op on older versions.
     */
    private fun applyDynamicTheme() {
        val useDynamic = SuiseiColors.shouldUseDynamicTheme(prefs)
        if (useDynamic) {
            // Use default Theme.OTAku — Material You will override at runtime.
            setTheme(R.style.Theme_OTAku)
            // Apply Material You dynamic colors (API 31+ only; no-op below)
            // DynamicColors is from com.google.android.material:material:1.11+.
            // Safe to call on all API levels — it checks internally.
            try {
                com.google.android.material.color.DynamicColors
                    .applyToActivityIfAvailable(this)
            } catch (_: Throwable) {
                // Defensive: if Material library is older than expected,
                // fall through silently. Theme.OTAku still works.
            }
        } else {
            // Use Suisei Blue fallback palette.
            setTheme(R.style.Theme_OTAku_Suisei)
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
            else -> super.onOptionsItemSelected(item)
        }
    }

    /** Cycle theme: System -> Light -> Dark -> System */
    private fun cycleTheme() {
        val current = prefs.getString("pref_theme_mode", "system") ?: "system"
        val next = when (current) {
            "system" -> "light"
            "light" -> "dark"
            else -> "system"
        }
        prefs.edit { putString("pref_theme_mode", next) }
        applyTheme()
        invalidateOptionsMenu()
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

    private fun setupThemeToggle() {
        // Theme toggle is handled via toolbar menu item (R.id.action_toggle_theme).
        // No extra setup needed here — onCreateOptionsMenu + onOptionsItemSelected handle it.
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

        // Auto-detect button: fill device field with Build.PRODUCT
        findViewById<View>(R.id.buttonAutoDetect)?.setOnClickListener {
            val deviceName = android.os.Build.PRODUCT
            editDevice?.setText(deviceName)
            prefs.edit { putString("device", deviceName) }
            updateOutputPreview()
            updateBuildButtonState()
            showLog("Auto-detected device: $deviceName")
        }

        // Listen for changes in device codename — update Build button state
        editDevice?.addTextChangedListener(object : android.text.TextWatcher {
            override fun afterTextChanged(s: android.text.Editable?) {
                val text = s?.toString()?.trim() ?: ""
                prefs.edit { putString("device", text) }
                updateOutputPreview()
                updateBuildButtonState()
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
        val spinner = findViewById<android.widget.Spinner>(R.id.spinnerCompression)
        val displayLabels = listOf(
            "none — no compression (100%)",
            "gzip — standard (~60%)",
            "bzip2 — high (~50%)",
            "xz — ultra (~45%)",
            "brotli — best (~40%)"
        )
        val adapter = ArrayAdapter(
            this,
            android.R.layout.simple_spinner_item,
            displayLabels
        ).also { it.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item) }
        spinner.adapter = adapter

        spinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: View?, position: Int, id: Long) {
                selectedCompression = OTABridge.COMPRESSION_ALGORITHMS[position]
                updateCompressionLevelSpinner()
                updateOutputPreview()
            }
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }

        // Initialize compression level spinner
        setupCompressionLevelSpinner()
    }

    // Compression level ranges per algorithm (matches Rust native backend LEVEL_RANGES)
    // Default level per algorithm (matches Rust native backend DEFAULT_LEVELS):
    //   gzip=6, bzip2=9, xz=6, brotli=6
    private val COMPRESSION_LEVELS: Map<String, Pair<Int, Int>> = mapOf(
        "none" to Pair(0, 0),     // no compression
        "gzip" to Pair(1, 9),     // stdlib gzip: levels 1-9, default 6
        "bzip2" to Pair(1, 9),    // stdlib bzip2: levels 1-9, default 9
        "xz" to Pair(0, 9),       // stdlib lzma: levels 0-9, default 6
        "brotli" to Pair(0, 11)   // brotli: quality 0-11, default 6
    )

    // Default compression level per algorithm (single source of truth for UI labels)
    private val DEFAULT_COMPRESSION_LEVELS: Map<String, Int> = mapOf(
        "none" to 0,
        "gzip" to 6,
        "bzip2" to 9,
        "xz" to 6,
        "brotli" to 6
    )

    private fun setupCompressionLevelSpinner() {
        val spinner = findViewById<android.widget.Spinner>(R.id.spinnerCompressionLevel)
        updateCompressionLevelSpinner()

        spinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: View?, position: Int, id: Long) {
                val items = getCurrentLevelItems()
                selectedCompressionLevel = if (position < items.size) items[position] else 0
            }
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }
    }

    private fun getCurrentLevelItems(): List<Int> {
        val range = COMPRESSION_LEVELS[selectedCompression] ?: (0 to 0)
        val (min, max) = range
        return if (min == 0 && max == 0) {
            listOf(0)  // "none" → just show "Default"
        } else {
            listOf(0) + (min..max).toList()  // 0 (default) + 1..9
        }
    }

    private fun updateCompressionLevelSpinner() {
        val spinner = findViewById<android.widget.Spinner>(R.id.spinnerCompressionLevel) ?: return
        val items = getCurrentLevelItems()
        val defaultLevel = DEFAULT_COMPRESSION_LEVELS[selectedCompression] ?: 0
        val labels = items.map { if (it == 0) "Default ($defaultLevel)" else "$it" }
        val adapter = ArrayAdapter(
            this,
            android.R.layout.simple_spinner_item,
            labels
        ).also { it.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item) }
        spinner.adapter = adapter
        // Reset selection to "Default"
        spinner.setSelection(0)
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

        findViewById<View>(R.id.buttonExecute).setOnClickListener {
            onBuildClicked()
        }

        findViewById<View>(R.id.buttonCopyLog).setOnClickListener {
            copyLogToClipboard()
        }

        findViewById<View>(R.id.buttonClearLog).setOnClickListener {
            findViewById<android.widget.TextView>(R.id.textViewLog).text = ""
            savedLogText.setLength(0)
        }

        // ── Log panel expand/collapse toggle ──
        // Clicking the header bar or the toggle button collapses the ENTIRE
        // log card (not just the ScrollView inside). When collapsed, the card
        // shrinks to just the header bar (~44dp), and the settings
        // NestedScrollView above gets all the freed space.
        // State is persisted in companion (survives Activity recreation).
        val logCard = findViewById<com.google.android.material.card.MaterialCardView>(R.id.logCard)
        val logHeader = findViewById<View>(R.id.logHeaderBar)
        val toggleBtn = findViewById<android.widget.ImageView>(R.id.buttonToggleLog)
        val logDivider = findViewById<View>(R.id.logDivider)
        val logScrollView = findViewById<android.widget.ScrollView>(R.id.scrollViewLog)

        fun applyLogExpandedState(expanded: Boolean) {
            // Slide-in / slide-out animation on the ScrollView.
            // Expand: ScrollView starts at translationY = -itsHeight (above header),
            //   then slides down to 0 — looks like content dropping in from the header.
            // Collapse: ScrollView slides up from 0 to -itsHeight — content slides
            //   up into the header and disappears.
            // The card LayoutParams change is instant (height swap), but the slide
            // animation masks it — user sees smooth content sliding, not a snap.
            if (expanded) {
                // Expand: make visible first, then slide down from top
                logScrollView?.visibility = View.VISIBLE
                logDivider?.visibility = View.VISIBLE
                logScrollView?.let { sv ->
                    sv.measure(View.MeasureSpec.UNSPECIFIED, View.MeasureSpec.UNSPECIFIED)
                    val slideDistance = sv.measuredHeight.toFloat().coerceAtLeast(200f)
                    sv.translationY = -slideDistance
                    sv.alpha = 0f
                    sv.animate()
                        ?.translationY(0f)
                        ?.alpha(1f)
                        ?.setDuration(300)
                        ?.setInterpolator(android.view.animation.OvershootInterpolator(0.8f))
                        ?.start()
                }
            } else {
                // Collapse: slide up, then hide
                logScrollView?.let { sv ->
                    val slideDistance = sv.height.toFloat().coerceAtLeast(200f)
                    sv.animate()
                        ?.translationY(-slideDistance)
                        ?.alpha(0f)
                        ?.setDuration(250)
                        ?.setInterpolator(android.view.animation.AccelerateInterpolator())
                        ?.withEndAction {
                            sv.visibility = View.GONE
                            sv.translationY = 0f
                            sv.alpha = 1f
                        }
                        ?.start()
                }
                logDivider?.visibility = View.GONE
            }
            toggleBtn?.setImageResource(if (expanded) R.drawable.ic_collapse_log else R.drawable.ic_expand_log)

            // Toggle the CARD's layout params.
            // When expanded: height=0dp + weight=1 → card takes its share of screen.
            // When collapsed: height=wrap_content + weight=0 → card shrinks to just
            // the header bar height, giving all freed space to the settings scroll.
            logCard?.let { card ->
                val params = card.layoutParams as android.widget.LinearLayout.LayoutParams
                if (expanded) {
                    params.height = 0
                    params.weight = 1f
                } else {
                    params.height = android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
                    params.weight = 0f
                }
                card.layoutParams = params
            }
        }
        // Initialize from companion state (default: expanded)
        applyLogExpandedState(isLogExpanded)

        val toggleLog = {
            isLogExpanded = !isLogExpanded
            applyLogExpandedState(isLogExpanded)
        }
        toggleBtn?.setOnClickListener { toggleLog() }

        // Pull (drag down) / Push (drag up) on the log header to toggle expand/collapse.
        // Simplified: no translationY visual feedback (caused bouncing/stuck).
        // Instead, the expand/collapse itself is animated via alpha fade on the ScrollView.
        //   - ACTION_DOWN: record start
        //   - ACTION_MOVE: if vertical drag > 32px, trigger toggle + reset start
        //   - ACTION_UP: if small movement, treat as tap → toggle
        logHeader?.setOnTouchListener { _, event ->
            when (event.actionMasked) {
                android.view.MotionEvent.ACTION_DOWN -> {
                    lastLogDragStartX = event.rawX
                    lastLogDragStartY = event.rawY
                    true
                }
                android.view.MotionEvent.ACTION_MOVE -> {
                    val dy = event.rawY - lastLogDragStartY
                    val absDy = Math.abs(dy)
                    val absDx = Math.abs(event.rawX - lastLogDragStartX)
                    if (absDy > absDx && absDy > 32f) {
                        if (dy < 0) {
                            if (isLogExpanded) toggleLog()
                        } else {
                            if (!isLogExpanded) toggleLog()
                        }
                        lastLogDragStartY = event.rawY
                        lastLogDragStartX = event.rawX
                    }
                    true
                }
                android.view.MotionEvent.ACTION_UP -> {
                    val dy = Math.abs(event.rawY - lastLogDragStartY)
                    val dx = Math.abs(event.rawX - lastLogDragStartX)
                    if (dy < 32f && dx < 32f) {
                        toggleLog()
                    }
                    true
                }
                else -> false
            }
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
            findViewById<android.widget.EditText>(R.id.editTextOutput)
                .setText(resolvedPath)
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
    //  Execution — Build to OTA ZIP
    // ═══════════════════════════════════════════════════════════════

    override fun onBackPressed() {
        if (isBuilding) {
            MaterialAlertDialogBuilder(this)
                .setTitle("Build in Progress")
                .setMessage("The build operation is running in the background " +
                    "and will continue even if you leave the app.")
                .setPositiveButton("Stay", null)
                .show()
        } else {
            super.onBackPressed()
        }
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
                val buildStartTime = System.currentTimeMillis()
                val heartbeatJob = CoroutineScope(kotlinx.coroutines.Dispatchers.Main).launch {
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
                    val result = OTABridge.dd(
                        images = images,
                        device = deviceValue,
                        compression = selectedCompression,
                        level = selectedCompressionLevel,
                        outputPath = outPath,
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
                                        for (j in 0 until partitionCount) {
                                            partitionProgress[j] = 100
                                        }
                                        currentPartitionIndex = partitionCount - 1
                                    }
                                    pIdx in 0 until partitionCount -> {
                                        // Use partitionPercent for per-partition bar fill
                                        partitionProgress[pIdx] = progress.partitionPercent
                                        currentPartitionIndex = pIdx
                                        // Mark all previous partitions as complete
                                        for (j in 0 until pIdx) {
                                            if (partitionProgress[j] < 100) partitionProgress[j] = 100
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
                                savedLogText.append(line)  // always persist
                                line
                            } else {
                                null
                            }

                            // Update UI progress bars and log (only if Activity is alive)
                            val current = activityRef?.get()
                            if (current != null && !current.isFinishing && !current.isDestroyed) {
                                current.runOnUiThread {
                                    // Update split progress bars
                                    val container = current.findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
                                    val barRow = container?.findViewWithTag<android.widget.LinearLayout>("bar_row")
                                    if (barRow != null && barRow.childCount == partitionCount) {
                                        for (i in 0 until partitionCount) {
                                            val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                                            bar?.let {
                                                it.isIndeterminate = false
                                                it.progress = partitionProgress[i]
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
                            savedLogText.append(logLine)

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

    /**
     * Handle build result — updates UI with success/failure status.
     */
    private fun handleBuildResult(success: Boolean, output: String, error: String?, durationMs: Long) {
        isExecuting = false
        setUIExecuting(false)

        if (success) {
            val duration = if (durationMs < 60000) "${durationMs / 1000}s"
                else "${durationMs / 60000}m ${durationMs % 60000 / 1000}s"
            // Show 100% progress bar briefly before switching to completion notification
            showProgressNotification("Build complete!", 100)
            showCompletionNotification(true, "Finished in $duration")
        } else {
            showLog("${error ?: "Unknown error"}", LogLevel.ERROR)
            showCompletionNotification(false, error ?: "Unknown error")
        }
    }

    // ═══════════════════════════════════════════════════
    //  UI Updates
    // ═══════════════════════════════════════════════════════════════

    private fun updateImageListUI() {
        val container = findViewById<android.widget.LinearLayout>(R.id.containerImageList)
        val removeButton = findViewById<View>(R.id.buttonRemoveAll)

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
                    setPadding(8, 4, 4, 4)
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
                    // FIX: Use explicit color resource instead of runtime attribute resolution.
                    // The previous code used context.theme.resolveAttribute(android.R.attr.textColorSecondary, tv, true)
                    // which could return 0 or an invalid color in some theme configurations, making
                    // the text invisible. Using ContextCompat.getColor with an explicit color resource
                    // guarantees the text is always visible.
                    setTextColor(
                        androidx.core.content.ContextCompat.getColor(
                            this@MainActivity,
                            if (isLoading) R.color.partition_text_loading else R.color.partition_text
                        )
                    )
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
                    insetTop = 0
                    insetBottom = 0
                    minimumWidth = 0
                    minWidth = 0
                    setPadding(dpToPx(8), 0, dpToPx(8), 0)
                    background = null
                    // Tint icon with colorError (red) so the delete action is
                    // visually distinct. colorError resolves correctly across
                    // all themes (default teal, Suisei Blue, Material You).
                    iconTint = android.content.res.ColorStateList.valueOf(
                        androidx.core.content.ContextCompat.getColor(
                            this@MainActivity, R.color.status_error
                        )
                    )
                    layoutParams = android.widget.LinearLayout.LayoutParams(
                        android.widget.LinearLayout.LayoutParams.WRAP_CONTENT,
                        android.widget.LinearLayout.LayoutParams.WRAP_CONTENT
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

        // Update Build button enabled state
        updateBuildButtonState()
    }

    /**
     * Disable the Build OTA Now button when required fields are empty.
     * Required: at least one partition image AND a device codename must be set.
     * The codename is mandatory for the flasher script's device verification
     * step in custom recovery (TWRP/OrangeFox).
     */
    private fun updateBuildButtonState() {
        val btnExecute = findViewById<com.google.android.material.floatingactionbutton.ExtendedFloatingActionButton>(R.id.buttonExecute)
        val device = findViewById<com.google.android.material.textfield.TextInputEditText>(R.id.editTextDevice)
            ?.text?.toString()?.trim() ?: ""
        // Don't allow Build if any partition is still loading (placeholder)
        val anyLoading = imageFiles.any { it.second.startsWith("loading:") }
        val canBuild = imageFiles.isNotEmpty() && !anyLoading && device.isNotEmpty() && !isBuilding
        btnExecute?.isEnabled = canBuild
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
        activityRef = WeakReference(this)
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
                    val savedProgress = partitionProgress.copyOf()
                    val savedIndex = currentPartitionIndex
                    setupSplitProgressBar(partitionNames)
                    savedProgress.copyInto(partitionProgress)
                    currentPartitionIndex = savedIndex
                    val barRow = findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
                        ?.findViewWithTag<android.widget.LinearLayout>("bar_row")
                    if (barRow != null) {
                        for (i in 0 until partitionCount) {
                            val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                            if (bar != null) {
                                bar.isIndeterminate = false
                                bar.progress = partitionProgress[i]
                            }
                        }
                    }
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
                for (i in 0 until partitionCount) {
                    partitionProgress[i] = 100
                }

                // Re-render progress bars at 100% if visible
                val barRow = findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
                    ?.findViewWithTag<android.widget.LinearLayout>("bar_row")
                if (barRow != null && barRow.childCount == partitionCount) {
                    for (i in 0 until partitionCount) {
                        val bar = barRow.getChildAt(i) as? com.google.android.material.progressindicator.LinearProgressIndicator
                        bar?.let {
                            it.isIndeterminate = false
                            it.progress = 100
                        }
                    }
                }

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
    }

    override fun onPause() {
        super.onPause()
        activityRef = null
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

    private fun dpToPx(dp: Int): Int = (dp * resources.displayMetrics.density).toInt()

    private fun setUIExecuting(executing: Boolean) {
        runOnUiThread {
            val btnExecute = findViewById<com.google.android.material.floatingactionbutton.ExtendedFloatingActionButton>(R.id.buttonExecute)
            btnExecute?.text = if (executing) "BUILDING OTA..." else getString(R.string.button_repack)
            btnExecute?.isEnabled = !executing
            updateBuildButtonState()
            val container = findViewById<android.widget.LinearLayout>(R.id.progressBarContainer)
            if (executing) {
                container?.visibility = View.VISIBLE
            } else {
                container?.visibility = View.GONE
                container?.removeAllViews()
                partitionCount = 0
                partitionProgress = IntArray(0)
                currentPartitionIndex = -1
                partitionNames = emptyList()
            }
            findViewById<View>(R.id.buttonAddImages)?.isEnabled = !executing
            findViewById<View>(R.id.buttonRemoveAll)?.isEnabled = !executing
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Log Level System
    // ═══════════════════════════════════════════════════════════════

    enum class LogLevel(val tag: String, val colorRes: Int) {
        DEBUG("DEBUG", R.color.log_debug),
        INFO("INFO", R.color.log_info),
        WARN("WARN", R.color.log_warning),
        ERROR("ERR ", R.color.log_error),
        SUCCESS("OK  ", R.color.log_success),
        PLAIN("", 0),
    }

    /**
     * UI-only log append — does NOT persist to savedLogText.
     * Caller must persist separately (via savedLogText.append or showLog).
     * Must be called on the UI thread (wrap in runOnUiThread).
     *
     * Used by build callbacks (onOutputLine, onProgress) to decouple
     * persistence (always runs) from UI update (only if Activity alive).
     * Fix K1+K3: previously callbacks called showLog() which is gated by
     * activityRef, losing both persist AND UI when app was backgrounded.
     */
    private fun appendLogLineUI(line: String, level: LogLevel = LogLevel.PLAIN) {
        val textView = findViewById<android.widget.TextView>(R.id.textViewLog) ?: return

        if (level == LogLevel.PLAIN) {
            textView.append(line)
        } else {
            val sdf = java.text.SimpleDateFormat("HH:mm:ss", java.util.Locale.US)
            val timestamp = sdf.format(java.util.Date())
            val prefix = "[$timestamp] [${level.tag}] "
            val colored = SpannableString("$prefix$line")
            try {
                colored.setSpan(
                    ForegroundColorSpan(ContextCompat.getColor(this, level.colorRes)),
                    0, prefix.length, SpannableString.SPAN_EXCLUSIVE_EXCLUSIVE
                )
            } catch (_: Exception) { /* fallback to plain */ }
            textView.append(colored)
        }

        // Scroll to bottom WITHOUT triggering parent NestedScrollView
        val scrollView = findViewById<android.widget.ScrollView>(R.id.scrollViewLog)
        scrollView?.post {
            val child = scrollView.getChildAt(0)
            if (child != null) {
                val target = child.bottom - scrollView.height
                scrollView.smoothScrollTo(0, if (target > 0) target else 0)
            }
        }
    }

    private fun showLog(text: String, level: LogLevel = LogLevel.INFO) {
        val line = if (text.endsWith("\n")) text else "$text\n"
        // Persist to companion object (survives Activity recreation)
        savedLogText.append(line)

        runOnUiThread {
            appendLogLineUI(line, level)
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
