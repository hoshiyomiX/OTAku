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
//  Modules (stubs for Phase 1, implementations in Phase 2-3)
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
///
/// @return "otaku-native 3.1.0 (rust)"
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
/// Kotlin: `external fun nativeCheckDeps(): String`
///
/// Returns JSON:
///   {
///     "available": ["none", "gzip", "bzip2", "xz", "brotli"],
///     "missing": [],
///     "all_ok": true,
///     "native_version": "3.1.0"
///   }
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
/// @return JSON result: {"success": bool, "output": str, "error": str|null, ...}
#[no_mangle]
pub extern "system" fn Java_com_hoshiyomi_otaku_NativeBridge_nativeBuildDd(
    env: JNIEnv,
    _class: JClass,
    _images_json: JString,
    _compression: JString,
    _level: jint,
    _output_path: JString,
    _device: JString,
    _skip_verify: jboolean,
) -> jstring {
    // Phase 3: Full implementation with compression, ZIP creation, progress.
    // For Phase 1, return a stub indicating the native bridge is loaded.
    let result = serde_json::json!({
        "success": false,
        "error": "DD build not yet implemented in native (Phase 3)",
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
