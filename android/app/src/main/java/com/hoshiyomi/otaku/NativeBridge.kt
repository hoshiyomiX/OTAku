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
            return DepCheckResult.error(nativeVersion = "not loaded")
        }
        return try {
            val jsonStr = nativeCheckDeps()
            parseDepCheckResult(jsonStr)
        } catch (e: Exception) {
            Log.e(TAG, "checkDeps failed: ${e.message}")
            DepCheckResult.error()
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
        maker: String = "",
        // T49: APK that carries the bundled otaku-decomp helper + the asset
        // entry matching this device ABI. Rust reads the bytes straight out
        // of the APK (it IS a zip) and embeds them into the flashable ZIP.
        helperApkPath: String,
        helperAsset: String
    ): DdBuildResult {
        if (!isLoaded) {
            return DdBuildResult.error("Native library not loaded: $loadError")
        }
        Log.d(TAG, "buildDd() images=${images.keys}, compression=$compression, level=$level, output=$outputPath, device=$device, skipVerify=$skipVerify, romName=$romName, maker=$maker")

        return try {
            val imagesJson = JSONObject(images).toString()
            val resultJson = nativeBuildDd(
                imagesJson, compression, level, outputPath, device,
                skipVerify, romName, maker, helperApkPath, helperAsset
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
     * The app uses this list to WARN about user-picked .img files whose
     * name (minus .img) is not in the list: the file still loads, but the
     * log tells the user to double-check the target partition name. A hard
     * refusal was demoted to a warning because the scan list is a static
     * known-list filtered by getprop — OEM-specific partitions absent from
     * the list are false negatives that must not block valid files. The
     * flasher script's recovery-time validation (resolve_target /
     * validate_target) remains the authoritative gate.
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
    //  Payload.bin inspect + extract (prototype)
    //  OTAku custom payload format — Rust implementation in rust/src/payload.rs
    //  (OTKU magic + protobuf manifest, T27 Option B), JSON in/out like the
    //  other bridges. AOSP payloads (CrAU) are rejected, not parsed.
    // ═══════════════════════════════════════════════════════════════

    /**
     * Read and parse an OTAku payload.bin file (custom "OTKU" format).
     *
     * Parses the OTKU header + DeltaArchiveManifest protobuf and returns
     * the partition list (name, size, operation count) plus file metadata.
     * Read-only — never writes anything.
     *
     * @param path Absolute path to the payload.bin file
     * @return PayloadInspectResult with partitions and header info, or error
     */
    fun readPayload(path: String): PayloadInspectResult {
        if (!isLoaded) {
            return PayloadInspectResult.error("Native library not loaded: $loadError")
        }
        return try {
            parsePayloadInspectResult(nativeReadPayload(path))
        } catch (e: Exception) {
            Log.e(TAG, "readPayload failed: ${e.message}")
            PayloadInspectResult.error("Native read failed: ${e.message}")
        }
    }

    /**
     * Extract and decompress one partition from a payload.bin file.
     *
     * Streams the decompressed image to outputPath (~8 MB RAM regardless
     * of partition size — never holds the full image in memory). On error
     * the partial output file is removed by the native layer.
     *
     * @param payloadPath Absolute path to the payload.bin file
     * @param partitionName Partition to extract (from readPayload list)
     * @param outputPath Destination .img file path
     * @return PayloadExtractResult with size + duration, or error
     */
    fun extractPartition(
        payloadPath: String,
        partitionName: String,
        outputPath: String
    ): PayloadExtractResult {
        if (!isLoaded) {
            return PayloadExtractResult.error("Native library not loaded: $loadError")
        }
        return try {
            parsePayloadExtractResult(
                nativeExtractPartition(payloadPath, partitionName, outputPath)
            )
        } catch (e: Exception) {
            Log.e(TAG, "extractPartition failed: ${e.message}")
            PayloadExtractResult.error("Native extract failed: ${e.message}")
        }
    }

    /**
     * Generate a payload.bin from partition images.
     *
     * Streams each partition's compressed data to a temp file in the
     * output directory (~8 MB RAM per partition — never holds the full
     * compressed output in memory), then assembles header + manifest +
     * data blobs into the output payload.bin.
     *
     * Progress: the native build writes a `<outputPath>.progress` sidecar
     * (phases: compressing → compressed → assembling) with the same JSON
     * schema as buildDd — poll it for real-time per-partition progress.
     * The native code deletes the sidecar before the call returns, on
     * both the success and the error path.
     *
     * @param images Map of partition name -> absolute path to .img file
     * @param compression Algorithm shared by ALL partitions
     * @param level Compression level (0 = algorithm default)
     * @param outputPath Destination payload.bin path
     * @param blockSize Manifest block size (<= 0 = 4096 default)
     * @param minorVersion Payload minor version (< 0 = 0)
     * @return WritePayloadResult with per-partition summaries, or error
     */
    fun writePayload(
        images: Map<String, String>,
        compression: String,
        level: Int,
        outputPath: String,
        blockSize: Int = 0,
        minorVersion: Int = 0
    ): WritePayloadResult {
        if (!isLoaded) {
            return WritePayloadResult.error("Native library not loaded: $loadError")
        }
        return try {
            val imagesJson = JSONObject(images).toString()
            parseWritePayloadResult(
                nativeWritePayload(imagesJson, compression, level, outputPath, blockSize, minorVersion)
            )
        } catch (e: Exception) {
            Log.e(TAG, "writePayload failed: ${e.message}")
            WritePayloadResult.error("Native write failed: ${e.message}")
        }
    }

    /**
     * Self-verify a payload.bin by re-reading it.
     *
     * Checks the OTKU magic (OTAku custom format), header + manifest parseability, partition
     * count, and echoes each partition's manifest hash. Header/manifest
     * only — fast even for multi-GB payloads.
     *
     * @param path Absolute path to the payload.bin file
     * @return VerifyPayloadResult with a human-readable check log
     */
    fun verifyPayload(path: String): VerifyPayloadResult {
        if (!isLoaded) {
            return VerifyPayloadResult.error("Native library not loaded: $loadError")
        }
        return try {
            parseVerifyPayloadResult(nativeVerifyPayload(path))
        } catch (e: Exception) {
            Log.e(TAG, "verifyPayload failed: ${e.message}")
            VerifyPayloadResult.error("Native verify failed: ${e.message}")
        }
    }

    /** One partition entry from a payload.bin manifest. */
    data class PayloadPartitionInfo(
        val name: String,
        /** Decompressed image size in bytes (0 if manifest lacks new_partition_info). */
        val sizeBytes: Long,
        /** Number of install operations for this partition. */
        val opCount: Int
    )

    /** Result of parsing a payload.bin (readPayload). */
    data class PayloadInspectResult(
        val success: Boolean,
        /** payload.bin format version (2 = OTAku custom format family). */
        val payloadVersion: Long = 0L,
        val manifestLen: Long = 0L,
        val minorVersion: Int = 0,
        /** Manifest block size in bytes (typically 4096). */
        val blockSize: Long = 0L,
        /** Absolute offset where partition data blobs start. */
        val dataOffset: Long = 0L,
        val fileSize: Long = 0L,
        val partitions: List<PayloadPartitionInfo> = emptyList(),
        val nativeVersion: String = "unknown",
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = PayloadInspectResult(success = false, error = msg)
        }
    }

    /** Result of extracting one partition (extractPartition). */
    data class PayloadExtractResult(
        val success: Boolean,
        val partition: String = "",
        val outputPath: String? = null,
        val fileSize: Long = 0L,
        val humanSize: String = "",
        val durationMs: Long = 0L,
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = PayloadExtractResult(success = false, error = msg)
        }
    }

    /** Per-partition summary from a payload.bin build (writePayload). */
    data class PayloadPartitionSummary(
        val name: String,
        val originalSize: Long,
        val compressedSize: Long,
        /** compressed / original (0.0-1.0+; >1 means incompressible data grew). */
        val ratio: Double,
        val algorithm: String,
        val sha256: String
    )

    /** Result of building a payload.bin (writePayload). */
    data class WritePayloadResult(
        val success: Boolean,
        val output: String = "",
        val outputPath: String? = null,
        val fileSize: Long = 0L,
        val partitions: List<PayloadPartitionSummary> = emptyList(),
        val durationMs: Long = 0L,
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = WritePayloadResult(success = false, error = msg)
        }
    }

    /** Result of self-verifying a payload.bin (verifyPayload). */
    data class VerifyPayloadResult(
        val success: Boolean,
        val output: String = "",
        val error: String? = null
    ) {
        companion object {
            fun error(msg: String) = VerifyPayloadResult(success = false, error = msg)
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
    ) {
        companion object {
            /** Create an error result indicating the native library is not functional. */
            fun error(nativeVersion: String = "error") = DepCheckResult(
                available = listOf("gzip"),
                missing = listOf("zstd", "xz", "bzip2", "lz4"),
                allOk = false,
                nativeVersion = nativeVersion
            )
        }
    }

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
                // AUDIT-F7: org.json's optString(key, fallback) only returns
                // the fallback when the KEY is absent — a JSON null value
                // yields the literal string "null". Guard with isNull()
                // (same pattern as parseDeviceCodenameResult / …Partitions)
                // so a null field stays null instead of leaking "null"
                // into zipPath / downstream file operations.
                zipPath = if (json.has("zip_path") && !json.isNull("zip_path")) json.optString("zip_path") else null,
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
                error = if (json.has("error") && !json.isNull("error")) json.optString("error") else "Unknown error",
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

    private fun parsePayloadInspectResult(jsonStr: String): PayloadInspectResult {
        val json = JSONObject(jsonStr)
        if (!json.optBoolean("success", false)) {
            return PayloadInspectResult(
                success = false,
                error = if (json.has("error") && !json.isNull("error")) {
                    json.optString("error")
                } else "Unknown error"
            )
        }
        val header = json.optJSONObject("header")
        val manifest = json.optJSONObject("manifest")
        val partitions = mutableListOf<PayloadPartitionInfo>()
        manifest?.optJSONArray("partitions")?.let { arr ->
            for (i in 0 until arr.length()) {
                val p = arr.optJSONObject(i) ?: continue
                val name = p.optString("partition_name", "")
                if (name.isEmpty()) continue
                val size = p.optJSONObject("new_partition_info")
                    ?.optLong("partition_size", 0L) ?: 0L
                val ops = p.optJSONArray("install_operations")?.length() ?: 0
                partitions.add(PayloadPartitionInfo(name, size, ops))
            }
        }
        return PayloadInspectResult(
            success = true,
            payloadVersion = header?.optLong("version", 0L) ?: 0L,
            manifestLen = header?.optLong("manifest_len", 0L) ?: 0L,
            minorVersion = header?.optInt("minor_version", 0) ?: 0,
            blockSize = manifest?.optLong("block_size", 0L) ?: 0L,
            dataOffset = json.optLong("data_offset", 0L),
            fileSize = json.optLong("file_size", 0L),
            partitions = partitions,
            nativeVersion = json.optString("native_version", "unknown")
        )
    }

    private fun parsePayloadExtractResult(jsonStr: String): PayloadExtractResult {
        val json = JSONObject(jsonStr)
        return if (json.optBoolean("success", false)) {
            PayloadExtractResult(
                success = true,
                partition = json.optString("partition", ""),
                outputPath = if (json.has("output_path") && !json.isNull("output_path")) {
                    json.optString("output_path")
                } else null,
                fileSize = json.optLong("file_size", 0L),
                humanSize = json.optString("human_size", ""),
                durationMs = json.optLong("duration_ms", 0L)
            )
        } else {
            PayloadExtractResult(
                success = false,
                partition = json.optString("partition", ""),
                error = if (json.has("error") && !json.isNull("error")) {
                    json.optString("error")
                } else "Unknown error",
                durationMs = json.optLong("duration_ms", 0L)
            )
        }
    }

    private fun parseWritePayloadResult(jsonStr: String): WritePayloadResult {
        val json = JSONObject(jsonStr)
        val partitions = mutableListOf<PayloadPartitionSummary>()
        json.optJSONArray("partitions")?.let { arr ->
            for (i in 0 until arr.length()) {
                val p = arr.optJSONObject(i) ?: continue
                partitions.add(
                    PayloadPartitionSummary(
                        name = p.optString("name", ""),
                        originalSize = p.optLong("original_size", 0L),
                        compressedSize = p.optLong("compressed_size", 0L),
                        ratio = p.optDouble("ratio", 1.0),
                        algorithm = p.optString("algorithm", ""),
                        sha256 = p.optString("sha256", "")
                    )
                )
            }
        }
        // AUDIT-F7 pattern: isNull() guards so JSON null stays null instead
        // of leaking the literal string "null" into file operations.
        return if (json.optBoolean("success", false)) {
            WritePayloadResult(
                success = true,
                output = json.optString("output", ""),
                outputPath = if (json.has("output_path") && !json.isNull("output_path")) {
                    json.optString("output_path")
                } else null,
                fileSize = json.optLong("file_size", 0L),
                partitions = partitions,
                durationMs = json.optLong("duration_ms", 0L)
            )
        } else {
            WritePayloadResult(
                success = false,
                output = json.optString("output", ""),
                partitions = partitions,
                durationMs = json.optLong("duration_ms", 0L),
                error = if (json.has("error") && !json.isNull("error")) {
                    json.optString("error")
                } else "Unknown error"
            )
        }
    }

    private fun parseVerifyPayloadResult(jsonStr: String): VerifyPayloadResult {
        val json = JSONObject(jsonStr)
        return if (json.optBoolean("success", false)) {
            VerifyPayloadResult(
                success = true,
                output = json.optString("output", "")
            )
        } else {
            VerifyPayloadResult(
                success = false,
                output = json.optString("output", ""),
                error = if (json.has("error") && !json.isNull("error")) {
                    json.optString("error")
                } else "Unknown error"
            )
        }
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
        maker: String,
        // T49: bundled decompressor source inside the installed APK
        helperApkPath: String,
        helperAsset: String
    ): String

    // Device codename detection (spoof-resistant — reads vendor partition props)
    // Rust signature: nativeDetectDeviceCodename() -> jstring (JSON)
    private external fun nativeDetectDeviceCodename(): String

    // Device partition scanner (no root — getprop based)
    // Rust signature: nativeScanDevicePartitions() -> jstring (JSON)
    private external fun nativeScanDevicePartitions(): String

    // Payload.bin inspect (prototype)
    // Rust signature: nativeReadPayload(path) -> jstring (JSON)
    private external fun nativeReadPayload(path: String): String

    // Payload.bin partition extraction (prototype)
    // Rust signature: nativeExtractPartition(payload_path, partition_name,
    //                                        output_path) -> jstring (JSON)
    private external fun nativeExtractPartition(
        payloadPath: String,
        partitionName: String,
        outputPath: String
    ): String

    // Payload.bin build (prototype)
    // Rust signature: nativeWritePayload(images_json, compression, level,
    //                                    output_path, block_size,
    //                                    minor_version) -> jstring (JSON)
    private external fun nativeWritePayload(
        imagesJson: String,
        compression: String,
        level: Int,
        outputPath: String,
        blockSize: Int,
        minorVersion: Int
    ): String

    // Payload.bin self-verification (prototype)
    // Rust signature: nativeVerifyPayload(path) -> jstring (JSON)
    private external fun nativeVerifyPayload(path: String): String

    // Cancellation flag (T36)
    // Rust signatures: nativeCancelBuild() / nativeResetCancel() — void,
    // fire-and-forget; see lib.rs CANCEL_REQUESTED / CANCEL_SENTINEL.
    private external fun nativeCancelBuild()
    private external fun nativeResetCancel()

    // ── Cancellation (T36) — Kotlin mirror + request/reset wrappers ──

    /**
     * Kotlin mirror of the native cancellation flag. Lets the extract
     * batch loop skip remaining partitions without an extra JNI
     * round-trip; kept in lockstep with the native flag by
     * [requestCancel]/[resetCancel], which always set BOTH.
     */
    @Volatile
    var cancelRequested: Boolean = false
        private set

    /**
     * Request cancellation of the running native operation (DD build /
     * payload build / payload extract). Fire-and-forget: the Rust loop
     * aborts at its next 4MB chunk checkpoint and returns the
     * "cancelled by user" sentinel, which the result layer maps to a
     * neutral "cancelled" banner instead of an ERROR one.
     */
    fun requestCancel() {
        cancelRequested = true
        if (isLoaded) {
            try {
                nativeCancelBuild()
            } catch (e: UnsatisfiedLinkError) {
                Log.w(TAG, "nativeCancelBuild unavailable: ${e.message}")
            }
        }
    }

    /**
     * Arm a fresh operation: clear both the Kotlin mirror and the native
     * flag so a stale cancel request cannot poison the next run. Called at
     * the start of every long-running operation.
     */
    fun resetCancel() {
        cancelRequested = false
        if (isLoaded) {
            try {
                nativeResetCancel()
            } catch (e: UnsatisfiedLinkError) {
                Log.w(TAG, "nativeResetCancel unavailable: ${e.message}")
            }
        }
    }
}
