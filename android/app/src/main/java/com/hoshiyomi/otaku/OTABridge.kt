package com.hoshiyomi.otaku

import android.os.Process
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File

/**
 * OTAResult — structured result from an OTAku operation.
 */
data class OTAResult(
    val success: Boolean,
    val output: String,
    val error: String? = null,
    val exitCode: Int = 0,
    val durationMs: Long = 0
) {
    val hasError: Boolean get() = !success || !error.isNullOrBlank()

    companion object {
        fun error(message: String, durationMs: Long = 0) = OTAResult(
            success = false, output = "", error = message, exitCode = -1, durationMs = durationMs
        )
        fun success(output: String, durationMs: Long = 0) = OTAResult(
            success = true, output = output, error = null, exitCode = 0, durationMs = durationMs
        )
    }
}

/**
 * ProgressUpdate — progress callback data for OTA build operations.
 *
 * Used by OTABridge.dd() and OTAService to report real-time progress
 * from the Rust native backend (NativeBridge).
 */
data class ProgressUpdate(
    val current: Int,
    val total: Int,
    val message: String,
    val percent: Int
)

/**
 * OTABridge — Kotlin singleton that bridges the Android UI to the Rust native backend.
 *
 * This app is DD-mode only: generates otaku-format flashable ZIPs
 * from partition images (.img) for TWRP/OrangeFox recovery flashing.
 *
 * Supported compression: none, gzip, bzip2, xz, brotli
 *
 * All operations use NativeBridge (Rust libotaku_native.so) — no Python dependency.
 */
object OTABridge {

    private const val TAG = "OTABridge"

    // Compression algorithm choices exposed in the UI spinner
    // Ordered by compression ratio: worst (none) → best (brotli)
    val COMPRESSION_ALGORITHMS = listOf("none", "gzip", "bzip2", "xz", "brotli")

    // All valid compression values (for validation)
    val ALL_COMPRESSION = setOf("none", "gzip", "bzip2", "xz", "brotli")


    // Compression level ranges per algorithm: (min, max, default)
    // Ranges match the Rust native backend LEVEL_RANGES and DEFAULT_LEVELS.
    val COMPRESS_LEVELS = mapOf(
        "none" to Triple(0, 0, 0),
        "gzip" to Triple(1, 9, 6),
        "bzip2" to Triple(1, 9, 9),
        "xz" to Triple(0, 9, 6),
        "brotli" to Triple(0, 11, 6)
    )

    // ═══════════════════════════════════════════════════════════════
    //  Core operation — DD mode only
    // ═══════════════════════════════════════════════════════════════

    /**
     * DD mode — Generate a dd-based flashable ZIP (otaku format).
     *
     * Produces a flashable ZIP with:
     *   - otaku.bin (compressed partition images)
     *   - META-INF/com/google/android/update-binary (TWRP/OrangeFox flasher script)
     *   - META-INF/com/google/android/updater-script (stub)
     *   - flash_info.txt (human-readable metadata)
     *
     * Uses the Rust native backend (NativeBridge) for all compression and
     * ZIP creation. Requires libotaku_native.so to be loaded.
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param device Device codename(s), comma-separated (e.g. "crosshatch" or "OP11,OP11A")
     * @param compression Compression algorithm: none, gzip, bzip2, xz, or brotli
     * @param level Compression level (0 = default per algorithm)
     * @param skipVerify Skip post-flash SHA-256 hash verification
     * @param outputPath Absolute path to output .zip file
     */
    suspend fun dd(
        images: Map<String, String>,
        device: String = "generic",
        compression: String = "gzip",
        level: Int = 0,
        skipVerify: Boolean = false,
        outputPath: String,
        onProgress: ((ProgressUpdate) -> Unit)? = null,
        onOutputLine: ((String) -> Unit)? = null
    ): OTAResult {
        if (images.isEmpty()) return OTAResult.error("No images specified for DD ZIP")
        if (compression !in ALL_COMPRESSION)
            return OTAResult.error("Invalid compression: '$compression'")

        // Rust native backend required
        if (!NativeBridge.isLoaded) {
            return OTAResult.error("Native backend not loaded: ${NativeBridge.loadError}")
        }

        val effectiveDevice = device.ifEmpty { "generic" }
        val buildStartTime = System.currentTimeMillis()

        // Compute total input size for progress estimation
        val totalInputBytes = images.values.sumOf { path ->
            try { java.io.File(path).length() } catch (_: Exception) { 0L }
        }

        // Log input parameters before the JNI call
        val debugStartMsg = "[DEBUG] dd() called: ${images.size} partitions, " +
            "compression=$compression, level=$level, device=$effectiveDevice, " +
            "output=$outputPath, total_input=${formatSize(totalInputBytes)}"
        Log.d(TAG, debugStartMsg)
        onOutputLine?.invoke(debugStartMsg)

        // Delete stale progress file from previous runs
        val progressFile = java.io.File("${outputPath}.progress")
        progressFile.delete()

        // Track the temp build file that Rust creates during compression.
        // Rust writes compressed data incrementally to this file, so its size
        // grows during the build — giving us live progress data.
        // The exact path is communicated via the .progress sidecar's "tmp_path" field.
        var tmpBuildFile: java.io.File? = null

        // Estimated final output size: sum of all input image sizes.
        // Rust computes this identically and sends it via the sidecar's "total_estimated".
        var estimatedFinalSize = totalInputBytes

        // Start progress polling coroutine — uses TWO data sources:
        // 1. The .progress sidecar file for partition name / phase / tmp_path info
        // 2. The tmp build file SIZE for granular percentage calculation
        //
        // The tmp file grows incrementally during compression, giving smooth
        // progress instead of the jump-at-completion behavior from sidecar-only.
        val progressJob = CoroutineScope(Dispatchers.IO).launch {
            var lastCurrent = 0
            var lastPercent = -1
            var lastPhase = ""
            var lastName = ""
            var lastTotal = 0

            while (isActive) {
                delay(500) // poll every 500ms
                try {
                    // --- Source 1: sidecar file for partition name, phase, and tmp_path ---
                    var current = lastCurrent
                    var total = lastTotal
                    var name = lastName
                    var phase = lastPhase
                    var bytesWrittenFromSidecar = 0L

                    if (progressFile.exists()) {
                        val content = progressFile.readText().trim()
                        if (content.isNotEmpty()) {
                            try {
                                val json = org.json.JSONObject(content)
                                current = json.optInt("current", lastCurrent)
                                total = json.optInt("total", lastTotal)
                                name = json.optString("name", lastName)
                                phase = json.optString("phase", lastPhase)
                                bytesWrittenFromSidecar = json.optLong("bytes_written", 0L)

                                // Discover the temp file path from Rust
                                val tmpPath = json.optString("tmp_path", "")
                                if (tmpPath.isNotEmpty() && tmpBuildFile == null) {
                                    val discovered = java.io.File(tmpPath)
                                    if (discovered.exists()) {
                                        tmpBuildFile = discovered
                                    }
                                }

                                // Use Rust's total_estimated if available (more accurate)
                                val rustEstimated = json.optLong("total_estimated", 0L)
                                if (rustEstimated > 0) {
                                    estimatedFinalSize = rustEstimated
                                }

                                lastCurrent = current
                                lastTotal = total
                                lastName = name
                                lastPhase = phase
                            } catch (_: Exception) {
                                // Parse error — use last known values
                            }
                        }
                    }

                    // --- Source 2: tmp build file size for percentage ---
                    // The tmp file grows as Rust compresses each partition.
                    // Its size directly reflects how much data has been processed.
                    val tmpFileSize = tmpBuildFile?.let { if (it.exists()) it.length() else 0L } ?: 0L

                    // Use tmp file size if available, otherwise fall back to sidecar bytes_written
                    val bytesProgress = if (tmpFileSize > 0) {
                        tmpFileSize
                    } else {
                        bytesWrittenFromSidecar
                    }

                    // Calculate percentage: bytes processed vs estimated final size
                    // Cap at 94% during compression (reserve 5% for scripts + ZIP writing)
                    val rawPercent = if (estimatedFinalSize > 0) {
                        (bytesProgress * 100 / estimatedFinalSize).toInt()
                    } else if (total > 0) {
                        ((current - 1) * 100 / total)
                    } else {
                        0
                    }

                    val percent = when {
                        phase == "writing_zip" -> 97  // ZIP writing is near the end
                        phase == "building_scripts" -> 95  // Scripts phase
                        else -> rawPercent.coerceIn(0, 94)  // Compression phase, cap at 94%
                    }

                    // Only emit if something changed
                    if (current != lastCurrent || percent != lastPercent || phase != lastPhase) {
                        lastPercent = percent

                        // Build progress message based on phase and partition info
                        val message = when (phase) {
                            "compressing" -> if (name.isNotEmpty()) "Compressing $name" else "Compressing…"
                            "compressed" -> if (name.isNotEmpty()) "Compressing $name" else "Compressing…"
                            "building_scripts" -> "Building flasher scripts"
                            "writing_zip" -> "Writing ZIP file"
                            else -> if (name.isNotEmpty()) "Processing $name" else "Building…"
                        }

                        onProgress?.invoke(ProgressUpdate(
                            current = current,
                            total = total,
                            message = message,
                            percent = percent
                        ))
                    }
                } catch (_: Exception) {
                    // Progress polling is non-critical — ignore parse errors
                }
            }
        }

        return withContext(Dispatchers.IO) {
            try {
                Process.setThreadPriority(Process.myTid(), -10)
            } catch (_: Exception) {}

            try {
                val ddResult = NativeBridge.buildDd(
                    images = images,
                    compression = compression,
                    level = level,
                    outputPath = outputPath,
                    device = effectiveDevice,
                    skipVerify = skipVerify
                )

                // Emit all Rust output lines to the log
                ddResult.output.split("\n").forEach { line ->
                    if (line.isNotBlank()) {
                        onOutputLine?.invoke(line)
                    }
                }

                // Log result summary after the JNI call returns
                val durationMs = System.currentTimeMillis() - buildStartTime
                val zipSizeStr = ddResult.zipSize?.let { formatSize(it) } ?: "N/A"
                val bundleSizeStr = ddResult.bundleSize?.let { formatSize(it) } ?: "N/A"
                val debugEndMsg = "[DEBUG] dd() returned: success=${ddResult.success}, " +
                    "duration=${ddResult.durationMs}ms, zip_size=$zipSizeStr, " +
                    "bundle_size=$bundleSizeStr"
                Log.d(TAG, debugEndMsg)
                onOutputLine?.invoke(debugEndMsg)

                if (ddResult.success) {
                    OTAResult.success(ddResult.output, ddResult.durationMs)
                } else {
                    OTAResult.error(
                        ddResult.error ?: "Native build failed",
                        ddResult.durationMs
                    ).copy(output = ddResult.output)
                }
            } finally {
                // Always cancel progress polling and clean up
                progressJob.cancel()
                progressFile.delete()
            }
        }
    }

    /**
     * Build a smart output filename based on device codename.
     *
     * Examples:
     *   - flashable_crosshatch.zip
     *   - flashable_OP11.zip
     *
     * Falls back to "flashable_generic.zip" when no device is specified.
     */
    fun buildOutputFileName(device: String = "generic"): String {
        val safeDevice = device.replace(Regex("[^a-zA-Z0-9_\\-]"), "_").lowercase()
        return "flashable_${safeDevice}.zip"
    }

    // ═══════════════════════════════════════════════════════════════
    //  Utility methods
    // ═══════════════════════════════════════════════════════════════

    /** Format byte size as human-readable string (e.g. "45.2MB"). */
    private fun formatSize(bytes: Long): String = when {
        bytes < 1024 -> "$bytes B"
        bytes < 1024 * 1024 -> String.format("%.1f KB", bytes / 1024.0)
        bytes < 1024 * 1024 * 1024 -> String.format("%.1f MB", bytes / (1024.0 * 1024))
        else -> String.format("%.2f GB", bytes / (1024.0 * 1024 * 1024))
    }
}
