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
    val percent: Int,
    val partitionPercent: Int = 0
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

        // Start progress polling coroutine — reads the .progress sidecar file
        // written by Rust every 4MB chunk during compression.
        //
        // Rust updates the sidecar with:
        //   - partition_percent: 0-100 for the current partition being compressed
        //   - overall_percent: weighted across all partitions (0-94%)
        //   - current, total, name, phase: partition tracking info
        //
        // This gives smooth, real-time progress instead of 0→100% jumps.
        val progressJob = CoroutineScope(Dispatchers.IO).launch {
            var lastOverallPercent = -1
            var lastPhase = ""
            var lastName = ""

            while (isActive) {
                delay(500) // poll every 500ms
                try {
                    if (!progressFile.exists()) continue

                    val content = progressFile.readText().trim()
                    if (content.isEmpty()) continue

                    try {
                        val json = org.json.JSONObject(content)
                        val current = json.optInt("current", 0)
                        val total = json.optInt("total", 0)
                        val name = json.optString("name", "")
                        val phase = json.optString("phase", "")
                        val partitionPercent = json.optInt("partition_percent", 0)
                        val overallPercent = json.optInt("overall_percent", 0)

                        // Compute the display percentage:
                        // - During compression: use Rust's overall_percent (0-94%)
                        // - Scripts phase: 95%
                        // - ZIP writing phase: 97%
                        val displayPercent = when {
                            phase == "writing_zip" -> 97
                            phase == "building_scripts" -> 95
                            phase == "compressed" -> {
                                // Partition just finished — show 94% overall
                                (current * 100 / (total.coerceAtLeast(1))).coerceAtMost(94)
                            }
                            else -> overallPercent.coerceIn(0, 94)
                        }

                        // Only emit if something changed
                        if (displayPercent != lastOverallPercent || name != lastName || phase != lastPhase) {
                            lastOverallPercent = displayPercent
                            lastName = name
                            lastPhase = phase

                            // Build progress message based on phase and partition info
                            // Message is clean (no percentage) — percentage is passed separately
                            // via partitionPercent field so UI can use it for per-partition bars.
                            val message = when (phase) {
                                "compressing" -> {
                                    if (name.isNotEmpty()) {
                                        "Compressing $name"
                                    } else {
                                        "Compressing…"
                                    }
                                }
                                "compressed" -> if (name.isNotEmpty()) "Compressed $name" else "Compressing…"
                                "building_scripts" -> "Building flasher scripts"
                                "writing_zip" -> "Writing ZIP file"
                                else -> if (name.isNotEmpty()) "Processing $name" else "Building…"
                            }

                            // partitionPercent: per-partition compression progress (0-100)
                            // percent (displayPercent): overall build progress (0-97)
                            val pPct = when {
                                phase == "writing_zip" -> 100
                                phase == "building_scripts" -> 100
                                phase == "compressed" -> 100
                                else -> partitionPercent
                            }

                            onProgress?.invoke(ProgressUpdate(
                                current = current,
                                total = total,
                                message = message,
                                percent = displayPercent,
                                partitionPercent = pPct
                            ))
                        }
                    } catch (_: Exception) {
                        // JSON parse error — ignore, try again next poll
                    }
                } catch (_: Exception) {
                    // Progress polling is non-critical — ignore all errors
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
