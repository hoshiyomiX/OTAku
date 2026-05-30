# OTAku

A **non-root** Android app for building flashable OTA ZIP packages from partition images — directly on your phone, no PC required.

> **No root. No Termux. No PC.** Just an APK and your partition images.

---

## Features

- **DD-mode flashable ZIP generation** — Build otaku-format flashable ZIPs that work with TWRP/OrangeFox recovery
- **Multiple compression algorithms** — none, gzip, bzip2, xz, brotli
- **Per-partition progress tracking** — Real-time compression progress for each partition
- **Device safety check** — Prevents flashing on wrong device models
- **SHA-256 verification** — Post-flash integrity verification (optional)
- **Dark/Light/System theme** — Material Design 3 with theme toggle

### Compression Support

| Algorithm | Ratio | Speed | Notes |
|-----------|-------|-------|-------|
| `none`    | 100%  | Fastest | No compression |
| `gzip`    | ~60%  | Fast    | Best balance |
| `bzip2`   | ~50%  | Medium  | Requires libbz2 |
| `xz`      | ~45%  | Slow    | Smallest output |
| `brotli`  | ~40%  | Slow    | Best ratio, requires brotli package |

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
│  │           OTABridge.kt                            │  │
│  │  Translates UI actions → Python function calls     │  │
│  └───────────────────────┬───────────────────────────┘  │
│                          │                              │
│  ┌───────────────────────▼───────────────────────────┐  │
│  │           PythonBridge.kt                         │  │
│  │  JNI (PyBridge) or ProcessBuilder execution        │  │
│  └───────────────────────┬───────────────────────────┘  │
│                          │                              │
│  ┌───────────────────────▼───────────────────────────┐  │
│  │        Python Runtime (CPython 3.13)              │  │
│  │  otaku/                                           │  │
│  │  ├─ __init__.py (CLI bridge)                       │  │
│  │  ├─ protobuf.py (minimal PB encoder/decoder)       │  │
│  │  ├─ compression.py (gzip/bz2/xz/brotli)           │  │
│  │  ├─ payload.py (read/write payload.bin)            │  │
│  │  ├─ ota_metadata.py (OTA ZIP metadata gen)         │  │
│  │  └─ modes/                                        │  │
│  │      └─ dd.py (dd-based flashable ZIP generator)   │  │
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

- **Bundled Python** — CPython 3.13 embedded in the APK via jniLibs, no external Python runtime needed
- **otaku** Python package (`src/otaku/`) implements DD-mode flashable ZIP generation with streaming compression
- **Zero external Python dependencies** — implements minimal protobuf encoder/decoder from scratch, using only Python stdlib modules
- **OTABridge.kt** provides a type-safe Kotlin API that bridges UI actions to Python execution
- **PyBridge.kt + pybridge.c** — JNI bridge using dlopen() + Py_Main() for in-process Python execution (no execve, no linker issues)

---

## Build Requirements

| Tool | Version | Notes |
|------|---------|-------|
| Android Studio | Hedgehog (2023.1.1)+ | Or IntelliJ IDEA with Android plugin |
| Android SDK | API 34 | compileSdk |
| JDK | 17 | |
| Python | 3.12+ | For build scripts only (not embedded in APK) |
| Gradle | 8.4+ | Via Android Gradle Plugin 8.2+ |

---

## Build Instructions

### 1. Clone the repository

```bash
git clone https://github.com/hoshiyomiX/OTAku.git
cd OTAku
```

### 2. Build the APK

```bash
cd android
./gradlew assembleArm8Debug
```

The debug APK will be at:
```
android/app/build/outputs/apk/arm8/debug/OTAku-3.18.0-arm8-debug.apk
```

### 3. Build a release APK (signed)

```bash
cd android
./gradlew assembleArm8Release
```

Configure signing in `android/app/build.gradle` under `signingConfigs`.

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
├── src/otaku/                        # Standalone Python package
│   ├── __init__.py                   # Main entry point, argument parsing
│   ├── protobuf.py                   # Minimal PB encoder/decoder
│   ├── compression.py                # gzip, bz2, xz, brotli + pure-Python SHA-256 fallback
│   ├── payload.py                    # AOSP payload.bin read/write
│   ├── ota_metadata.py               # OTA ZIP metadata generation
│   └── modes/
│       ├── __init__.py
│       └── dd.py                     # DD-mode flashable ZIP generator
├── android/                          # Android project
│   ├── settings.gradle.kts           # Plugin management + project includes
│   └── app/
│       ├── build.gradle              # App-level build config
│       └── src/main/
│           ├── AndroidManifest.xml
│           ├── java/com/hoshiyomi/otaku/
│           │   ├── MainActivity.kt    # Main UI activity
│           │   ├── OTABridge.kt       # Kotlin→Python bridge
│           │   ├── OTAResult.kt       # (embedded in OTABridge.kt)
│           │   ├── PythonBridge.kt    # Python runtime manager
│           │   ├── PyBridge.kt        # JNI wrapper (dlopen + Py_Main)
│           │   ├── OTAkuApp.kt        # Application class + notification channel
│           │   ├── service/
│           │   │   └── OTAService.kt  # Foreground service for builds
│           │   └── data/
│           │       └── BackupAgent.kt # Backup helper
│           └── res/
│               ├── values/
│               ├── layout/
│               ├── drawable/
│               ├── mipmap-*/
│               └── xml/
├── .github/workflows/
│   ├── build.yml                     # CI: debug + release APK on push/PR
│   ├── release.yml                   # Tag-triggered GitHub Release
│   ├── build-aarch64.yml             # .pyz-only CI
│   └── release-aarch64.yml           # .pyz-only release
├── scripts/
│   ├── build_pyz.py                  # Build otaku.pyz zipapp
│   ├── prepare_python_runtime.sh     # Package Termux Python for jniLibs
│   ├── validate_elf.py               # ELF validation utility
│   └── jni/
│       └── pybridge.c                # JNI bridge (dlopen + Py_Main)
├── docs/
│   └── ARCHITECTURE.md
├── .gitignore
└── README.md
```

---

## License

This project is provided as-is for educational and personal use. The underlying `otaku` package implements the AOSP payload.bin v2 (Brillo) format specification and the otaku dd-based flashable ZIP format.

---

## Acknowledgments

- [AOSP update_engine](https://android.googlesource.com/platform/system/update_engine/) — payload.bin format specification
- [payload_dumper](https://github.com/nicholasgasior/payload-dumper) — reference implementation
