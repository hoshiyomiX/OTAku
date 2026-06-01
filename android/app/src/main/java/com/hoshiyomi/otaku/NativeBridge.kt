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

    // ═══════════════════════════════════════════════════════════════
    //  Payload Operations (Phase 2)
    // ═══════════════════════════════════════════════════════════════

    /**
     * Read and parse a payload.bin file.
     *
     * Parses the "CrAU" header, protobuf manifest, and computes data offset.
     * Does NOT read data blobs into memory — only metadata.
     *
     * @param path Absolute path to the payload.bin file
     * @return PayloadResult with header, manifest, and partition info
     */
    fun readPayload(path: String): PayloadResult {
        if (!isLoaded) {
            return PayloadResult.error("Native library not loaded: $loadError")
        }
        return try {
            val jsonStr = nativeReadPayload(path)
            parsePayloadResult(jsonStr)
        } catch (e: Exception) {
            PayloadResult.error("Read payload failed: ${e.message}")
        }
    }

    /**
     * Extract and decompress a partition from a payload.bin file.
     *
     * Reads the payload, finds the specified partition, decompresses each
     * operation's data, and saves the result to outputPath.
     *
     * @param payloadPath Absolute path to the payload.bin file
     * @param partitionName Name of the partition to extract (e.g. "boot", "system")
     * @param outputPath Absolute path for the output .img file
     * @return ExtractResult with success/error and file info
     */
    fun extractPartition(
        payloadPath: String,
        partitionName: String,
        outputPath: String
    ): ExtractResult {
        if (!isLoaded) {
            return ExtractResult.error("Native library not loaded: $loadError")
        }
        return try {
            val jsonStr = nativeExtractPartition(payloadPath, partitionName, outputPath)
            parseExtractResult(jsonStr)
        } catch (e: Exception) {
            ExtractResult.error("Extract failed: ${e.message}")
        }
    }

    /**
     * Generate a payload.bin from partition images.
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param compression Compression algorithm: "none", "gzip", "bzip2", "xz", "brotli"
     * @param outputPath Absolute path for output payload.bin
     * @param blockSize Block size in bytes (default 4096)
     * @param minorVersion Payload minor version (default 0)
     * @return WritePayloadResult with success/error and partition summaries
     */
    fun writePayload(
        images: Map<String, String>,
        compression: String = "gzip",
        outputPath: String,
        blockSize: Int = 4096,
        minorVersion: Int = 0
    ): WritePayloadResult {
        if (!isLoaded) {
            return WritePayloadResult.error("Native library not loaded: $loadError")
        }
        return try {
            val imagesJson = JSONObject(images).toString()
            val jsonStr = nativeWritePayload(
                imagesJson, compression, 0, outputPath, blockSize, minorVersion
            )
            parseWritePayloadResult(jsonStr)
        } catch (e: Exception) {
            WritePayloadResult.error("Write payload failed: ${e.message}")
        }
    }

    /**
     * Verify a payload.bin file by re-reading and checking its structure.
     *
     * @param path Absolute path to the payload.bin file
     * @return VerifyResult with success/error and diagnostic info
     */
    fun verifyPayload(path: String): VerifyResult {
        if (!isLoaded) {
            return VerifyResult(error = "Native library not loaded: $loadError")
        }
        return try {
            val jsonStr = nativeVerifyPayload(path)
            parseVerifyResult(jsonStr)
        } catch (e: Exception) {
            VerifyResult(error = "Verify failed: ${e.message}")
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Compression Operations (Phase 2)
    // ═══════════════════════════════════════════════════════════════

    /**
     * Compress a file using the specified algorithm.
     *
     * Reads the input file, compresses with the given algorithm and level,
     * and writes the compressed output to the specified path.
     *
     * @param inputPath Absolute path to the input file
     * @param outputPath Absolute path for the compressed output
     * @param algorithm Compression algorithm: "none", "gzip", "bzip2", "xz", "brotli"
     * @param level Compression level (0 = algorithm default)
     * @return CompressResult with success/error and size info
     */
    fun compress(
        inputPath: String,
        outputPath: String,
        algorithm: String = "gzip",
        level: Int = 0
    ): CompressResult {
        if (!isLoaded) {
            return CompressResult.error("Native library not loaded: $loadError")
        }
        return try {
            val jsonStr = nativeCompress(inputPath, outputPath, algorithm, level)
            parseCompressResult(jsonStr)
        } catch (e: Exception) {
            CompressResult.error("Compress failed: ${e.message}")
        }
    }

    /**
     * Decompress a file.
     *
     * @param inputPath Absolute path to the compressed input file
     * @param outputPath Absolute path for the decompressed output
     * @param algorithm Decompression algorithm, or "auto" for auto-detect
     * @return CompressResult with success/error and size info
     */
    fun decompress(
        inputPath: String,
        outputPath: String,
        algorithm: String = "auto"
    ): CompressResult {
        if (!isLoaded) {
            return CompressResult.error("Native library not loaded: $loadError")
        }
        return try {
            val jsonStr = nativeDecompress(inputPath, outputPath, algorithm)
            parseCompressResult(jsonStr)
        } catch (e: Exception) {
            CompressResult.error("Decompress failed: ${e.message}")
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  DD Build (Phase 3 stub)
    // ═══════════════════════════════════════════════════════════════

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
    //  Result data classes
    // ═══════════════════════════════════════════════════════════════

    data class DepCheckResult(
        val available: List<String>,
        val missing: List<String>,
        val allOk: Boolean,
        val nativeVersion: String
    )

    data class PayloadResult(
        val success: Boolean,
        val header: PayloadHeaderInfo? = null,
        val manifest: ManifestInfo? = null,
        val dataOffset: Long = 0,
        val fileSize: Long = 0,
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = PayloadResult(success = false, error = msg)
        }
    }

    data class PayloadHeaderInfo(
        val version: Long,
        val manifestLen: Long,
        val metadataSignatureLen: Long,
        val minorVersion: Int
    )

    data class ManifestInfo(
        val blockSize: Long,
        val minorVersion: Int,
        val partitions: List<PartitionInfo>,
        val maxTimestamp: Long,
        val hasSourceMetadata: Boolean
    )

    data class PartitionInfo(
        val partitionName: String,
        val runPostinstall: Boolean,
        val operations: List<OpInfo>,
        val newPartitionInfo: PartitionInfoData? = null,
        val oldPartitionInfo: PartitionInfoData? = null
    )

    data class OpInfo(
        val type: Int,
        val typeName: String,
        val dataOffset: Long,
        val dataLength: Long,
        val dstLength: Int
    )

    data class PartitionInfoData(
        val partitionSize: Long,
        val hash: String
    )

    data class ExtractResult(
        val success: Boolean,
        val partition: String = "",
        val outputPath: String = "",
        val fileSize: Long = 0,
        val humanSize: String = "",
        val durationMs: Long = 0,
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = ExtractResult(success = false, error = msg)
        }
    }

    data class WritePayloadResult(
        val success: Boolean,
        val output: String = "",
        val outputPath: String? = null,
        val fileSize: Long? = null,
        val partitions: List<PartitionSummary> = emptyList(),
        val durationMs: Long = 0,
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = WritePayloadResult(success = false, error = msg)
        }
    }

    data class PartitionSummary(
        val name: String,
        val originalSize: Long,
        val compressedSize: Long,
        val ratio: Double,
        val algorithm: String,
        val opType: Int,
        val opTypeName: String,
        val sha256: String
    )

    data class VerifyResult(
        val success: Boolean = false,
        val output: String = "",
        val error: String? = null
    )

    data class CompressResult(
        val success: Boolean,
        val inputPath: String = "",
        val outputPath: String = "",
        val algorithm: String = "",
        val inputSize: Long = 0,
        val outputSize: Long = 0,
        val ratio: Double = 1.0,
        val sha256: String = "",
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = CompressResult(success = false, error = msg)
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

    private fun parsePayloadResult(jsonStr: String): PayloadResult {
        val json = JSONObject(jsonStr)
        if (!json.optBoolean("success", false)) {
            return PayloadResult.error(json.optString("error", "Unknown error"))
        }
        val headerJson = json.optJSONObject("header") ?: return PayloadResult.error("No header in response")
        val manifestJson = json.optJSONObject("manifest") ?: return PayloadResult.error("No manifest in response")

        val header = PayloadHeaderInfo(
            version = headerJson.optLong("version", 0),
            manifestLen = headerJson.optLong("manifest_len", 0),
            metadataSignatureLen = headerJson.optLong("metadata_signature_len", 0),
            minorVersion = headerJson.optInt("minor_version", 0)
        )

        val partitionsArr = manifestJson.optJSONArray("partitions") ?: emptyList()
        val partitions = (0 until partitionsArr.length()).mapNotNull { i ->
            val pj = partitionsArr.optJSONObject(i) ?: return@mapNotNull null
            val opsArr = pj.optJSONArray("install_operations") ?: emptyList()
            val ops = (0 until opsArr.length()).mapNotNull { j ->
                val oj = opsArr.optJSONObject(j) ?: return@mapNotNull null
                OpInfo(
                    type = oj.optInt("type", 0),
                    typeName = oj.optString("type_name", ""),
                    dataOffset = oj.optLong("data_offset", 0),
                    dataLength = oj.optLong("data_length", 0),
                    dstLength = oj.optInt("dst_length", 0)
                )
            }
            PartitionInfo(
                partitionName = pj.optString("partition_name", ""),
                runPostinstall = pj.optBoolean("run_postinstall", false),
                operations = ops
            )
        }

        val manifest = ManifestInfo(
            blockSize = manifestJson.optLong("block_size", 4096),
            minorVersion = manifestJson.optInt("minor_version", 0),
            partitions = partitions,
            maxTimestamp = manifestJson.optLong("max_timestamp", 0),
            hasSourceMetadata = manifestJson.optBoolean("has_source_metadata", false)
        )

        return PayloadResult(
            success = true,
            header = header,
            manifest = manifest,
            dataOffset = json.optLong("data_offset", 0),
            fileSize = json.optLong("file_size", 0)
        )
    }

    private fun parseExtractResult(jsonStr: String): ExtractResult {
        val json = JSONObject(jsonStr)
        return if (json.optBoolean("success", false)) {
            ExtractResult(
                success = true,
                partition = json.optString("partition", ""),
                outputPath = json.optString("output_path", ""),
                fileSize = json.optLong("file_size", 0),
                humanSize = json.optString("human_size", ""),
                durationMs = json.optLong("duration_ms", 0)
            )
        } else {
            ExtractResult.error(json.optString("error", "Unknown error"))
        }
    }

    private fun parseWritePayloadResult(jsonStr: String): WritePayloadResult {
        val json = JSONObject(jsonStr)
        if (!json.optBoolean("success", false)) {
            return WritePayloadResult.error(json.optString("error", "Unknown error"))
        }
        val partsArr = json.optJSONArray("partitions") ?: emptyList()
        val partitions = (0 until partsArr.length()).mapNotNull { i ->
            val pj = partsArr.optJSONObject(i) ?: return@mapNotNull null
            PartitionSummary(
                name = pj.optString("name", ""),
                originalSize = pj.optLong("original_size", 0),
                compressedSize = pj.optLong("compressed_size", 0),
                ratio = pj.optDouble("ratio", 1.0),
                algorithm = pj.optString("algorithm", ""),
                opType = pj.optInt("op_type", 0),
                opTypeName = pj.optString("op_type_name", ""),
                sha256 = pj.optString("sha256", "")
            )
        }
        return WritePayloadResult(
            success = true,
            output = json.optString("output", ""),
            outputPath = json.optString("output_path", null),
            fileSize = if (json.has("file_size")) json.optLong("file_size") else null,
            partitions = partitions,
            durationMs = json.optLong("duration_ms", 0)
        )
    }

    private fun parseVerifyResult(jsonStr: String): VerifyResult {
        val json = JSONObject(jsonStr)
        return VerifyResult(
            success = json.optBoolean("success", false),
            output = json.optString("output", ""),
            error = json.optString("error", null)
        )
    }

    private fun parseCompressResult(jsonStr: String): CompressResult {
        val json = JSONObject(jsonStr)
        return if (json.optBoolean("success", false)) {
            CompressResult(
                success = true,
                inputPath = json.optString("input_path", ""),
                outputPath = json.optString("output_path", ""),
                algorithm = json.optString("algorithm", ""),
                inputSize = json.optLong("input_size", 0),
                outputSize = json.optLong("output_size", 0),
                ratio = json.optDouble("ratio", 1.0),
                sha256 = json.optString("sha256", "")
            )
        } else {
            CompressResult.error(json.optString("error", "Unknown error"))
        }
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

    // Version & Dependencies
    private external fun nativeGetVersion(): String
    private external fun nativeCheckDeps(): String

    // Payload Operations
    private external fun nativeReadPayload(path: String): String
    private external fun nativeExtractPartition(
        payloadPath: String,
        partitionName: String,
        outputPath: String
    ): String
    private external fun nativeWritePayload(
        imagesJson: String,
        compression: String,
        level: Int,
        outputPath: String,
        blockSize: Int,
        minorVersion: Int
    ): String
    private external fun nativeVerifyPayload(path: String): String

    // Compression Operations
    private external fun nativeCompress(
        inputPath: String,
        outputPath: String,
        algorithm: String,
        level: Int
    ): String
    private external fun nativeDecompress(
        inputPath: String,
        outputPath: String,
        algorithm: String
    ): String

    // DD Build (Phase 3)
    private external fun nativeBuildDd(
        imagesJson: String,
        compression: String,
        level: Int,
        outputPath: String,
        device: String,
        skipVerify: Int
    ): String
}
