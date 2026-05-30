# Architecture

## System Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                         Android Device                               │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │                    User Interface Layer                        │  │
│  │                    (Kotlin / Material 3)                       │  │
│  │                                                                │  │
│  │  MainActivity                                                  │  │
│  │  ├── TopAppBar (app title + theme toggle)                     │  │
│  │  ├── SettingsSection                                           │  │
│  │  │   ├── Device codename + Auto-detect                        │  │
│  │  │   ├── Image file picker (SAF)                              │  │
│  │  │   ├── Compression algorithm + level                        │  │
│  │  │   └── Output directory + Browse (SAF directory picker)     │  │
│  │  ├── ActionSection                                             │  │
│  │  │   ├── BuildButton (BUILD OTA NOW)                          │  │
│  │  │   ├── Per-partition progress bars                          │  │
│  │  │   └── CancelButton                                         │  │
│  │  └── LogSection                                                │  │
│  │      └── ScrollView > TextView (append-only log output)       │  │
│  └────────────────────────┬───────────────────────────────────────┘  │
│                           │                                          │
│  ┌────────────────────────▼───────────────────────────────────────┐  │
│  │                   Bridge Layer                                 │  │
│  │                                                                │  │
│  │  OTABridge.kt (Kotlin singleton)                               │  │
│  │  ├── dd(images, device, compress, level, outputPath) → OTAResult│  │
│  │  └── buildOutputFileName(device) → String                     │  │
│  │                                                                │  │
│  │  PythonBridge.kt (Runtime manager)                             │  │
│  │  ├── ensureInitialized(context) → InitResult                   │  │
│  │  ├── executePyz(args, onProgress, onOutputLine) → ExecResult   │  │
│  │  └── checkDependencies() → dependency map                     │  │
│  │                                                                │  │
│  │  PyBridge.kt (JNI wrapper — dlopen + Py_Main)                  │  │
│  │  └── runPython(libDir, pyzPath, stdlibDir, args) → PyResult    │  │
│  └────────────────────────┬───────────────────────────────────────┘  │
│                           │                                          │
│  ┌────────────────────────▼───────────────────────────────────────┐  │
│  │                 Python Runtime Layer                           │  │
│  │                 (CPython 3.13 via jniLibs + ProcessBuilder)     │  │
│  │                                                                │  │
│  │  otaku/                                                        │  │
│  │  ├── __init__.py          Main entry point, argument parsing   │  │
│  │  ├── protobuf.py          Minimal PB encoder/decoder           │  │
│  │  ├── compression.py       gzip, bz2, xz, brotli               │  │
│  │  ├── payload.py           AOSP payload.bin read/write          │  │
│  │  ├── ota_metadata.py      OTA ZIP metadata generation          │  │
│  │  └── modes/                                                  │  │
│  │      └── dd.py           Generate dd-based flashable ZIP       │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │                    Storage Layer                               │  │
│  │                                                                │  │
│  │  App-Internal Storage (no permissions needed)                  │  │
│  │  ├── /data/data/com.hoshiyomi.otaku/files/                    │  │
│  │  │   ├── input/            User-provided input images          │  │
│  │  │   └── python/           Extracted Python runtime + .pyz     │  │
│  │                                                                │  │
│  │  Shared Storage (via SAF / MANAGE_EXTERNAL_STORAGE)            │  │
│  │  ├── /storage/emulated/0/OTAku/  Default output directory      │  │
│  │  └── User-selected via ActivityResultLauncher                 │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

## Data Flow — DD Mode (only mode)

```
User selects partition images → selects compression → taps BUILD
    │
    ▼
OTABridge.dd(images, device, compression, level, outputPath)
    │
    ▼
PythonBridge.executePyz(["dd", "--image", ..., "--partition", ..., "--compress", ...])
    │
    ├── [JNI mode] PyBridge.runPython(libDir, pyzPath, stdlibDir, args)
    │   └── dlopen(libpython3.13.so) → Py_Main() → in-process execution
    │
    └── [exec mode] ProcessBuilder(python3, otaku.pyz, args...)
        └── LD_LIBRARY_PATH + PYTHONHOME configured from jniLibs/stdlib
    │
    ▼
otaku/modes/dd.py:run()
    ├── Hash each image (SHA-256)
    ├── Compress each partition (streaming, with __PROGRESS__ markers)
    ├── Build otaku.bin header (DDBU format)
    ├── Generate flasher script (update-binary)
    ├── Write output ZIP (otaku.bin + scripts + flash_info.txt)
    └── Return result dict
    │
    ▼
OTAResult(success, output, error, durationMs)
    │
    ▼
UI: Per-partition progress bars + log output + notification
```

## Python → Kotlin → UI Call Chain

```kotlin
// 1. User taps Build button in MainActivity.kt
fun onBuildClicked() {
    val result = OTABridge.dd(
        images = imageFiles.toMap(),
        device = deviceValue,
        compression = selectedCompression,
        level = selectedCompressionLevel,
        outputPath = outPath,
        onProgress = { progress -> /* update UI */ },
        onOutputLine = { line -> /* append to log */ }
    )
    handleBuildResult(result)
}

// 2. OTABridge.kt serializes arguments and calls Python
object OTABridge {
    suspend fun dd(...): OTAResult {
        val args = mutableListOf("dd", "--image", ..., "--partition", ..., "-o", outputPath)
        return executePyz(args, onProgress, onOutputLine)
    }
}

// 3. PythonBridge.kt executes the .pyz
object PythonBridge {
    fun executePyz(args, onProgress, onOutputLine): ExecResult {
        // JNI mode: PyBridge.runPython() — in-process, no subprocess
        // Exec mode: ProcessBuilder(python3, otaku.pyz, args)
    }
}
```

## File I/O Paths on Android

### App-Specific Storage (Primary)

All Python file operations use app-internal storage by default:

```
/data/data/com.hoshiyomi.otaku/files/
├── input/                    # User-provided input files (copied from SAF)
│   ├── boot.img
│   ├── vendor.img
│   └── odm.img
├── output/                   # Output OTA ZIPs (or user-selected directory)
│   └── flashable_device.zip
├── python/                   # Extracted Python runtime
│   ├── otaku.pyz           # Python zipapp (extracted from assets)
│   └── stdlib/               # Python standard library
└── keys/                     # RSA keys for signing
```

### Shared Storage (Secondary)

```
/storage/emulated/0/OTAku/    # Default output directory
    └── flashable_device.zip
```

## Thread Model

```
Main Thread (UI)
├── Button clicks → dispatch to buildScope (application-scoped coroutine)
├── Progress bar updates → posted to Main thread via onProgress callback
└── TextView log append → posted to Main thread via onOutputLine callback

buildScope (Application-scoped CoroutineScope)
├── OTABridge.dd() → PythonBridge.executePyz()
├── WakeLock held during build operation
└── Progress callback → Main thread

Python Thread (JNI mode: in-process / exec mode: subprocess)
├── Python functions execute sequentially
├── __PROGRESS__ markers parsed from stdout (exec) or retroactively (JNI)
└── Returns to Kotlin when complete
```

## Non-Root Limitations

### What Works Without Root

| Operation | Mechanism | Storage Location |
|-----------|-----------|-----------------|
| Select partition images | SAF file picker | App storage / SAF |
| Compress partition images | Python stdlib + brotli | App storage / SAF |
| Generate flashable ZIP | zipfile module | App storage / SAF |
| Stream progress to UI | __PROGRESS__ markers | N/A |

### What Does NOT Work Without Root

| Operation | Reason | Workaround |
|-----------|--------|------------|
| Direct dd to block device | `/dev/block/by-name/*` requires root | Generate flashable ZIP, flash via recovery |
| Read current partition | `/dev/block/by-name/*` requires root | Use extracted OTA images |
| Write to /system or /vendor | Read-only mounted partitions | Generate OTA ZIP, flash via recovery |
