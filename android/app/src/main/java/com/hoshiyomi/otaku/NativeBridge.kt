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
 * All compression algorithms (gzip, bzip2, xz, brotli) are always available
 * because they're statically compiled into the Rust .so.
 */
object NativeBridge {

    private const val TAG = "NativeBridge"

    /** Whether the native library was loaded successfully. */
    @Volatile
    var isLoaded: Boolean = false
        private set

    /** Error message if native library failed to load. */
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
    //  Public API
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
                available = listOf("none"),
                missing = listOf("gzip", "bzip2", "xz", "brotli"),
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
                available = listOf("none"),
                missing = listOf("gzip", "bzip2", "xz", "brotli"),
                allOk = false,
                nativeVersion = "error"
            )
        }
    }

    /**
     * Build a DD-mode flashable ZIP from partition images.
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param compression Compression algorithm: "none", "gzip", "bzip2", "xz", "brotli"
     * @param level Compression level (0 = default per algorithm)
     * @param outputPath Absolute path for output .zip file
     * @param device Device codename(s), comma-separated
     * @param skipVerify Skip post-flash SHA-256 verification
     * @return OTAResult with success/error info
     */
    fun buildDd(
        images: Map<String, String>,
        compression: String = "gzip",
        level: Int = 0,
        outputPath: String,
        device: String = "generic",
        skipVerify: Boolean = false
    ): OTAResult {
        if (!isLoaded) {
            return OTAResult.error("Native library not loaded: $loadError")
        }
        return try {
            val imagesJson = JSONObject(images).toString()
            val resultJson = nativeBuildDd(
                imagesJson, compression, level, outputPath, device,
                if (skipVerify) 1 else 0
            )
            parseOtaResult(resultJson)
        } catch (e: Exception) {
            OTAResult.error("Native build failed: ${e.message}")
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Result parsing
    // ═══════════════════════════════════════════════════════════════

    data class DepCheckResult(
        val available: List<String>,
        val missing: List<String>,
        val allOk: Boolean,
        val nativeVersion: String
    )

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

    private fun parseOtaResult(jsonStr: String): OTAResult {
        val json = JSONObject(jsonStr)
        return if (json.optBoolean("success", false)) {
            OTAResult.success(
                json.optString("output", ""),
                json.optLong("duration_ms", 0)
            )
        } else {
            OTAResult.error(
                json.optString("error", "Unknown error"),
                json.optLong("duration_ms", 0)
            )
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  JNI external declarations
    // ═══════════════════════════════════════════════════════════════

    private external fun nativeGetVersion(): String
    private external fun nativeCheckDeps(): String
    private external fun nativeBuildDd(
        imagesJson: String,
        compression: String,
        level: Int,
        outputPath: String,
        device: String,
        skipVerify: Int
    ): String
}
