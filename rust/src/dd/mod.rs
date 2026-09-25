//! DD mode — Generate an otaku-format flashable ZIP from partition images.
//!
//! Produces a flashable ZIP containing (T49 layout — deterministic order,
//! the flasher walks local file headers from byte 0):
//!   - otaku-decomp (bundled universal decompressor, stored, unix 755)
//!   - otaku.bin (DDBU header + compressed partition data)
//!   - META-INF/com/google/android/update-binary (TWRP/OrangeFox flasher script)
//!   - META-INF/com/google/android/updater-script (stub)
//!   - flash_info.txt (human-readable metadata)
//!
//! otaku.bin format:
//!   Header (4096 bytes, padded):
//!     magic "DDBU" (4B) + version (u16 LE) + compress_id (u16 LE)
//!     + num_parts (u16 LE) + header_size (u16 LE) + zero-padding
//!   Data:
//!     each partition compressed, padded to 4096 alignment
//!
//! Compress IDs (internal binary format — UI no longer exposes "none" or "brotli"):
//!   0 = none (internal),  1 = gzip,  2 = bzip2,  3 = xz,  4 = brotli (REMOVED→passthrough),  5 = lz4,  6 = zstd
//!
//! Ported from Python modes/dd.py (849 lines) to Rust with identical semantics.

use std::fs::File;
use std::io::{Read, Seek, Write, SeekFrom};
use std::path::Path;

use crate::compression::{
    compress_id, hash_and_compress_file_to_writer_with_progress, is_alg, resolve_level,
    ALG_GZIP, ALG_BZIP2, ALG_XZ, ALG_LZ4, ALG_ZSTD,
};

mod script;
#[cfg(test)]
mod tests;

use self::script::build_update_script;

// ---------------------------------------------------------------------------
//  Progress sidecar file
// ---------------------------------------------------------------------------

/// Write progress with per-partition compression percentage.
///
/// `partition_percent` is 0-100 for the current partition being compressed.
/// The overall build percentage is calculated as:
///   completed_partitions * 100 / total_partitions + partition_percent / total_partitions
//
// 9 args is intentional — each value is used exactly once and grouping them
// into a ProgressInfo struct would just add boilerplate (struct definition +
// field assignments at every call site) without improving readability.
// JNI-side progress polling is performance-sensitive (called per-chunk), so
// passing values directly avoids a struct allocation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_progress_with_percent(
    output_path: &str,
    current: usize,
    total: usize,
    name: &str,
    phase: &str,
    bytes_written: u64,
    tmp_path: Option<&str>,
    total_estimated: u64,
    partition_percent: i32,
) {
    let progress_path = format!("{}.progress", output_path);
    // Overall percent = (completed partitions * 100 + current partition percent) / total
    // Use checked_div to handle total=0 without an explicit if-else.
    let numerator = (current.saturating_sub(1)) * 100 + partition_percent as usize;
    let overall_percent = numerator.checked_div(total).unwrap_or(0);
    let content = serde_json::json!({
        "current": current,
        "total": total,
        "name": name,
        "phase": phase,
        "bytes_written": bytes_written,
        "tmp_path": tmp_path.unwrap_or(""),
        "total_estimated": total_estimated,
        "partition_percent": partition_percent,
        "overall_percent": overall_percent
    });
    // BUG FIX: Write progress file atomically to prevent Kotlin from reading
    // a partial JSON. Write to a temp file first, then rename — on Linux/ext4,
    // rename() on the same filesystem is atomic. This eliminates the race where
    // Kotlin polls mid-write and gets a truncated JSON like {"current":1,"total":
    let _ = (|| {
        let tmp_progress_path = format!("{}.progress.tmp", output_path);
        std::fs::write(&tmp_progress_path, content.to_string()).ok()?;
        std::fs::rename(&tmp_progress_path, &progress_path).ok()
    })();
}

/// Delete the progress sidecar file (called on build completion or error).
/// Shared by the DD build (run_dd_build) and the payload.bin build
/// (write_payload) — both write `<output_path>.progress`.
pub(crate) fn delete_progress_file(output_path: &str) {
    let progress_path = format!("{}.progress", output_path);
    let _ = std::fs::remove_file(&progress_path);
}

// ---------------------------------------------------------------------------
//  DD format constants
// ---------------------------------------------------------------------------

/// DDBU header magic
pub const DDBUNDLE_MAGIC: [u8; 4] = *b"DDBU";
pub const DDBUNDLE_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 4096;
pub const ALIGN: usize = 4096;

// ---------------------------------------------------------------------------
//  Compress ID mapping (matches Python COMPRESS_ID_MAP)
// ---------------------------------------------------------------------------

/// Get the shell decompressor command for a compress ID.
fn decomp_cmd_for_id(compress_id: u16) -> &'static str {
    // T49: this now names the ALGORITHM passed to the bundled helper
    // (`otaku-decomp -a <alg>`), not a recovery binary to look up.
    // F3 (T25): ids 4 and unknown are UNREACHABLE from the generated script —
    // it aborts on them before any decompressor wiring (see script.rs gate).
    // The arms remain as defense-in-depth for future call sites; they map to
    // "cat" rather than panicking in case a diagnostic path ever asks.
    match compress_id {
        0 => "cat",
        1 => "gzip",
        2 => "bzip2",
        3 => "xz",
        4 => "cat",  // legacy brotli — script ABORTS on this id (F3 gate)
        5 => "lz4",
        6 => "zstd",
        _ => "cat",  // unknown — script ABORTS on unknown ids (F3 gate)
    }
}

/// Get the file extension for a compress ID.
///
/// Currently only used by tests — not referenced in the production build/flash
/// pipeline. Gated with `#[cfg(test)]` to avoid dead-code warning in release
/// builds while keeping the test functional.
#[cfg(test)]
fn decomp_ext_for_id(compress_id: u16) -> &'static str {
    match compress_id {
        0 => ".raw",
        1 => ".gz",
        2 => ".bz2",
        3 => ".xz",
        4 => ".br",
        5 => ".lz4",
        6 => ".zst",
        _ => ".raw",
    }
}

// ---------------------------------------------------------------------------
//  Build result
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DdBuildResult {
    pub success: bool,
    pub output: String,
    pub zip_path: Option<String>,
    pub zip_size: Option<u64>,
    pub bundle_size: Option<u64>,
    /// Total uncompressed size of all partition images.
    /// Used by the flasher script for pre-flash free space verification.
    pub total_unc_size: Option<u64>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
//  Partition metadata (collected during build)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PartitionMeta {
    name: String,
    unc_size: u64,
    hash_hex: String,
    comp_size: u64,
    data_offset: u64,
    /// SHA-256 of the COMPRESSED partition data (not the uncompressed data).
    /// Used for pre-flash integrity verification to catch corrupt bundles
    /// before any block device is touched. Empty string for bundles built
    /// with older OTAku versions (flash script skips the check when empty).
    comp_hash_hex: String,
}

// ---------------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------------

/// Escape a string for safe interpolation into a double-quoted shell variable.
/// Prevents shell command injection via partition names or device codenames.
/// Replaces characters that have special meaning in double-quoted shell strings:
///   ` \ " $ are all escaped with backslash.
fn shell_escape_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '`' => out.push_str("\\`"),
            '$' => out.push_str("\\$"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

/// Round `offset` up to the next multiple of `alignment`.
fn align_up(offset: usize, alignment: usize) -> usize {
    let remainder = offset % alignment;
    if remainder > 0 {
        offset + alignment - remainder
    } else {
        offset
    }
}

/// Format a byte count as a human-readable string.
fn human_size(size_bytes: u64) -> String {
    if size_bytes < 1024 {
        format!("{} bytes", size_bytes)
    } else if size_bytes < 1048576 {
        format!("{:.1} KB", size_bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", size_bytes as f64 / 1048576.0)
    }
}

// ---------------------------------------------------------------------------
//  Header builder
// ---------------------------------------------------------------------------

/// Build the 4096-byte otaku.bin header.
///
/// Format: magic "DDBU" (4B) + version (u16 LE) + compress_id (u16 LE)
///         + num_parts (u16 LE) + header_size (u16 LE) + zero-padding
fn build_header(compress_id: u16, num_parts: u16) -> Vec<u8> {
    let mut hdr = Vec::with_capacity(HEADER_SIZE);
    // Magic (4 bytes)
    hdr.extend_from_slice(&DDBUNDLE_MAGIC);
    // Version (u16 LE)
    hdr.extend_from_slice(&DDBUNDLE_VERSION.to_le_bytes());
    // Compress ID (u16 LE)
    hdr.extend_from_slice(&compress_id.to_le_bytes());
    // Num parts (u16 LE)
    hdr.extend_from_slice(&num_parts.to_le_bytes());
    // Header size (u16 LE)
    hdr.extend_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    // Zero-pad to HEADER_SIZE
    hdr.resize(HEADER_SIZE, 0u8);
    hdr
}

// modul script.rs (pembangun update-binary) & tests.rs dipisah Fase-1; lihat dd/script.rs, dd/tests.rs

// ---------------------------------------------------------------------------
//  flash_info.txt builder
// ---------------------------------------------------------------------------

/// Build the flash_info.txt human-readable metadata.
//
// 9 args matches the caller's variable list 1:1. Grouping into a struct
// would force the caller to construct a FlashInfoArgs struct that mirrors
// the run_dd_build parameter list — pure boilerplate.
#[allow(clippy::too_many_arguments)]
fn build_flash_info(
    compress_name: &str,
    bundle_size: u64,
    total_unc_size: u64,
    num_parts: usize,
    partitions_meta: &[PartitionMeta],
    device: &str,
    level: i32,
    skip_verify: bool,
    rom_name: &str,
    maker: &str,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.push("OTAku — Custom Payload Maker".to_string());
    lines.push("by hoshiyomiX".to_string());
    if !rom_name.is_empty() || !maker.is_empty() {
        lines.push(String::new());
        let rom = if rom_name.is_empty() { "N/A" } else { rom_name };
        let mk = if maker.is_empty() { "N/A" } else { maker };
        lines.push(format!("ROM: {} | Maker: {}", rom, mk));
    }
    lines.push(String::new());
    lines.push(format!(
        "Generated: {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));
    lines.push(format!(
        "Compression: {}{}",
        compress_name,
        if level > 0 {
            format!(" (level {})", level)
        } else {
            String::new()
        }
    ));
    lines.push(format!(
        "Bundle size: {} bytes ({})",
        bundle_size,
        human_size(bundle_size)
    ));
    lines.push(format!(
        "Total flash size: {} bytes ({})",
        total_unc_size,
        human_size(total_unc_size)
    ));
    lines.push(format!("Partitions: {}", num_parts));
    lines.push(format!(
        "Verification: {}",
        if skip_verify { "disabled" } else { "enabled" }
    ));
    if !device.is_empty() {
        lines.push(format!("Target device: {}", device));
    }
    // Recovery requirement (T30): stated up-front so a user who sideloads this
    // ZIP into stock OEM recovery knows WHY it was rejected before guessing.
    // Stock recovery verifies package signatures (CERT.RSA/.SF/MANIFEST.MF)
    // before update-binary ever runs; OTAku ZIPs are unsigned by OEM keys, and
    // stock ramdisks lack the toolbox commands + lptools the flasher needs.
    lines.push("Requires: TWRP/OrangeFox recovery (or fastbootd)".to_string());
    lines.push("Note: stock OEM recovery cannot flash this ZIP (unsigned, missing tools)".to_string());
    lines.push(String::new());

    for p in partitions_meta {
        lines.push(format!("  [{}]", p.name));
        lines.push(format!(
            "    Uncompressed: {} bytes ({})",
            p.unc_size,
            human_size(p.unc_size)
        ));
        lines.push(format!(
            "    Compressed:   {} bytes ({})",
            p.comp_size,
            human_size(p.comp_size)
        ));
        lines.push(format!("    SHA-256:      {}", p.hash_hex));
        lines.push(format!("    Data offset:  {}", p.data_offset));
        lines.push(String::new());
    }

    lines.join("\n")
}

/// T49: read the bundled decompressor bytes out of the installed APK.
///
/// The APK is itself a ZIP — open it with the same `zip` crate that builds
/// the flashable ZIP and pull the asset entry for the device's ABI. The
/// bytes are then embedded as entry #1 of the produced flashable ZIP.
fn read_helper_from_apk(apk_path: &str, helper_asset: &str) -> Result<Vec<u8>, String> {
    let file = File::open(apk_path)
        .map_err(|e| format!("Cannot open APK '{}': {}", apk_path, e))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| format!("Cannot read APK '{}' as ZIP: {}", apk_path, e))?;
    let mut entry = archive
        .by_name(helper_asset)
        .map_err(|e| format!(
            "Bundled decompressor '{}' not found in APK: {}. \
Reinstall the current OTAku app — every flashable ZIP needs the helper.",
            helper_asset, e
        ))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Cannot read '{}' from APK: {}", helper_asset, e))?;
    if bytes.len() < 64 * 1024 {
        return Err(format!(
            "Bundled decompressor '{}' is suspiciously small ({} bytes) — APK corrupt?",
            helper_asset,
            bytes.len()
        ));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
//  Public API: run_dd_build
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
//  Public API: run_dd_build
// ---------------------------------------------------------------------------

/// Generate an otaku-format flashable ZIP from partition images.
///
/// # Arguments
/// * `images` - List of (partition_name, image_path) pairs
/// * `compression` - Compression algorithm: "zstd", "xz", "bzip2", "gzip", "lz4"
/// * `level` - Compression level (0 = default per algorithm)
/// * `output_path` - Absolute path for output .zip file
/// * `device` - Device codename(s), comma-separated (empty = no device check)
/// * `skip_verify` - Skip post-flash SHA-256 verification
/// * `rom_name` - Cosmetic: ROM name shown in flash_info.txt + flasher banner
/// * `maker` - Cosmetic: ROM maker shown in flash_info.txt + flasher banner
/// * `helper_apk_path` - T49: installed APK path — source of the bundled decompressor asset
/// * `helper_asset` - T49: APK asset entry of otaku-decomp for this device's ABI
///
/// # Returns
/// DdBuildResult with success/error, paths, sizes, and log output.
//
// T49: now 10 args (helper apk path + asset appended). Each arg is used
// in a distinct phase (validation, helper load, compression, script
// building, flash_info). Grouping into a DdBuildArgs struct would just
// shift the boilerplate to lib.rs (which would still receive the JNI
// args + have to construct the struct). Allow clippy::too_many_arguments.

#[allow(clippy::too_many_arguments)]
pub fn run_dd_build(
    images: &[(String, String)], // (partition_name, image_path)
    compression: &str,
    level: i32,
    output_path: &str,
    device: &str,
    skip_verify: bool,
    rom_name: &str,
    maker: &str,
    // T49: location of the bundled decompressor inside the installed APK
    helper_apk_path: &str, // e.g. /data/app/…/base.apk
    helper_asset: &str,    // e.g. tools/otaku-decomp/arm64-v8a/otaku-decomp
) -> DdBuildResult {
    let start = std::time::Instant::now();
    let mut lines: Vec<String> = Vec::new();

    // ── Validate inputs ──
    if images.is_empty() {
        return DdBuildResult {
            success: false,
            output: "[!] Error: no images specified".to_string(),
            zip_path: None,
            zip_size: None,
            bundle_size: None,
            total_unc_size: None,
            error: Some("no images specified".to_string()),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // BUG FIX: Validate partition count against flasher script limit.
    // The update-binary validates HDR_NUM_PARTS ≤ 20, so a bundle with
    // more than 20 partitions would be built but rejected by the flasher.
    // Also validate against u16 max (DDBU header field is u16).
    const MAX_PARTITIONS: usize = 20;
    if images.len() > MAX_PARTITIONS {
        return DdBuildResult {
            success: false,
            output: format!(
                "[!] Error: {} partitions specified — maximum is {} (flasher script limit)",
                images.len(), MAX_PARTITIONS
            ),
            zip_path: None,
            zip_size: None,
            bundle_size: None,
            total_unc_size: None,
            error: Some(format!("too many partitions: {} > {}", images.len(), MAX_PARTITIONS)),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    if output_path.is_empty() {
        return DdBuildResult {
            success: false,
            output: "[!] Error: output_path is required".to_string(),
            zip_path: None,
            zip_size: None,
            bundle_size: None,
            total_unc_size: None,
            error: Some("output_path is required".to_string()),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Validate compression algorithm — "none" and "brotli" removed from user-facing options.
    // All 5 algorithms: zstd, xz, bzip2, gzip, lz4 (ordered by ratio).
    let is_valid_compression = is_alg(compression, ALG_ZSTD)
        || is_alg(compression, ALG_XZ)
        || is_alg(compression, ALG_BZIP2)
        || is_alg(compression, ALG_GZIP)
        || is_alg(compression, ALG_LZ4);

    if !is_valid_compression {
        return DdBuildResult {
            success: false,
            output: format!(
                "[!] Error: unsupported compression '{}'. Supported: zstd, xz, bzip2, gzip, lz4",
                compression
            ),
            zip_path: None,
            zip_size: None,
            bundle_size: None,
            total_unc_size: None,
            error: Some(format!("unsupported compression: {}", compression)),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    let compress_id_val = compress_id(compression);
    let compress_name = compression.to_string();

    // Validate all image files exist
    for (name, path) in images {
        if !Path::new(path).is_file() {
            return DdBuildResult {
                success: false,
                output: format!("[!] Image not found: {} -> {}", name, path),
                zip_path: None,
                zip_size: None,
                bundle_size: None,
                total_unc_size: None,
                error: Some(format!("image file not found: {}", path)),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    // ── Run the build pipeline ──
    // ── T49: load the bundled decompressor from the APK asset ──
    // Fail fast: a missing/corrupt helper aborts BEFORE spending minutes
    // compressing partitions — every flashable ZIP needs the helper
    // (helper-only design, no recovery fallback). Reading straight from
    // the installed APK keeps it always-fresh with zero extra files.
    let helper_bytes = match read_helper_from_apk(helper_apk_path, helper_asset) {
        Ok(b) => b,
        Err(e) => {
            return DdBuildResult {
                success: false,
                output: format!("[!] Error: {}", e),
                zip_path: None,
                zip_size: None,
                bundle_size: None,
                total_unc_size: None,
                error: Some(e),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };
    let helper_size = helper_bytes.len() as u64;
    let helper_sha256 = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&helper_bytes);
        digest.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    };

    let result: Result<DdBuildResult, String> = (|| {
        let num_parts = images.len();
        // Resolve effective level: 0 = algorithm default (zstd→3, gzip→6, etc.)
        // Always show the actual level used — never display the raw sentinel 0.
        let level_opt = if level > 0 { Some(level) } else { None };
        let effective_level = resolve_level(&compress_name, level_opt);
        let level_display = format!(" (level {})", effective_level);

        // ── Compute total estimated size (sum of all input image sizes) ──
        // Used by Kotlin for progress percentage calculation.
        let total_estimated: u64 = images.iter().map(|(_, path)| {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        }).sum();

        // ── Create temp file for incremental bundle writing ──
        // Writing compressed data incrementally (partition by partition) instead
        // of accumulating in memory allows Kotlin to monitor the growing file size
        // for live progress display.
        //
        // CRITICAL: Use the output file's parent directory for the temp file,
        // NOT std::env::temp_dir(). On Android, temp_dir() returns /data/local/tmp
        // which may be cleaned by the OS during long builds (2+ minutes for large
        // partitions). This caused "Cannot open temp file for header: No such file
        // or directory" when the file vanished between close and re-open.
        // The output directory (e.g. /storage/emulated/0/OTAku/) is user-accessible
        // storage that persists reliably throughout the build.
        let output_parent = Path::new(output_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let bundle_tmp_path = output_parent.join("otaku_build_tmp.bin");
        let bundle_tmp_path_str = bundle_tmp_path.to_string_lossy().to_string();

        // BUG FIX: Pre-check write permission before spending minutes compressing.
        // On Android 11+ scoped storage, File::create may fail with EACCES if
        // the app doesn't have MANAGE_EXTERNAL_STORAGE or the path is restricted.
        // Testing with a small file avoids failing after minutes of compression.
        if let Err(e) = (|| -> Result<(), String> {
            let test_path = output_parent.join(".otaku_write_test");
            std::fs::write(&test_path, b"test").map_err(|e| format!("{}", e))?;
            std::fs::remove_file(&test_path).map_err(|e| format!("{}", e))?;
            Ok(())
        })() {
            return Err(format!(
                "Cannot write to output directory '{}': {}. \
                 Check storage permissions (MANAGE_EXTERNAL_STORAGE).",
                output_parent.display(), e
            ));
        }

        // Clean up stale temp file from previous builds
        let _ = std::fs::remove_file(&bundle_tmp_path);

        // Open temp file with read+write access and keep it open for the
        // entire build. This avoids the close+reopen pattern that caused
        // "Cannot open temp file for header: No such file or directory"
        // when Android cleaned up the temp directory between operations.
        //
        // Using a single file handle also eliminates the OOM risk from
        // `hash_and_compress_file_with_progress` which returned the entire
        // compressed output as Vec<u8> (351MB for vendor). Now we stream
        // compressed chunks directly to the file via
        // `hash_and_compress_file_to_writer_with_progress`.
        let mut tmp_file = File::create(&bundle_tmp_path)
            .map_err(|e| format!("Cannot create temp file: {}", e))?;

        // Write placeholder header (will overwrite later after we know all offsets)
        tmp_file.write_all(&vec![0u8; HEADER_SIZE])
            .map_err(|e| format!("Cannot write header placeholder: {}", e))?;
        tmp_file.flush()
            .map_err(|e| format!("Cannot flush header placeholder: {}", e))?;

        // ── Header info ──
        let partition_names: Vec<&str> = images.iter().map(|(n, _)| n.as_str()).collect();
        lines.push("\u{2550} OTAku \u{2550}".to_string());
        lines.push(format!("  Partitions  : {}", partition_names.join(", ")));
        lines.push(format!("  Compression : {}{}", compress_name, level_display));
        if skip_verify {
            lines.push("  Verify      : disabled".to_string());
        }
        lines.push(format!(
            "  Device      : {}",
            if device.is_empty() {
                "generic"
            } else {
                device
            }
        ));
        lines.push(format!(
            "  Output      : {}",
            Path::new(output_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        lines.push(String::new());

        // ── Step 1: Build otaku.bin (streaming to temp file — OOM-safe) ──
        lines.push("[1/3] Building otaku.bin".to_string());
        lines.push(format!(
            "  Compressing {} partition(s) with {}{}",
            num_parts, compress_name, level_display
        ));

        // Warn about high compression levels on mobile
        if compress_name == "xz" && effective_level >= 7 {
            lines.push(format!(
                "  ! {} level {} is very slow on mobile. Level 6 recommended.",
                compress_name, effective_level
            ));
        }

        let mut partitions_meta: Vec<PartitionMeta> = Vec::new();
        // (level_opt already computed above — used by resolve_level and compression)

        // Stream compressed data directly to the temp file.
        // Each partition is compressed chunk-by-chunk and written to the
        // file as compressed output is produced — the full compressed data
        // is NEVER held in a Vec<u8>. This eliminates OOM risk for large
        // partitions (vendor 1GB → 351MB compressed would previously be
        // held entirely in RAM).
        //
        // After all partitions, we seek back to position 0 and overwrite
        // the header placeholder — all with the same file handle, no
        // close+reopen that could fail on Android's volatile temp dirs.
        for (i, (name, path)) in images.iter().enumerate() {
            // T36: user cancellation — stop before starting the next
            // partition (chunk-level aborts happen inside
            // hash_and_compress_file_to_writer_with_progress).
            if crate::cancel_requested() {
                return Err(crate::CANCEL_SENTINEL.to_string());
            }
            log::info!(
                "[{}/{}] Compressing {} ({})",
                i + 1,
                num_parts,
                name,
                path
            );

            // Write progress: starting this partition (0%)
            write_progress_with_percent(
                output_path, i + 1, num_parts, name, "compressing",
                tmp_file.stream_position().unwrap_or(HEADER_SIZE as u64),
                Some(&bundle_tmp_path_str), total_estimated,
                0, // partition_percent = 0%
            );

            // Get the uncompressed file size for progress calculation
            let unc_size = std::fs::metadata(path)
                .map_err(|e| format!("Cannot stat {}: {}", path, e))?
                .len();

            // Record data_offset BEFORE compression (current file position - header)
            // BUG FIX: Use stream_position() instead of metadata().len() — the
            // file handle's position is authoritative and avoids potential
            // discrepancies with on-disk metadata on some Android filesystems.
            let data_offset = tmp_file.stream_position()
                .unwrap_or(HEADER_SIZE as u64) - HEADER_SIZE as u64;

            // Stream compress: reads input in 4MB chunks, compresses, and
            // writes compressed output directly to the temp file.
            // The Sha256Writer inside computes comp_hash_hex on-the-fly.
            // The CountingWriter inside tracks comp_size.
            // Peak RAM: ~8MB (read buf + compressor internal) vs 351MB before.
            let output_path_clone = output_path.to_string();
            let bundle_tmp_path_str_clone = bundle_tmp_path_str.clone();
            let name_clone = name.clone();

            // Seek to current end of file for this partition's data
            // BUG FIX: Use stream_position() for consistency — no need to seek
            // since we're already at the end from the previous partition.
            let current_end = tmp_file.stream_position().unwrap_or(HEADER_SIZE as u64);
            tmp_file.seek(SeekFrom::Start(current_end))
                .map_err(|e| format!("Cannot seek to end for {}: {}", name, e))?;

            // Stream compress: pass file by value, get it back on return.
            // This avoids holding both a reference and the file itself,
            // which would violate Rust's borrow rules.
            let (result, returned_file) = hash_and_compress_file_to_writer_with_progress(
                path,
                &compress_name,
                level_opt,
                tmp_file,  // move file into the function
                Some(&mut |bytes_read: u64, file_size: u64| {
                    let pct = (bytes_read * 100)
                        .checked_div(file_size)
                        .map(|v| v as i32)
                        .unwrap_or(100);
                    // Get current file size for progress via path (not file handle,
                    // which has been moved into the compression function).
                    let current_size = std::fs::metadata(&bundle_tmp_path)
                        .map(|m| m.len())
                        .unwrap_or(HEADER_SIZE as u64);
                    write_progress_with_percent(
                        &output_path_clone,
                        i + 1,
                        num_parts,
                        &name_clone,
                        "compressing",
                        current_size,
                        Some(&bundle_tmp_path_str_clone),
                        total_estimated,
                        pct,
                    );
                }),
            )?;

            // Re-acquire the file handle from the compression function return
            tmp_file = returned_file;

            let comp_size = result.comp_size;
            let hash_hex = result.unc_hash_hex;
            let comp_hash_hex = result.comp_hash_hex;

            partitions_meta.push(PartitionMeta {
                name: name.clone(),
                unc_size,
                hash_hex,
                comp_size,
                data_offset,
                comp_hash_hex,
            });

            // Align to 4096 boundary
            // BUG FIX: Use stream_position() instead of metadata().len()
            let current_pos = tmp_file.stream_position().unwrap_or(0);
            let aligned = align_up(current_pos as usize, ALIGN);
            if aligned > current_pos as usize {
                let padding = aligned - current_pos as usize;
                tmp_file.seek(std::io::SeekFrom::Start(current_pos))
                    .map_err(|e| format!("Cannot seek for alignment: {}", e))?;
                tmp_file.write_all(&vec![0u8; padding])
                    .map_err(|e| format!("Cannot write alignment padding: {}", e))?;
            }

            tmp_file.flush()
                .map_err(|e| format!("Cannot flush temp file after {}: {}", name, e))?;

            // Write progress: partition done (100% for this partition)
            write_progress_with_percent(
                output_path, i + 1, num_parts, name, "compressed",
                tmp_file.stream_position().unwrap_or(0),
                Some(&bundle_tmp_path_str), total_estimated,
                100, // partition_percent = 100%
            );

            let ratio = if unc_size > 0 {
                comp_size as f64 / unc_size as f64 * 100.0
            } else {
                100.0
            };
            lines.push(format!(
                "    {}: {} -> {} bytes ({:.1}%)",
                name, unc_size, comp_size, ratio
            ));
        }

        // Overwrite the header placeholder with the real header.
        // Same file handle — no close+reopen, no risk of "No such file or directory".
        let header = build_header(compress_id_val, num_parts as u16);
        tmp_file.seek(std::io::SeekFrom::Start(0))
            .map_err(|e| format!("Cannot seek to start for header: {}", e))?;
        tmp_file.write_all(&header)
            .map_err(|e| format!("Cannot write header: {}", e))?;
        tmp_file.flush()
            .map_err(|e| format!("Cannot flush header: {}", e))?;

        // Close the temp file — all writes are complete
        drop(tmp_file);

        let bundle_size = std::fs::metadata(&bundle_tmp_path)
            .map(|m| m.len())
            .unwrap_or(0);
        lines.push(format!("  Bundle size  : {}", human_size(bundle_size)));

        // ── Compute total uncompressed size (for free space check during flashing) ──
        // This is the sum of all partition image sizes — it represents the minimum
        // free space needed on target partitions to flash the entire bundle.
        let total_unc_size: u64 = partitions_meta.iter().map(|p| p.unc_size).sum();
        lines.push(format!(
            "  Total flash size: {} ({})",
            human_size(total_unc_size),
            total_unc_size
        ));
        lines.push(String::new());

        // ── Step 2: Build flasher scripts ──
        lines.push("[2/3] Building flasher scripts".to_string());
        write_progress_with_percent(
            output_path, num_parts, num_parts, "scripts", "building_scripts",
            bundle_size, Some(&bundle_tmp_path_str), total_estimated,
            100, // all partitions done
        );

        // T49: report the bundled decompressor facts (loaded + verified
        // before the build closure — see run_dd_build prologue).
        lines.push(format!(
            "  otaku-decomp : {} bytes (SHA-256 {}…)",
            helper_size,
            &helper_sha256[..16.min(helper_sha256.len())]
        ));

        let update_binary = build_update_script(
            num_parts,
            compress_id_val,
            &compress_name,
            &partitions_meta,
            total_unc_size,
            device,
            skip_verify,
            helper_size,
            &helper_sha256,
            helper_asset,
        );

        // Inject ROM name and Maker into the flasher banner (Issue #4 fix).
        //
        // build_update_script generates a fixed banner:
        //   ======================================
        //     OTAku — Custom Payload Maker
        //           by hoshiyomiX
        //   ======================================
        //
        // When the user provides ROM name and/or Maker via the UI input
        // fields, we inject a "ROM: ... | Maker: ..." line between
        // "by hoshiyomiX" and the closing border. This appears in the
        // recovery flashing log (TWRP/OrangeFox ui_print) so the user
        // can identify what they're flashing.
        //
        // We use post-processing (not a build_update_script parameter) to
        // avoid changing 26 test call sites — rom_name/maker are cosmetic,
        // not functional, so tests don't need to know about them.
        let update_binary = if !rom_name.is_empty() || !maker.is_empty() {
            let rom_display = if rom_name.is_empty() { "N/A" } else { rom_name };
            let maker_display = if maker.is_empty() { "N/A" } else { maker };
            // BUG FIX (NEW-K): Shell-escape ROM name and maker to prevent command
            // injection. Previously, raw values were interpolated into double-quoted
            // shell strings, allowing $(cmd) or backtick injection to execute
            // arbitrary commands with root privileges in recovery shell.
            // F10 fix (T25): shell_escape_dq only escapes QUOTES — a newline in
            // the input still breaks out of the ui_print line, injecting a whole
            // new shell line. Strip newlines first (same treatment as the O-2
            // header_info sanitizer).
            let rom_sanitized = rom_display.replace(['\n', '\r'], " ");
            let maker_sanitized = maker_display.replace(['\n', '\r'], " ");
            let rom_escaped = shell_escape_dq(&rom_sanitized);
            let maker_escaped = shell_escape_dq(&maker_sanitized);
            let injection = format!(
                "ui_print \"        by hoshiyomiX\"\nui_print \"  ROM: {} | Maker: {}\"",
                rom_escaped, maker_escaped
            );
            update_binary.replacen(
                "ui_print \"        by hoshiyomiX\"",
                &injection,
                1,
            )
        } else {
            update_binary
        };
        // updater-script is a stub — TWRP/OrangeFox only require the file to
        // exist and contain a valid edify expression. The actual flash logic
        // lives in update-binary (a shell script invoked by recovery).
        // `assert(1==1)` is the canonical no-op edify statement: it parses
        // cleanly in all recovery edify evaluators and never errors out.
        // The previous "#Mtk client script" was a shell-style comment that
        // some TWRP builds warned about as a syntax error.
        let updater_script = "assert(1==1);\n";
        let flash_info = build_flash_info(
            &compress_name,
            bundle_size,
            total_unc_size,
            num_parts,
            &partitions_meta,
            device,
            level,
            skip_verify,
            rom_name,
            maker,
        );

        lines.push(format!(
            "  update-binary : {} bytes",
            update_binary.len()
        ));
        lines.push(format!(
            "  flash_info.txt : {} bytes",
            flash_info.len()
        ));
        lines.push(String::new());

        // ── Step 3: Write output ZIP ──
        lines.push(format!(
            "[3/3] Writing {}",
            Path::new(output_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        write_progress_with_percent(
            output_path, num_parts, num_parts, "writing_zip", "writing_zip",
            bundle_size, Some(&bundle_tmp_path_str), total_estimated,
            100, // all partitions done
        );

        // Ensure output directory exists
        if let Some(parent) = Path::new(output_path).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create output dir: {}", e))?;
        }

        // Create the output ZIP with ZIP_STORED (no compression — the data is already compressed)
        {
            let zip_file =
                File::create(output_path).map_err(|e| format!("Cannot create ZIP: {}", e))?;
            let mut zip = zip::ZipWriter::new(zip_file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(true);  // ZIP64: support otaku.bin > 4 GB

            // T49: entry #1 = bundled decompressor. MUST be first — the
            // flasher walks local file headers from byte 0 expecting
            // "otaku-decomp" then "otaku.bin". Stored + unix 755 (F13):
            // recoveries that exec extracted files rely on the mode bit.
            // large_file(false): a ~1-2 MB binary never needs ZIP64.
            let exec_options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(false)
                .unix_permissions(0o755);
            zip.start_file("otaku-decomp", exec_options)
                .map_err(|e| format!("Cannot start otaku-decomp in ZIP: {}", e))?;
            zip.write_all(&helper_bytes)
                .map_err(|e| format!("Cannot write otaku-decomp: {}", e))?;

            // Add otaku.bin (from the temp file we built incrementally)
            zip.start_file("otaku.bin", options)
                .map_err(|e| format!("Cannot start otaku.bin in ZIP: {}", e))?;
            let mut bundle_file = File::open(&bundle_tmp_path)
                .map_err(|e| format!("Cannot open temp bundle: {}", e))?;
            std::io::copy(&mut bundle_file, &mut zip)
                .map_err(|e| format!("Cannot write otaku.bin to ZIP: {}", e))?;

            // Add flash_info.txt
            zip.start_file("flash_info.txt", options)
                .map_err(|e| format!("Cannot start flash_info.txt in ZIP: {}", e))?;
            zip.write_all(flash_info.as_bytes())
                .map_err(|e| format!("Cannot write flash_info.txt: {}", e))?;

            // Add update-binary
            // F13 fix (T25): carry the executable bit (0o755) in the ZIP entry —
            // recoveries that exec the extracted file directly rely on it; a
            // modeless entry defaults to 0o600 on some unzip implementations.
            zip.start_file(
                "META-INF/com/google/android/update-binary",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored)
                    .large_file(true)
                    .unix_permissions(0o755),
            )
                .map_err(|e| format!("Cannot start update-binary in ZIP: {}", e))?;
            zip.write_all(update_binary.as_bytes())
                .map_err(|e| format!("Cannot write update-binary: {}", e))?;

            // Add updater-script
            zip.start_file("META-INF/com/google/android/updater-script", options)
                .map_err(|e| format!("Cannot start updater-script in ZIP: {}", e))?;
            zip.write_all(updater_script.as_bytes())
                .map_err(|e| format!("Cannot write updater-script: {}", e))?;

            zip.finish()
                .map_err(|e| format!("Cannot finalize ZIP: {}", e))?;
        }

        // Clean up temp file
        let _ = std::fs::remove_file(&bundle_tmp_path);

        // Clean up progress file — build is complete
        delete_progress_file(output_path);

        let zip_size = std::fs::metadata(output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        lines.push(format!("  ZIP size      : {}", human_size(zip_size)));
        lines.push(String::new());

        // ── Summary ──
        let elapsed = start.elapsed();
        lines.push(format!("\u{2550} Done in {:.1}s \u{2550}", elapsed.as_secs_f64()));
        lines.push(format!("  Output  : {}", output_path));
        lines.push(format!("  ZIP size: {}", human_size(zip_size)));

        Ok(DdBuildResult {
            success: true,
            output: lines.join("\n"),
            zip_path: Some(output_path.to_string()),
            zip_size: Some(zip_size),
            bundle_size: Some(bundle_size),
            total_unc_size: Some(total_unc_size),
            error: None,
            duration_ms: elapsed.as_millis() as u64,
        })
    })();

    // Clean up progress file on error too
    delete_progress_file(output_path);

    match result {
        Ok(r) => r,
        Err(e) => {
            // BUG FIX: Clean up temp file on error — previously the bundle
            // temp file (potentially GB) was orphaned in the output directory.
            let output_parent = Path::new(output_path)
                .parent()
                .unwrap_or_else(|| Path::new("."));
            let bundle_tmp_path = output_parent.join("otaku_build_tmp.bin");
            let _ = std::fs::remove_file(&bundle_tmp_path);

            lines.push(format!("[!] Error: {}", e));
            DdBuildResult {
                success: false,
                output: lines.join("\n"),
                zip_path: None,
                zip_size: None,
                bundle_size: None,
                total_unc_size: None,
                error: Some(e),
                duration_ms: start.elapsed().as_millis() as u64,
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  Unit tests
// ---------------------------------------------------------------------------

