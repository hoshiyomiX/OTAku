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
    /**
     * T36: true when this result represents a deliberate user cancellation
     * (native sentinel "cancelled by user", or the coroutine-cancellation
     * path which reuses the word). Callers show a neutral banner instead
     * of an ERROR one.
     */
    val isCancelled: Boolean
        get() = error?.contains("cancel", ignoreCase = true) == true

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
 * Primary mode: DD-mode — generates otaku-format flashable ZIPs from partition
 * images (.img) for TWRP/OrangeFox recovery flashing.
 *
 * Payload.bin toolchain (prototype): inspect → extract → verify → write —
 * works on OTAku's OWN custom payload format (magic "OTKU", T27 Option B):
 * extracts partition images from a payload.bin (sidecar progress + WakeLock
 * for long runs), self-verifies generated payloads, and builds a payload.bin
 * from the loaded partition images (sidecar progress, same as extraction).
 * NOTE: real AOSP OTA payloads (magic "CrAU") are deliberately NOT
 * supported — the Rust layer rejects them with a clear error.
 *
 * Supported compression: zstd, xz, bzip2, gzip, lz4  ("none" and "brotli" excluded from user-facing options)
 *
 * All operations use NativeBridge (Rust libotaku_native.so) — no Python dependency.
 */
object OTABridge {

    private const val TAG = "OTABridge"

    // Compression algorithm choices exposed in the UI spinner
    // Ordered by compression ratio: best (zstd ~35%) → fastest (lz4 ~70%)
    // "none" removed — users should always compress OTA packages.
    val COMPRESSION_ALGORITHMS = listOf("zstd", "xz", "bzip2", "gzip", "lz4")

    // All valid compression values — derived from COMPRESSION_ALGORITHMS
    // to avoid duplicating the algorithm list. "none" excluded — users
    // should always compress OTA packages.
    val ALL_COMPRESSION: Set<String> = COMPRESSION_ALGORITHMS.toSet()


    // Compression level ranges per algorithm: (min, max, default)
    // Ranges match the Rust native backend LEVEL_RANGES and DEFAULT_LEVELS.
    val COMPRESS_LEVELS = mapOf(
        "zstd" to Triple(1, 22, 3),
        "xz" to Triple(0, 9, 6),
        "bzip2" to Triple(1, 9, 9),
        "gzip" to Triple(1, 9, 6),
        "lz4" to Triple(1, 12, 4)
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
     * @param compression Compression algorithm: zstd, xz, bzip2, gzip, lz4
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
        romName: String = "",
        maker: String = "",
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

        // Resolve effective level: 0 = algorithm default (zstd→3, gzip→6, etc.)
        // Always log the actual level used — never display the raw sentinel 0.
        val effectiveLevel = if (level > 0) level else COMPRESS_LEVELS[compression]?.third ?: level

        // Log input parameters before the JNI call
        val debugStartMsg = "[DEBUG] dd() called: ${images.size} partitions, " +
            "compression=$compression, level=$effectiveLevel, device=$effectiveDevice, " +
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
        // Use coroutineScope{} to tie lifecycle to the parent dd() call,
        // preventing orphaned CoroutineScope leaks on multiple builds.
        val progressScope = CoroutineScope(kotlinx.coroutines.SupervisorJob() + Dispatchers.IO)
        val progressJob = progressScope.launch {
            var lastOverallPercent = -1
            var lastPhase = ""
            var lastName = ""

            while (isActive) {
                delay(500) // poll every 500ms
                // IMPL-015: Check parent scope cancellation — if the build
                // coroutine was cancelled (e.g., user clicked Remove All),
                // stop polling immediately instead of continuing for up to 500ms.
                if (!isActive) break
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

        // AUDIT-F1: try/finally OUTSIDE withContext. Previously the cleanup
        // lived only inside the withContext block's own finally — if the
        // caller's coroutine was cancelled before withContext could enter
        // the block, the polling scope would leak forever (500ms polling
        // loop with no owner) and the .progress sidecar would never be
        // deleted. Moving cleanup to an outer finally guarantees it runs
        // on every exit path, including early CancellationException.
        try {
            return withContext(Dispatchers.IO) {
                try {
                    // IMPL-018: Use -4 (THREAD_PRIORITY_URGENT_DISPLAY) instead of -10.
                    // -10 is in the audio priority range and can cause audio glitching
                    // during long compression builds. -4 gives I/O work higher priority
                    // than default (0) without interfering with audio playback.
                    Process.setThreadPriority(Process.myTid(), -4)
                } catch (_: Exception) {}

                val ddResult = NativeBridge.buildDd(
                    images = images,
                    compression = compression,
                    level = level,
                    outputPath = outputPath,
                    device = effectiveDevice,
                    skipVerify = skipVerify,
                    romName = romName,
                    maker = maker
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
                val totalUncSizeStr = ddResult.totalUncSize?.let { formatSize(it) } ?: "N/A"
                val debugEndMsg = "[DEBUG] dd() returned: success=${ddResult.success}, " +
                    "duration=${ddResult.durationMs}ms, zip_size=$zipSizeStr, " +
                    "bundle_size=$bundleSizeStr, total_flash_size=$totalUncSizeStr"
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
                // NOTE: progressScope/progressFile cleanup deliberately NOT here —
                // handled by the outer finally (AUDIT-F1) so it also covers the
                // withContext-entry cancellation path.
            }
        } finally {
            // Always cancel progress polling and clean up — on EVERY exit path
            // (normal return, JNI exception, or caller cancellation).
            // IMPL-015: Cancel the scope (cancels all children including
            // progressJob), then delete the sidecar file so stale data
            // doesn't persist into the next build.
            progressScope.coroutineContext[kotlinx.coroutines.Job]?.cancel()
            progressJob.cancel()
            progressFile.delete()
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
    //  Payload.bin inspect + extract (prototype)
    // ═══════════════════════════════════════════════════════════════

    /**
     * Inspect an OTAku payload.bin (custom "OTKU" format) — parse header +
     * manifest, list partitions.
     *
     * Read-only: parses the OTKU header and protobuf manifest on
     * Dispatchers.IO and returns the partition list. Output lines are
     * streamed to onOutputLine for the UI log (same pattern as dd()).
     *
     * @param path Absolute path to the payload.bin file
     * @param onOutputLine Optional log-line callback
     * @return PayloadInspectResult (null only when the native backend
     *         is not loaded — the error is already logged)
     */
    suspend fun inspectPayload(
        path: String,
        onOutputLine: ((String) -> Unit)? = null
    ): NativeBridge.PayloadInspectResult? {
        if (!NativeBridge.isLoaded) {
            val msg = "Native backend not loaded: ${NativeBridge.loadError}"
            Log.e(TAG, msg)
            onOutputLine?.invoke("[!] $msg")
            return null
        }
        return withContext(Dispatchers.IO) {
            onOutputLine?.invoke("[*] Inspecting payload: $path")
            val result = NativeBridge.readPayload(path)
            if (result.success) {
                onOutputLine?.invoke(
                    "[+] Payload OK — v${result.payloadVersion}, " +
                        "${result.partitions.size} partitions, " +
                        "block=${result.blockSize}, file=${formatSize(result.fileSize)}"
                )
            } else {
                onOutputLine?.invoke("[!] Inspect failed: ${result.error}")
            }
            result
        }
    }

    /**
     * Extract one partition image from a payload.bin.
     *
     * Streams the decompressed image to outputPath on Dispatchers.IO
     * (~8 MB RAM regardless of partition size). Callers drive the loop
     * over partitions and aggregate results — this function handles
     * exactly one partition per call.
     *
     * Progress: Rust writes a `.progress` sidecar next to the output image
     * (same convention as dd()); this function polls it every 500ms and
     * emits ProgressUpdate with partitionPercent (Rust-authoritative) and
     * the batch-aware overall percent — callers must pass `current`/`total`
     * from their loop so the math matches the visible batch position:
     *   overall = ((current - 1) * 100 + partitionPercent) / total.
     * CPU/CPU-alive for long extractions is the CALLER's job (OTAService
     * foreground + WakeLock — see MainActivity.extractAllPayloadPartitions).
     *
     * @param payloadPath Absolute path to the payload.bin file
     * @param partitionName Partition to extract (from inspectPayload)
     * @param outputPath Destination .img file path
     * @param current 1-based position of this partition in the batch
     * @param total Number of partitions in the batch
     * @param onProgress Optional progress callback (sidecar-driven)
     * @param onOutputLine Optional log-line callback
     * @return PayloadExtractResult with size + duration, or error
     */
    suspend fun extractPayloadPartition(
        payloadPath: String,
        partitionName: String,
        outputPath: String,
        current: Int = 1,
        total: Int = 1,
        onProgress: ((ProgressUpdate) -> Unit)? = null,
        onOutputLine: ((String) -> Unit)? = null
    ): NativeBridge.PayloadExtractResult {
        // Delete stale progress sidecar from a previous run of this output
        val progressFile = java.io.File("${outputPath}.progress")
        progressFile.delete()

        // Sidecar poller — same pattern as dd() (AUDIT-F1: the try/finally
        // lives OUTSIDE withContext so a caller cancellation before entry
        // still cancels the polling scope and deletes the sidecar).
        val progressScope = CoroutineScope(kotlinx.coroutines.SupervisorJob() + Dispatchers.IO)
        val progressJob = progressScope.launch {
            var lastPartitionPercent = -1
            var lastOverallPercent = -1
            while (isActive) {
                delay(500)
                if (!isActive) break
                try {
                    if (!progressFile.exists()) continue
                    val content = progressFile.readText().trim()
                    if (content.isEmpty()) continue
                    try {
                        val json = org.json.JSONObject(content)
                        val name = json.optString("name", "")
                        val bytesWritten = json.optLong("bytes_written", 0L)
                        val totalEstimated = json.optLong("total_estimated", 0L)
                        val partitionPercent = json.optInt("partition_percent", 0)

                        // Batch-aware overall percent — current/total come
                        // from the caller's loop; Rust only knows the
                        // single-partition view (sidecar current/total = 1/1).
                        val overallPercent =
                            (((current - 1) * 100 + partitionPercent) / total.coerceAtLeast(1))
                                .coerceIn(0, 100)

                        // Emit only when something visibly changed (same
                        // dedup discipline as dd()'s poller).
                        if (partitionPercent != lastPartitionPercent ||
                            overallPercent != lastOverallPercent
                        ) {
                            lastPartitionPercent = partitionPercent
                            lastOverallPercent = overallPercent
                            val message = when {
                                // No estimate in the manifest → byte counter
                                totalEstimated <= 0L && name.isNotEmpty() ->
                                    "Extracting $name (${formatSize(bytesWritten)})"
                                name.isNotEmpty() -> "Extracting $name"
                                else -> "Extracting…"
                            }
                            onProgress?.invoke(ProgressUpdate(
                                current = current,
                                total = total,
                                message = message,
                                percent = overallPercent,
                                partitionPercent = partitionPercent
                            ))
                        }
                    } catch (_: Exception) {
                        // JSON parse error — mid-write read; retry next poll
                    }
                } catch (_: Exception) {
                    // Progress polling is non-critical — ignore all errors
                }
            }
        }

        try {
            return withContext(Dispatchers.IO) {
                onOutputLine?.invoke("[*] Extracting '$partitionName' …")
                val result = NativeBridge.extractPartition(payloadPath, partitionName, outputPath)
                if (result.success) {
                    onOutputLine?.invoke(
                        "[+] '$partitionName' → ${result.outputPath} " +
                            "(${result.humanSize}, ${result.durationMs} ms)"
                    )
                } else {
                    onOutputLine?.invoke("[!] Extract '$partitionName' failed: ${result.error}")
                }
                result
            }
        } finally {
            // Every exit path: cancel the poller and remove the sidecar so
            // stale data can't leak into the next extraction.
            progressScope.coroutineContext[kotlinx.coroutines.Job]?.cancel()
            progressJob.cancel()
            progressFile.delete()
        }
    }

    /**
     * Self-verify a payload.bin by re-reading it (header + manifest only —
     * fast even for multi-GB payloads).
     *
     * @param path Absolute path to the payload.bin file
     * @param onOutputLine Optional log-line callback for the check log
     * @return VerifyPayloadResult with the human-readable check log
     */
    suspend fun verifyPayload(
        path: String,
        onOutputLine: ((String) -> Unit)? = null
    ): NativeBridge.VerifyPayloadResult {
        if (!NativeBridge.isLoaded) {
            val msg = "Native backend not loaded: ${NativeBridge.loadError}"
            Log.e(TAG, msg)
            onOutputLine?.invoke("[!] $msg")
            return NativeBridge.VerifyPayloadResult.error(msg)
        }
        return withContext(Dispatchers.IO) {
            onOutputLine?.invoke("[*] Verifying payload: $path")
            val result = NativeBridge.verifyPayload(path)
            result.output.split("\n").forEach { line ->
                if (line.isNotBlank()) onOutputLine?.invoke(line)
            }
            if (!result.success) {
                onOutputLine?.invoke("[!] Verification failed: ${result.error}")
            }
            result
        }
    }

    /**
     * Build a payload.bin from partition images.
     *
     * Streams each partition's compressed data to a temp file in the output
     * directory (~8 MB RAM per partition — same OOM discipline as dd()),
     * then assembles header + manifest + data blobs.
     *
     * Progress: Rust writes a `.progress` sidecar next to the output
     * payload.bin (same convention as dd()); this function polls it every
     * 500ms and emits ProgressUpdate — current/total/partition_percent are
     * Rust-authoritative (partitions are processed alphabetically).
     * Phases: compressing → compressed → assembling, mapped to 0-94% /
     * ≤94% / 97% (same ladder as the DD build). The poller is cancelled
     * and the sidecar deleted on every exit path (AUDIT-F1 discipline);
     * the native code also deletes the sidecar before returning.
     *
     * CPU-alive for long builds is the CALLER's job (OTAService foreground
     * + WakeLock — see MainActivity.buildPayloadBin).
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param compression Algorithm shared by ALL partitions
     * @param level Compression level (0 = algorithm default)
     * @param outputPath Destination payload.bin path
     * @param blockSize Manifest block size in bytes (<= 0 = Rust default 4096)
     * @param minorVersion Payload minor version (< 0 = 0 at the JNI boundary)
     * @param onProgress Optional progress callback (sidecar-driven)
     * @param onOutputLine Optional log-line callback
     * @return WritePayloadResult with per-partition summaries, or error
     */
    suspend fun writePayload(
        images: Map<String, String>,
        compression: String,
        level: Int,
        outputPath: String,
        blockSize: Int = 0,
        minorVersion: Int = 0,
        onProgress: ((ProgressUpdate) -> Unit)? = null,
        onOutputLine: ((String) -> Unit)? = null
    ): NativeBridge.WritePayloadResult {
        if (images.isEmpty()) {
            return NativeBridge.WritePayloadResult.error("No images specified for payload.bin")
        }
        if (compression !in ALL_COMPRESSION) {
            return NativeBridge.WritePayloadResult.error("Invalid compression: '$compression'")
        }
        if (!NativeBridge.isLoaded) {
            val msg = "Native backend not loaded: ${NativeBridge.loadError}"
            Log.e(TAG, msg)
            onOutputLine?.invoke("[!] $msg")
            return NativeBridge.WritePayloadResult.error(msg)
        }

        // Delete stale progress sidecar from a previous run (same as dd()).
        val progressFile = java.io.File("${outputPath}.progress")
        progressFile.delete()

        // Sidecar poller — same pattern as dd()/extractPayloadPartition():
        // Rust rewrites the JSON atomically per 4MB chunk; we poll at
        // 500ms. The phase ladder mirrors the DD build (assembling → 97,
        // compressed → ≤94, else Rust overall coerced to 0-94).
        val progressScope = CoroutineScope(kotlinx.coroutines.SupervisorJob() + Dispatchers.IO)
        val progressJob = progressScope.launch {
            var lastOverallPercent = -1
            var lastPhase = ""
            var lastName = ""
            while (isActive) {
                delay(500)
                if (!isActive) break
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

                        val displayPercent = when (phase) {
                            "assembling" -> 97
                            "compressed" -> (current * 100 / total.coerceAtLeast(1)).coerceAtMost(94)
                            else -> overallPercent.coerceIn(0, 94)
                        }

                        // Emit only when something visibly changed (same
                        // dedup discipline as dd()'s poller).
                        if (displayPercent != lastOverallPercent || name != lastName || phase != lastPhase) {
                            lastOverallPercent = displayPercent
                            lastName = name
                            lastPhase = phase
                            val message = when (phase) {
                                "compressing" -> if (name.isNotEmpty()) "Compressing $name" else "Compressing…"
                                "compressed" -> if (name.isNotEmpty()) "Compressed $name" else "Compressing…"
                                "assembling" -> "Assembling payload.bin"
                                else -> if (name.isNotEmpty()) "Processing $name" else "Building…"
                            }
                            // During assembling / partition-complete the
                            // per-partition bar is saturated at 100.
                            val pPct = when (phase) {
                                "assembling", "compressed" -> 100
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
                        // JSON parse error — mid-write read; retry next poll
                    }
                } catch (_: Exception) {
                    // Progress polling is non-critical — ignore all errors
                }
            }
        }

        // AUDIT-F1: cleanup OUTSIDE withContext — every exit path
        // (including caller cancellation before the JNI call starts)
        // cancels the poller and deletes the sidecar.
        try {
            return withContext(Dispatchers.IO) {
                val result = NativeBridge.writePayload(
                    images = images,
                    compression = compression,
                    level = level,
                    outputPath = outputPath,
                    blockSize = blockSize,
                    minorVersion = minorVersion
                )
                result.output.split("\n").forEach { line ->
                    if (line.isNotBlank()) onOutputLine?.invoke(line)
                }
                result
            }
        } finally {
            progressScope.coroutineContext[kotlinx.coroutines.Job]?.cancel()
            progressJob.cancel()
            progressFile.delete()
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Utility methods
    // ═══════════════════════════════════════════════════════════════

    /** Format byte size as human-readable string (e.g. "45.2MB"). Clamps negative to 0. */
    private fun formatSize(bytes: Long): String {
        // IMPL-020: Guard against negative input (e.g. corrupted file size)
        val b = if (bytes < 0) 0L else bytes
        return when {
        b < 1024 -> "$b B"
        b < 1024 * 1024 -> String.format("%.1f KB", b / 1024.0)
        b < 1024 * 1024 * 1024 -> String.format("%.1f MB", b / (1024.0 * 1024))
        else -> String.format("%.2f GB", b / (1024.0 * 1024 * 1024))
    }}
}
