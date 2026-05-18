package com.hoshiyomi.payloadtoolkit

import android.content.Context
import android.util.Log
import com.hoshiyomi.payloadtoolkit.BuildConfig
import java.io.File
import java.io.FileOutputStream
import java.io.InputStream
import java.util.zip.ZipInputStream

/**
 * PythonBridge — Manages .pyz extraction and Python runtime discovery.
 *
 * Architecture (v3 — bundled Python):
 *   1. python-runtime.zip is bundled as an Android asset (~22 MB)
 *      Contains Termux Python 3.13 + stdlib + C extensions for aarch64
 *   2. payload_toolkit.pyz is bundled as an Android asset (~35 KB)
 *   3. On first init, both are extracted to app-internal storage
 *   4. Python is invoked via ProcessBuilder with LD_LIBRARY_PATH / PYTHONHOME
 *
 * Fallback: if bundled runtime extraction fails (asset missing), falls back
 * to discovering a system Python (Termux or other).
 */
object PythonBridge {

    private const val TAG = "PythonBridge"
    private const val PYZ_ASSET_NAME = "payload_toolkit.pyz"
    private const val RUNTIME_ASSET_NAME = "python-runtime.zip"
    private const val PYTHON_DIR_NAME = "python"
    private const val RUNTIME_SUBDIR = "runtime"

    /**
     * System Python paths — used as fallback when bundled runtime is unavailable.
     */
    private val SYSTEM_PYTHON_PATHS = listOf(
        "/data/data/com.termux/files/usr/bin/python3",
        "/data/data/com.termux/files/usr/bin/python",
        "/system/bin/python3",
        "/usr/bin/python3",
        "/usr/local/bin/python3"
    )

    @Volatile private var initialized = false
    private var pythonPath: String? = null
    private var pyzPath: String? = null
    private var runtimeDir: String? = null
    private var isBundledPython = false
    private val initLock = Any()

    /**
     * Result of Python initialization attempt.
     */
    data class InitResult(
        val success: Boolean,
        val pythonPath: String?,
        val pyzPath: String?,
        val isBundled: Boolean = false,
        val error: String? = null
    )

    /**
     * Initialize: extract assets and prepare Python runtime.
     *
     * Priority:
     *   1. Bundled Python runtime (from python-runtime.zip asset)
     *   2. System Python (Termux or other)
     *
     * @param context Android context (for asset extraction)
     * @return [InitResult] with paths or error description
     */
    fun ensureInitialized(context: Context?): InitResult {
        if (initialized) return InitResult(true, pythonPath, pyzPath, isBundledPython)

        synchronized(initLock) {
            if (initialized) return InitResult(true, pythonPath, pyzPath, isBundledPython)

            val ctx = context
                ?: return InitResult(false, null, null, error = "Context required for initialization")

            // Step 1: Extract .pyz from assets
            val extractedPyz = extractPyz(ctx)
            if (extractedPyz == null) {
                return InitResult(false, null, null, error = "Failed to extract $PYZ_ASSET_NAME from assets")
            }
            pyzPath = extractedPyz
            Log.d(TAG, "Extracted $PYZ_ASSET_NAME to $pyzPath")

            // Step 2: Try bundled Python runtime first
            val extractedRuntime = extractPythonRuntime(ctx)
            if (extractedRuntime != null) {
                runtimeDir = extractedRuntime
                val pyBin = findBundledPythonBinary(extractedRuntime)
                if (pyBin != null) {
                    pyBin.setExecutable(true)
                    pythonPath = pyBin.absolutePath
                    isBundledPython = true
                    Log.d(TAG, "Using bundled Python at: $pythonPath")
                } else {
                    Log.w(TAG, "Bundled runtime extracted but python binary not found")
                    runtimeDir = null
                }
            }

            // Step 3: Fallback to system Python
            if (pythonPath == null) {
                val foundPython = findSystemPython()
                if (foundPython != null) {
                    pythonPath = foundPython
                    isBundledPython = false
                    Log.d(TAG, "Using system Python at: $pythonPath")
                }
            }

            if (pythonPath == null) {
                val hint = "No Python available. Bundled runtime extraction may have failed."
                Log.w(TAG, hint)
                return InitResult(false, null, pyzPath, error = hint)
            }

            // Step 4: Verify Python can run the .pyz
            val verifyResult = verifySetup()
            if (verifyResult != null) {
                Log.w(TAG, "Verification failed: $verifyResult")
                return InitResult(false, pythonPath, pyzPath, isBundledPython, verifyResult)
            }

            initialized = true
            return InitResult(true, pythonPath, pyzPath, isBundledPython)
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Asset extraction
    // ═══════════════════════════════════════════════════════════════

    /**
     * Extract payload_toolkit.pyz from assets to app-internal storage.
     */
    private fun extractPyz(context: Context): String? {
        val pythonDir = File(context.filesDir, PYTHON_DIR_NAME).also { it.mkdirs() }
        val pyzFile = File(pythonDir, PYZ_ASSET_NAME)

        // Check if extraction is needed (compare size with asset)
        if (pyzFile.exists()) {
            try {
                val assetSize = context.assets.open(PYZ_ASSET_NAME).use { it.available().toLong() }
                if (assetSize > 0 && assetSize == pyzFile.length()) {
                    return pyzFile.absolutePath
                }
            } catch (_: Exception) {}
        }

        return try {
            context.assets.open(PYZ_ASSET_NAME).use { input ->
                FileOutputStream(pyzFile).use { output -> input.copyTo(output) }
            }
            pyzFile.absolutePath
        } catch (e: Exception) {
            Log.e(TAG, "Failed to extract $PYZ_ASSET_NAME", e)
            null
        }
    }

    /**
     * Extract python-runtime.zip from assets to app-internal storage.
     * Uses a version marker to avoid re-extraction on every launch.
     */
    private fun extractPythonRuntime(context: Context): String? {
        val pythonDir = File(context.filesDir, PYTHON_DIR_NAME)
        val runtimeDir = File(pythonDir, RUNTIME_SUBDIR)
        // Version-specific marker: re-extract when APK version changes
        val markerFile = File(pythonDir, ".runtime_v${BuildConfig.VERSION_CODE}")

        if (markerFile.exists() && runtimeDir.exists() && runtimeDir.isDirectory) {
            val children = runtimeDir.listFiles()?.size ?: 0
            if (children > 0) {
                Log.d(TAG, "Runtime already extracted (${children} top-level items)")
                return runtimeDir.absolutePath
            }
        }

        // Clean any previous extraction
        runtimeDir.deleteRecursively()

        return try {
            Log.d(TAG, "Extracting $RUNTIME_ASSET_NAME to $runtimeDir...")
            extractZipAsset(context, RUNTIME_ASSET_NAME, runtimeDir)
            markerFile.createNewFile()

            // Make the python binary executable
            findBundledPythonBinary(runtimeDir)?.setExecutable(true)

            Log.d(TAG, "Runtime extraction complete")
            runtimeDir.absolutePath
        } catch (e: Exception) {
            Log.w(TAG, "Failed to extract bundled Python runtime", e)
            null
        }
    }

    /**
     * Extract a ZIP asset to a target directory.
     */
    private fun extractZipAsset(context: Context, assetName: String, targetDir: File) {
        targetDir.mkdirs()
        context.assets.open(assetName).use { input ->
            ZipInputStream(input).use { zipIn ->
                var entry = zipIn.nextEntry
                while (entry != null) {
                    val outFile = File(targetDir, entry.name)
                    if (entry.isDirectory) {
                        outFile.mkdirs()
                    } else {
                        outFile.parentFile?.mkdirs()
                        FileOutputStream(outFile).use { out -> zipIn.copyTo(out) }
                    }
                    zipIn.closeEntry()
                    entry = zipIn.nextEntry
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Python binary discovery
    // ═══════════════════════════════════════════════════════════════

    /**
     * Find the Python binary in the extracted bundled runtime.
     * Looks for bin/python3 or bin/python3.x (exact version).
     */
    private fun findBundledPythonBinary(runtimeDir: String): File? {
        return findBundledPythonBinary(File(runtimeDir))
    }

    private fun findBundledPythonBinary(runtimeDir: File): File? {
        val binDir = File(runtimeDir, "bin")
        if (!binDir.isDirectory) return null

        // Prefer python3 (the copy made from symlinks during zip creation)
        val python3 = File(binDir, "python3")
        if (python3.isFile) return python3

        // Fallback: look for python3.x
        val versioned = binDir.listFiles()?.firstOrNull {
            it.isFile && it.name.matches(Regex("python3\\.\\d+"))
        }
        return versioned
    }

    /**
     * Discover a system Python binary (Termux or other).
     */
    private fun findSystemPython(): String? {
        for (path in SYSTEM_PYTHON_PATHS) {
            val file = File(path)
            if (file.exists() && file.canExecute()) return path
        }

        return try {
            val pb = ProcessBuilder("which", "python3")
            val process = pb.start()
            val output = process.inputStream.bufferedReader().readText().trim()
            process.waitFor()
            if (process.exitValue() == 0 && output.isNotEmpty()) output else null
        } catch (e: Exception) {
            null
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Verification & execution
    // ═══════════════════════════════════════════════════════════════

    /**
     * Quick smoke test: run .pyz --version.
     */
    private fun verifySetup(): String? {
        val py = pythonPath ?: return "No Python path"
        val pyz = pyzPath ?: return "No .pyz path"

        return try {
            val pb = ProcessBuilder(py, pyz, "--version")
                .redirectErrorStream(true)
            configureEnvironment(pb)
            val process = pb.start()
            val output = process.inputStream.bufferedReader().readText().trim()
            val exitCode = process.waitFor()
            if (exitCode == 0 && output.isNotEmpty()) {
                Log.d(TAG, "Verify OK: $output")
                null
            } else {
                "Python returned exit code $exitCode: $output"
            }
        } catch (e: Exception) {
            "Failed to run Python: ${e.message}"
        }
    }

    /**
     * Execute payload_toolkit.pyz with given arguments.
     * Automatically sets the correct environment for bundled or system Python.
     *
     * @param args CLI arguments (e.g., ["info", "-i", "/path/to/payload.bin"])
     * @return [ExecResult] with stdout, stderr, exit code, and duration
     */
    fun executePyz(args: List<String>): ExecResult {
        val py = pythonPath
            ?: return ExecResult("", "Python not initialized", -1, 0)
        val pyz = pyzPath
            ?: return ExecResult("", ".pyz not found", -1, 0)

        val startTime = System.currentTimeMillis()
        return try {
            val command = mutableListOf(py, pyz)
            command.addAll(args)

            val pb = ProcessBuilder(command).redirectErrorStream(true)
            configureEnvironment(pb)

            val process = pb.start()
            val output = process.inputStream.bufferedReader().readText()
            val exitCode = process.waitFor()
            val duration = System.currentTimeMillis() - startTime

            ExecResult(output, null, exitCode, duration)
        } catch (e: Exception) {
            val duration = System.currentTimeMillis() - startTime
            ExecResult("", "Execution failed: ${e.message}", -1, duration)
        }
    }

    /**
     * Execute payload_toolkit.pyz with Termux environment variables set.
     * Kept for backward compatibility — now delegates to executePyz() which
     * auto-configures the environment based on [isBundledPython].
     */
    fun executePyzWithTermuxEnv(args: List<String>): ExecResult {
        return executePyz(args)
    }

    // ═══════════════════════════════════════════════════════════════
    //  Environment configuration
    // ═══════════════════════════════════════════════════════════════

    /**
     * Configure ProcessBuilder environment for the current Python source.
     *
     * Bundled Python needs:
     *   LD_LIBRARY_PATH → runtime/lib (for libpython, liblzma, etc.)
     *   PYTHONHOME      → runtime root (for stdlib lookup)
     *   PATH            → runtime/bin (for subprocesses)
     *
     * System/Termux Python needs:
     *   LD_LIBRARY_PATH → Termux lib path
     *   PATH            → Termux bin path
     */
    private fun configureEnvironment(pb: ProcessBuilder) {
        val env = pb.environment()

        if (isBundledPython && runtimeDir != null) {
            env["LD_LIBRARY_PATH"] = "$runtimeDir/lib"
            env["PYTHONHOME"] = runtimeDir!!
            env["PATH"] = "$runtimeDir/bin:${env.getOrDefault("PATH", "")}"
            env["TMPDIR"] = "${runtimeDir!!}/../tmp"
        } else {
            // System Python (Termux or other)
            val py = pythonPath ?: return
            if (py.contains("termux")) {
                env["TERMUX_PREFIX"] = "/data/data/com.termux/files/usr"
                env["LD_LIBRARY_PATH"] = "/data/data/com.termux/files/usr/lib"
                env["PATH"] = "/data/data/com.termux/files/usr/bin:${env.getOrDefault("PATH", "")}"
                env["HOME"] = "/data/data/com.termux/files/home"
                env["TMPDIR"] = "/data/data/com.termux/files/usr/tmp"
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  Public accessors
    // ═══════════════════════════════════════════════════════════════

    fun isReady(): Boolean = initialized && pythonPath != null && pyzPath != null

    fun getPythonPath(): String? = pythonPath

    fun getPyzPath(): String? = pyzPath

    fun isBundled(): Boolean = isBundledPython

    /**
     * Run dependency health check via .pyz --check-deps.
     * Returns a human-readable report string.
     */
    fun checkDependencies(): String {
        val py = pythonPath
            ?: return "ERROR: Python not initialized"
        val pyz = pyzPath
            ?: return "ERROR: .pyz not extracted from assets"

        return try {
            val pb = ProcessBuilder(py, pyz, "--check-deps")
                .redirectErrorStream(true)
            configureEnvironment(pb)
            val process = pb.start()
            val output = process.inputStream.bufferedReader().readText().trim()
            process.waitFor()
            output.ifEmpty { "Dependency check returned no output (exit code ${process.exitValue()})" }
        } catch (e: Exception) {
            "Failed to check dependencies: ${e.message}"
        }
    }

    /**
     * Check if Termux is installed on the device.
     */
    fun isTermuxInstalled(): Boolean {
        return SYSTEM_PYTHON_PATHS.any { it.contains("termux") && File(it).exists() }
    }
}

/**
 * Result of executing a Python subprocess.
 */
data class ExecResult(
    val output: String,
    val error: String?,
    val exitCode: Int,
    val durationMs: Long
) {
    val success: Boolean get() = exitCode == 0 && error == null
}
