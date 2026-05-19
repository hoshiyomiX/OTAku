package com.hoshiyomi.payloadtoolkit

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/**
 * PayloadResult — structured result from a payload_toolkit operation.
 */
data class PayloadResult(
    val success: Boolean,
    val output: String,
    val error: String? = null,
    val exitCode: Int = 0,
    val durationMs: Long = 0
) {
    val hasError: Boolean get() = !success || !error.isNullOrBlank()

    companion object {
        fun error(message: String, durationMs: Long = 0) = PayloadResult(
            success = false, output = "", error = message, exitCode = -1, durationMs = durationMs
        )
        fun success(output: String, durationMs: Long = 0) = PayloadResult(
            success = true, output = output, error = null, exitCode = 0, durationMs = durationMs
        )
    }
}

/**
 * PayloadBridge — Kotlin singleton that bridges the Android UI to payload_toolkit.pyz.
 *
 * Primary use case: Repack partition images (.img) into a flashable OTA ZIP.
 *
 * The .pyz is run as a subprocess using the device's Python (Termux or system).
 */
object PayloadBridge {

    // Compression algorithm choices (dd mode supports gzip, bzip2, xz)
    val COMPRESSION_ALGORITHMS = listOf("gzip", "bzip2", "xz")

    /**
     * Execute payload_toolkit.pyz with the given CLI arguments.
     * PythonBridge.executePyz auto-configures the environment based on
     * whether Python is bundled or system (Termux).
     */
    private suspend fun executePyz(args: List<String>): PayloadResult {
        return withContext(Dispatchers.IO) {
            val execResult = PythonBridge.executePyz(args)

            if (execResult.success) {
                PayloadResult.success(execResult.output, execResult.durationMs)
            } else {
                PayloadResult.error(
                    message = execResult.error ?: "Exit code ${execResult.exitCode}",
                    durationMs = execResult.durationMs
                ).copy(output = execResult.output)
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Core operations
    // ═══════════════════════════════════════════════════════════════

    /**
     * INFO mode — Parse and display payload.bin metadata.
     */
    suspend fun getInfo(payloadPath: String, verbose: Boolean = false): PayloadResult {
        val args = mutableListOf("info", "-i", payloadPath)
        if (verbose) args.add("-v")
        return executePyz(args)
    }

    /**
     * DUMP mode — Extract partition images from payload.bin.
     */
    suspend fun dump(
        payloadPath: String,
        outputDir: String,
        partitions: List<String> = emptyList()
    ): PayloadResult {
        val args = mutableListOf("dump", "-i", payloadPath, "-o", outputDir)
        if (partitions.isNotEmpty()) {
            args.add("-p")
            args.add(partitions.joinToString(","))
        }
        return executePyz(args)
    }

    /**
     * GEN mode — Generate a partial payload.bin from .img files.
     */
    suspend fun gen(
        images: Map<String, String>,
        compression: String = "none",
        outputPath: String
    ): PayloadResult {
        if (images.isEmpty()) return PayloadResult.error("No images specified for generation")
        if (compression !in COMPRESSION_ALGORITHMS)
            return PayloadResult.error("Invalid compression: '$compression'")

        val firstPath = images.values.first()
        val imagesDir = File(firstPath).parentFile?.absolutePath
            ?: return PayloadResult.error("Cannot determine images directory")

        val args = mutableListOf("gen", "-i", imagesDir, "-o", outputPath)
        if (compression != "none") {
            args.add("-c")
            args.add(compression)
        }
        return executePyz(args)
    }

    /**
     * DD mode — Generate a dd-based flashable ZIP (ddbundle format).
     *
     * This is the primary mode for Payload Toolkit.
     * Produces a flashable ZIP with:
     *   - ddbundle.bin (compressed partition images)
     *   - META-INF/com/google/android/update-binary (TWRP/OrangeFox flasher script)
     *   - META-INF/com/google/android/updater-script (stub)
     *   - flash_info.txt (human-readable metadata)
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param device Device codename (e.g. "crosshatch", "S666LN-OP")
     * @param compression Compression algorithm: gzip, bzip2, or xz
     * @param outputPath Absolute path to output .zip file
     */
    suspend fun dd(
        images: Map<String, String>,
        device: String = "generic",
        compression: String = "gzip",
        outputPath: String
    ): PayloadResult {
        if (images.isEmpty()) return PayloadResult.error("No images specified for DD ZIP")
        if (compression !in COMPRESSION_ALGORITHMS)
            return PayloadResult.error("Invalid compression: '$compression'")

        // dd mode uses --image (repeatable) + --partition (repeatable)
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
        if (device.isNotBlank() && device != "generic") {
            args.add("--device")
            args.add(device)
        } else {
            // Always pass --device so the Python side receives it (even if empty/generic)
            args.add("--device")
            args.add(device.ifEmpty { "generic" })
        }
        return executePyz(args)
    }

    /**
     * Build a smart output filename based on selected partitions and compression.
     *
     * Examples:
     *   - flashable_dd_odm_dlkm_v16_gzip.zip
     *   - flashable_boot_vendor_v16_bzip2.zip
     */
    fun buildOutputFileName(images: Map<String, String>, compression: String, version: Int = 16): String {
        val partitionNames = images.keys.sorted().joinToString("_")
        val compressSuffix = if (compression == "none") "raw" else compression
        return "flashable_dd_${partitionNames}_v${version}_${compressSuffix}.zip"
    }

    /**
     * SIGN mode — Sign an existing payload.bin with RSA key.
     */
    suspend fun sign(
        inputPath: String,
        outputPath: String,
        keyPath: String,
        certPath: String
    ): PayloadResult {
        val args = mutableListOf("sign", "-i", inputPath, "-k", keyPath, "-o", outputPath)
        return executePyz(args)
    }

    // ═══════════════════════════════════════════════════════════════
    //  Utility methods
    // ═══════════════════════════════════════════════════════════════

    suspend fun validatePayload(payloadPath: String): Boolean {
        return try { getInfo(payloadPath).success } catch (_: Exception) { false }
    }

    suspend fun getPyzVersion(): String? {
        return withContext(Dispatchers.IO) {
            try {
                val result = PythonBridge.executePyz(listOf("--version"))
                if (result.success) result.output.trim() else null
            } catch (_: Exception) { null }
        }
    }
}
