//! otaku-native — Rust + JNI native backend for OTAku Android
//!
//! Replaces the Python .pyz + Termux .so runtime with a single
//! cargo-ndk compiled .so that Android's bionic linker loads
//! natively — zero ELF hacks, zero LD_PRELOAD, zero dlopen hacks.
//!
//! JNI naming convention:
//!   Java_com_hoshiyomi_otaku_NativeBridge_<method_name>

use jni::objects::JClass;
use jni::objects::JString;
use jni::sys::{jboolean, jint, jstring};
use jni::JNIEnv;

// ---------------------------------------------------------------------------
//  Modules
// ---------------------------------------------------------------------------

pub mod proto;
pub mod compression;
pub mod payload;
pub mod dd;

// ---------------------------------------------------------------------------
//  JNI: nativeGetVersion
// ---------------------------------------------------------------------------

/// Return the native library version string.
///
/// Kotlin: `external fun nativeGetVersion(): String`
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeGetVersion(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let version = format!(
        "otaku-native {} (rust)",
        env!("CARGO_PKG_VERSION")
    );
    match env.new_string(version) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            let fallback = env.new_string("otaku-native (unknown)");
            match fallback {
                Ok(s) => s.into_raw(),
                Err(_) => std::ptr::null_mut(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeCheckDeps
// ---------------------------------------------------------------------------

/// Check which compression algorithms are available.
///
/// With Rust static linking, ALL algorithms are always available.
/// No more "bz2 unavailable" or "OpenSSL not available" errors.
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeCheckDeps(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let result = serde_json::json!({
        "available": ["none", "gzip", "bzip2", "xz", "brotli"],
        "missing": [],
        "all_ok": true,
        "native_version": env!("CARGO_PKG_VERSION")
    });
    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeReadPayload
// ---------------------------------------------------------------------------

/// Read and parse a payload.bin file.
///
/// Kotlin: `external fun nativeReadPayload(path: String): String`
///
/// Returns JSON:
///   { "header": {...}, "manifest": {...}, "data_offset": N, "file_size": N }
/// Or on error:
///   { "success": false, "error": "..." }
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeReadPayload(
    mut env: JNIEnv,
    _class: JClass,
    path: JString,
) -> jstring {
    let path_str: String = match env.get_string(&path) {
        Ok(s) => s.into(),
        Err(_) => {
            return make_error_json(&env, "Invalid path string");
        }
    };

    let result = match payload::read_payload(&path_str) {
        Ok(info) => {
            let json = payload::payload_info_to_json(&info);
            serde_json::json!({
                "success": true,
                "header": json.header,
                "manifest": json.manifest,
                "data_offset": json.data_offset,
                "file_size": json.file_size,
                "native_version": env!("CARGO_PKG_VERSION"),
            })
        }
        Err(e) => {
            serde_json::json!({
                "success": false,
                "error": e,
                "native_version": env!("CARGO_PKG_VERSION"),
            })
        }
    };

    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeExtractPartition
// ---------------------------------------------------------------------------

/// Extract and decompress a partition from a payload.bin file.
///
/// Kotlin: `external fun nativeExtractPartition(
///     payloadPath: String, partitionName: String, outputPath: String
/// ): String`
///
/// Extracts the partition image and saves it to outputPath.
/// Returns JSON with success/error and file info.
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeExtractPartition(
    mut env: JNIEnv,
    _class: JClass,
    payload_path: JString,
    partition_name: JString,
    output_path: JString,
) -> jstring {
    let payload_str: String = match env.get_string(&payload_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid payload path"),
    };
    let partition_str: String = match env.get_string(&partition_name) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid partition name"),
    };
    let output_str: String = match env.get_string(&output_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid output path"),
    };

    let start = std::time::Instant::now();

    // Read payload info
    let info = match payload::read_payload(&payload_str) {
        Ok(i) => i,
        Err(e) => {
            return make_error_json(&env, &format!("Read payload failed: {}", e));
        }
    };

    // Extract and decompress
    let data = match payload::extract_and_decompress_partition(&info, &partition_str) {
        Ok(d) => d,
        Err(e) => {
            return make_error_json(
                &env,
                &format!("Extract partition '{}' failed: {}", partition_str, e),
            );
        }
    };

    // Write to output file
    if let Some(parent) = std::path::Path::new(&output_str).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    match std::fs::write(&output_str, &data) {
        Ok(_) => {}
        Err(e) => {
            return make_error_json(&env, &format!("Write output failed: {}", e));
        }
    }

    let elapsed = start.elapsed();
    let file_size = data.len() as u64;
    let result = serde_json::json!({
        "success": true,
        "partition": partition_str,
        "output_path": output_str,
        "file_size": file_size,
        "human_size": payload::human_size(file_size),
        "duration_ms": elapsed.as_millis() as u64,
        "native_version": env!("CARGO_PKG_VERSION"),
    });

    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeWritePayload
// ---------------------------------------------------------------------------

/// Generate a payload.bin from partition images.
///
/// Kotlin: `external fun nativeWritePayload(
///     imagesJson: String, compression: String, level: Int,
///     outputPath: String, blockSize: Int, minorVersion: Int
/// ): String`
///
/// imagesJson: {"partition_name": "/path/to/image.img", ...}
/// Returns JSON with success/error and partition summaries.
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeWritePayload(
    mut env: JNIEnv,
    _class: JClass,
    images_json: JString,
    compression: JString,
    _level: jint,
    output_path: JString,
    block_size: jint,
    minor_version: jint,
) -> jstring {
    let images_str: String = match env.get_string(&images_json) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid images JSON"),
    };
    let comp_str: String = match env.get_string(&compression) {
        Ok(s) => s.into(),
        Err(_) => "gzip".to_string(),
    };
    let output_str: String = match env.get_string(&output_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid output path"),
    };

    // Parse images JSON: {"partition_name": "path", ...}
    let images: std::collections::HashMap<String, String> = match serde_json::from_str(&images_str)
    {
        Ok(m) => m,
        Err(e) => {
            return make_error_json(&env, &format!("Invalid images JSON: {}", e));
        }
    };

    let partitions_data: Vec<payload::PartitionData> = images
        .into_iter()
        .map(|(name, path)| payload::PartitionData {
            name,
            image_path: path,
            compress: comp_str.clone(),
        })
        .collect();

    let result = payload::write_payload(
        &output_str,
        &partitions_data,
        if block_size > 0 {
            block_size as u32
        } else {
            payload::DEFAULT_BLOCK_SIZE
        },
        minor_version as u32,
    );

    // Serialize result to JSON
    let json = serde_json::to_string(&result).unwrap_or_else(|e| {
        format!(
            "{{\"success\":false,\"error\":\"Serialize error: {}\"}}",
            e
        )
    });

    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeBuildDd
// ---------------------------------------------------------------------------

/// Build a DD-mode flashable ZIP from partition images.
///
/// Kotlin: `external fun nativeBuildDd(...): String`
///
/// @param images_json   JSON: {"partition_name": "/path/to/image.img", ...}
/// @param compression   "none" | "gzip" | "bzip2" | "xz" | "brotli"
/// @param level         Compression level (0 = default)
/// @param output_path   Absolute path for output .zip
/// @param device        Device codename(s), comma-separated
/// @param skip_verify   Skip SHA-256 post-flash verification (0/1)
/// @return JSON result: {"success": bool, "output": str, "error": str|null,
///                       "zip_path": str|null, "zip_size": int|null, "bundle_size": int|null,
///                       "duration_ms": int, "native_version": str}
///
/// Progress is reported via a sidecar file at `<output_path>.progress` that
/// Kotlin polls every 500ms. This avoids JNI callback complexity (which
/// failed in v3.4/v3.5 due to JNIEnv issues, exception clearing, and
/// local reference table overflow).
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeBuildDd(
    mut env: JNIEnv,
    _class: JClass,
    images_json: JString,
    compression: JString,
    level: jint,
    output_path: JString,
    device: JString,
    skip_verify: jboolean,
) -> jstring {
    // Parse JNI string arguments
    let images_str: String = match env.get_string(&images_json) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid images JSON"),
    };
    let comp_str: String = match env.get_string(&compression) {
        Ok(s) => s.into(),
        Err(_) => "gzip".to_string(),
    };
    let output_str: String = match env.get_string(&output_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid output path"),
    };
    let device_str: String = match env.get_string(&device) {
        Ok(s) => s.into(),
        Err(_) => String::new(),
    };

    // Parse images JSON: {"partition_name": "path", ...}
    let images_map: std::collections::HashMap<String, String> = match serde_json::from_str(&images_str) {
        Ok(m) => m,
        Err(e) => {
            return make_error_json(&env, &format!("Invalid images JSON: {}", e));
        }
    };

    // Convert HashMap to Vec<(String, String)> and sort alphabetically by partition name.
    // Kotlin sorts partition names for the progress bar (images.keys.sorted()),
    // so Rust must compress in the same order so that log messages and per-partition
    // progress bar indices match.
    let mut images_vec: Vec<(String, String)> = images_map.into_iter().collect();
    images_vec.sort_by(|a, b| a.0.cmp(&b.0));

    // Call the DD build pipeline
    let result = dd::run_dd_build(
        &images_vec,
        &comp_str,
        level,
        &output_str,
        &device_str,
        skip_verify != 0,
    );

    // Serialize result to JSON
    let json = serde_json::to_string(&result).unwrap_or_else(|e| {
        format!(
            "{{\"success\":false,\"error\":\"Serialize error: {}\"}}",
            e
        )
    });

    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeDetectDeviceCodename
// ---------------------------------------------------------------------------

/// Detect device codename from vendor partition properties (spoof-resistant).
///
/// Kotlin: `external fun nativeDetectDeviceCodename(): String`
///
/// Reads 4 sources in priority order:
///   1. `getprop ro.product.vendor.device` (vendor partition, hard to spoof)
///   2. `getprop ro.product.board`        (vendor partition, hard to spoof)
///   3. `/vendor/build.prop ro.product.vendor.device` (fallback if getprop empty)
///   4. `/vendor/build.prop ro.product.board`         (fallback if getprop empty)
///
/// If `ro.product.vendor.device` and `ro.product.board` differ, returns BOTH
/// as a comma-separated string: `"vendor_device,board"`. This matches the
/// flasher script's comma-separated TARGET_DEVICE format.
///
/// Returns JSON:
///   { "success": true, "codename": "alioth" | "alioth,sm8350" | ...,
///     "vendor_device": "alioth", "board": "sm8350",
///     "sources_tried": ["getprop ro.product.vendor.device", ...] }
/// Or on error:
///   { "success": false, "codename": "", "error": "all 4 sources empty" }
///
/// Why these 4 sources (and not Build.PRODUCT):
///   - `Build.PRODUCT` reads `ro.product.name` which is set by init from
///     `/system/build.prop` — easily overridden by Magisk resetprop or GSI.
///   - `ro.product.vendor.*` is set from `/vendor/build.prop` — vendor
///     partition is rarely modified by Magisk/GSI, so it survives spoofing.
///   - `ro.product.board` identifies the SoC/board — useful for devices
///     where vendor.device uses OEM-specific naming (e.g. `alioth` vs `sm8350`).
///
/// App and flasher validator use the SAME 4 sources in the SAME priority,
/// so they produce the same codename(s).
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeDetectDeviceCodename(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let result = detect_device_codename();
    match env.new_string(result) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Read device codename from 4 vendor-partition sources.
/// Returns JSON string for JNI bridge.
fn detect_device_codename() -> String {
    let mut sources_tried: Vec<&'static str> = Vec::new();
    let mut vendor_device = String::new();
    let mut board = String::new();

    // Helper: run getprop and trim output
    let getprop = |prop: &str| -> String {
        std::process::Command::new("getprop")
            .arg(prop)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    // Helper: parse /vendor/build.prop for a property key
    let parse_vendor_prop = |key: &str| -> String {
        std::fs::read_to_string("/vendor/build.prop")
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find_map(|line| {
                        let prefix = format!("{}=", key);
                        line.strip_prefix(&prefix).map(|v| {
                            v.trim().trim_matches('\r').to_string()
                        })
                    })
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_default()
    };

    // Source 1: getprop ro.product.vendor.device
    sources_tried.push("getprop ro.product.vendor.device");
    vendor_device = getprop("ro.product.vendor.device");

    // Source 2: getprop ro.product.board
    sources_tried.push("getprop ro.product.board");
    board = getprop("ro.product.board");

    // Source 3: /vendor/build.prop ro.product.vendor.device (fallback)
    if vendor_device.is_empty() {
        sources_tried.push("/vendor/build.prop ro.product.vendor.device");
        vendor_device = parse_vendor_prop("ro.product.vendor.device");
    }

    // Source 4: /vendor/build.prop ro.product.board (fallback)
    if board.is_empty() {
        sources_tried.push("/vendor/build.prop ro.product.board");
        board = parse_vendor_prop("ro.product.board");
    }

    // Build codename: if both present and differ → comma-separated
    // If both present and same → single value
    // If only one present → that one
    // If neither present → empty (error)
    let codename = if !vendor_device.is_empty() && !board.is_empty() {
        if vendor_device == board {
            vendor_device.clone()
        } else {
            format!("{},{}", vendor_device, board)
        }
    } else if !vendor_device.is_empty() {
        vendor_device.clone()
    } else if !board.is_empty() {
        board.clone()
    } else {
        String::new()
    };

    let success = !codename.is_empty();
    let error_val: serde_json::Value = if success {
        serde_json::Value::Null
    } else {
        serde_json::json!("all 4 sources returned empty")
    };

    serde_json::json!({
        "success": success,
        "codename": codename,
        "vendor_device": vendor_device,
        "board": board,
        "sources_tried": sources_tried,
        "native_version": env!("CARGO_PKG_VERSION"),
        "error": error_val
    }).to_string()
}

// ---------------------------------------------------------------------------
//  JNI: nativeScanDevicePartitions
// ---------------------------------------------------------------------------

/// Scan device for ACTUAL partition names present on this device (no root required).
///
/// Kotlin: `external fun nativeScanDevicePartitions(): String`
///
/// Reads /sys/block/ (world-readable on Android) to detect partitions that
/// physically exist on this device — NOT a static list of "what could exist".
///
/// Detection method:
///   - GPT partitions: /sys/block/sda/sda1/uevent → PARTNAME=<name>
///     (boot, dtbo, vbmeta, init_boot, recovery, super, userdata, etc.)
///   - dm-linear (dynamic) partitions: /sys/block/dm-0/dm/name → <name>
///     (system, vendor, product, system_ext, odm, vendor_dlkm, etc.)
///
/// Normalizes A/B slot suffixes (boot_a → boot, vbmeta_b → vbmeta).
/// Filters out dangerous/non-flashable partitions (userdata, cache, super,
/// persist, modem EFS, etc.) via BLOCKLIST.
///
/// Returns JSON:
///   { "success": true, "partitions": ["boot","system","vendor",...],
///     "dynamic_partitions": true, "slot_suffix": "_a",
///     "android_version": "16" }
/// Or on error (e.g. /sys/block/ not readable):
///   { "success": false, "partitions": [], "error": "..." }
///
/// Why no root: /sys/block/ and its subdirectory uevent/dm/name files are
/// world-readable on Android. SELinux allows untrusted_app to read /sys/block/.
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeScanDevicePartitions(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let result = scan_device_partitions();
    match env.new_string(result) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Scan device for ACTUAL partition names present on the device (no root required).
///
/// Reads /sys/block/ (world-readable on Android) to detect:
///   - GPT partitions: /sys/block/sda/sda1/uevent → PARTNAME=<name>
///   - dm-linear (dynamic) partitions: /sys/block/dm-0/dm/name → <name>
///
/// Normalizes A/B slot suffixes (boot_a → boot, vbmeta_b → vbmeta).
/// Filters out dangerous partitions (userdata, cache, super).
///
/// Returns JSON string for JNI bridge.
fn scan_device_partitions() -> String {
    // Helper: run getprop and trim output
    let getprop = |prop: &str| -> String {
        std::process::Command::new("getprop")
            .arg(prop)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    // Device metadata (for JSON output, not for building partition list)
    let dp_enabled = getprop("ro.boot.dynamic_partitions");
    let dynamic_partitions = dp_enabled == "true" || dp_enabled == "1";
    let slot_suffix = getprop("ro.boot.slot_suffix");
    let android_release = getprop("ro.build.version.release");

    // ── Scan /sys/block/ for ACTUAL partitions on this device ──
    //
    // Two types of partition names we care about:
    //
    // 1. GPT partitions (physical): /sys/block/sda/sda1/uevent contains
    //    "PARTNAME=boot_a" (or "PARTNAME=vbmeta_b", etc.)
    //    These are boot, dtbo, vbmeta, init_boot, recovery, super, etc.
    //
    // 2. dm-linear partitions (dynamic): /sys/block/dm-0/dm/name contains
    //    "system" (or "vendor", "product", etc.)
    //    These are system, vendor, product, system_ext, odm, etc.
    //    On A/B devices, dm names usually don't have _a/_b suffix (only
    //    active slot is mapped).
    //
    // Both /sys/block/ and its subdirectory uevent/dm/name files are
    // world-readable on Android (SELinux allows untrusted_app to read
    // /sys/block/). No root needed.

    // Partitions that should NEVER be in the flashable list
    // (dangerous or not relevant for standalone OTA flash).
    const BLOCKLIST: &[&str] = &[
        "userdata",   // DANGEROUS — would wipe all user data
        "cache",      // legacy, deprecated, not flashed via OTA
        "super",      // container for dynamic partitions, not standalone flash target
        "frp",        // factory reset protection
        "keystore",   // hardware keystore
        "modemst1", "modemst2",  // modem EFS — radio calibration data
        "fsg", "fsc",  // modem backup
        "ssd",        // secure storage
        "persist",    // persistent partition (sensor calibration, DRM keys)
        "misc",       // bootloader misc (recovery command, BCB)
        "param",      // Samsung param partition
        "efs",        // Samsung EFS (radio IMEI)
    ];

    let mut found: Vec<String> = Vec::new();

    // Read /sys/block/ directory
    let block_dir = match std::fs::read_dir("/sys/block/") {
        Ok(d) => d,
        Err(e) => {
            // /sys/block/ not readable (rare — some heavily restricted devices)
            // Fall back to permissive mode (empty list = no validation)
            log::warn!("Cannot read /sys/block/: {} — falling back to permissive mode", e);
            return serde_json::json!({
                "success": false,
                "partitions": [],
                "dynamic_partitions": dynamic_partitions,
                "slot_suffix": slot_suffix,
                "android_version": android_release,
                "error": format!("Cannot read /sys/block/: {}", e),
                "native_version": env!("CARGO_PKG_VERSION")
            }).to_string();
        }
    };

    for entry in block_dir.flatten() {
        let dev_name = entry.file_name().to_string_lossy().to_string();

        // Skip non-partition block devices
        if dev_name.starts_with("loop")
            || dev_name.starts_with("zram")
            || dev_name.starts_with("ram")
            || dev_name.starts_with("sr")
        {
            continue;
        }

        // ── Case 1: Physical disk (sda, mmcblk0, nvme0n1, etc.) ──
        // Iterate its partitions and read PARTNAME from uevent
        if dev_name.starts_with("sd")
            || dev_name.starts_with("mmcblk")
            || dev_name.starts_with("nvme")
        {
            let disk_path = format!("/sys/block/{}", dev_name);
            if let Ok(parts) = std::fs::read_dir(&disk_path) {
                for part in parts.flatten() {
                    let part_name = part.file_name().to_string_lossy().to_string();
                    let uevent_path = format!("{}/{}/uevent", disk_path, part_name);
                    if let Ok(content) = std::fs::read_to_string(&uevent_path) {
                        // Parse uevent for PARTNAME=<value>
                        for line in content.lines() {
                            if let Some(pname) = line.strip_prefix("PARTNAME=") {
                                let pname = pname.trim();
                                if !pname.is_empty() {
                                    found.push(pname.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── Case 2: Device-mapper partition (dm-0, dm-1, etc.) ──
        // Read dm/name for the dm-linear device name
        if dev_name.starts_with("dm-") {
            let dm_name_path = format!("/sys/block/{}/dm/name", dev_name);
            if let Ok(content) = std::fs::read_to_string(&dm_name_path) {
                let dm_name = content.trim();
                if !dm_name.is_empty() {
                    found.push(dm_name.to_string());
                }
            }
        }
    }

    // ── Normalize: strip A/B slot suffix (_a or _b at end) ──
    // boot_a → boot, vbmeta_b → vbmeta, init_boot_a → init_boot
    // But DON'T strip if the partition name genuinely ends with _a/_b
    // (none do in AOSP standard).
    let slot_suffix_lower = slot_suffix.to_lowercase();
    let suffix = if slot_suffix_lower == "_a" {
        Some("_a")
    } else if slot_suffix_lower == "_b" {
        Some("_b")
    } else {
        None
    };

    let normalized: Vec<String> = found
        .iter()
        .map(|name| {
            // Strip slot suffix if present
            if let Some(sfx) = suffix {
                if name.ends_with(sfx) {
                    return name[..name.len() - sfx.len()].to_string();
                }
            }
            // Also strip _a/_b even if slot_suffix was empty
            // (some devices have slot suffixes in GPT even if getprop returned empty)
            if name.ends_with("_a") && name.len() > 2 {
                name[..name.len() - 2].to_string()
            } else if name.ends_with("_b") && name.len() > 2 {
                name[..name.len() - 2].to_string()
            } else {
                name.clone()
            }
        })
        .collect();

    // ── Filter: remove blocklisted partitions ──
    let filtered: Vec<String> = normalized
        .into_iter()
        .filter(|name| {
            let lower = name.to_lowercase();
            !BLOCKLIST.iter().any(|&blocked| blocked == lower.as_str())
        })
        .collect();

    // Deduplicate and sort
    let mut unique: Vec<String> = filtered;
    unique.sort();
    unique.dedup();

    serde_json::json!({
        "success": !unique.is_empty(),
        "partitions": unique,
        "dynamic_partitions": dynamic_partitions,
        "slot_suffix": slot_suffix,
        "android_version": android_release,
        "native_version": env!("CARGO_PKG_VERSION"),
        "error": if unique.is_empty() {
            Some("No partitions detected via /sys/block/ scan".to_string())
        } else {
            None
        }
    }).to_string()
}

// ---------------------------------------------------------------------------
//  JNI: nativeVerifyPayload
// ---------------------------------------------------------------------------

/// Verify a payload.bin file by re-reading and checking its structure.
///
/// Kotlin: `external fun nativeVerifyPayload(path: String): String`
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeVerifyPayload(
    mut env: JNIEnv,
    _class: JClass,
    path: JString,
) -> jstring {
    let path_str: String = match env.get_string(&path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid path string"),
    };

    let result = payload::verify_payload(&path_str);
    let json = serde_json::to_string(&result).unwrap_or_else(|e| {
        format!(
            "{{\"success\":false,\"error\":\"Serialize error: {}\"}}",
            e
        )
    });

    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeCompress
// ---------------------------------------------------------------------------

/// Compress data (for testing / direct use).
///
/// Kotlin: `external fun nativeCompress(
///     inputPath: String, outputPath: String,
///     algorithm: String, level: Int
/// ): String`
///
/// Reads input file, compresses with the given algorithm, writes to output.
/// Returns JSON with success/error and size info.
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeCompress(
    mut env: JNIEnv,
    _class: JClass,
    input_path: JString,
    output_path: JString,
    algorithm: JString,
    level: jint,
) -> jstring {
    let input_str: String = match env.get_string(&input_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid input path"),
    };
    let output_str: String = match env.get_string(&output_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid output path"),
    };
    let alg_str: String = match env.get_string(&algorithm) {
        Ok(s) => s.into(),
        Err(_) => "gzip".to_string(),
    };

    let level_opt = if level > 0 { Some(level) } else { None };

    let (compressed, hash_hex) = match compression::hash_and_compress_file(
        &input_str,
        &alg_str,
        level_opt,
    ) {
        Ok(r) => r,
        Err(e) => {
            return make_error_json(&env, &format!("Compress failed: {}", e));
        }
    };

    // Write compressed output
    if let Some(parent) = std::path::Path::new(&output_str).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    match std::fs::write(&output_str, &compressed) {
        Ok(_) => {}
        Err(e) => {
            return make_error_json(&env, &format!("Write failed: {}", e));
        }
    }

    let input_size = std::fs::metadata(&input_str)
        .map(|m| m.len())
        .unwrap_or(0);
    let output_size = compressed.len() as u64;
    let ratio = if input_size > 0 {
        output_size as f64 / input_size as f64
    } else {
        1.0
    };

    let result = serde_json::json!({
        "success": true,
        "input_path": input_str,
        "output_path": output_str,
        "algorithm": alg_str,
        "input_size": input_size,
        "output_size": output_size,
        "ratio": ratio,
        "sha256": hash_hex,
        "native_version": env!("CARGO_PKG_VERSION"),
    });

    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  JNI: nativeDecompress
// ---------------------------------------------------------------------------

/// Decompress a file (for testing / direct use).
///
/// Kotlin: `external fun nativeDecompress(
///     inputPath: String, outputPath: String,
///     algorithm: String
/// ): String`
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeDecompress(
    mut env: JNIEnv,
    _class: JClass,
    input_path: JString,
    output_path: JString,
    algorithm: JString,
) -> jstring {
    let input_str: String = match env.get_string(&input_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid input path"),
    };
    let output_str: String = match env.get_string(&output_path) {
        Ok(s) => s.into(),
        Err(_) => return make_error_json(&env, "Invalid output path"),
    };
    let alg_str: String = match env.get_string(&algorithm) {
        Ok(s) => s.into(),
        Err(_) => "auto".to_string(),
    };

    // Read input
    let compressed = match std::fs::read(&input_str) {
        Ok(d) => d,
        Err(e) => {
            return make_error_json(&env, &format!("Read failed: {}", e));
        }
    };

    // Decompress
    let decompressed = match compression::decompress(&compressed, &alg_str) {
        Ok(d) => d,
        Err(e) => {
            return make_error_json(&env, &format!("Decompress failed: {}", e));
        }
    };

    // Write output
    if let Some(parent) = std::path::Path::new(&output_str).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    match std::fs::write(&output_str, &decompressed) {
        Ok(_) => {}
        Err(e) => {
            return make_error_json(&env, &format!("Write failed: {}", e));
        }
    }

    let result = serde_json::json!({
        "success": true,
        "input_path": input_str,
        "output_path": output_str,
        "algorithm": alg_str,
        "input_size": compressed.len(),
        "output_size": decompressed.len(),
        "native_version": env!("CARGO_PKG_VERSION"),
    });

    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  Helper: create a JSON error string for JNI return
// ---------------------------------------------------------------------------

fn make_error_json(env: &JNIEnv, error: &str) -> jstring {
    let result = serde_json::json!({
        "success": false,
        "error": error,
        "native_version": env!("CARGO_PKG_VERSION")
    });
    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
//  Android library init
// ---------------------------------------------------------------------------

/// Called when System.loadLibrary("otaku_native") loads the .so.
/// Initialize Android logger so `log::info!()` etc. go to logcat.
#[no_mangle]
pub extern "system" fn JNI_OnLoad(
    _vm: jni::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jint {
    // Initialize android_logger to route log crate to Android logcat
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("otaku-native"),
    );
    log::info!("otaku-native {} loaded", env!("CARGO_PKG_VERSION"));

    // Return JNI version 1_6
    jni::sys::JNI_VERSION_1_6 as jint
}
