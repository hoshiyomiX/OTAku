# OTAku

A **non-root** Android app for building flashable OTA ZIP packages from partition images — directly on your phone, no PC required.

> **No root. No Termux. No PC.** Just an APK and your partition images.

---

## Features

- **DD-mode flashable ZIP generation** — Build otaku-format flashable ZIPs that work with TWRP/OrangeFox recovery
- **Multiple compression algorithms** — none, gzip, bzip2, xz, brotli (all statically compiled into the native library)
- **Per-partition progress tracking** — Real-time compression progress for each partition via a JSON sidecar file
- **Device safety check** — Prevents flashing on wrong device models (comma-separated codename list supported)
- **SHA-256 verification** — Post-flash integrity verification (optional, fast 1MB-block FIFO-pipeline hash)
- **Dynamic partition resize** — Resizes `system`, `vendor`, `product`, `system_ext`, `odm*`, `vendor_dlkm`, plus OEM-specific partitions (`mi_ext`, `my_*`, `optics`, `prism`) via `lptools`
- **A/B slot awareness** — Detects active slot from `/proc/cmdline` + `getprop`, flashes to the inactive slot
- **ZIP64 support** — `otaku.bin` can exceed 4 GB
- **Dark/Light/System theme** — Material Design 3 with theme toggle

### Compression Support

| Algorithm | Ratio | Speed | Notes |
|-----------|-------|-------|-------|
| `none`    | 100%  | Fastest | No compression |
| `gzip`    | ~60%  | Fast    | Best balance, uses `flate2`/`miniz_oxide` |
| `bzip2`   | ~50%  | Medium  | Uses `bzip2` crate |
| `xz`      | ~45%  | Slow    | Smallest output, uses `xz2`/`liblzma` |
| `brotli`  | ~40%  | Slow    | Best ratio, pure-Rust `brotli` crate |

All five algorithms are **always available** — they are statically compiled into `libotaku_native.so`. There are no runtime dependency checks.

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Android APK                          │
│  ┌───────────────────────────────────────────────────┐  │
│  │              Kotlin UI Layer                      │  │
│  │  MainActivity (Material Design 3)                  │  │
│  │  ├─ Device codename + Auto-detect                 │  │
│  │  ├─ Partition image picker (SAF)                   │  │
│  │  ├─ Compression selector + level                   │  │
│  │  ├─ Per-partition progress bars                    │  │
│  │  └─ Log output + Copy/Clear                        │  │
│  └───────────────────────┬───────────────────────────┘  │
│                          │                              │
│  ┌───────────────────────▼───────────────────────────┐  │
│  │           OTABridge.kt                             │  │
│  │  Translates UI actions → NativeBridge calls        │  │
│  │  Polls <output>.progress sidecar every 500ms       │  │
│  └───────────────────────┬───────────────────────────┘  │
│                          │                              │
│  ┌───────────────────────▼───────────────────────────┐  │
│  │           NativeBridge.kt                          │  │
│  │  JNI wrapper around libotaku_native.so             │  │
│  │  JSON in/out — parses Rust results to data classes │  │
│  └───────────────────────┬───────────────────────────┘  │
│                          │ JNI                          │
│  ┌───────────────────────▼───────────────────────────┐  │
│  │     libotaku_native.so (Rust cdylib)               │  │
│  │  cargo-ndk compiled for arm64-v8a + armeabi-v7a    │  │
│  │  Statically links: flate2, bzip2, xz2, brotli,     │  │
│  │                    sha2, prost, zip, chrono         │  │
│  │  Modules:                                          │  │
│  │  ├─ lib.rs     JNI entry points (8 external fns)   │  │
│  │  ├─ dd.rs      DD-mode flashable ZIP generator     │  │
│  │  ├─ payload.rs AOSP payload.bin read/write         │  │
│  │  ├─ proto.rs   Hand-written prost structs          │  │
│  │  └─ compression.rs  gzip/bz2/xz/brotli + SHA-256   │  │
│  └───────────────────────────────────────────────────┘  │
│                                                         │
│  ┌───────────────────────────────────────────────────┐  │
│  │        Android Storage                             │  │
│  │  ├─ App-internal: /data/data/.../files/            │  │
│  │  └─ Shared: /storage/emulated/0/OTAku/             │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

### Key Components

- **Rust native backend** — `libotaku_native.so` is compiled by `cargo-ndk` for `arm64-v8a` and `armeabi-v7a`, placed in `android/app/src/main/jniLibs/` by the CI build step. All compression algorithms are statically linked — no runtime dependency checks.
- **NativeBridge.kt** — Kotlin `object` that loads `libotaku_native.so` via `System.loadLibrary("otaku_native")` and exposes typed wrappers (`buildDd`, `readPayload`, `extractPartition`, `compress`, `decompress`, etc.). Each wrapper parses the JSON returned by Rust into a typed data class.
- **OTABridge.kt** — High-level Kotlin API. `OTABridge.dd()` calls `NativeBridge.buildDd()` on `Dispatchers.IO`, polls `<output_path>.progress` every 500ms to emit `ProgressUpdate` callbacks to the UI, and translates `DdBuildResult` into `OTAResult`.
- **OTAService.kt** — Foreground service that holds a `WakeLock` during long builds so the OS doesn't kill the app under Doze.
- **DD mode (`rust/src/dd.rs`)** — Generates an otaku-format flashable ZIP containing `otaku.bin` (DDBU header + compressed partition data), `META-INF/com/google/android/update-binary` (TWRP/OrangeFox flasher script), `META-INF/com/google/android/updater-script` (stub), and `flash_info.txt` (human-readable metadata).

### otaku.bin (DDBU) Format

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0      | 4 B  | magic | `DDBU` (0x44 0x44 0x42 0x55) |
| 4      | 2 B  | version | u16 LE, currently `1` |
| 6      | 2 B  | compress_id | u16 LE: 0=none, 1=gzip, 2=bzip2, 3=xz, 4=brotli |
| 8      | 2 B  | num_parts | u16 LE |
| 10     | 2 B  | header_size | u16 LE, always `4096` |
| 12     | 4084 B | padding | zero-pad to 4096 |
| 4096   | …    | data | each partition compressed, 4096-aligned |

### Progress Reporting Mechanism

Rust writes a JSON sidecar file at `<output_path>.progress` after every 4 MB chunk during compression. Kotlin polls this file every 500ms and emits `ProgressUpdate` callbacks to the UI. This avoids JNI callback complexity (which previously failed in v3.4/v3.5 due to `JNIEnv` reentrancy, exception-clearing, and local-reference-table overflow issues — see commit `c292e88`).

The sidecar JSON contains: `current`, `total`, `name`, `phase`, `bytes_written`, `tmp_path`, `total_estimated`, `partition_percent`, `overall_percent`.

---

## Build Requirements

| Tool | Version | Notes |
|------|---------|-------|
| Android Studio | Hedgehog (2023.1.1)+ | Or IntelliJ IDEA with Android plugin |
| Android SDK | API 34 | compileSdk |
| Android NDK | r26d | For `cargo-ndk` cross-compilation |
| JDK | 17 | |
| Rust | stable | `cargo-ndk` installed via `cargo install cargo-ndk` |
| Gradle | 8.6 | Downloaded by CI; local dev can use the wrapper |

No Python toolchain is required. The entire native backend is Rust.

---

## Build Instructions

### 1. Clone the repository

```bash
git clone https://github.com/hoshiyomiX/OTAku.git
cd OTAku
```

### 2. Build the Rust native library

```bash
cd rust
# Install targets if not already present
rustup target add aarch64-linux-android armv7-linux-androideabi
cargo install cargo-ndk

# Set ANDROID_NDK_HOME (adjust path to your NDK installation)
export ANDROID_NDK_HOME=/path/to/ndk/26.3.11579264

# Build .so files for both ABIs
cargo ndk \
  -t arm64-v8a \
  -t armeabi-v7a \
  -o ../android/app/src/main/jniLibs \
  build --release
```

The output `.so` files will be at:
```
android/app/src/main/jniLibs/arm64-v8a/libotaku_native.so
android/app/src/main/jniLibs/armeabi-v7a/libotaku_native.so
```

### 3. Build the APK

```bash
cd ../android
./gradlew assembleArm8Debug
```

The debug APK will be at:
```
android/app/build/outputs/apk/arm8/debug/OTAku-1.0.0-arm8-debug.apk
```

### 4. Build a release APK (signed)

```bash
cd android
./gradlew assembleArm8Release
```

Configure signing in `android/app/build.gradle` under `signingConfigs`. The CI build currently uses the debug key for release builds (see `.github/workflows/build.yml`).

### 5. Available build flavors

| Flavor | ABI | Use case |
|--------|-----|----------|
| `arm7` | `armeabi-v7a` | 32-bit ARM (older devices) |
| `arm8` | `arm64-v8a` | 64-bit ARM (modern devices) |
| `universal` | both | Larger APK, runs on either |

---

## Tested Devices

| Device | SoC | Android Version | Status |
|--------|-----|-----------------|--------|
| itel S666LN | UNISOC T606 | 13 (Tiramisu) | Planned |
| Samsung Galaxy A54 | Exynos 1380 | 14 (Upside Down Cake) | Planned |
| Google Pixel 7 | Tensor G2 | 14 (Upside Down Cake) | Planned |
| Xiaomi Redmi Note 12 | Snapdragon 685 | 13 (Tiramisu) | Planned |

> **minSdk 26** (Android 8.0 Oreo)

---

## Project Structure

```
OTAku/
├── rust/                             # Rust native backend
│   ├── Cargo.toml                    # otaku-native crate (cdylib)
│   ├── build.rs                      # prost-build config (currently no-op)
│   ├── proto/
│   │   └── update_metadata.proto     # AOSP payload.bin protobuf schema
│   └── src/
│       ├── lib.rs                    # JNI entry points (8 external fns)
│       ├── dd.rs                     # DD-mode flashable ZIP generator
│       ├── payload.rs                # AOSP payload.bin read/write
│       ├── proto.rs                  # Hand-written prost structs
│       └── compression.rs            # gzip/bz2/xz/brotli + SHA-256
├── android/                          # Android project
│   ├── settings.gradle.kts           # Plugin management + project includes
│   ├── build.gradle.kts              # Root build file
│   └── app/
│       ├── build.gradle              # App-level build config (Groovy DSL)
│       ├── proguard-rules.pro
│       └── src/main/
│           ├── AndroidManifest.xml
│           ├── jniLibs/              # Populated by cargo-ndk (gitignored)
│           │   ├── arm64-v8a/libotaku_native.so
│           │   └── armeabi-v7a/libotaku_native.so
│           └── java/com/hoshiyomi/otaku/
│               ├── MainActivity.kt    # Main UI activity
│               ├── OTABridge.kt       # Kotlin→Native bridge + OTAResult + ProgressUpdate
│               ├── NativeBridge.kt    # JNI wrapper + result data classes
│               ├── OTAkuApp.kt        # Application class + notification channel
│               ├── service/
│               │   └── OTAService.kt  # Foreground service for builds (WakeLock)
│               └── data/
│                   └── BackupAgent.kt # Backup helper
├── .github/workflows/
│   ├── build.yml                     # CI: build Rust .so → build debug + release APK
│   └── smoke-test.yml                # CI: regression tests on generated flasher script
├── docs/
│   └── ARCHITECTURE.md               # Detailed architecture + data flow
├── LICENSE
├── .gitignore
└── README.md
```

---

## Output ZIP Contents

A flashable ZIP generated by OTAku contains:

| File | Purpose |
|------|---------|
| `otaku.bin` | DDBU-format bundle: 4096-byte header + compressed partition data, 4096-aligned |
| `META-INF/com/google/android/update-binary` | TWRP/OrangeFox flasher shell script (executed by recovery) |
| `META-INF/com/google/android/updater-script` | Stub (`assert(1==1);`) — required by recovery but logic lives in update-binary |
| `flash_info.txt` | Human-readable metadata: compression, bundle size, per-partition SHA-256 |

### Flasher Script Pipeline (update-binary)

The flasher script runs these steps in order:

1. **Extract otaku.bin** from the ZIP (with pre-extract ZIP-listing size verification)
2. **Pre-flash partition table verify** — checks structural integrity (offset bounds, hash format, alignment) before touching any block device
3. **Bundle integrity + decompressor availability** — validates DDBU magic, version, compress_id, partition count; checks that the decompressor binary exists
4. **Device compatibility** (optional) — compares `TARGET_DEVICE` against `getprop` + `build.prop` + `/proc/cmdline` fallbacks, with `choose`/`read`/default-abort interactive confirmation
5. **Slot detection** — reads `androidboot.slot_suffix` from `/proc/cmdline` and `getprop`
6. **Partition validation** — checks each target block device exists, is a block device, is unmounted; collects resize list
7. **Resize dynamic partitions** — uses `lptools resize` (with `lptools remove`+`create` fallback); tracks original sizes in `RESIZED_ORIGINAL` for cleanup-trap rollback
8. **Flash each partition** — FIFO pipeline: `dd extract → decompressor → dd write` (avoids tmpfs exhaustion for 4GB+ partitions)
9. **Post-flash SHA-256 verify** (optional) — fast 1MB-block FIFO-pipeline hash; falls back to legacy 4KB path if `mkfifo`/`sha256sum` unavailable
10. **Slot verification** — verifies active slot didn't change mid-flash, calls `bootctl set-active-boot-slot` if needed

A cleanup trap (`trap cleanup_abort EXIT INT TERM HUP`) restores resized partitions and re-maps dynamic partitions if the script aborts.

---

## Recovery Requirements

The generated flasher script requires:

- **TWRP** or **OrangeFox** recovery (or any recovery that supports the `update-binary 3 <fd> <zip>` calling convention)
- **`lptools`** — required for dynamic partition resize. Available in most modern OrangeFox/TWRP builds with `OF_ENABLE_LPTOOLS=1`. Without lptools, dynamic partition resize will fail (the script aborts with a clear error message).
- **`busybox`** or **`toybox`** — provides `dd`, `unzip`, `awk`, `grep`, `mount`, `umount`, `blockdev`, `getprop`, `mkfifo`, `sha256sum`
- **`/tmp` writable** — used for `otaku.bin` extraction and FIFO pipelines

---

## License

Licensed under the MIT License — see [LICENSE](LICENSE).

This project is provided "as is" for educational and personal use.

---

## Acknowledgments

- [AOSP update_engine](https://android.googlesource.com/platform/system/update_engine/) — payload.bin format specification
- [payload_dumper](https://github.com/vm03/payload-dumper) — reference Python implementation of payload.bin parsing
- [phhusson/vendor_lptools](https://github.com/phhusson/vendor_lptools) — dynamic partition management tool used by the flasher script
