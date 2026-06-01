package com.hoshiyomi.otaku

import android.os.Process
import kotlinx.coroutines.Dispatchers
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
 * OTABridge — Kotlin singleton that bridges the Android UI to otaku.pyz.
 *
 * This app is DD-mode only: generates otaku-format flashable ZIPs
 * from partition images (.img) for TWRP/OrangeFox recovery flashing.
 *
 * Supported compression: none, gzip, bzip2, xz, brotli
 */
object OTABridge {

    // Compression algorithm choices exposed in the UI spinner
    // Ordered by compression ratio: worst (none) → best (brotli)
    val COMPRESSION_ALGORITHMS = listOf("none", "gzip", "bzip2", "xz", "brotli")

    // All valid compression values (for validation)
    val ALL_COMPRESSION = setOf("none", "gzip", "bzip2", "xz", "brotli")


    // Compression level ranges per algorithm: (min, max, default)
    // Ranges match Python compression.py LEVEL_RANGES and DEFAULT_LEVELS.
    val COMPRESS_LEVELS = mapOf(
        "none" to Triple(0, 0, 0),
        "gzip" to Triple(1, 9, 6),
        "bzip2" to Triple(1, 9, 9),
        "xz" to Triple(0, 9, 6),
        "brotli" to Triple(0, 11, 6)
    )


    /**
     * Execute otaku.pyz with the given CLI arguments.
     * PythonBridge.executePyz auto-configures the environment based on
     * whether Python is bundled or system (Termux).
     */
    private suspend fun executePyz(args: List<String>, onProgress: ((ProgressUpdate) -> Unit)? = null, onOutputLine: ((String) -> Unit)? = null): OTAResult {
        return withContext(Dispatchers.IO) {
            // Boost thread priority for CPU-intensive build operations.
            // THREAD_PRIORITY_DEFAULT=0, negative = higher priority.
            // -10 gives ~80% CPU allocation (substantial boost over default).
            try {
                Process.setThreadPriority(Process.myTid(), -10)
            } catch (_: Exception) {}

            val execResult = PythonBridge.executePyz(args, onProgress, onOutputLine)

            if (execResult.success) {
                OTAResult.success(execResult.output, execResult.durationMs)
            } else {
                OTAResult.error(
                    message = execResult.error ?: "Exit code ${execResult.exitCode}",
                    durationMs = execResult.durationMs
                ).copy(output = execResult.output)
            }
        }
    }

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
     * ZIP creation. Falls back to PythonBridge if native library is not loaded.
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

        // Prefer Rust native backend (faster, no Python dependency)
        if (NativeBridge.isLoaded) {
            return withContext(Dispatchers.IO) {
                try {
                    Process.setThreadPriority(Process.myTid(), -10)
                } catch (_: Exception) {}

                val ddResult = NativeBridge.buildDd(
                    images = images,
                    compression = compression,
                    level = level,
                    outputPath = outputPath,
                    device = device.ifEmpty { "generic" },
                    skipVerify = skipVerify
                )

                if (ddResult.success) {
                    OTAResult.success(ddResult.output, ddResult.durationMs)
                } else {
                    OTAResult.error(
                        ddResult.error ?: "Native build failed",
                        ddResult.durationMs
                    ).copy(output = ddResult.output)
                }
            }
        }

        // Fallback: Python backend (legacy, for devices without native .so)
        val args = mutableListOf("dd")
        for ((name, path) in images) {
            args.add("--image"); args.add(path)
            args.add("--partition"); args.add(name)
        }
        args.add("-o")
        args.add(outputPath)
        if (compression != "gzip") {
            args.add("--compress")
            args.add(compression)
        }
        if (level > 0) {
            args.add("--compress-level")
            args.add(level.toString())
        }
        if (skipVerify) {
            args.add("--skip-verify")
        }
        args.add("--device")
        args.add(device.ifEmpty { "generic" })
        return executePyz(args, onProgress, onOutputLine)
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

    suspend fun getPyzVersion(): String? {
        return withContext(Dispatchers.IO) {
            try {
                val result = PythonBridge.executePyz(listOf("--version"))
                if (result.success) result.output.trim() else null
            } catch (_: Exception) { null }
        }
    }
}
