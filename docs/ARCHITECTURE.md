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
│  │  │   ├── Floating Build FAB "Build OTA Now" (T19 —            │
│  │  │   │ enabled when ready; bottom-end overlay)                │
│  │  │   └── Progress pop-up dialog (T29 — MD3 circular        │  │
│  │  │       indicator mirroring the notification; full        │  │
│  │  │       actions also live in the TopAppBar overflow       │  │
│  │  │       menu since T14: Build OTA ZIP / Inspect /         │  │
│  │  │       Build payload.bin)                                │  │
│  │  └── LogSection (FLOATING overlay, T16)                    │  │
│  │      └── MaterialCardView gravity=bottom — morphable       │  │
│  │          pill ↔ card > ScrollView > TextView (log output)  │  │
│  └────────────────────────┬───────────────────────────────────────┘  │
│                           │                                          │
│  ┌────────────────────────▼───────────────────────────────────────┐  │
│  │                   Bridge Layer                                 │  │
│  │                                                                │  │
│  │  OTABridge.kt (Kotlin object)                                  │  │
│  │  ├── dd(images, device, compress, level, outputPath,           │  │
│  │  │       skipVerify, onProgress, onOutputLine) → OTAResult     │  │
│  │  ├── buildOutputFileName(device) → String                     │  │
│  │  ├── COMPRESSION_ALGORITHMS / ALL_COMPRESSION / COMPRESS_LEVELS│  │
│  │  └── Progress polling coroutine (500ms interval)               │  │
│  │                                                                │  │
│  │  NativeBridge.kt (Kotlin object — JNI wrapper)                 │  │
│  │  ├── isLoaded / loadError                                      │  │
│  │  ├── getVersion() → String                                     │  │
│  │  ├── checkDeps() → DepCheckResult                              │  │
│  │  ├── buildDd(images, compress, level, output, device, skip) → DdBuildResult │  │
│  │  ├── detectDeviceCodename() → DeviceCodenameResult             │  │
│  │  ├── scanDevicePartitions() → DevicePartitionsResult           │  │
│  │  ├── readPayload(path) → PayloadInspectResult [prototype]      │  │
│  │  ├── extractPartition(payload, name, out) → PayloadExtractResult │  │
│  │  ├── writePayload(images, comp, level, out) → WritePayloadResult │ │
│  │  └── verifyPayload(path) → VerifyPayloadResult [prototype]     │  │
│  └────────────────────────┬───────────────────────────────────────┘  │
│                           │ JNI (System.loadLibrary("otaku_native")) │
│  ┌────────────────────────▼───────────────────────────────────────┐  │
│  │              Native Backend (Rust cdylib)                      │  │
│  │              libotaku_native.so                                │  │
│  │                                                                │  │
│  │  cargo-ndk compiled for arm64-v8a + armeabi-v7a                │  │
│  │  Statically links: flate2, bzip2, xz2, zstd, lz4, sha2, prost, │  │
│  │                    serde, serde_json, zip, chrono, log          │  │
│  │                                                                │  │
│  │  src/lib.rs        — 9 JNI entry points (JSON in/out)          │  │
│  │  src/dd/           — DD-mode flashable ZIP generator           │  │
│  │    mod.rs · script.rs · tests.rs (T27 Fase-1 split)            │  │
│  │  src/payload.rs    — OTAku custom payload (OTKU) r/w           │  │
│  │  src/proto.rs      — Hand-written prost structs                │  │
│  │  src/compression.rs — gzip/bz2/xz/zstd/lz4 + SHA-256           │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │                    Storage Layer                               │  │
│  │                                                                │  │
│  │  App-Internal Storage (no permissions needed)                  │  │
│  │  ├── /data/data/com.hoshiyomi.otaku/files/                    │  │
│  │  │   ├── input/            User-provided input images          │  │
│  │  │   ├── output/           Output OTA ZIPs (or user-selected)  │  │
│  │  │   └── <tmp>/otaku_build_tmp.bin  Incremental bundle build   │  │
│  │                                                                │  │
│  │  Shared Storage (via SAF / MANAGE_EXTERNAL_STORAGE)            │  │
│  │  ├── /storage/emulated/0/OTAku/  Default output directory      │  │
│  │  └── User-selected via ActivityResultLauncher                 │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```


UI lifecycle invariant (T22): `onPause` invalidates the cached view
references (IMPL-008) and the top of `onResume` re-resolves them via
`cacheViews()` before re-syncing the Build FAB gate (`updateBuildFab()`).
Any code that gates UI state on cached views must tolerate a null cache
between onPause and onResume — the FAB gate self-heals by re-resolving
on demand. Until T22 the re-resolve half of IMPL-008 was missing, which
froze the Build FAB grayed-out after every SAF picker trip until a
theme-switch `recreate()` rebuilt the caches.

Since T23 all three long-running operations (DD build, payload build,
payload extract) set the companion `isBuilding` flag plus a progress
heartbeat, so a mid-operation recreate reconnects correctly and no
second operation can start concurrently.

## Data Flow — DD Mode (only mode)

```
User selects partition images → selects compression → taps BUILD
    │
    ▼
OTABridge.dd(images, device, compression, level, outputPath, skipVerify)
    │
    ├── Validate: images non-empty, compression in ALL_COMPRESSION,
    │             NativeBridge.isLoaded == true
    ├── Delete stale <outputPath>.progress sidecar
    ├── Launch progress polling coroutine (Dispatchers.IO)
    │       polls <outputPath>.progress every 500ms
    │       parses JSON → emits ProgressUpdate callbacks to UI
    │
    ▼
NativeBridge.buildDd(images, compression, level, outputPath, device, skipVerify)
    │
    ▼ JNI call: Java_com_hoshiyomi_otaku_NativeBridge_nativeBuildDd
    │
    │  Rust receives: images_json (HashMap), compression, level,
    │                 output_path, device, skip_verify (jboolean)
    │
    ▼
rust::dd::run_dd_build(images, compression, level, output_path, device, skip_verify)
    │
    ├── [1/3] Build otaku.bin (incremental temp file)
    │   ├── For each partition:
    │   │   ├── Hash + compress with progress callback (4MB chunks)
    │   │   ├── Write compressed data to temp file (4096-aligned)
    │   │   └── Update <outputPath>.progress sidecar JSON
    │   └── Overwrite header placeholder with real DDBU header
    │
    ├── [2/3] Build flasher scripts
    │   ├── update-binary (TWRP/OrangeFox shell script — see below)
    │   ├── updater-script (stub: "assert(1==1);\n")
    │   └── flash_info.txt (human-readable metadata)
    │
    ├── [3/3] Write output ZIP
    │   ├── ZIP64 enabled (large_file=true) for otaku.bin > 4GB
    │   ├── Compression method: Stored (data already compressed)
    │   └── Add: otaku.bin, flash_info.txt, META-INF/.../update-binary,
    │            META-INF/.../updater-script
    │
    ├── Delete <outputPath>.progress sidecar
    │
    ▼
DdBuildResult { success, output, zip_path, zip_size, bundle_size,
                error, duration_ms }
    │
    ▼ JNI return: JSON string
    │
    ▼
NativeBridge.parseDdBuildResult(jsonStr) → DdBuildResult
    │
    ▼
OTABridge: emit Rust output lines to log, cancel progress polling,
           return OTAResult.success / OTAResult.error
    │
    ▼
UI: Progress pop-up (T29) + log output + notification
```

## Payload.bin Toolchain Flow (prototype)

```
Inspect:  menu "Inspect payload.bin…" → SAF picker → readPayload →
          partition table in the log card → extract-all dialog
Extract:  extractAllPayloadPartitions → OTAService("Extracting payload…")
          + companion WakeLock → per partition:
          extractPartition → <output>.img.progress sidecar → 500ms poller
          → split bars + notification "Extracting system (2/7) — 43%"
Write:    menu "Build payload.bin…" → compression dialog → writePayload
          (images, compression, level) → OTAService("Building payload…")
          + WakeLock → <output>.progress sidecar → 500ms poller
          → split bars + notification "Compressing system (2/7) — 43%"
          → per-partition summaries in the log
Verify:   verifyPayload(output) — OTKU magic + header + manifest re-read
          (fast: no data-blob re-hash) → check log in the UI
```

## Kotlin → JNI → Rust Call Chain

```kotlin
// 1. User taps Build button in MainActivity.kt
fun onBuildClicked() {
    lifecycleScope.launch {
        val result = OTABridge.dd(
            images = imageFiles.toMap(),
            device = deviceValue,
            compression = selectedCompression,
            level = selectedCompressionLevel,
            outputPath = outPath,
            skipVerify = skipVerifyCheckbox.isChecked,
            onProgress = { progress -> /* notification + progress pop-up */ },
            onOutputLine = { line -> /* append to log TextView */ }
        )
        handleBuildResult(result)
    }
}

// 2. OTABridge.kt serializes args and calls NativeBridge
object OTABridge {
    suspend fun dd(...): OTAResult {
        // ... progress polling coroutine setup ...
        return withContext(Dispatchers.IO) {
            val ddResult = NativeBridge.buildDd(
                images = images,
                compression = compression,
                level = level,
                outputPath = outputPath,
                device = effectiveDevice,
                skipVerify = skipVerify
            )
            // ... emit log lines, return OTAResult ...
        }
    }
}

// 3. NativeBridge.kt marshals to JNI
object NativeBridge {
    fun buildDd(images, compression, level, outputPath, device, skipVerify): DdBuildResult {
        val imagesJson = JSONObject(images).toString()
        val resultJson = nativeBuildDd(
            imagesJson, compression, level, outputPath, device, skipVerify
        )
        return parseDdBuildResult(resultJson)
    }

    private external fun nativeBuildDd(
        imagesJson: String, compression: String, level: Int,
        outputPath: String, device: String, skipVerify: Boolean
    ): String
}
```

```rust
// 4. Rust JNI entry point (rust/src/lib.rs)
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeBuildDd(
    mut env: JNIEnv, _class: JClass,
    images_json: JString, compression: JString, level: jint,
    output_path: JString, device: JString, skip_verify: jboolean,
) -> jstring {
    // Parse JNI strings, sort partitions alphabetically,
    // call dd::run_dd_build(), serialize result to JSON, return
}

// 5. Rust DD build pipeline (rust/src/dd/ — mod.rs orchestrates,
// script.rs holds the flasher templates)
pub fn run_dd_build(
    images: &[(String, String)],
    compression: &str, level: i32,
    output_path: &str, device: &str, skip_verify: bool,
) -> DdBuildResult { /* ... */ }
```

## Progress Sidecar Protocol

The progress sidecar file is the **only** mechanism for Rust → Kotlin progress reporting. It avoids JNI callbacks (which failed in v3.4/v3.5 due to `JNIEnv` reentrancy, exception clearing, and local-reference-table overflow — see commits `1c35d7c` and `c292e88`).

### File location

`<output_path>.progress` — same directory + filename as the output ZIP, with `.progress` suffix appended. Payload extraction uses the same convention: the sidecar sits next to the extracted `.img` (`<output>.img.progress`), written by `payload::ProgressSidecarWriter` (4 MB chunk cadence, `partition_percent` from the manifest's expected size; batch position is tracked Kotlin-side by the extract loop).

### Write side (Rust)

`write_progress_with_percent()` in `dd/mod.rs` writes the JSON after every 4 MB chunk during compression; it is shared (`pub(crate)`) with the payload.bin build — `write_payload` calls it from the `_with_progress` compression callback (`compressing` / `compressed`) and from the data-blob copy loop (`assembling`). For payload extraction, `ProgressSidecarWriter` in `payload.rs` slices every `write_all` into ≤4 MB chunks and rewrites the sidecar atomically (tmp + rename) after each. The write is best-effort — failures are silently ignored (`let _ = std::fs::write(...)`).

### Read side (Kotlin)

`OTABridge.dd()` launches a coroutine on `Dispatchers.IO` that polls every 500ms, reads the file, parses the JSON, and emits a `ProgressUpdate` callback if any field changed since the last poll.

### JSON schema

```json
{
  "current": 2,
  "total": 5,
  "name": "system",
  "phase": "compressing",
  "bytes_written": 104857600,
  "tmp_path": "/data/data/.../otaku_build_tmp.bin",
  "total_estimated": 5368709120,
  "partition_percent": 45,
  "overall_percent": 29
}
```

| Field | Description |
|-------|-------------|
| `current` | 1-indexed partition number being processed |
| `total` | Total number of partitions |
| `name` | Partition name (e.g. `"system"`, `"vendor"`) or `"scripts"` / `"writing_zip"` during non-compression phases |
| `phase` | One of: `compressing`, `compressed`, `building_scripts`, `writing_zip` |
| `bytes_written` | Bytes written to the temp bundle file so far |
| `tmp_path` | Path to the temp bundle file (for debugging) |
| `total_estimated` | Sum of all input image sizes (for percentage estimation) |
| `partition_percent` | 0–100 progress within the current partition |
| `overall_percent` | 0–94 weighted progress across all partitions (capped at 94 to leave room for scripts + ZIP write phases) |

### Display percentage mapping (Kotlin)

| Phase | Display percent |
|-------|-----------------|
| `compressing` / `compressed` | `overall_percent` (0–94) |
| `building_scripts` | 95 |
| `writing_zip` | 97 |
| Final (after JNI returns) | 100 |

## File I/O Paths on Android

### App-Specific Storage (Primary)

All file operations default to app-internal storage:

```
/data/data/com.hoshiyomi.otaku/files/
├── input/                    # User-provided input files (copied from SAF)
│   ├── boot.img
│   ├── vendor.img
│   └── odm.img
└── output/                   # Output OTA ZIPs (or user-selected directory)
    └── flashable_device.zip
```

### Temp Storage (Bundle Build)

```
<std::env::temp_dir()>/
└── otaku_build_tmp.bin       # Incremental bundle build (header + compressed partitions)
                              # Deleted after ZIP is finalized
```

### Shared Storage (Secondary)

```
/storage/emulated/0/OTAku/    # Default output directory
    └── flashable_device.zip
```

### Progress Sidecar

```
<output_path>.progress        # Same dir as output ZIP, .progress suffix
                              # Written by Rust, polled by Kotlin, deleted on completion
```

## Thread Model

```
Main Thread (UI)
├── Button clicks → dispatch to buildScope (application-scoped coroutine)
├── Progress pop-up + notification updates → mirrored via
│   showProgressNotification (T29)
└── TextView log append → posted to Main thread via onOutputLine callback

buildScope (Application-scoped CoroutineScope)
├── OTABridge.dd() → withContext(Dispatchers.IO) → NativeBridge.buildDd()
├── extractPayloadPartition() per partition (sidecar poller per call)
├── writePayload() → NativeBridge (single JNI call) + sidecar poller
├── verifyPayload() → NativeBridge (single fast JNI call)
├── WakeLock held during build / extract / payload-write operations (via OTAService foreground service)
├── Process.setThreadPriority(-10) for the build thread
└── Progress polling coroutine (Dispatchers.IO, 500ms interval)

Rust Native Thread (in-process, called from Kotlin Dispatchers.IO)
├── Compression runs synchronously on the calling thread
├── Progress sidecar writes are non-blocking (best-effort fs::write)
└── Returns to Kotlin when complete (no callbacks)
```

## Flasher Script Pipeline (update-binary)

The generated `META-INF/com/google/android/update-binary` is a POSIX `sh` script invoked by TWRP/OrangeFox with `update-binary 3 <fd> <zip>`. It runs these steps in order:

> **Recovery scope — custom recovery only.** These ZIPs target TWRP/OrangeFox
> (or `fastbootd`); a stock OEM recovery rejects them at its signature gate
> because the ZIP carries no `META-INF/CERT.RSA`/`.SF`/`MANIFEST.MF` block
> (OTAku is not an OEM signer). Stock ramdisks additionally lack the command
> set the script requires (`od`, `awk`, `readlink`, `blockdev`, `mkfifo`,
> `sha256sum`, …) and never ship `lptools`. This is a deliberate constraint
> of the format, not a compatibility defect.

| Step | Phase | Action |
|------|-------|--------|
| 0 | Open payload | Direct ZIP read (dd at computed local-file-header offset — `otaku.bin` is ZIP-Stored); fallback: extract to `/tmp/` + size verify against ZIP central directory listing |
| 1 | Pre-flash verify | Validate partition table: offset bounds, hash format (64 hex chars), unc_size > 0, 4096 alignment |
| 2 | Integrity + decompressor | Verify DDBU magic, version, compress_id, num_parts, header_size; check decompressor binary exists |
| 3 | Device check (optional) | Compare `TARGET_DEVICE` against `getprop` + `/vendor/build.prop` fallbacks; detected mismatch aborts, undetectable codename warns and proceeds (no interactivity — `read`/`choose` are unusable in the update-binary protocol pipe) |
| 4 | Slot detection | Read `androidboot.slot_suffix` from `/proc/cmdline` and `getprop` |
| 5 | Partition validation | Check each block device exists, is a block device, is unmounted; collect resize list |
| 6 | Resize dynamic partitions | Use `lptools resize` (fallback: `lptools remove`+`create`); track original sizes for rollback |
| 7+ | Flash each partition | FIFO pipeline: `dd extract → decompressor → dd write`; verify with `sha256sum` if not skipped |
| Final | Slot verify + sync | Verify active slot didn't change; `bootctl set-active-boot-slot` if needed; `sync` |

### Cleanup Trap

`trap cleanup_abort EXIT INT TERM HUP` — on any exit (including signals), restores resized dynamic partitions to their original sizes via `RESIZED_ORIGINAL` list, then re-maps all dynamic partitions. `CLEANUP_DONE` guard prevents double-execution. Disabled via `CLEANUP_DONE=1` on successful completion.

### Dynamic Partition Names

The flasher recognizes these dynamic partition names (lives inside `super`, resizable via `lptools`):

| Category | Names |
|----------|-------|
| AOSP standard | `system`, `vendor`, `product`, `system_ext`, `odm`, `odm_dlkm`, `vendor_dlkm` |
| Xiaomi | `mi_ext`, `my_product`, `my_engineering`, `my_stock`, `my_carrier`, `my_region`, `my_bigball`, `my_preload`, `my_company` |
| Samsung | `optics`, `prism` |
| Other | `cache`, `userdata` |

Adding a name is safe — `is_dynamic_partition()` returns false for partitions that don't exist on the device.

## CI Artifact Pipeline (T24)

The Build workflow publishes artifacts in two layers. Each matrix leg
uploads its own `OTAku-<abi>-release` (APK plus the R8 `mapping.txt`
required to de-obfuscate crashes from the minified release build) and
`OTAku-<abi>-debug` artifacts. A final `publish` job then downloads every
`OTAku-*` artifact of the run and re-uploads one flat `OTAku-all-builds`
bundle containing all six APKs plus the three mapping files.

The final job exists because GitHub's run page lists artifacts per
attempt: after "re-run failed jobs" (for example a transient Maven
Central outage), the run summary only shows artifacts uploaded during
the latest attempt. Because the publish job always runs in the newest
attempt (`if: !cancelled()`) and every upload sets `overwrite: true`,
the complete bundle is re-published after any rerun and remains visible
on the main run page. Artifact names are never suffixed with the attempt
number; reruns replace same-named artifacts instead of conflicting.

ABI packaging invariant: `ndk.abiFilters` must be declared ONLY on the
product flavors. AGP unions `defaultConfig`'s filters into every flavor,
which once silently un-filtered all three APKs (each shipped both ABIs).
The `Verify APK ABI filtering` CI step fails the build if an APK's ABI
set does not exactly match its flavor.

## Non-Root Limitations

### What Works Without Root

| Operation | Mechanism | Storage Location |
|-----------|-----------|-----------------|
| Select partition images | SAF file picker | App storage / SAF |
| Compress partition images | Rust native library (flate2, bzip2, xz2, zstd/lz4) | App storage / SAF |
| Generate flashable ZIP | Rust `zip` crate (ZIP64, Stored) | App storage / SAF |
| Stream progress to UI | JSON sidecar file polling | App storage |
| Verify post-flash integrity | `sha256sum` in flasher script | N/A (recovery-side) |

### What Does NOT Work Without Root

| Operation | Reason | Workaround |
|-----------|--------|------------|
| Direct dd to block device | `/dev/block/by-name/*` requires root | Generate flashable ZIP, flash via recovery |
| Read current partition | `/dev/block/by-name/*` requires root | Use extracted OTA images |
| Write to /system or /vendor | Read-only mounted partitions | Generate OTA ZIP, flash via recovery |
| Resize dynamic partitions at runtime | `lptools` requires root | Deferred to recovery flasher script |
