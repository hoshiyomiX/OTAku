package com.hoshiyomi.otaku

import android.util.Log
import org.json.JSONObject

/**
 * NativeBridge — Kotlin interface to the Rust native backend (libotaku_native.so).
 *
 * Replaces the entire Python runtime (PythonBridge + PyBridge + pybridge.c) with
 * direct JNI calls to a cargo-ndk compiled Rust library.
 *
 * Architecture:
 *   Kotlin → JNI → libotaku_native.so (Rust, statically links all compression)
 *
 * No Python, no dlopen, no LD_PRELOAD, no ELF manipulation.
 * All compression algorithms (zstd, xz, bzip2, gzip, lz4) are always available
 * because they're statically compiled into the Rust .so.
 */
object NativeBridge {

    private const val TAG = "NativeBridge"

    /** Whether the native library was loaded successfully. */
    @Volatile
    var isLoaded: Boolean = false
        private set

    /** Error message if native library failed to load. */
    @Volatile
    var loadError: String? = null
        private set

    init {
        try {
            System.loadLibrary("otaku_native")
            isLoaded = true
            Log.d(TAG, "libotaku_native.so loaded successfully")
        } catch (e: UnsatisfiedLinkError) {
            loadError = e.message
            Log.e(TAG, "Failed to load libotaku_native.so: ${e.message}")
        } catch (e: Exception) {
            loadError = e.message
            Log.e(TAG, "Exception loading libotaku_native.so: ${e.message}")
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Version & Dependencies
    // ═══════════════════════════════════════════════════════════════

    /**
     * Get the native library version string.
     *
     * @return Version string like "otaku-native 3.1.0 (rust)" or error message
     */
    fun getVersion(): String {
        if (!isLoaded) return "native library not loaded: $loadError"
        return try {
            nativeGetVersion()
        } catch (e: Exception) {
            "error: ${e.message}"
        }
    }

    /**
     * Check which compression algorithms are available.
     *
     * With Rust static linking, ALL algorithms are always available.
     * This method exists for API compatibility and diagnostic logging.
     *
     * @return DepCheckResult with all algorithms marked as available
     */
    fun checkDeps(): DepCheckResult {
        if (!isLoaded) {
            return DepCheckResult(
                available = listOf("gzip"),
                missing = listOf("zstd", "xz", "bzip2", "lz4"),
                allOk = false,
                nativeVersion = "not loaded"
            )
        }
        return try {
            val jsonStr = nativeCheckDeps()
            parseDepCheckResult(jsonStr)
        } catch (e: Exception) {
            Log.e(TAG, "checkDeps failed: ${e.message}")
            DepCheckResult(
                available = listOf("gzip"),
                missing = listOf("zstd", "xz", "bzip2", "lz4"),
                allOk = false,
                nativeVersion = "error"
            )
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  DD Build (Phase 3 — full implementation)
    // ═══════════════════════════════════════════════════════════════

    /**
     * Build a DD-mode flashable ZIP from partition images.
     *
     * Generates a flashable ZIP containing:
     *   - otaku.bin (DDBU header + compressed partition data)
     *   - META-INF/com/google/android/update-binary (TWRP/OrangeFox flasher)
     *   - META-INF/com/google/android/updater-script (stub)
     *   - flash_info.txt (human-readable metadata)
     *
     * Progress is reported via a sidecar file at `<output_path>.progress`
     * that Kotlin polls every 500ms. This avoids JNI callback complexity.
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param compression Compression algorithm: "zstd", "xz", "bzip2", "gzip", "lz4"
     * @param level Compression level (0 = default per algorithm)
     * @param outputPath Absolute path for output .zip file
     * @param device Device codename(s), comma-separated
     * @param skipVerify Skip post-flash SHA-256 verification
     * @return DdBuildResult with success/error, paths, sizes
     */
    fun buildDd(
        images: Map<String, String>,
        compression: String = "gzip",
        level: Int = 0,
        outputPath: String,
        device: String = "generic",
        skipVerify: Boolean = false,
        romName: String = "",
        maker: String = ""
    ): DdBuildResult {
        if (!isLoaded) {
            return DdBuildResult.error("Native library not loaded: $loadError")
        }
        Log.d(TAG, "buildDd() images=${images.keys}, compression=$compression, level=$level, output=$outputPath, device=$device, skipVerify=$skipVerify, romName=$romName, maker=$maker")

        return try {
            val imagesJson = JSONObject(images).toString()
            val resultJson = nativeBuildDd(
                imagesJson, compression, level, outputPath, device,
                skipVerify, romName, maker
            )
            val result = parseDdBuildResult(resultJson)
            Log.d(TAG, "buildDd() result: success=${result.success}, zip_path=${result.zipPath}, duration=${result.durationMs}ms")
            result
        } catch (e: Exception) {
            Log.e(TAG, "buildDd() failed: ${e.message}")
            DdBuildResult.error("Native build failed: ${e.message}")
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Device codename detection (spoof-resistant)
    // ═══════════════════════════════════════════════════════════════

    /**
     * Detect device codename from vendor partition properties.
     *
     * Reads 4 sources (getprop + /vendor/build.prop for both
     * ro.product.vendor.device and ro.product.board). If the two values
     * differ, returns BOTH as comma-separated string — matches the
     * flasher script's comma-separated TARGET_DEVICE format.
     *
     * Spoof-resistant because vendor partition is rarely modified by
     * Magisk/GSI/LineageOS (which typically only touch /system).
     *
     * @return DeviceCodenameResult with codename (or empty + error if all sources empty)
     */
    fun detectDeviceCodename(): DeviceCodenameResult {
        if (!isLoaded) {
            return DeviceCodenameResult.error("Native library not loaded: $loadError")
        }
        return try {
            val resultJson = nativeDetectDeviceCodename()
            parseDeviceCodenameResult(resultJson)
        } catch (e: Exception) {
            Log.e(TAG, "detectDeviceCodename() failed: ${e.message}")
            DeviceCodenameResult.error("Native detect failed: ${e.message}")
        }
    }

    data class DeviceCodenameResult(
        val success: Boolean,
        val codename: String = "",
        val vendorDevice: String = "",
        val board: String = "",
        val sourcesTried: List<String> = emptyList(),
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = DeviceCodenameResult(success = false, error = msg)
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Device partition scanner (no root — getprop based)
    // ═══════════════════════════════════════════════════════════════

    /**
     * Scan device for supported partition names.
     *
     * Returns a list of partition names that this device supports, based on
     * getprop queries (ro.boot.dynamic_partitions, ro.boot.slot_suffix,
     * ro.build.version.release). No root required.
     *
     * The app uses this list to validate user-picked .img files: if the
     * filename (minus .img) does not match any partition in this list, the
     * app refuses to load it and prints a warning. This prevents the user
     * from accidentally renaming system.img to vendor.img (which would brick
     * the device when flashed to the wrong partition).
     *
     * @return DevicePartitionsResult with list of supported partitions
     */
    fun scanDevicePartitions(): DevicePartitionsResult {
        if (!isLoaded) {
            return DevicePartitionsResult.error("Native library not loaded: $loadError")
        }
        return try {
            val resultJson = nativeScanDevicePartitions()
            parseDevicePartitionsResult(resultJson)
        } catch (e: Exception) {
            Log.e(TAG, "scanDevicePartitions() failed: ${e.message}")
            DevicePartitionsResult.error("Native scan failed: ${e.message}")
        }
    }

    data class DevicePartitionsResult(
        val success: Boolean,
        val partitions: List<String> = emptyList(),
        val dynamicPartitions: Boolean = false,
        val slotSuffix: String = "",
        val androidVersion: String = "unknown",
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = DevicePartitionsResult(success = false, error = msg)
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Result data classes
    // ═══════════════════════════════════════════════════════════════

    data class DepCheckResult(
        val available: List<String>,
        val missing: List<String>,
        val allOk: Boolean,
        val nativeVersion: String
    )

    /**
     * Result of a DD build operation (Phase 3).
     *
     * Contains the output log, ZIP path and sizes, and error info.
     */
    data class DdBuildResult(
        val success: Boolean,
        val output: String = "",
        val zipPath: String? = null,
        val zipSize: Long? = null,
        val bundleSize: Long? = null,
        /** Total uncompressed size of all partition images.
         *  Used by the flasher script for pre-flash free space verification. */
        val totalUncSize: Long? = null,
        val error: String? = null,
        val durationMs: Long = 0
    ) {
        companion object {
            fun error(msg: String) = DdBuildResult(success = false, error = msg)
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Result parsing
    // ═══════════════════════════════════════════════════════════════

    private fun parseDepCheckResult(jsonStr: String): DepCheckResult {
        val json = JSONObject(jsonStr)
        val available = json.optJSONArray("available")?.let {
            (0 until it.length()).map { i -> it.getString(i) }
        } ?: emptyList()
        val missing = json.optJSONArray("missing")?.let {
            (0 until it.length()).map { i -> it.getString(i) }
        } ?: emptyList()
        return DepCheckResult(
            available = available,
            missing = missing,
            allOk = json.optBoolean("all_ok", false),
            nativeVersion = json.optString("native_version", "unknown")
        )
    }

    private fun parseDdBuildResult(jsonStr: String): DdBuildResult {
        val json = JSONObject(jsonStr)
        return if (json.optBoolean("success", false)) {
            DdBuildResult(
                success = true,
                output = json.optString("output", ""),
                zipPath = json.optString("zip_path", null),
                zipSize = if (json.has("zip_size")) json.optLong("zip_size") else null,
                bundleSize = if (json.has("bundle_size")) json.optLong("bundle_size") else null,
                totalUncSize = if (json.has("total_unc_size")) json.optLong("total_unc_size") else null,
                error = null,
                durationMs = json.optLong("duration_ms", 0)
            )
        } else {
            DdBuildResult(
                success = false,
                output = json.optString("output", ""),
                error = json.optString("error", "Unknown error"),
                durationMs = json.optLong("duration_ms", 0)
            )
        }
    }

    private fun parseDeviceCodenameResult(jsonStr: String): DeviceCodenameResult {
        val json = JSONObject(jsonStr)
        val sourcesArray = json.optJSONArray("sources_tried")
        val sources = mutableListOf<String>()
        if (sourcesArray != null) {
            for (i in 0 until sourcesArray.length()) {
                sources.add(sourcesArray.optString(i, ""))
            }
        }
        return DeviceCodenameResult(
            success = json.optBoolean("success", false),
            codename = json.optString("codename", ""),
            vendorDevice = json.optString("vendor_device", ""),
            board = json.optString("board", ""),
            sourcesTried = sources,
            error = if (json.has("error") && !json.isNull("error")) json.optString("error") else null
        )
    }

    private fun parseDevicePartitionsResult(jsonStr: String): DevicePartitionsResult {
        val json = JSONObject(jsonStr)
        val partitionsArray = json.optJSONArray("partitions")
        val partitions = mutableListOf<String>()
        if (partitionsArray != null) {
            for (i in 0 until partitionsArray.length()) {
                partitions.add(partitionsArray.optString(i, ""))
            }
        }
        return DevicePartitionsResult(
            success = json.optBoolean("success", false),
            partitions = partitions,
            dynamicPartitions = json.optBoolean("dynamic_partitions", false),
            slotSuffix = json.optString("slot_suffix", ""),
            androidVersion = json.optString("android_version", "unknown"),
            error = if (json.has("error") && !json.isNull("error")) json.optString("error") else null
        )
    }

    // ═══════════════════════════════════════════════════════════════
    //  JNI external declarations
    // ═══════════════════════════════════════════════════════════════

    // Version & Dependencies
    private external fun nativeGetVersion(): String
    private external fun nativeCheckDeps(): String

    // DD Build (Phase 3)
    // Rust signature: nativeBuildDd(images_json, compression, level, output_path, device, skip_verify: jboolean)
    // jboolean maps to Kotlin Boolean (not Int)
    private external fun nativeBuildDd(
        imagesJson: String,
        compression: String,
        level: Int,
        outputPath: String,
        device: String,
        skipVerify: Boolean,
        romName: String,
        maker: String
    ): String

    // Device codename detection (spoof-resistant — reads vendor partition props)
    // Rust signature: nativeDetectDeviceCodename() -> jstring (JSON)
    private external fun nativeDetectDeviceCodename(): String

    // Device partition scanner (no root — getprop based)
    // Rust signature: nativeScanDevicePartitions() -> jstring (JSON)
    private external fun nativeScanDevicePartitions(): String
}
