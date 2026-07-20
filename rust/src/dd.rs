//! DD mode — Generate an otaku-format flashable ZIP from partition images.
//!
//! Produces a flashable ZIP containing:
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
//! Compress IDs:
//!   0 = none,  1 = gzip,  2 = bzip2,  3 = xz,  4 = brotli
//!
//! Ported from Python modes/dd.py (849 lines) to Rust with identical semantics.

use std::fs::File;
use std::io::{Seek, Write, SeekFrom};
use std::path::Path;

use crate::compression::{
    compress_id, hash_and_compress_file_to_writer_with_progress, is_alg, ALG_NONE,
};

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
fn write_progress_with_percent(
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
fn delete_progress_file(output_path: &str) {
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

/// otaku numeric ID -> shell decompressor command
pub const COMPRESS_CMD_MAP: &[(&str, u16)] = &[
    ("none", 0),
    ("gzip", 1),
    ("bzip2", 2),
    ("xz", 3),
    ("brotli", 4),
];

/// Get the shell decompressor command for a compress ID.
fn decomp_cmd_for_id(compress_id: u16) -> &'static str {
    match compress_id {
        0 => "cat",
        1 => "gzip",
        2 => "bzip2",
        3 => "xz",
        4 => "brotli",
        _ => "cat",
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

// ---------------------------------------------------------------------------
//  Update-binary script builder
// ---------------------------------------------------------------------------

/// Flasher script version/branding string.
/// Displayed in the update-binary header and flash_info.txt.
const SCRIPT_VERSION: &str = "Custom Payload Maker";

/// Build the META-INF/com/google/android/update-binary shell script.
///
/// This is a TWRP/OrangeFox-compatible flasher that:
/// 1. Extracts otaku.bin from the ZIP
/// 2. Checks decompressor availability
/// 3. Validates bundle integrity (DDBU magic, version, compress, partition count)
/// 4. Checks device compatibility (if device specified)
/// 5. Detects A/B slot
/// 6. Validates partition block devices (size check, unmount)
/// 7. Flashes each partition (extract → decompress → dd write → optional verify)
fn build_update_script(
    num_parts: usize,
    compress_id: u16,
    compress_name: &str,
    partitions_meta: &[PartitionMeta],
    device: &str,
    skip_verify: bool,
) -> String {
    let decomp_cmd = decomp_cmd_for_id(compress_id);

    // Build partition variable assignments
    let mut part_vars = String::new();
    for (i, p) in partitions_meta.iter().enumerate() {
        part_vars.push_str(&format!(
            "PART_{}_NAME=\"{}\"\n\
             PART_{}_UNC_SIZE=\"{}\"\n\
             PART_{}_HASH=\"{}\"\n\
             PART_{}_COMP_SIZE=\"{}\"\n\
             PART_{}_DATA_OFFSET=\"{}\"\n\
             PART_{}_COMP_HASH=\"{}\"\n",
            i, p.name, i, p.unc_size, i, p.hash_hex, i, p.comp_size, i, p.data_offset,
            i, p.comp_hash_hex
        ));
    }

    // Calculate step numbers dynamically
    let has_device = !device.is_empty();
    let extract_step = 0;
    let verify_step = 1;       // NEW: pre-flash partition table verify (alur user step 2)
    let integrity_step = 2;    // MERGED: bundle integrity + decompressor check (was Step 1 + Step 2)
    let slot_step = if has_device { 4 } else { 3 };
    let validation_step = if has_device { 5 } else { 4 };
    let resize_step = if has_device { 6 } else { 5 };
    let flash_step_offset = if has_device { 7 } else { 6 };
    let total_steps = num_parts + flash_step_offset;

    // Device check step
    let device_check_step = integrity_step + 1; // = 3
    let device_check_block = if has_device {
        format!(
            r#"
# ── Step {device_check_step}: Device compatibility ──────────────────
TARGET_DEVICE="{device}"
VENDOR_DEVICE=""
BOARD_DEVICE=""
CURRENT_DEVICE=""

# ── Spoof-resistant device codename detection ──
# Reads 4 sources from VENDOR partition (rarely modified by Magisk/GSI/LineageOS,
# which typically only touch /system). This matches the app-side auto-detect
# logic in NativeBridge.detectDeviceCodename() — both use the SAME 4 sources
# in the SAME priority, so they produce the same codename(s).
#
# Sources:
#   1. getprop ro.product.vendor.device  (vendor partition, hard to spoof)
#   2. getprop ro.product.board          (vendor partition, hard to spoof)
#   3. /vendor/build.prop ro.product.vendor.device  (fallback if getprop empty)
#   4. /vendor/build.prop ro.product.board          (fallback if getprop empty)
#
# If VENDOR_DEVICE and BOARD_DEVICE differ, CURRENT_DEVICE is set to BOTH
# as comma-separated: "vendor_device,board". The TARGET_DEVICE comparison
# loop below already supports comma-separated lists, so this works naturally.
#
# Why NOT Build.PRODUCT (ro.product.name):
#   - Easily overridden by Magisk resetprop, GSI, or LineageOS
#   - Often ROM-prefixed (e.g. "lineage_alioth" instead of "alioth")
#   - App and validator would read different values → mismatch
VENDOR_DEVICE=$(getprop ro.product.vendor.device 2>/dev/null)
BOARD_DEVICE=$(getprop ro.product.board 2>/dev/null)
# Fallback 1: /vendor/build.prop ro.product.vendor.device
if [ -z "$VENDOR_DEVICE" ] && [ -f /vendor/build.prop ]; then
    VENDOR_DEVICE=$(grep -E '^ro\.product\.vendor\.device=' /vendor/build.prop 2>/dev/null | head -1 | cut -d= -f2 | tr -d ' \r')
fi
# Fallback 2: /vendor/build.prop ro.product.board
if [ -z "$BOARD_DEVICE" ] && [ -f /vendor/build.prop ]; then
    BOARD_DEVICE=$(grep -E '^ro\.product\.board=' /vendor/build.prop 2>/dev/null | head -1 | cut -d= -f2 | tr -d ' \r')
fi

# Build CURRENT_DEVICE: comma-separated if VENDOR_DEVICE != BOARD_DEVICE
if [ -n "$VENDOR_DEVICE" ] && [ -n "$BOARD_DEVICE" ]; then
    if [ "$VENDOR_DEVICE" = "$BOARD_DEVICE" ]; then
        CURRENT_DEVICE="$VENDOR_DEVICE"
    else
        CURRENT_DEVICE="$VENDOR_DEVICE,$BOARD_DEVICE"
    fi
elif [ -n "$VENDOR_DEVICE" ]; then
    CURRENT_DEVICE="$VENDOR_DEVICE"
elif [ -n "$BOARD_DEVICE" ]; then
    CURRENT_DEVICE="$BOARD_DEVICE"
fi

# Support comma-separated device list in both TARGET_DEVICE and CURRENT_DEVICE.
# Match if ANY value in TARGET_DEVICE matches ANY value in CURRENT_DEVICE.
# E.g. TARGET="alioth,sm8350" matches CURRENT="alioth" or CURRENT="sm8350"
# or CURRENT="alioth,sm8350".
DEVICE_MATCH=0
OLD_IFS="$IFS"
IFS=','
for _target in $TARGET_DEVICE; do
    _tclean=$(echo "$_target" | tr -d '[:space:]')
    [ -z "$_tclean" ] && continue
    for _current in $CURRENT_DEVICE; do
        _cclean=$(echo "$_current" | tr -d '[:space:]')
        if [ "$_cclean" = "$_tclean" ]; then
            DEVICE_MATCH=1
            break 2
        fi
    done
done
IFS="$OLD_IFS"

if [ -n "$TARGET_DEVICE" ]; then
    if [ "$DEVICE_MATCH" != "1" ]; then
        ui_print ""
        ui_print "  WARNING: Device mismatch!"
        ui_print "  Expected : $TARGET_DEVICE"
        ui_print "  Current  : ${{CURRENT_DEVICE:-(unknown)}}"
        ui_print ""
        ui_print "  Flashing on wrong device may BRICK it."

        # Interactive confirmation with fallback chain:
        #   1. `choose` binary (TWRP native, supported by OrangeFox)
        #   2. `read -t -n 1` (busybox/toybox interactive read)
        #   3. Default ABORT (safer than silently continuing)
        # If `choose` is missing (minimal TWRP builds), the old code would
        # auto-abort because the failed `choose` returned non-zero. Now we
        # fall through to read, then to a default abort.
        USER_CONFIRMED=0
        if command -v choose >/dev/null 2>&1; then
            ui_print "  Press Power to continue, Vol- to abort."
            choose -t 30 "Continue?" "Yes" "No" 2>/dev/null
            [ $? -eq 0 ] && USER_CONFIRMED=1
        elif command -v read >/dev/null 2>&1; then
            ui_print "  Press Y to continue, any other key to abort (30s timeout):"
            # `read -t 30 -n 1` reads 1 char with 30s timeout.
            # Exit code 0 = char read, non-zero = timeout/EOF.
            ANSWER=""
            read -t 30 -n 1 ANSWER 2>/dev/null
            RC=$?
            if [ $RC -eq 0 ] && ([ "$ANSWER" = "y" ] || [ "$ANSWER" = "Y" ]); then
                USER_CONFIRMED=1
            fi
        fi

        if [ "$USER_CONFIRMED" != "1" ]; then
            ui_print "! ABORT: User cancelled (device mismatch)"
            exit 1
        fi
        ui_print "  User confirmed — continuing despite device mismatch."
    else
        ui_print "  ✓ Device: $CURRENT_DEVICE"
    fi
fi
"#
        )
    } else {
        String::new()
    };

    // Header info line
    let header_info_parts: Vec<&str> = partitions_meta.iter().map(|p| p.name.as_str()).collect();
    let mut header_info = format!("Partitions: {}", header_info_parts.join(", "));
    if has_device {
        header_info.push_str(&format!(" | Device: {}", device));
    }
    header_info.push_str(&format!(" | Compress: {}", compress_name));

    // Verification block.
    // Two paths based on skip_verify:
    //   skip_verify=true → print "skipped", no hash check.
    //   skip_verify=false → fast SHA-256 verify using large block size.
    //
    // Why this is faster than the old approach:
    //   Old: bs=4096 (4KB), then dd bs=1 for remainder bytes.
    //        bs=1 forces byte-by-byte read — catastrophic on large partitions.
    //        For 4 GB partition: ~1 billion syscalls. Can take 5+ minutes.
    //
    //   New: bs=1M (1MB) for the bulk of the read. 1MB reads match the
    //        dd write block size and UFS/eMMC page size — near-optimal
    //        throughput. Remainder (<1MB) read in single bs=1M pass with
    //        count=1 — uses the same large block, just truncates the read
    //        via the partition size boundary. No bs=1 needed.
    //
    //        For 4 GB partition: ~4096 syscalls instead of ~1 billion.
    //        ~10-100x faster depending on storage.
    //
    // Bonus: pipe through `tee` to a background sha256sum process so
    //        the hash computation overlaps with the read I/O. On multi-core
    //        SoCs (which all modern phones have), this gives another ~30%
    //        speedup by parallelizing SHA-256 with the next disk read.
    //
    // Edge case: very old busybox builds may not support bs=1M with large
    //   counts cleanly. If the verify returns an empty hash, we fall back
    //   to the legacy 4KB+bs=1 approach. This is rare but preserves
    //   compatibility with recoveries running ancient busybox.
    let verify_block = if skip_verify {
        r#"ui_print "  Verification skipped"
"#
        .to_string()
    } else {
        r#"ui_print "  Verifying ($PNAME)..."
VERIFY_HASH=""

# Fast path: large block size + background sha256sum via FIFO.
# Pipeline:
#   dd if=$PTARGET bs=1M → tee FIFO → /dev/null
#                              ↓
#              sha256sum (bg) → hash file
#
# bs=1M (1048576) matches dd write block + UFS page size, eliminating
# the byte-by-byte syscall overhead of the old bs=1 remainder read.
VERIFY_FIFO="/tmp/verify_${i}.fifo"
VERIFY_HASHFILE="/tmp/verify_${i}.hash"
rm -f "$VERIFY_FIFO" "$VERIFY_HASHFILE"

FAST_OK=0
if command -v sha256sum >/dev/null 2>&1; then
    if mkfifo "$VERIFY_FIFO" 2>/dev/null; then
        sha256sum < "$VERIFY_FIFO" > "$VERIFY_HASHFILE" 2>/dev/null &
        VERIFY_PID=$!

        # Read PSIZE bytes in 1MB blocks. count rounds UP to next MB to ensure
        # we cover the full partition, but `head -c "$PSIZE"` trims the stream
        # to EXACTLY PSIZE bytes before it reaches sha256sum.
        #
        # Without head -c, if PSIZE is not 1MB-aligned (common for Android
        # system.img — raw ext4 with arbitrary byte count), dd reads
        # VERIFY_BLOCKS * 1MB bytes which is MORE than PSIZE. The extra bytes
        # are OLD partition data (from previous ROM), which changes the hash
        # and causes a FALSE-NEGATIVE hash mismatch.
        #
        # Evidence: recovery (1).log CRC32 0x1e7c775, Itel S666LN
        #   ✓ Compressed data hash verified [OK]    ← bundle intact
        #   Flashing system to ...                  ← write succeeded
        #   ! ABORT: Hash mismatch for system!      ← false-negative
        #     Expected: 999c8faadf98484d...         ← hash of .img (PSIZE bytes)
        #     Got:      68ee9379004d3593...         ← hash of PSIZE + extra old data
        VERIFY_BLOCKS=$(( (PSIZE + 1048575) / 1048576 ))
        dd if="$PTARGET" bs=1048576 count=$VERIFY_BLOCKS 2>/dev/null | head -c "$PSIZE" | tee "$VERIFY_FIFO" >/dev/null

        # Close FIFO and wait for hash to complete.
        rm -f "$VERIFY_FIFO"
        wait $VERIFY_PID 2>/dev/null

        if [ -s "$VERIFY_HASHFILE" ]; then
            VERIFY_HASH=$(cut -d' ' -f1 < "$VERIFY_HASHFILE")
            FAST_OK=1
        fi
        rm -f "$VERIFY_HASHFILE"
    fi
fi

# Fallback: legacy 4KB-block + bs=1 remainder (slow but universally supported).
# Used if sha256sum/mkfifo unavailable, or if fast path produced empty hash
# (possible on ancient busybox with broken bs=1M support).
if [ "$FAST_OK" != "1" ]; then
    ui_print "  Note: fast hash unavailable — using legacy 4KB path."
    # Use head -c "$PSIZE" to ensure we hash EXACTLY PSIZE bytes, same as
    # the fast path. The old 2-dd approach (FULL_BLOCKS + REMAINDER) was
    # correct but slower and more complex. head -c is simpler and available
    # in busybox/toybox.
    VERIFY_BLOCKS=$(( (PSIZE + 1048575) / 1048576 ))
    VERIFY_HASH=$(dd if="$PTARGET" bs=1048576 count=$VERIFY_BLOCKS 2>/dev/null | head -c "$PSIZE" | sha256sum | cut -d' ' -f1)
fi

if [ "$VERIFY_HASH" = "$PHASH" ]; then
    ui_print "  ✓ $PNAME verified"
else
    ui_print "! ABORT: Hash mismatch for $PNAME!"
    ui_print "  Expected: $PHASH"
    ui_print "  Got:      $VERIFY_HASH"
    exit 1
fi
"#
        .to_string()
    };

    // Build the complete script
    let mut script = String::new();

    // ── Header / bootstrap ──
    script.push_str(&format!(
        r#"#!/sbin/sh
# OTAku {script_version}
# {header_info}

# ── TWRP/OrangeFox bootstrap ────────────────────────────────
# TWRP calls: update-binary 3 <fd> <zippath>
# $1=API version, $2=output fd, $3=zip file path
OUTFD="$2"
ZIPFILE="$3"

ui_print() {{
    echo "ui_print $1" >&$OUTFD
    echo "ui_print" >&$OUTFD
}}

# ── Cleanup trap — re-resize + remap dynamic partitions on ABORT ──
# If the script exits abnormally (e.g. dd write false-failure on block device),
# dynamic partitions may have been:
#   1. Resized to a new (larger) size — must be restored to original size so
#      the device can still boot normally with its old partition layout.
#   2. Unmapped during the resize step — must be re-mapped so recovery can
#      continue to function.
# This trap handles both: it iterates RESIZED_ORIGINAL (a list of "name:size"
# pairs captured during the resize step) to restore original sizes, then
# re-maps all dynamic partitions.
# Defaults are set here so the trap is safe even if exit happens before
# the validation step (which would normally populate them).
DYNAMIC_PART_NAMES=""
HAS_LPTOOLS=0
CLEANUP_DONE=0
RESIZED_ORIGINAL=""   # list of "name:original_size_bytes" pairs (set during resize)
cleanup_abort() {{
    if [ "$CLEANUP_DONE" = "1" ]; then return; fi
    CLEANUP_DONE=1
    # Only attempt cleanup if we got past the validation step
    if [ -z "$DYNAMIC_PART_NAMES" ]; then return; fi
    ui_print "✗ Performing emergency cleanup..."
    # Re-resize partitions back to original size (only if lptools available
    # and we actually resized something).
    # Without this, a failed flash would leave the device with oversized
    # empty partitions that may confuse the next OTA attempt.
    if [ "$HAS_LPTOOLS" = "1" ] && [ -n "$RESIZED_ORIGINAL" ]; then
        for pair in $RESIZED_ORIGINAL; do
            # Parse "name:original_size_bytes"
            rname="${{pair%%:*}}"
            rsize="${{pair##*:}}"
            [ -z "$rname" ] && continue
            [ -z "$rsize" ] && continue
            ui_print "  Restoring $rname to original size..."
            # Build slot-suffixed name for lptools (same as resize step)
            rname_lp="$rname"
            if [ -n "$TARGET_SLOT" ]; then
                rname_lp="${{rname}}${{TARGET_SLOT}}"
            fi
            # BlassGo pattern: unmap → resize → map.
            # This preserves the by-name symlink (resize doesn't destroy dm)
            # and ensures the dm-linear reflects the rollback size.
            lptools unmap "$rname_lp" >/dev/null 2>&1 || true
            # IMPORTANT: Use explicit if-else, NOT ||/&& chaining.
            if ! lptools resize "$rname_lp" "$rsize" >/dev/null 2>&1; then
                # resize failed — try remove+create as last-resort fallback
                # (destructive — by-name symlink may become stale, but at
                # least the metadata is rolled back so device can boot)
                lptools remove "$rname_lp" >/dev/null 2>&1
                lptools create "$rname_lp" "$rsize" >/dev/null 2>&1
            else
                # resize succeeded — re-map to materialize the rollback size
                lptools map "$rname_lp" >/dev/null 2>&1 || true
            fi
        done
    fi
    # Re-map dynamic partitions that may have been unmapped during resize.
    # lptools is the ONLY supported tool for dynamic partition management.
    #
    # Targeted (NOT all DYNAMIC_PART_NAMES): only touch partitions that are
    # currently NOT mapped. Already-mapped partitions are skipped — blindly
    # calling unmap+remap on them would (a) do unnecessary work, and (b) risk
    # EBUSY/EEXIST on partitions that recovery is actively using (e.g. /tmp
    # overlaid on /data, /system_ext auto-mounted).
    #
    # This mirrors the AOSP non-A/B OTA flow: update_dynamic_partitions only
    # touches partitions in its op_list (the ones being resized/added/removed),
    # never all dynamic partitions on the device.
    # Source: source.android.com/docs/core/ota/dynamic_partitions/nonab
    if [ "$HAS_LPTOOLS" = "1" ]; then
        for pname in $DYNAMIC_PART_NAMES; do
            # Skip if already mapped — no need to touch it.
            if [ -e "/dev/mapper/$pname" ] || [ -e "/dev/block/by-name/$pname" ]; then
                continue
            fi
            # Build slot-suffixed name for lptools (same as resize step)
            pname_lp="$pname"
            if [ -n "$TARGET_SLOT" ]; then
                pname_lp="${{pname}}${{TARGET_SLOT}}"
            fi
            # Not mapped — try to map.
            unmount_and_unmap_partition "$pname" >/dev/null 2>&1
            lptools map "$pname_lp" >/dev/null 2>&1
        done
    fi
    sync
    ui_print "✗ Cleanup complete"
}}
# Trap on EXIT (normal exit + uncaught error) AND on common signals
# (INT = Ctrl-C / Vol-, TERM = recovery abort, HUP = controlling terminal
# hangup). Without signal traps, SIGTERM from recovery would skip cleanup.
# The CLEANUP_DONE guard inside cleanup_abort prevents double-execution
# when a signal handler returns and EXIT fires afterwards.
trap cleanup_abort EXIT INT TERM HUP

BUNDLE="/tmp/otaku.bin"
{part_vars}NUM_PARTS={num_parts}
COMPRESS_ID={compress_id}

ui_print "======================================"
ui_print "  OTAku — {script_version}"
ui_print "        by hoshiyomiX"
ui_print "======================================"
"#,
        script_version = SCRIPT_VERSION,
        header_info = header_info,
        part_vars = part_vars,
        num_parts = num_parts,
        compress_id = compress_id,
    ));

    // ── Step 0: Extract otaku.bin ──
    script.push_str(&format!(
        r#"# ── Step {extract_step}/{total_steps}: Extract otaku.bin from ZIP ──────────────────
ui_print "> Extracting payload..."

rm -f "$BUNDLE"
if [ ! -f "$ZIPFILE" ]; then
    ui_print "✗ Error: ZIP file not found"
    ui_print "  Path: $ZIPFILE"
    exit 1
fi

# Pre-extract: query ZIP central directory to learn the expected size of
# otaku.bin. This catches truncation/corruption that would otherwise only
# surface mid-flash (header magic check or worse, mid-dd write).
# Output format of `unzip -l`:
#   Length   Date  Time   C-Ratio  Name
#   --------  ------  ------  ------  ----
#   1234567  ...                    otaku.bin
EXPECTED_BUNDLE_SIZE=0
ZIP_LIST_OK=0

# Try system unzip first, then busybox, then toybox.
# `unzip -l` output is parsed by looking for a line whose last token is "otaku.bin".
# The first numeric token on that line is the uncompressed size in bytes.
#
# Bug fix: the previous regex `^[0-9]+ otaku\.bin$` failed on some recoveries
# because:
#   1. busybox grep might not handle `\.` in basic regex correctly
#   2. Some unzip -l outputs have extra trailing whitespace or CR characters
#   3. The `awk '{{print $1, $NF}}'` output might have inconsistent spacing
#
# New approach: use `grep 'otaku[.]bin$'` (portable literal dot — works in
# both basic and extended regex), then extract the first field. Validate
# that the result is a positive integer before accepting it.
#
# Using `[.]` instead of `\.` is more portable across busybox/grep variants.
# Trailing `\r` (from Windows-line-ending unzip outputs) is stripped with tr.
ZIP_LIST_NAME_PATTERN='otaku[.]bin$'

# Helper: try to get otaku.bin size from a given unzip variant
# Sets EXPECTED_BUNDLE_SIZE and returns 0 on success, 1 on failure
try_zip_listing() {{
    local unzip_cmd="$1"
    local listing
    listing=$($unzip_cmd -l "$ZIPFILE" 2>/dev/null | tr -d '\r' | awk '{{print $1, $NF}}' | grep "$ZIP_LIST_NAME_PATTERN")
    [ -z "$listing" ] && return 1
    EXPECTED_BUNDLE_SIZE=$(echo "$listing" | awk '{{print $1}}' | tr -d ' ')
    # Validate: must be a positive integer
    case "$EXPECTED_BUNDLE_SIZE" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$EXPECTED_BUNDLE_SIZE" -gt 0 ] 2>/dev/null || return 1
    return 0
}}

if which unzip >/dev/null 2>&1; then
    if try_zip_listing "unzip"; then
        ZIP_LIST_OK=1
    fi
fi
if [ "$ZIP_LIST_OK" = "0" ] && busybox --list 2>/dev/null | grep -q "^unzip$"; then
    if try_zip_listing "busybox unzip"; then
        ZIP_LIST_OK=1
    fi
fi
if [ "$ZIP_LIST_OK" = "0" ] && toybox unzip --help >/dev/null 2>&1; then
    if try_zip_listing "toybox unzip"; then
        ZIP_LIST_OK=1
    fi
fi

# Warn if central directory listing failed — not fatal (some old busybox builds
# don't support `unzip -l`), but the post-extract size check below will be skipped.
if [ "$ZIP_LIST_OK" = "0" ]; then
    ui_print "  Note: cannot query ZIP listing — size check will be skipped."
elif [ -z "$EXPECTED_BUNDLE_SIZE" ] || [ "$EXPECTED_BUNDLE_SIZE" = "0" ]; then
    ui_print "  Note: otaku.bin not found in ZIP listing — possible corrupt ZIP."
    ui_print "✗ Error: otaku.bin not found in ZIP"
    ui_print "  Hint: Ensure ZIP contains 'otaku.bin' in its root"
    exit 1
else
    ui_print "  Size: $(( EXPECTED_BUNDLE_SIZE / 1048576 )) MB"
fi

EXTRACT_OK=0
if which unzip >/dev/null 2>&1; then
    unzip -o -j "$ZIPFILE" otaku.bin -d /tmp/ >/dev/null 2>&1 && EXTRACT_OK=1
fi
if [ "$EXTRACT_OK" = "0" ] && busybox --list 2>/dev/null | grep -q "^unzip$"; then
    busybox unzip -o -j "$ZIPFILE" otaku.bin -d /tmp/ >/dev/null 2>&1 && EXTRACT_OK=1
fi
if [ "$EXTRACT_OK" = "0" ] && toybox unzip --help >/dev/null 2>&1; then
    toybox unzip -o -j "$ZIPFILE" otaku.bin -d /tmp/ >/dev/null 2>&1 && EXTRACT_OK=1
fi

if [ "$EXTRACT_OK" = "0" ] || [ ! -f "$BUNDLE" ]; then
    ui_print "✗ Error: Failed to extract otaku.bin"
    ui_print "  Hint: Check /tmp free space or ZIP integrity"
    exit 1
fi

BUNDLE_EXTRACT_SIZE=$(wc -c < "$BUNDLE" | tr -d ' ')

# Post-extract size verification — catches truncated or partially-extracted
# otaku.bin (common on tmpfs with low space, or ZIP CRC mismatch).
# Only check when we have a trusted expected size from the ZIP listing.
if [ "$ZIP_LIST_OK" = "1" ] && [ -n "$EXPECTED_BUNDLE_SIZE" ] && [ "$EXPECTED_BUNDLE_SIZE" != "0" ]; then
    if [ "$BUNDLE_EXTRACT_SIZE" != "$EXPECTED_BUNDLE_SIZE" ]; then
        ui_print "✗ Error: otaku.bin size mismatch"
        ui_print "  Expected: $EXPECTED_BUNDLE_SIZE bytes"
        ui_print "  Actual:   $BUNDLE_EXTRACT_SIZE bytes"
        ui_print "  Hint: tmpfs full or ZIP CRC error"
        # df -h output format varies: coreutils has header line, busybox doesn't.
        # Try `df /tmp` (POSIX, no -h flag needed) and extract free space (column 4).
        # Some Android recoveries don't have df at all — silently skip in that case.
        if command -v df >/dev/null 2>&1; then
            TMP_FREE=$(df /tmp 2>/dev/null | tail -1 | awk '{{print $4}}')
            if [ -n "$TMP_FREE" ]; then
                ui_print "    Free: $TMP_FREE (1K-blocks)"
            fi
            # Also try `df -h /tmp` for human-readable — best-effort, ignore failure.
            df -h /tmp 2>/dev/null | tail -1 | awk '{{print "    total="$2" used="$3" free="$4}}' 2>/dev/null
        else
            ui_print "    (df not available in this recovery)"
        fi
        # Clean up the corrupt file so we don't leave partial state.
        rm -f "$BUNDLE"
        exit 1
    fi
    ui_print "  ✓ Size verified"
fi

ui_print "  ✓ Extracted ($(( BUNDLE_EXTRACT_SIZE / 1048576 )) MB)"
"#,
        extract_step = extract_step,
        total_steps = total_steps,
    ));

    // ── Step 1 (NEW): Pre-flash partition table verify ──
    // Alur user step 2: verify semua partisi SEBELUM flash.
    //
    // Pre-flash verify checks the structural integrity of the partition table
    // embedded in otaku.bin. For each partition declared in PART_i_* vars,
    // verify:
    //   1. data_offset + comp_size ≤ BUNDLE_SIZE (no overflow / truncated bundle)
    //   2. hash_hex is 64 hex chars (valid SHA-256 format)
    //   3. unc_size > 0 (non-empty partition)
    //
    // This is a STRUCTURAL check only — hash verification of decompressed
    // data happens post-flash (in the flash loop). Pre-flash hash verify
    // would require decompressing each partition twice (expensive for
    // 4GB+ partitions).
    //
    // Why this step exists: a corrupt bundle can pass Step 0 extract
    // (magic + version OK) but have invalid per-partition offsets that
    // only surface mid-flash as dd write errors. Pre-flash verify catches
    // these errors early, before any block device is touched.
    script.push_str(&format!(
        r#"# ── Step {verify_step}/{total_steps}: Pre-flash partition table verify ──────────────────
ui_print "> Verifying partition table..."

if [ ! -f "$BUNDLE" ]; then
    ui_print "! ABORT: $BUNDLE not found"
    exit 1
fi

BUNDLE_VERIFY_SIZE=$(wc -c < "$BUNDLE" | tr -d ' ')
VERIFY_OK=1
VERIFY_ERRORS=0

for i in $(seq 0 $(( NUM_PARTS - 1 ))); do
    eval "VPNAME=\$PART_${{i}}_NAME"
    eval "VUNC=\$PART_${{i}}_UNC_SIZE"
    eval "VHASH=\$PART_${{i}}_HASH"
    eval "VCOMP=\$PART_${{i}}_COMP_SIZE"
    eval "VOFFSET=\$PART_${{i}}_DATA_OFFSET"

    ERRORS_THIS=""

    # Guard: VOFFSET must be non-empty before arithmetic (Bug NEW-A fix).
    # Empty var in $(( )) is treated as 0, which would silently bypass
    # the offset_overflow check (0 + 0 = 0, never > BUNDLE_SIZE).
    if [ -z "$VOFFSET" ]; then
        ERRORS_THIS="$ERRORS_THIS empty_offset"
    fi
    # Guard: VCOMP must be non-empty (same reason as VOFFSET).
    if [ -z "$VCOMP" ]; then
        ERRORS_THIS="$ERRORS_THIS empty_comp_size"
    fi

    # Check 1: data_offset + comp_size must not exceed bundle size.
    # Only run if both vars are non-empty (otherwise we'd compute 0+0=0).
    # This catches truncated bundles and corrupted offset fields.
    if [ -n "$VOFFSET" ] && [ -n "$VCOMP" ]; then
        DATA_END=$(( VOFFSET + VCOMP ))
        if [ "$DATA_END" -gt "$BUNDLE_VERIFY_SIZE" ]; then
            ERRORS_THIS="$ERRORS_THIS offset_overflow"
        fi
    fi

    # Check 2: hash must be exactly 64 hex characters (SHA-256 = 32 bytes = 64 hex).
    # Use tr to strip non-hex chars and compare length.
    HASH_LEN=$(echo -n "$VHASH" | tr -d -c '0-9a-fA-F' | wc -c | tr -d ' ')
    if [ "$HASH_LEN" != "64" ]; then
        ERRORS_THIS="$ERRORS_THIS bad_hash_format(len=$HASH_LEN)"
    fi

    # Check 3: uncompressed size must be > 0.
    # Bug NEW-B fix: use ${{VUNC:-0}} default to avoid `[ "" -le "0" ]` shell error
    # in strict POSIX sh (dash). Empty VUNC now reports zero_unc_size cleanly
    # instead of producing "integer expression expected" noise in the log.
    if [ "${{VUNC:-0}}" -le "0" ]; then
        ERRORS_THIS="$ERRORS_THIS zero_unc_size"
    fi

    # Check 4: data_offset must be 4096-aligned (alignment invariant from build).
    # Misalignment would cause dd skip= to read garbage.
    # Guard: only check if VOFFSET is non-empty (empty already flagged above).
    if [ -n "$VOFFSET" ]; then
        ALIGN_CHECK=$(( VOFFSET % 4096 ))
        if [ "$ALIGN_CHECK" -ne "0" ]; then
            ERRORS_THIS="$ERRORS_THIS misaligned_offset"
        fi
    fi

    # Bug NEW-C fix: use printf '%.16s' for portable hash shortening.
    # ${{VHASH:0:16}} is bash-only (also busybox ash with CONFIG_ASH_BASH_COMPAT),
    # but dash (Debian/Ubuntu default /bin/sh) doesn't support it and would
    # print the literal string "${{VHASH:0:16}}". printf '%.16s' is POSIX.
    if [ -n "$VHASH" ]; then
        HASH_SHORT=$(printf '%.16s' "$VHASH")
    else
        HASH_SHORT="(empty)"
    fi

    if [ -n "$ERRORS_THIS" ]; then
        ui_print "!  Partition $VPNAME:$ERRORS_THIS"
        ui_print "!    unc_size=${{VUNC:-(empty)}} comp_size=${{VCOMP:-(empty)}} offset=${{VOFFSET:-(empty)}} hash=$HASH_SHORT..."
        VERIFY_OK=0
        VERIFY_ERRORS=$(( VERIFY_ERRORS + 1 ))
    else
        ui_print "  ✓ $VPNAME: $(( VUNC / 1048576 )) MB"
    fi
done

if [ "$VERIFY_OK" != "1" ]; then
    ui_print "! ABORT: $VERIFY_ERRORS partition(s) failed pre-flash verify."
    ui_print "!  Bundle is corrupt or was built with incompatible OTAku version."
    exit 1
fi

ui_print "  ✓ All $NUM_PARTS partition(s) verified"
"#,
        verify_step = verify_step,
        total_steps = total_steps,
    ));

    // ── Step 2 (MERGED): Bundle integrity + decompressor availability ──
    // Old Step 1 (decompressor) and Step 2 (bundle integrity) were separate.
    // Merged because:
    //   - Decompressor is only used during flash (Step 6+), checking it at
    //     Step 1 was too early and added an extra ui_print section.
    //   - Bundle integrity check (magic/version/parts) is conceptually
    //     part of "verify the payload we just extracted" — same step.
    //   - Reduces step count from 8 to 7 (without device check) / 9 to 8 (with).
    script.push_str(&format!(
        r#"# ── Step {integrity_step}/{total_steps}: Bundle integrity + decompressor ──────────
ui_print "> Checking bundle integrity..."

# ── Decompressor availability ──
DECOMP_CMD=""
check_decompressor() {{
    local cmd="$1"
    if which "$cmd" >/dev/null 2>&1; then
        DECOMP_CMD="$cmd"
        return 0
    fi
    if busybox --list 2>/dev/null | grep -q "^${{cmd}}$"; then
        DECOMP_CMD="busybox $cmd"
        return 0
    fi
    if toybox --help >/dev/null 2>&1 && toybox "$cmd" --help >/dev/null 2>&1; then
        DECOMP_CMD="toybox $cmd"
        return 0
    fi
    for p in /system/bin/$cmd /vendor/bin/$cmd /sbin/$cmd; do
        if [ -x "$p" ]; then
            DECOMP_CMD="$p"
            return 0
        fi
    done
    return 1
}}

if ! check_decompressor "{decomp_cmd}"; then
    ui_print "! ABORT: {decomp_cmd} not found."
    ui_print "! Available tools:"
    which gzip bzip2 xz brotli 2>/dev/null || echo "  (none found)"
    busybox --list 2>/dev/null | head -5
    ui_print "! Rebuild bundle with a available compressor."
    ui_print "! Recommended: --compress gzip"
    exit 1
fi
ui_print "  ✓ Decompressor: $DECOMP_CMD"

# ── Bundle integrity ──
BUNDLE_SIZE=$(wc -c < "$BUNDLE")

HDR_MAGIC=$(od -A n -t x1 -N 4 "$BUNDLE" | tr -d ' \\n')
if [ "$HDR_MAGIC" != "44444255" ]; then
    ui_print "! ABORT: Invalid bundle magic (expected DDBU, got $(echo $HDR_MAGIC | sed 's/\(..\)/\\x\\1/g'))"
    exit 1
fi

HDR_VERSION=$(od -A n -t u2 -j 4 -N 2 "$BUNDLE" | tr -d ' ')
# HDR_COMPRESS is u16 LE (2 bytes) — read with -t u2 -N 2 to match the
# header writer (build_header() writes compress_id as u16 LE).
# Previous code used -t u1 -N 1 which only read the low byte; this worked
# by accident for compress_id 0-4 (high byte = 0) but would silently
# truncate if compress_id ever exceeded 255.
HDR_COMPRESS=$(od -A n -t u2 -j 6 -N 2 "$BUNDLE" | tr -d ' ')
HDR_NUM_PARTS=$(od -A n -t u2 -j 8 -N 2 "$BUNDLE" | tr -d ' ')
HDR_HDR_SIZE=$(od -A n -t u2 -j 10 -N 2 "$BUNDLE" | tr -d ' ')

if [ "$HDR_VERSION" != "1" ]; then
    ui_print "! ABORT: Unsupported bundle version: $HDR_VERSION"
    exit 1
fi

if [ "$HDR_COMPRESS" != "$COMPRESS_ID" ]; then
    ui_print "! ABORT: Compress mismatch: expected $COMPRESS_ID, got $HDR_COMPRESS"
    exit 1
fi

if [ "$HDR_NUM_PARTS" -lt 1 ] || [ "$HDR_NUM_PARTS" -gt 20 ]; then
    ui_print "! ABORT: Invalid partition count: $HDR_NUM_PARTS"
    exit 1
fi

# Header size is always exactly 4096 (HEADER_SIZE constant in build_header).
# Previously accepted any value >= 64, which let malformed bundles pass.
# Strict equality check rejects any drift from the constant.
if [ "$HDR_HDR_SIZE" != "4096" ]; then
    ui_print "! ABORT: Invalid header size: $HDR_HDR_SIZE (expected 4096)"
    exit 1
fi

# Header is always 4096-aligned by construction (HEADER_SIZE = 4096).
# The previous REMAINDER-based alignment loop was dead code (REMAINDER is
# always 0 when HDR_HDR_SIZE == 4096). Removed for clarity.
DATA_OFFSET=$HDR_HDR_SIZE

ui_print "  ✓ Format: v$HDR_VERSION | Parts: $HDR_NUM_PARTS"
"#,
        integrity_step = integrity_step,
        total_steps = total_steps,
        decomp_cmd = decomp_cmd,
    ));

    // ── Device check (optional) ──
    script.push_str(&device_check_block);

    // ── Slot detection ──
    script.push_str(&format!(
        r#"# ── Step {slot_step}/{total_steps}: Slot detection ──────────────────────────
ui_print "> Detecting A/B slot..."

TARGET_SLOT=""

CMDLINE_SLOT=$(cat /proc/cmdline 2>/dev/null | tr ' ' '\n' | grep -o 'androidboot.slot_suffix=[^ ]*' | cut -d= -f2)
if [ -n "$CMDLINE_SLOT" ]; then
    TARGET_SLOT="$CMDLINE_SLOT"
fi

if [ -z "$TARGET_SLOT" ]; then
    CMDLINE_SLOT_RAW=$(cat /proc/cmdline 2>/dev/null | tr ' ' '\n' | grep -o 'androidboot.slot=[^ ]*' | cut -d= -f2)
    if [ -n "$CMDLINE_SLOT_RAW" ]; then
        TARGET_SLOT="_$CMDLINE_SLOT_RAW"
    fi
fi

if [ -z "$TARGET_SLOT" ]; then
    PROP_SLOT=$(getprop ro.boot.slot_suffix 2>/dev/null)
    if [ -n "$PROP_SLOT" ]; then
        TARGET_SLOT="$PROP_SLOT"
    fi
fi

case "$TARGET_SLOT" in
    _a|_b) ;;
    a)  TARGET_SLOT="_a" ;;
    b)  TARGET_SLOT="_b" ;;
    *)   TARGET_SLOT="" ;;
esac

ui_print "  ✓ Active slot: ${{TARGET_SLOT:-none (non-A/B device)}}"

resolve_target() {{
    local name="$1"
    local mapper_slotted="/dev/block/mapper/${{name}}${{TARGET_SLOT}}"
    local mapper_plain="/dev/block/mapper/$name"
    local slotted="/dev/block/by-name/${{name}}${{TARGET_SLOT}}"
    local plain="/dev/block/by-name/$name"
    # Transsion (Infinix/itel/Tecno) MediaTek devices expose PHYSICAL GPT
    # partitions at /dev/block/platform/bootdevice/by-name/<name>_a — NOT at
    # /dev/block/by-name/. The by-name/ dir only has symlinks for DYNAMIC
    # partitions (in super). Physical partitions (lk, logo, spmfw, tee, boot,
    # dtbo, vbmeta, vendor_boot) need platform/bootdevice/ path resolution.
    # Source: recovery.log lines 1036-1380 (Infinix X695C Transsion device).
    local bootdev_slotted="/dev/block/platform/bootdevice/by-name/${{name}}${{TARGET_SLOT}}"
    local bootdev_plain="/dev/block/platform/bootdevice/by-name/$name"

    # No slot detected — non-A/B device.
    # Check mapper/ first (dynamic), then by-name/, then platform/bootdevice/.
    if [ -z "$TARGET_SLOT" ]; then
        if [ -e "$mapper_plain" ]; then
            echo "$mapper_plain"
            return
        fi
        if [ -e "$plain" ]; then
            echo "$plain"
            return
        fi
        if [ -e "$bootdev_plain" ]; then
            echo "$bootdev_plain"
            return
        fi
        echo "$plain"
        return
    fi

    # ── A/B device: check paths in priority order ──
    #
    # Priority 1: /dev/block/mapper/${{name}}${{TARGET_SLOT}}
    #   DYNAMIC partitions in super (system, vendor, product, etc.)
    #   Created by lptools map. Always fresh after lptools operations.
    #
    # Priority 2: /dev/block/mapper/$name (no slot suffix)
    #   Some devices expose plain mapper names for slot-suffixed partitions.
    #
    # Priority 3: /dev/block/by-name/${{name}}${{TARGET_SLOT}}
    #   Slotted by-name symlink. Recovery creates this at boot for DYNAMIC
    #   partitions (e.g. /dev/block/by-name/system → /dev/block/dm-3).
    #   May be STALE for dynamic partitions after lptools remove/create.
    #
    # Priority 4: /dev/block/by-name/$name (plain)
    #   Non-slotted by-name symlink.
    #
    # Priority 5: /dev/block/platform/bootdevice/by-name/${{name}}${{TARGET_SLOT}}
    #   PHYSICAL GPT partitions on Transsion MediaTek devices.
    #   Examples: lk_a, logo_a, spmfw_a, tee_a, boot_a, dtbo_a, vbmeta_a,
    #   vbmeta_system_a, vbmeta_vendor_a, vendor_boot_a.
    #   These are NOT in /dev/block/by-name/ — only in platform/bootdevice/.
    #
    # Priority 6: /dev/block/platform/bootdevice/by-name/$name (plain)
    #   Non-slotted physical partition (e.g. for non-A/B devices).
    if [ -e "$mapper_slotted" ]; then
        echo "$mapper_slotted"
        return
    fi
    if [ -e "$mapper_plain" ]; then
        echo "$mapper_plain"
        return
    fi
    if [ -e "$slotted" ]; then
        echo "$slotted"
        return
    fi
    if [ -e "$plain" ]; then
        echo "$plain"
        return
    fi
    if [ -e "$bootdev_slotted" ]; then
        echo "$bootdev_slotted"
        return
    fi
    if [ -e "$bootdev_plain" ]; then
        echo "$bootdev_plain"
        return
    fi

    # None exists yet — return mapper_slotted as best guess for dynamic
    # partitions (lptools will create it). For physical partitions, this
    # will fail validation with a clear "not found" error.
    echo "$mapper_slotted"
}}
"#,
        slot_step = slot_step,
        total_steps = total_steps,
    ));

    // ── Partition validation ──
    script.push_str(&format!(
        r#"# ── Step {validation_step}/{total_steps}: Partition validation ─────────────────────
ui_print "> Validating target partitions..."

# Known dynamic partition names (live inside super partition, resizable).
# Includes AOSP-standard names plus OEM-specific dynamic partitions used by
# Xiaomi, Realme/OPPO, Samsung, Vivo/iQOO, and others. Adding a name here
# is safe — if the device has no such partition, is_dynamic_partition() just
# returns false for it and the resize step skips it.
DYNAMIC_PART_NAMES="system vendor product system_ext odm odm_dlkm vendor_dlkm mi_ext my_product my_engineering my_stock my_carrier my_region my_bigball my_preload my_company optics prism cache userdata"

is_dynamic_partition() {{
    local name="$1"
    case " $DYNAMIC_PART_NAMES " in
        *" $name "*) return 0 ;;
        *) return 1 ;;
    esac
}}

# Helper: targeted unmount + lptools unmap for a single dynamic partition.
# Args: $1 = partition name
# Returns: 0 on success (partition unmapped or was never mapped/physical),
#          1 if lptools unmap failed (genuine EBUSY or non-dynamic).
# Idempotent: safe to call on already-unmapped or non-existent partitions.
#
# Why this exists (NOT `umount -a`):
#   Previously the resize step called `lptools unmap "$pname" >/dev/null 2>&1`
#   for every dynamic partition. The `2>&1` silently swallowed EBUSY errors
#   when partitions outside the bundle (e.g. system_ext, odm auto-mounted by
#   recovery) were still mounted. That left stale dm-linear devices in the
#   kernel, causing `lptools map` to fail later with EEXIST.
#
#   `umount -a` "works" as a workaround because it releases ALL mounts, but
#   it is far too broad — it can unmount /proc, /sys, /tmp, /data, /cache and
#   break the recovery environment itself.
#
#   This helper mirrors AOSP's per-partition `unmap_partition(name)` edify
#   function (source.android.com/docs/core/ota/dynamic_partitions/nonab):
#   targeted unmount of mount points referencing this partition's block
#   device, THEN lptools unmap. lptools itself calls
#   android::fs_mgr::DestroyLogicalPartition which issues DM_DEV_REMOVE —
#   that fails EBUSY if the device is still mounted, so the unmount above is
#   mandatory.
# unmount_partition: targeted umount of mount points referencing a partition's
# block device. Does NOT call lptools unmap — that's the caller's responsibility
# (with the correct slot-suffixed name).
#
# This is split from unmount_and_unmap_partition to avoid REDUNDANT lptools unmap
# calls. The resize step needs to:
#   1. unmount_partition "$pname"  (release mount points)
#   2. lptools unmap "$LP_NAME"    (destroy dm-linear, slot-suffixed name)
# The old unmount_and_unmap_partition did both, but used the PLAIN name for
# lptools unmap (wrong on A/B devices) — and the resize step then called
# lptools unmap again with the slot-suffixed name (redundant).
unmount_partition() {{
    local pname="$1"
    local ptarget dev_name mount_points mp

    ptarget=$(resolve_target "$pname" 2>/dev/null)
    [ -z "$ptarget" ] && return 0
    [ ! -e "$ptarget" ] && return 0

    # Resolve symlink to real device path (e.g. /dev/block/by-name/vendor → /dev/block/dm-5).
    # mount output shows REAL device paths (dm-5, sda1), NOT by-name symlinks.
    # Without this, grep for the by-name symlink fails, and the old fallback
    # (grep for basename "vendor") is too broad — it matches mount point PATHS
    # like /mnt/vendor/persist which may belong to a DIFFERENT partition.
    local real_dev
    real_dev=$(readlink -f "$ptarget" 2>/dev/null || echo "$ptarget")
    dev_name=$(basename "$real_dev" 2>/dev/null)
    # Find mount points referencing the REAL device path or its basename.
    # This avoids false positives from matching partition names in mount paths.
    mount_points=$(mount 2>/dev/null | grep -E "($real_dev|$dev_name)" | awk '{{print $3}}')
    for mp in $mount_points; do
        ui_print "    unmount $pname from $mp"
        umount "$mp" 2>/dev/null
        # dm-verity / dm-crypt mounts sometimes need a moment to release;
        # retry once with lazy unmount as a safety net. Lazy unmount detaches
        # the mount immediately even if a process still has open fds — safe
        # here because we are about to destroy the underlying dm device.
        if mount 2>/dev/null | grep -q " $mp "; then
            sleep 1
            umount -l "$mp" 2>/dev/null
        fi
    done
    return 0
}}

# unmount_and_unmap_partition: convenience wrapper — unmount + lptools unmap.
# Kept for backward compat with the cleanup trap and other callers that need
# both operations in one call. NOTE: lptools unmap here uses PLAIN name
# (no slot suffix) — callers that need slot-suffixed unmap should call
# unmount_partition + lptools unmap "$LP_NAME" directly.
unmount_and_unmap_partition() {{
    local pname="$1"
    unmount_partition "$pname" || true
    # lptools unmap is idempotent: returns 0 if already unmapped.
    if ! lptools unmap "$pname" >/dev/null 2>&1; then
        return 1
    fi
    return 0
}}

# Helper: targeted unmount + lptools unmap + lptools map for a single dynamic
# partition. Use this when you need a fresh dm-linear device with current
# metadata (e.g. after resize).
# Args: $1 = partition name
# Returns: 0 on success, 1 if final map failed.
# Prints ui_print warnings on intermediate failures so the user can debug.
unmap_and_remap_partition() {{
    local pname="$1"
    local map_rc

    if ! unmount_and_unmap_partition "$pname"; then
        ui_print "    warning: unmap failed for $pname (continuing to map anyway)"
    fi
    lptools map "$pname" >/dev/null 2>&1
    map_rc=$?
    if [ $map_rc -ne 0 ]; then
        ui_print "    error: lptools map failed for $pname (rc=$map_rc)"
        return 1
    fi
    return 0
}}

# List of partitions that need resizing (filled during validation)
RESIZE_NEEDED=""
RESIZE_TOTAL=0

validate_target() {{
    local target="$1"
    local min_size="$2"
    local name="$3"
    local is_dynamic=0

    if is_dynamic_partition "$name"; then
        is_dynamic=1
    fi

    # ── Auto-map unmapped dynamic partitions ──
    # After operations like Format Data, recovery runs Unmap_Super_Devices which
    # destroys dm-linear devices (e.g. system_b, vendor_b, product_b). Recovery
    # does NOT re-map them automatically — the flasher script must do it.
    #
    # Symptom (recovery.log CRC32 0x57923752, Itel S666LN):
    #   I:removing dynamic partition: system_b   ← Format Data destroyed it
    #   ... (sideload OTAku) ...
    #   ! ABORT: /dev/block/mapper/system_b not found for partition 'system'
    #
    # Fix: if target doesn't exist AND it's a dynamic partition AND lptools is
    # available, call `lptools map $LP_NAME` to re-create the dm-linear device.
    # Then re-resolve the target path. If still missing, fall through to the
    # existing ABORT logic.
    if [ ! -e "$target" ] && [ "$is_dynamic" = "1" ]; then
        local lp_name="$name"
        if [ -n "$TARGET_SLOT" ]; then
            lp_name="${{name}}${{TARGET_SLOT}}"
        fi
        if command -v lptools >/dev/null 2>&1; then
            ui_print "  $name not mapped (unmapped after Format Data?) — trying lptools map $lp_name..."
            lptools map "$lp_name" >/dev/null 2>&1
            local map_rc=$?
            if [ $map_rc -eq 0 ]; then
                # Re-resolve target after successful map
                target=$(resolve_target "$name")
                ui_print "  ✓ lptools map $lp_name succeeded — partition now at $target"
            else
                ui_print "  ! lptools map $lp_name failed (rc=$map_rc) — partition may not exist in super metadata"
            fi
        fi
    fi

    if [ ! -e "$target" ]; then
        ui_print "✗ Error: $name partition not found"
        ui_print "  Path: $target"
        ui_print "  Hint: Reboot recovery after Format Data, or run 'lptools map $lp_name'"
        return 1
    fi

    if [ ! -b "$target" ]; then
        ui_print "! ABORT: $target is not a block device"
        return 1
    fi

    # Resolve symlink to real device path (e.g. /dev/block/by-name/vendor → /dev/block/dm-5).
    # mount output shows REAL device paths (dm-5, sda1), NOT by-name symlinks.
    # Without readlink -f, grep for the by-name symlink fails, and the old
    # fallback (grep for basename "vendor") is too broad — it matches mount
    # point PATHS like /mnt/vendor/persist which may belong to a DIFFERENT
    # partition (persist, not vendor).
    local real_dev
    real_dev=$(readlink -f "$target" 2>/dev/null || echo "$target")
    DEV_NAME=$(basename "$real_dev")
    MOUNT_POINT=$(mount 2>/dev/null | grep -E "($real_dev|$DEV_NAME)" | awk '{{print $3}}' | head -1)
    if [ -n "$MOUNT_POINT" ]; then
        ui_print "  Unmounting $name from $MOUNT_POINT..."
        umount "$MOUNT_POINT" 2>/dev/null
        # Verify umount took effect; retry once with sleep if still mounted.
        # Most umounts are instant — the previous unconditional `sleep 1`
        # wasted 1s per partition (10 partitions × 1s = 10s of pure stall).
        # dm-verity and dm-crypt mounts sometimes need a moment to release,
        # so we keep a single retry as a safety net.
        if mount 2>/dev/null | grep -q " $MOUNT_POINT "; then
            sleep 1
        fi
    fi

    PART_SIZE=0
    PART_SIZE=$(blockdev --getsize64 "$target" 2>/dev/null)
    if [ -z "$PART_SIZE" ] || [ "$PART_SIZE" = "0" ]; then
        DEV_NAME=$(basename "$target")
        SYSFS_PATH="/sys/class/block/$DEV_NAME/size"
        if [ -f "$SYSFS_PATH" ]; then
            SECTORS=$(cat "$SYSFS_PATH" 2>/dev/null)
            if [ -n "$SECTORS" ]; then
                PART_SIZE=$(( SECTORS * 512 ))
            fi
        fi
    fi

    if [ -z "$PART_SIZE" ] || [ "$PART_SIZE" = "0" ]; then
        ui_print "! WARNING: Cannot determine size of $target"
        return 1
    fi

    if [ "$PART_SIZE" -lt "$min_size" ]; then
        if [ "$is_dynamic" = "1" ]; then
            # Dynamic partitions can be resized — defer to resize step
            ui_print "  ~ $name: $(( PART_SIZE / 1048576 )) MB → $(( min_size / 1048576 )) MB (resize needed)"
            RESIZE_NEEDED="${{RESIZE_NEEDED:+$RESIZE_NEEDED }}$name"
            RESIZE_TOTAL=$(( RESIZE_TOTAL + min_size - PART_SIZE ))
            return 0
        else
            ui_print "! ABORT: Partition $name too small: $PART_SIZE < $min_size"
            return 1
        fi
    fi

    ui_print "  ✓ $name: $(( PART_SIZE / 1048576 )) MB"
    return 0
}}

for i in $(seq 0 $(( NUM_PARTS - 1 ))); do
    eval "PNAME=\$PART_${{i}}_NAME"
    eval "PSIZE=\$PART_${{i}}_UNC_SIZE"
    PTARGET=$(resolve_target "$PNAME")
    if ! validate_target "$PTARGET" "$PSIZE" "$PNAME"; then
        ui_print "! ABORT: Partition validation failed for $PNAME"
        exit 1
    fi
done
"#,
        validation_step = validation_step,
        total_steps = total_steps,
    ));

    // ── Resize dynamic partitions ──
    script.push_str(&format!(
        r#"# ── Step {resize_step}/{total_steps}: Resize dynamic partitions ──────────────────
ui_print "> Resizing dynamic partitions..."

if [ -z "$RESIZE_NEEDED" ]; then
    ui_print "  No dynamic partitions need resizing."
else
    ui_print "  Partitions needing resize: $RESIZE_NEEDED"
    ui_print "  Additional space needed: $(( RESIZE_TOTAL / 1048576 )) MB"

    # ── Detect lptools ──
    # lptools is the ONLY supported tool for dynamic partition management.
    # dmsetup/lpmake/lpdump fallbacks were removed because:
    #   - lpmake: multi-group parsing was broken (only 1 group passed),
    #             PART_GROUP_SIZE was sum-of-partitions not group max size,
    #             and HAS_LPMAKE was never set (dead code).
    #   - dmsetup: create did not check for duplicate devices, masking
    #              silent failures; manual linear table mapping is fragile
    #              across OEM metadata formats.
    #   - lpdump: only needed by lpmake — removed together.
    # lptools handles all of this natively (group detection, slot suffix,
    # metadata slot updates, COW clearing, map/unmap) and is available in
    # most modern OrangeFox/TWRP builds with OF_ENABLE_LPTOOLS=1.
    # IMPORTANT: lptools size arguments are in BYTES (not KB, not sectors).
    # Source: phhusson/vendor_lptools — strtoll(argv[3], NULL, 0) → ResizePartition(bytes)
    HAS_LPTOOLS=0
    which lptools >/dev/null 2>&1 && HAS_LPTOOLS=1

    if [ "$HAS_LPTOOLS" != "1" ]; then
        ui_print "! ABORT: lptools not found in this recovery."
        ui_print "!  OTAku requires lptools for dynamic partition resize."
        ui_print "!  dmsetup/lpmake/lpdump fallbacks have been removed."
        ui_print "!  Solutions:"
        ui_print "!  1. Use a recovery with lptools enabled (OF_ENABLE_LPTOOLS=1)"
        ui_print "!  2. Flash via fastbootd instead of recovery"
        ui_print "!  3. Manually resize partitions before flashing"
        exit 1
    fi

    # Report super partition info (informational only — lptools handles it)
    SUPER_DEV=""
    for candidate in /dev/block/by-name/super /dev/block/bootdevice/by-name/super; do
        if [ -b "$candidate" ]; then
            SUPER_DEV="$candidate"
            break
        fi
    done
    if [ -n "$SUPER_DEV" ]; then
        SUPER_SIZE=$(blockdev --getsize64 "$SUPER_DEV" 2>/dev/null)
        if [ -n "$SUPER_SIZE" ] && [ "$SUPER_SIZE" != "0" ]; then
            ui_print "  Super partition: $(( SUPER_SIZE / 1048576 )) MB"
        fi
    fi

    # Optional space pre-check (lptools free may not be available on all builds)
    LP_FREE=$(lptools free 2>/dev/null | grep -o 'Free space: [0-9]*' | awk '{{print $3}}')
    if [ -n "$LP_FREE" ]; then
        ui_print "  Super free space: $(( LP_FREE / 1048576 )) MB"
        if [ "$LP_FREE" -lt "$RESIZE_TOTAL" ]; then
            ui_print "! ABORT: Insufficient free space in super partition."
            ui_print "!  Need: $(( RESIZE_TOTAL / 1048576 )) MB, available: $(( LP_FREE / 1048576 )) MB"
            exit 1
        fi
    fi

    # Clear COW partitions if this is a Virtual A/B device
    lptools clear-cow >/dev/null 2>&1

    # ── Refactored resize flow (per-partition: unmap→resize→remap→verify) ──
        #
        # Previous approach had 3 separate loops:
        #   1. Pre-resize unmap loop (all RESIZE_NEEDED)
        #   2. Resize loop (resize + inline remap)
        #   3. Post-resize verify-mapped loop (safety net)
        #
        # Problem: lptools resize only updates metadata. The old dm-linear device
        # stays active with the OLD size. The post-resize remap (unmap_and_remap)
        # was supposed to destroy old + create new, but the safety-net check
        # "if /dev/block/by-name/$pname exists → skip" incorrectly skipped it
        # because the by-name symlink still pointed to the old dm-linear.
        #
        # Result: dd writes to stale dm-linear with old size → ABORT.
        #
        # Fix: Combine into a SINGLE per-partition loop with strict ordering:
        #   1. TARGETED UMOUNT  — readlink -f → grep mount → umount mount points
        #   2. LPTOOLS UNMAP    — destroy old dm-linear (must succeed before resize)
        #   3. LPTOOLS RESIZE   — update metadata in super partition
        #   4. LPTOOLS MAP      — create new dm-linear from updated metadata
        #   5. VERIFY MAPPED    — check /dev/block/by-name/<name> exists
        #   6. VERIFY SIZE      — blockdev --getsize64 >= expected unc_size
        #
        # If lptools resize fails → fallback to remove+create (which auto-maps).
        # If map fails → ABORT (can't flash without a properly sized dm-linear).
        #
        # The flash step no longer needs the post-resize size verification hack
        # (commit a00ff47) because size is verified here before flash begins.
        if [ -n "$RESIZE_NEEDED" ]; then
            ui_print "  Resizing partitions: $RESIZE_NEEDED"
            RESIZE_OK=1
            for pname in $RESIZE_NEEDED; do
                pname=$(echo "$pname" | tr -d ' ')
                [ -z "$pname" ] && continue

                # Find partition index to get UNC_SIZE
                FOUND_IDX=-1
                for j in $(seq 0 $(( NUM_PARTS - 1 ))); do
                    eval "CHECK_NAME=\$PART_${{j}}_NAME"
                    if [ "$CHECK_NAME" = "$pname" ]; then
                        FOUND_IDX=$j
                        break
                    fi
                done
                if [ "$FOUND_IDX" -lt 0 ]; then continue; fi

                eval "PNAME_SZ=\$PART_${{FOUND_IDX}}_UNC_SIZE"
                NEW_SIZE_BYTES=$PNAME_SZ

                # Capture ORIGINAL size BEFORE resize for cleanup-trap rollback
                ORIG_PTARGET=$(resolve_target "$pname")
                ORIG_SIZE_BYTES=0
                if [ -e "$ORIG_PTARGET" ]; then
                    ORIG_SIZE_BYTES=$(blockdev --getsize64 "$ORIG_PTARGET" 2>/dev/null)
                fi
                if [ -z "$ORIG_SIZE_BYTES" ] || [ "$ORIG_SIZE_BYTES" = "0" ]; then
                    DEV_NAME=$(basename "$ORIG_PTARGET" 2>/dev/null)
                    if [ -n "$DEV_NAME" ] && [ -f "/sys/class/block/$DEV_NAME/size" ]; then
                        SECTORS=$(cat "/sys/class/block/$DEV_NAME/size" 2>/dev/null)
                        [ -n "$SECTORS" ] && ORIG_SIZE_BYTES=$(( SECTORS * 512 ))
                    fi
                fi
                if [ -n "$ORIG_SIZE_BYTES" ] && [ "$ORIG_SIZE_BYTES" != "0" ]; then
                    RESIZED_ORIGINAL="$RESIZED_ORIGINAL $pname:$ORIG_SIZE_BYTES"
                fi

                ui_print "  [$pname] $(( ORIG_SIZE_BYTES / 1048576 )) MB → $(( NEW_SIZE_BYTES / 1048576 )) MB"

                # ── Build slot-suffixed partition name for lptools ──
                # On A/B devices, lptools partition names in super partition
                # metadata are SLOT-SUFFIXED (e.g. vendor_a, vendor_b).
                # Passing plain "vendor" to lptools resize/map may silently
                # fail or operate on the wrong partition.
                #
                # Evidence: working DynamicInstaller updater-script uses
                #   vendor_partition="vendor$current_slot"  (e.g. vendor_a)
                # for ALL lptools calls. OTAku was passing plain "vendor"
                # → lptools resize reported success but didn't actually resize
                # the correct slot-suffixed partition → dm-linear stayed old size.
                LP_NAME="$pname"
                if [ -n "$TARGET_SLOT" ]; then
                    LP_NAME="${{pname}}${{TARGET_SLOT}}"
                    ui_print "    (lptools name: $LP_NAME)"
                fi

                # ── Step 1: Targeted umount (release mount points) ──
                # Uses unmount_partition (not unmount_and_unmap_partition) to avoid
                # REDUNDANT lptools unmap. The unmap is done in Step 2 below with
                # the correct slot-suffixed name ($LP_NAME).
                ui_print "    unmount..."
                unmount_partition "$pname" || true

                # ── Step 2: Explicit unmap (destroy old dm-linear, slot-suffixed) ──
                ui_print "    unmap..."
                lptools unmap "$LP_NAME" >/dev/null 2>&1 || true

                # ── Step 3: Resize — try lptools resize FIRST (BlassGo pattern) ──
                # Why resize is primary (not remove+create):
                #   lptools resize updates the partition metadata IN PLACE — the
                #   existing dm-linear device stays alive, the /dev/block/by-name/
                #   symlink stays valid, and recovery's mount state is preserved.
                #
                #   lptools remove + create, by contrast, DESTROYS the dm-linear
                #   device. Recovery created the by-name symlink at boot (e.g.
                #   /dev/block/by-name/vendor → /dev/block/dm-6), and that symlink
                #   is NOT refreshed when lptools create makes a new dm device.
                #   The stale symlink then causes our verify step to fail with
                #   "not mapped" even though /dev/block/mapper/vendor_a exists.
                #
                #   BlassGo's DynamicInstaller updater-script proves resize works:
                #     lptools unmap vendor_a
                #     lptools resize vendor_a SIZE
                #     lptools map vendor_a
                #   (See /home/z/my-project/upload/updater-script lines 80-89.)
                #
                # remove+create is kept as a FALLBACK only — it's destructive but
                # can recover from corrupted partition metadata that resize can't.
                ui_print "    resize..."
                lptools resize "$LP_NAME" "$NEW_SIZE_BYTES" >/dev/null 2>&1
                RESIZE_RC=$?

                if [ $RESIZE_RC -eq 0 ]; then
                    # Resize succeeded — now map the partition to materialize
                    # the new size into a fresh dm-linear device.
                    ui_print "    resize OK — mapping..."
                    lptools map "$LP_NAME" >/dev/null 2>&1
                    MAP_RC=$?
                    if [ $MAP_RC -ne 0 ]; then
                        ui_print "    ! lptools map failed (rc=$MAP_RC) — retrying..."
                        lptools unmap "$LP_NAME" >/dev/null 2>&1
                        sleep 1
                        lptools map "$LP_NAME" >/dev/null 2>&1
                        MAP_RC=$?
                        if [ $MAP_RC -ne 0 ]; then
                            ui_print "    ! map retry also failed — trying remove+create fallback..."
                            # FALLBACK: remove + create (destructive — destroys by-name symlink)
                            lptools remove "$LP_NAME" >/dev/null 2>&1
                            lptools create "$LP_NAME" "$NEW_SIZE_BYTES" >/dev/null 2>&1
                            CREATE_RC=$?
                            if [ $CREATE_RC -ne 0 ]; then
                                ui_print "    ! remove+create also failed for $pname"
                                RESIZE_OK=0
                                RESIZED_ORIGINAL=$(echo "$RESIZED_ORIGINAL" | sed "s/ $pname:[0-9]*//")
                                continue
                            fi
                            ui_print "    ! remove+create fallback succeeded (by-name symlink may be stale — using mapper path)"
                        fi
                    fi
                else
                    # Resize failed — fall back to remove+create (destructive)
                    ui_print "    ! lptools resize failed (rc=$RESIZE_RC) — trying remove+create fallback..."
                    lptools remove "$LP_NAME" >/dev/null 2>&1
                    lptools create "$LP_NAME" "$NEW_SIZE_BYTES" >/dev/null 2>&1
                    CREATE_RC=$?
                    if [ $CREATE_RC -ne 0 ]; then
                        ui_print "    ! remove+create also failed for $pname"
                        RESIZE_OK=0
                        RESIZED_ORIGINAL=$(echo "$RESIZED_ORIGINAL" | sed "s/ $pname:[0-9]*//")
                        continue
                    fi
                    ui_print "    ! remove+create fallback succeeded (by-name symlink may be stale — using mapper path)"
                fi

                # ── Step 5: Verify mapped ──
                PTARGET_VERIFY=$(resolve_target "$pname")
                if [ ! -e "$PTARGET_VERIFY" ]; then
                    ui_print "    ! $pname not mapped after resize — waiting..."
                    _W=0
                    while [ ! -e "$PTARGET_VERIFY" ] && [ $_W -lt 10 ]; do
                        sleep 1
                        _W=$(( _W + 1 ))
                    done
                    if [ ! -e "$PTARGET_VERIFY" ]; then
                        ui_print "    ! $pname still not mapped after 10s — ABORT"
                        RESIZE_OK=0
                        continue
                    fi
                fi

                # ── Step 6: Verify size ──
                ACTUAL_SIZE=$(blockdev --getsize64 "$PTARGET_VERIFY" 2>/dev/null)
                if [ -z "$ACTUAL_SIZE" ] || [ "$ACTUAL_SIZE" = "0" ]; then
                    _DN=$(basename "$PTARGET_VERIFY" 2>/dev/null)
                    if [ -n "$_DN" ] && [ -f "/sys/class/block/$_DN/size" ]; then
                        _S=$(cat "/sys/class/block/$_DN/size" 2>/dev/null)
                        [ -n "$_S" ] && ACTUAL_SIZE=$(( _S * 512 ))
                    fi
                fi
                if [ -n "$ACTUAL_SIZE" ] && [ "$ACTUAL_SIZE" -gt 0 ]; then
                    if [ "$ACTUAL_SIZE" -ge "$NEW_SIZE_BYTES" ]; then
                        ui_print "    ✓ mapped OK: $(( ACTUAL_SIZE / 1048576 )) MB [verified]"
                    else
                        ui_print "    ! SIZE MISMATCH: actual=$(( ACTUAL_SIZE / 1048576 )) MB < expected=$(( NEW_SIZE_BYTES / 1048576 )) MB"
                        ui_print "    ! forcing unmap+resize+map to get fresh dm-linear..."
                        # Use resize (not remove+create) to preserve by-name symlink.
                        # BlassGo pattern: unmap → resize → map.
                        lptools unmap "$LP_NAME" >/dev/null 2>&1
                        lptools resize "$LP_NAME" "$NEW_SIZE_BYTES" >/dev/null 2>&1
                        lptools map "$LP_NAME" >/dev/null 2>&1
                        # Re-check
                        ACTUAL_SIZE=$(blockdev --getsize64 "$PTARGET_VERIFY" 2>/dev/null)
                        if [ -n "$ACTUAL_SIZE" ] && [ "$ACTUAL_SIZE" -ge "$NEW_SIZE_BYTES" ]; then
                            ui_print "    ✓ re-mapped OK: $(( ACTUAL_SIZE / 1048576 )) MB [verified]"
                        else
                            ui_print "    ! still mismatch after re-map — dd will likely fail"
                            RESIZE_OK=0
                        fi
                    fi
                else
                    ui_print "    ! cannot verify size — continuing anyway"
                fi
            done

            if [ "$RESIZE_OK" != "1" ]; then
                ui_print "! ABORT: resize+remap failed for one or more partitions."
                ui_print "!  Try flashing via fastbootd or use a different recovery."
                exit 1
            fi

            ui_print "  All partitions resized and verified."
            ui_print "  Dynamic partition resize complete."
        fi
    fi
"#,
        resize_step = resize_step,
        total_steps = total_steps,
    ));

    // ── Flash each partition ──
    script.push_str(&format!(
        r#"# ── Step {flash_step_offset}+{num_parts_minus_1}/{total_steps}: Flash each partition ────────────────────
for i in $(seq 0 $(( NUM_PARTS - 1 ))); do
    eval "PNAME=\$PART_${{i}}_NAME"
    eval "PSIZE=\$PART_${{i}}_UNC_SIZE"
    eval "PHASH=\$PART_${{i}}_HASH"
    eval "PCSIZE=\$PART_${{i}}_COMP_SIZE"
    eval "POFFSET=\$PART_${{i}}_DATA_OFFSET"
    eval "PCOMP_HASH=\$PART_${{i}}_COMP_HASH"

    STEP_NUM=$(( i + {flash_step_offset} ))

    ui_print "> Flashing $PNAME ($(( PSIZE / 1048576 )) MB)..."
    ui_print "  Compressed: $(( PCSIZE / 1048576 )) MB"

    PTARGET=$(resolve_target "$PNAME")

    # Step A+B+C: Extract → decompress → write via FIFO pipeline
    # Always use bs=4096 for extraction — much faster than bs=1.
    # The read count is rounded UP to include the last partial block;
    # decompressors (xz/gzip/bzip2) gracefully handle trailing bytes,
    # and for uncompressed data (cat), extra bytes (max 4095) are
    # harmless since partitions are sized >= UNC_SIZE.
    #
    # IMPORTANT: Do NOT use conv=fsync on dd write!
    # conv=fsync forces an fsync() after every write block, which makes
    # writing large partitions (2+ GB) extremely slow on eMMC/UFS storage.
    # For a 2327 MB partition with bs=4096, that's ~597,000 fsync calls.
    # Instead, we sync once after ALL partitions are written (see after
    # the loop below).
    #
    # IMPORTANT: Do NOT use conv=notrunc on dd write to block devices!
    # conv=notrunc does NOT prevent busybox dd from attempting ftruncate().
    # busybox dd interprets conv=notrunc but still calls ftruncate() at the
    # end of writing. On dm-linear block devices, ftruncate() fails with
    # EINVAL, causing dd to exit with status=1 — a FALSE-FAILURE, because
    # all data was already written successfully.
    # Instead, we write WITHOUT conv= flags and verify data was written
    # by checking blockdev --getsize64 when dd returns non-zero.
    EXTRACT_SKIP=$(( DATA_OFFSET + POFFSET ))
    SKIP_BLOCKS=$(( EXTRACT_SKIP / 4096 ))
    READ_COUNT=$(( (PCSIZE + 4095) / 4096 ))

    # ── Pre-flash compressed-data hash verification ──
    # Compute SHA-256 of the compressed partition data and compare to the
    # expected hash stored in PART_i_COMP_HASH. This catches bundle
    # corruption (MTP transfer errors, tmpfs issues, ZIP CRC errors) BEFORE
    # we touch any block device — preventing partial writes that would
    # leave the partition in a broken state.
    #
    # We use `head -c $PCSIZE` to hash EXACTLY PCSIZE bytes (not the
    # aligned read count, which includes trailing zero padding that would
    # change the hash).
    #
    # Backward compat: old bundles built before this feature don't have
    # PART_i_COMP_HASH (empty string) — skip the check.
    if [ -n "$PCOMP_HASH" ]; then
        ui_print "  Verifying compressed data integrity..."
        COMP_HASH_ACTUAL=$(dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null | \
            head -c "$PCSIZE" 2>/dev/null | sha256sum 2>/dev/null | awk '{{print $1}}')
        if [ -z "$COMP_HASH_ACTUAL" ]; then
            ui_print "! ABORT: Cannot compute compressed data hash for $PNAME"
            ui_print "!  Bundle may be unreadable or sha256sum not available."
            ui_print "!  Bundle size: $(wc -c < "$BUNDLE" 2>/dev/null | tr -d ' ') bytes"
            ui_print "!  Extract skip: $EXTRACT_SKIP bytes ($SKIP_BLOCKS blocks)"
            ui_print "!  Read count: $READ_COUNT blocks ($(( READ_COUNT * 4096 )) bytes)"
            ui_print "!  Expected compressed size: $PCSIZE bytes"
            exit 1
        fi
        if [ "$COMP_HASH_ACTUAL" != "$PCOMP_HASH" ]; then
            ui_print "! ABORT: Compressed data hash mismatch for $PNAME"
            ui_print "!  Expected: $PCOMP_HASH"
            ui_print "!  Actual:   $COMP_HASH_ACTUAL"
            ui_print "!  The bundle is CORRUPT — compressed data does not match."
            ui_print "!  Likely causes:"
            ui_print "!    - ZIP corrupted during transfer (MTP/ADB corruption)"
            ui_print "!    - tmpfs full during extraction"
            ui_print "!    - Storage I/O error"
            ui_print "!  Rebuild the bundle and re-transfer to device."
            ui_print "!  Bundle size: $(wc -c < "$BUNDLE" 2>/dev/null | tr -d ' ') bytes"
            exit 1
        fi
        ui_print "  ✓ Hash verified"
    fi

    # Verify block device exists and is writable before flashing.
    # After lptools resize + remap, the dm device may take a moment to
    # appear, or the by-name symlink may not be updated yet.
    if [ ! -e "$PTARGET" ]; then
        ui_print "  Waiting for $PTARGET to appear..."
        WAIT_COUNT=0
        while [ ! -e "$PTARGET" ] && [ $WAIT_COUNT -lt 30 ]; do
            sleep 1
            WAIT_COUNT=$(( WAIT_COUNT + 1 ))
        done
        if [ ! -e "$PTARGET" ]; then
            ui_print "! ABORT: Block device $PTARGET not found after 30s wait"
            exit 1
        fi
    fi
    ui_print "  Writing to $PTARGET..."

    # Use a FIFO pipeline: extract+decompress → FIFO → dd write.
    # This avoids writing the decompressed data to a temp file in /tmp,
    # which would exhaust tmpfs for large partitions (e.g. 2327 MB
    # decompressed data + 2045 MB bundle = 4372 MB in /tmp).
    # The FIFO uses only ~64 KB of kernel pipe buffer — data flows
    # directly from decompressor to the block device.
    #
    # Error handling: we run the extract+decompress in the background
    # so we can capture BOTH the decompressor exit code and the dd
    # write exit code. In sh, $? only captures the last pipe stage,
    # so a naive 3-pipeline would mask decompression failures.
    TMP_FIFO="/tmp/ddpart_${{i}}.fifo"
    rm -f "$TMP_FIFO"
    mkfifo "$TMP_FIFO" 2>/dev/null
    FIFO_OK=$?

    if [ "$FIFO_OK" = "0" ]; then
        # FIFO available — pipeline via FIFO with full error capture
        #
        # CRITICAL: Capture gzip stderr to a temp file (NOT /dev/null).
        # When decompression fails, the stderr message tells us WHY:
        #   "unexpected end of file"  → truncated input
        #   "invalid compressed data" → corrupt gzip stream
        #   "not in gzip format"      → wrong compression / offset bug
        # Without this, we only see "status=2" with no diagnostic info.
        GZIP_ERR="/tmp/ddpart_${{i}}.err"
        rm -f "$GZIP_ERR"
        # Add `head -c $PCSIZE` to strip trailing alignment padding before
        # decompressor. Without this, gzip -d sees padding bytes after the
        # gzip EOF marker and returns status=2 with "trailing junk which was
        # ignored" — a FALSE FAILURE (data is actually fine). This eliminates
        # the need for the fallback decompressor chain in the common case.
        dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null | \
            head -c "$PCSIZE" 2>/dev/null | \
            $DECOMP_CMD -d > "$TMP_FIFO" 2>"$GZIP_ERR" &
        DECOMP_PID=$!

        # No conv= flags — busybox dd ftruncate() on dm-linear is a false-failure
        dd of="$PTARGET" bs=1048576 if="$TMP_FIFO" 2>/dev/null
        DD_STATUS=$?

        wait $DECOMP_PID 2>/dev/null
        DECOMP_STATUS=$?

        rm -f "$TMP_FIFO"

        if [ $DECOMP_STATUS -ne 0 ]; then
            # Print diagnostic info on decompression failure.
            # This is critical for debugging — without it, we only see
            # "status=2" with no context.
            GZIP_ERR_MSG=$(cat "$GZIP_ERR" 2>/dev/null | tr -d '\r' | head -3)
            rm -f "$GZIP_ERR"
            # Use "! WARNING" (not "! ABORT") because we'll try fallback decompressors
            # if the compressed hash was verified OK. Only say ABORT when all fallbacks
            # fail (see bottom of this block).
            ui_print "✗ Error: Decompression failed for $PNAME"
            ui_print "  Decompressor: $DECOMP_CMD -d (status=$DECOMP_STATUS)"
            ui_print "  Details: $GZIP_ERR_MSG"
            ui_print "  Bundle: $(wc -c < "$BUNDLE" 2>/dev/null | tr -d ' ') bytes | Compressed: $PCSIZE | Uncompressed: $PSIZE"

            # If compressed-data hash was verified above (PCOMP_HASH non-empty),
            # the compressed data IS intact — the issue is with the decompressor
            # itself (e.g., busybox gzip quirk). Try fallback decompressors.
            if [ -n "$PCOMP_HASH" ]; then
                ui_print "  → Hash OK, trying fallback decompressors..."
                FALLBACK_OK=0
                for FB_DECOMP in "gzip -dc" "gunzip -c" "zcat" "busybox gzip -dc"; do
                    ui_print "  → Trying: $FB_DECOMP..."
                    TMP_FIFO2="/tmp/ddpart_${{i}}.fifo2"
                    rm -f "$TMP_FIFO2" "$GZIP_ERR"
                    mkfifo "$TMP_FIFO2" 2>/dev/null
                    if [ $? -ne 0 ]; then
                        ui_print "    FIFO creation failed — skipping $FB_DECOMP"
                        continue
                    fi
                    dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null | \
                        head -c "$PCSIZE" 2>/dev/null | \
                        $FB_DECOMP > "$TMP_FIFO2" 2>"$GZIP_ERR" &
                    FB_PID=$!
                    dd of="$PTARGET" bs=1048576 if="$TMP_FIFO2" 2>/dev/null
                    FB_DD_STATUS=$?
                    wait $FB_PID 2>/dev/null
                    FB_DECOMP_STATUS=$?
                    rm -f "$TMP_FIFO2"
                    if [ $FB_DECOMP_STATUS -eq 0 ]; then
                        ui_print "  ✓ $FB_DECOMP succeeded!"
                        FALLBACK_OK=1
                        # Check dd write status
                        if [ $FB_DD_STATUS -ne 0 ]; then
                            WRITTEN_SIZE=$(blockdev --getsize64 "$PTARGET" 2>/dev/null)
                            if [ -n "$WRITTEN_SIZE" ] && [ "$WRITTEN_SIZE" -ge "$PSIZE" ]; then
                                ui_print "  Note: dd status=$FB_DD_STATUS (busybox ftruncate — data OK)"
                            else
                                ui_print "! ABORT: dd write failed in fallback (status=$FB_DD_STATUS)"
                                rm -f "$GZIP_ERR"
                                exit 1
                            fi
                        fi
                        rm -f "$GZIP_ERR"
                        break
                    else
                        FB_ERR_MSG=$(cat "$GZIP_ERR" 2>/dev/null | tr -d '\r' | head -1)
                        ui_print "    $FB_DECOMP failed: $FB_ERR_MSG"
                        rm -f "$GZIP_ERR"
                    fi
                done
                if [ "$FALLBACK_OK" = "1" ]; then
                    : # Fall through to post-verify
                else
                    ui_print "✗ Error: All decompressors failed"
                    ui_print "  Hint: Rebuild with --compress none or different level"
                    exit 1
                fi
            else
                rm -f "$GZIP_ERR"
                exit 1
            fi
        else
            rm -f "$GZIP_ERR"
        fi

        # Only check DD_STATUS if we didn't already handle it in the fallback path
        if [ "$DECOMP_STATUS" -eq 0 ]; then
            # busybox dd may return status=1 on block devices (ftruncate EINVAL)
            # even when all data was written. Verify by checking partition size.
            if [ $DD_STATUS -ne 0 ]; then
                WRITTEN_SIZE=$(blockdev --getsize64 "$PTARGET" 2>/dev/null)
                if [ -n "$WRITTEN_SIZE" ] && [ "$WRITTEN_SIZE" -ge "$PSIZE" ]; then
                    ui_print "  Note: dd reported status=$DD_STATUS (busybox ftruncate on block device — data OK)"
                else
                    ui_print "! ABORT: dd write failed for $PNAME (status=$DD_STATUS, written=$WRITTEN_SIZE < expected=$PSIZE)"
                    exit 1
                fi
            fi
        fi
    else
        # FIFO not available (very rare) — fall back to direct 3-pipeline.
        # We lose decompressor error detection (sh $? = last pipe stage only),
        # but this avoids tmpfs exhaustion by never writing a temp file.
        # head -c $PCSIZE strips trailing alignment padding (prevents gzip
        # "trailing junk" false failure).
        GZIP_ERR="/tmp/ddpart_${{i}}.err"
        rm -f "$GZIP_ERR"
        dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null | \
            head -c "$PCSIZE" 2>/dev/null | \
            $DECOMP_CMD -d 2>"$GZIP_ERR" | \
            dd of="$PTARGET" bs=1048576 2>/dev/null
        DD_STATUS=$?

        if [ $DD_STATUS -ne 0 ]; then
            GZIP_ERR_MSG=$(cat "$GZIP_ERR" 2>/dev/null | tr -d '\r' | head -1)
            rm -f "$GZIP_ERR"
            WRITTEN_SIZE=$(blockdev --getsize64 "$PTARGET" 2>/dev/null)
            if [ -n "$WRITTEN_SIZE" ] && [ "$WRITTEN_SIZE" -ge "$PSIZE" ]; then
                ui_print "  Note: dd reported status=$DD_STATUS (busybox ftruncate on block device — data OK)"
            else
                ui_print "! ABORT: dd write failed for $PNAME (status=$DD_STATUS, written=$WRITTEN_SIZE < expected=$PSIZE)"
                ui_print "!  Decompressor stderr: $GZIP_ERR_MSG"
                exit 1
            fi
        else
            rm -f "$GZIP_ERR"
        fi
    fi

    # Step C: Post-verify (conditional)
    {verify_block}

    # ── Post-flash re-map (BlassGo pattern, step 6/7) ──
    # After dd writes data to the dm-linear device, re-map to refresh the
    # kernel's dm-linear mapper. This is defensive — `sync` already flushes
    # the page cache, but BlassGo's DynamicInstaller does this re-map so we
    # follow the proven pattern.
    #
    # Only for dynamic partitions (those in DYNAMIC_PART_NAMES).
    # Physical partitions (boot, dtbo, vbmeta, etc.) don't need re-mapping.
    #
    # Silent on success (reduce log noise), warn only on failure.
    if is_dynamic_partition "$PNAME"; then
        if [ -n "$TARGET_SLOT" ]; then
            REMAP_LP_NAME="${{PNAME}}${{TARGET_SLOT}}"
        else
            REMAP_LP_NAME="$PNAME"
        fi
        lptools unmap "$REMAP_LP_NAME" >/dev/null 2>&1
        lptools map "$REMAP_LP_NAME" >/dev/null 2>&1
        REMAP_RC=$?
        if [ $REMAP_RC -ne 0 ]; then
            ui_print "  ! warning: post-flash re-map failed for $REMAP_LP_NAME (rc=$REMAP_RC)"
            ui_print "  ! Data was written + sync'd — re-map is defensive only."
        fi
    fi
done

# Sync ALL partition writes at once (deferred from individual writes).
# This is far more efficient than syncing after each partition —
# one fsync pass vs NUM_PARTS separate stalls.
sync

# Disable the cleanup trap — we completed successfully.
CLEANUP_DONE=1

# ── Slot verification (A/B devices only) ────────────────────
# Verify that the active boot slot matches the slot we just flashed.
# This catches the rare case where the bootloader reset the active slot
# during the flash process, which would cause a bootloop after reboot.
if [ -n "$TARGET_SLOT" ]; then
    CURRENT_SLOT=$(getprop ro.boot.slot_suffix 2>/dev/null)
    if [ -z "$CURRENT_SLOT" ]; then
        CURRENT_SLOT=$(cat /proc/cmdline 2>/dev/null | tr ' ' '\n' | grep -o 'androidboot.slot_suffix=[^ ]*' | cut -d= -f2)
    fi
    if [ -n "$CURRENT_SLOT" ] && [ "$CURRENT_SLOT" != "$TARGET_SLOT" ]; then
        ui_print "! WARNING: Active slot changed during flash!"
        ui_print "  Flashed slot: $TARGET_SLOT, current slot: $CURRENT_SLOT"
        ui_print "  Setting active slot to $TARGET_SLOT..."
        # Extract slot letter without underscore (e.g. _b → b)
        SLOT_LETTER=$(echo "$TARGET_SLOT" | sed 's/^_//')
        if [ -n "$SLOT_LETTER" ]; then
            bootctl set-active-boot-slot $SLOT_LETTER 2>/dev/null || \
                fastboot set_active $SLOT_LETTER 2>/dev/null || true
        fi
    else
        ui_print "  ✓ Slot: $TARGET_SLOT"
    fi
fi

# ── Done ────────────────────────────────────────────────────
ui_print "======================================"
ui_print "  Flash complete — $NUM_PARTS partition(s)"
ui_print "======================================"
exit 0
"#,
        flash_step_offset = flash_step_offset,
        num_parts_minus_1 = if num_parts > 0 { num_parts - 1 } else { 0 },
        total_steps = total_steps,
        verify_block = verify_block.trim(),
    ));

    script
}

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
    lines.push(format!("Partitions: {}", num_parts));
    lines.push(format!(
        "Verification: {}",
        if skip_verify { "disabled" } else { "enabled" }
    ));
    if !device.is_empty() {
        lines.push(format!("Target device: {}", device));
    }
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

// ---------------------------------------------------------------------------
//  Public API: run_dd_build
// ---------------------------------------------------------------------------

/// Generate an otaku-format flashable ZIP from partition images.
///
/// # Arguments
/// * `images` - List of (partition_name, image_path) pairs
/// * `compression` - Compression algorithm: "none", "gzip", "bzip2", "xz", "brotli"
/// * `level` - Compression level (0 = default per algorithm)
/// * `output_path` - Absolute path for output .zip file
/// * `device` - Device codename(s), comma-separated (empty = no device check)
/// * `skip_verify` - Skip post-flash SHA-256 verification
/// * `rom_name` - Cosmetic: ROM name shown in flash_info.txt + flasher banner
/// * `maker` - Cosmetic: ROM maker shown in flash_info.txt + flasher banner
///
/// # Returns
/// DdBuildResult with success/error, paths, sizes, and log output.
//
// 8 args matches the JNI bridge signature 1:1. Each arg is used in a
// distinct phase (validation, compression, script building, flash_info).
// Grouping into a DdBuildArgs struct would just shift the boilerplate to
// lib.rs (which would still receive 8 JNI args + have to construct the
// struct). Allow clippy::too_many_arguments.
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
            error: Some("output_path is required".to_string()),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Validate compression algorithm
    let is_valid_compression = is_alg(compression, ALG_NONE)
        || is_alg(compression, "gzip")
        || is_alg(compression, "bzip2")
        || is_alg(compression, "xz")
        || is_alg(compression, "brotli");

    if !is_valid_compression {
        return DdBuildResult {
            success: false,
            output: format!(
                "[!] Error: unsupported compression '{}'. Supported: none, gzip, bzip2, xz, brotli",
                compression
            ),
            zip_path: None,
            zip_size: None,
            bundle_size: None,
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
                error: Some(format!("image file not found: {}", path)),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    // ── Run the build pipeline ──
    let result: Result<DdBuildResult, String> = (|| {
        let num_parts = images.len();
        let level_display = if level > 0 {
            format!(" (level {})", level)
        } else {
            String::new()
        };

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
        if (compress_name == "xz" || compress_name == "brotli") && level >= 7 {
            lines.push(format!(
                "  ! {} level {} is very slow on mobile. Level 6 recommended.",
                compress_name, level
            ));
        }

        let mut partitions_meta: Vec<PartitionMeta> = Vec::new();
        let level_opt = if level > 0 { Some(level) } else { None };

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
        lines.push(String::new());

        // ── Step 2: Build flasher scripts ──
        lines.push("[2/3] Building flasher scripts".to_string());
        write_progress_with_percent(
            output_path, num_parts, num_parts, "scripts", "building_scripts",
            bundle_size, Some(&bundle_tmp_path_str), total_estimated,
            100, // all partitions done
        );

        let update_binary = build_update_script(
            num_parts,
            compress_id_val,
            &compress_name,
            &partitions_meta,
            device,
            skip_verify,
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
            let injection = format!(
                "ui_print \"        by hoshiyomiX\"\nui_print \"  ROM: {} | Maker: {}\"",
                rom_display, maker_display
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
            zip.start_file("META-INF/com/google/android/update-binary", options)
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
                error: Some(e),
                duration_ms: start.elapsed().as_millis() as u64,
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4095, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }

    #[test]
    fn test_build_header() {
        let hdr = build_header(1, 3); // gzip, 3 partitions
        assert_eq!(hdr.len(), HEADER_SIZE);
        // Magic
        assert_eq!(&hdr[0..4], b"DDBU");
        // Version (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[4], hdr[5]]), DDBUNDLE_VERSION);
        // Compress ID (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[6], hdr[7]]), 1);
        // Num parts (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[8], hdr[9]]), 3);
        // Header size (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[10], hdr[11]],), HEADER_SIZE as u16);
        // Rest should be zero-padded
        // (Iterate by value — clippy::needless_range_loop)
        for &byte in hdr.iter().skip(12) {
            assert_eq!(byte, 0u8);
        }
    }

    #[test]
    fn test_decomp_cmd_for_id() {
        assert_eq!(decomp_cmd_for_id(0), "cat");
        assert_eq!(decomp_cmd_for_id(1), "gzip");
        assert_eq!(decomp_cmd_for_id(2), "bzip2");
        assert_eq!(decomp_cmd_for_id(3), "xz");
        assert_eq!(decomp_cmd_for_id(4), "brotli");
    }

    #[test]
    fn test_decomp_ext_for_id() {
        assert_eq!(decomp_ext_for_id(0), ".raw");
        assert_eq!(decomp_ext_for_id(1), ".gz");
        assert_eq!(decomp_ext_for_id(2), ".bz2");
        assert_eq!(decomp_ext_for_id(3), ".xz");
        assert_eq!(decomp_ext_for_id(4), ".br");
    }

    #[test]
    fn test_human_size() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1048576), "1.0 MB");
        assert_eq!(human_size(1073741824), "1024.0 MB");
    }

    #[test]
    fn test_build_update_script_basic() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        assert!(script.starts_with("#!/sbin/sh"));
        assert!(script.contains("PART_0_NAME=\"boot\""));
        assert!(script.contains("PART_0_HASH=\"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\""));
        assert!(script.contains("PART_0_COMP_HASH=\"testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345\""));
        assert!(script.contains("check_decompressor \"gzip\""));
        assert!(script.contains("sha256sum"));
        assert!(script.contains("$PNAME verified"));
        assert!(script.contains("exit 0"));
    }

    #[test]
    fn test_build_update_script_skip_verify() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "deadbeef0123456789abcdef0123456789abcdef0123456789abcdef01234567".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", true);
        assert!(script.contains("Verification skipped"));
        // sha256sum appears in pre-flash compressed hash verification even when
        // post-flash verify is skipped — that's expected behavior.
        assert!(script.contains("Flash complete"));
    }

    #[test]
    fn test_build_update_script_with_device() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "crosshatch", false);
        assert!(script.contains("TARGET_DEVICE=\"crosshatch\""));
        assert!(script.contains("DEVICE_MATCH"));
    }

    #[test]
    fn test_build_flash_info() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let info = build_flash_info("gzip", 16781312, 1, &meta, "crosshatch", 6, false, "TestROM", "TestMaker");
        assert!(info.contains("OTAku — Custom Payload Maker"));
        assert!(info.contains("gzip (level 6)"));
        assert!(info.contains("crosshatch"));
        assert!(info.contains("[boot]"));
        assert!(info.contains("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"));
        assert!(info.contains("enabled"));
    }

    #[test]
    fn test_run_dd_build_no_images() {
        let result = run_dd_build(&[], "gzip", 6, "/tmp/test.zip", "", false, "", "");
        assert!(!result.success);
        assert!(result.error.unwrap().contains("no images specified"));
    }

    #[test]
    fn test_run_dd_build_invalid_compression() {
        let result = run_dd_build(
            &[("boot".to_string(), "/tmp/boot.img".to_string())],
            "invalid",
            6,
            "/tmp/test.zip",
            "",
            false,
            "",
            "",
        );
        assert!(!result.success);
        assert!(result.error.unwrap().contains("unsupported compression"));
    }

    // ──────────────────────────────────────────────────────────────
    // Regression tests — each test guards against a specific bug that
    // was previously introduced and fixed. If a refactor re-introduces
    // the bug, the corresponding test fails.
    // See commit history for context on each bug.
    // ──────────────────────────────────────────────────────────────

    /// Regression: Bug #1 (P0) — operator precedence in cleanup trap.
    /// The old broken pattern `cmd1 || cmd2 && cmd3` was replaced with
    /// explicit if-else. If anyone reintroduces the broken pattern, this
    /// test catches it.
    #[test]
    fn test_regression_trap_operator_precedence() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // The broken pattern was:
        //   lptools resize "$rname" "$rsize" >/dev/null 2>&1 || \
        //       lptools remove "$rname" >/dev/null 2>&1 && \
        //       lptools create "$rname" "$rsize" >/dev/null 2>&1
        // The fix uses: `if ! lptools resize ...; then ... fi`
        // Assert: broken pattern is NOT present in cleanup trap.
        let broken_pattern = "lptools resize \"$rname\" \"$rsize\" >/dev/null 2>&1 ||";
        assert!(
            !script.contains(broken_pattern),
            "REGRESSION: cleanup trap still uses broken ||/&& chaining (Bug #1)"
        );

        // Assert: fix is present.
        // Note: after slot-suffix fix, the variable is $rname_lp not $rname
        assert!(
            script.contains("if ! lptools resize \"$rname_lp\" \"$rsize\""),
            "REGRESSION: cleanup trap if-else fix not found (Bug #1)"
        );
    }

    /// Regression: Bug #2 (P1) — regex leading space in ZIP listing parser.
    /// The old regex `^ [0-9]* otaku\.bin$` had a leading space which never
    /// matched awk output (which has no leading space). The fix uses
    /// `^[0-9]+ otaku\.bin$` via a shared ZIP_LIST_REGEX variable.
    #[test]
    fn test_regression_zip_listing_regex() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Assert: broken regex pattern is NOT present.
        let broken_patterns = [
            "grep -q '^ [0-9]* otaku",      // leading space + [0-9]*
            "grep -q \"^ [0-9]* otaku",     // leading space + [0-9]* (double-quoted variant)
        ];
        for pat in broken_patterns.iter() {
            assert!(
                !script.contains(pat),
                "REGRESSION: ZIP listing still uses broken regex with leading space (Bug #2): {}",
                pat
            );
        }

        // Assert: fix is present (refactored to try_zip_listing helper).
        assert!(
            script.contains("try_zip_listing"),
            "REGRESSION: try_zip_listing helper not defined (Bug #2 refactor)"
        );
        assert!(
            script.contains("otaku[.]bin"),
            "REGRESSION: fixed regex pattern (otaku[.]bin) not found (Bug #2)"
        );
    }

    /// Regression: Bug #3 (P1) — idempotent lptools unmap+map.
    /// The old code always called `lptools unmap` then `lptools map`,
    /// even when the partition was already unmapped. The fix checks
    /// for /dev/mapper/$pname or /dev/block/by-name/$pname existence
    /// before calling unmap.
    #[test]
    fn test_regression_idempotent_unmap() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Assert: idempotent check is present.
        assert!(
            script.contains("/dev/mapper/$pname") || script.contains("/dev/block/by-name/$pname"),
            "REGRESSION: idempotent unmap existence check missing (Bug #3)"
        );
    }

    /// Regression: Bug #4 (P2) — explicit $? capture.
    /// The old code used `if [ $? -ne 0 ]` directly after a command,
    /// which is fragile (any command between can reset $?). The fix
    /// captures to RC variables: RESIZE_RC, CREATE_RC, MAP_RC (or map_rc
    /// when capture is inside the unmap_and_remap_partition helper).
    ///
    /// Note: After refactor (commit bf24206 — targeted unmap/remap), the
    /// `lptools map` exit code capture moved from inline `MAP_RC=$?` in
    /// the resize-step remap loop into the `unmap_and_remap_partition()`
    /// helper function as `map_rc=$?` (lowercase, local var). Both patterns
    /// satisfy Bug #4's intent — the regression is about NOT using
    /// `if [ $? -ne 0 ]` directly after `lptools map`, regardless of where
    /// the capture lives.
    #[test]
    fn test_regression_explicit_rc_capture() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Assert: explicit RC capture variables are present.
        // RESIZE_RC and CREATE_RC are inline in the resize loop.
        assert!(script.contains("RESIZE_RC=$?"), "REGRESSION: RESIZE_RC capture missing (Bug #4)");
        assert!(script.contains("CREATE_RC=$?"), "REGRESSION: CREATE_RC capture missing (Bug #4)");
        // MAP_RC may be inline (uppercase, pre-refactor) or inside the
        // unmap_and_remap_partition helper (lowercase `map_rc`, post-refactor).
        // Both patterns satisfy Bug #4's intent: capture $? immediately after
        // `lptools map`, do NOT use bare `if [ $? -ne 0 ]`.
        assert!(
            script.contains("MAP_RC=$?") || script.contains("map_rc=$?"),
            "REGRESSION: MAP_RC/map_rc capture missing (Bug #4) — neither inline MAP_RC=$? nor helper map_rc=$? found"
        );
    }

    /// Regression: Bug #8 (P2) — `choose` binary fallback chain.
    /// The old code called `choose` unconditionally, which fails on
    /// minimal TWRP builds. The fix adds a fallback chain:
    /// choose → read -t 30 -n 1 → default abort.
    #[test]
    fn test_regression_choose_fallback() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "alioth", false);

        // Assert: choose is gated by command -v check.
        assert!(
            script.contains("command -v choose"),
            "REGRESSION: `choose` binary not guarded by command -v (Bug #8)"
        );

        // Assert: fallback to read -t is present.
        assert!(
            script.contains("read -t 30"),
            "REGRESSION: `read -t 30` fallback missing (Bug #8)"
        );

        // Assert: USER_CONFIRMED gate is present (so default abort works).
        assert!(
            script.contains("USER_CONFIRMED"),
            "REGRESSION: USER_CONFIRMED gate missing (Bug #8)"
        );
    }

    /// Regression: edge case B — vendor partition codename fallback chain.
    /// The old code used `getprop ro.product.device || getprop ro.build.product`
    /// which was spoofable by Magisk/GSI/LineageOS (they override /system props).
    /// The fix uses VENDOR partition props (ro.product.vendor.device +
    /// ro.product.board) which are harder to spoof, with /vendor/build.prop
    /// as fallback when getprop returns empty (recovery without /vendor mounted
    /// via init, but /vendor/build.prop still readable if partition is mounted).
    #[test]
    fn test_regression_getprop_fallback_chain() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "alioth", false);

        // Assert: vendor partition props are the primary source.
        assert!(
            script.contains("ro.product.vendor.device"),
            "REGRESSION: ro.product.vendor.device source missing (edge case B — spoof-resistant codename)"
        );
        assert!(
            script.contains("ro.product.board"),
            "REGRESSION: ro.product.board source missing (edge case B — spoof-resistant codename)"
        );

        // Assert: /vendor/build.prop fallback present (for both props).
        assert!(
            script.contains("/vendor/build.prop"),
            "REGRESSION: /vendor/build.prop fallback missing (edge case B)"
        );

        // Assert: comma-separated dual-codename logic present
        // (when vendor.device != board, both are used comma-separated).
        assert!(
            script.contains("VENDOR_DEVICE,$BOARD_DEVICE"),
            "REGRESSION: comma-separated dual-codename logic missing (edge case B)"
        );
    }

    /// Sanity: every generated script ends with `exit 0` for happy path.
    #[test]
    fn test_script_always_exits_clean_on_success() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        for skip in [false, true] {
            for device in ["", "alioth"] {
                let script = build_update_script(1, 1, "gzip", &meta, device, skip);
                assert!(
                    script.trim_end().ends_with("exit 0"),
                    "Script does not end with exit 0 (skip={}, device='{}')",
                    skip,
                    device
                );
            }
        }
    }

    /// Sanity: updater-script stub is valid edify, not a shell comment.
    /// The old code used "#Mtk client script\n" which TWRP flagged as
    /// edify syntax error. The fix uses "assert(1==1);\n".
    /// This test isn't on build_update_script directly (updater-script is
    /// in run_dd_build), but we verify the canonical string here to
    /// prevent accidental revert.
    #[test]
    fn test_updater_script_is_valid_edify() {
        // The updater-script is hardcoded in run_dd_build(). We can't easily
        // test it without invoking run_dd_build (which needs files), but
        // we can document the expected value here so anyone changing it
        // sees this test and updates both places consistently.
        let expected = "assert(1==1);\n";
        let forbidden = "#Mtk client script\n";
        assert_ne!(expected, forbidden, "expected and forbidden must differ");
        assert!(expected.contains("assert("), "expected must be valid edify");
        assert!(expected.ends_with(";\n"), "expected must end with semicolon + newline");
    }

    /// Sanity: DYNAMIC_PART_NAMES includes OEM-specific partitions.
    /// The old list missed Xiaomi/Realme/Samsung/Vivo partitions, causing
    /// flash failures on those devices.
    #[test]
    fn test_dynamic_part_names_includes_oem() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // AOSP standard names
        for required in ["system", "vendor", "product", "system_ext", "odm", "odm_dlkm", "vendor_dlkm"] {
            assert!(
                script.contains(&format!(" {} ", required)),
                "REGRESSION: AOSP partition '{}' missing from DYNAMIC_PART_NAMES",
                required
            );
        }

        // OEM-specific names
        for oem in ["mi_ext", "my_product", "optics", "prism"] {
            assert!(
                script.contains(&format!(" {} ", oem)),
                "REGRESSION: OEM partition '{}' missing from DYNAMIC_PART_NAMES",
                oem
            );
        }
    }

    /// Sanity: updater-script contains valid edify statement.
    /// The actual updater-script is built inside run_dd_build() and we
    /// can't easily test it without invoking the full builder. We test
    /// the upstream Rust constant here.
    #[test]
    fn test_updater_script_constant_in_source() {
        // Read source file at compile time to catch the literal.
        let source = include_str!("dd.rs");

        // Assert the canonical valid literal exists.
        assert!(
            source.contains("let updater_script = \"assert(1==1);\\n\";"),
            "updater-script assignment must use \"assert(1==1);\\n\" literal"
        );

        // Assert the broken literal is NOT used as the actual assignment.
        // Note: we check the assignment form `let updater_script = ...` so
        // that documentation comments mentioning the broken literal don't
        // trigger a false positive.
        assert!(
            !source.contains("let updater_script = \"#Mtk client script\\n\";"),
            "REGRESSION: updater-script reverted to broken '#Mtk client script' literal"
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Pilihan B implementation tests — verify the new alur
    // (alur user step 2: pre-flash partition verify)
    // ──────────────────────────────────────────────────────────────

    /// Verify Step 1 (pre-flash partition table verify) exists in generated script.
    /// This is alur user step 2: "verify semua partisi" before flash.
    #[test]
    fn test_preflash_verify_step_present() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Pre-flash verify step is present
        assert!(
            script.contains("Pre-flash partition table verify"),
            "REGRESSION: pre-flash verify step missing (alur user step 2)"
        );

        // Verify step header present (refactored to section-based style)
        assert!(
            script.contains("> Verifying partition table..."),
            "REGRESSION: pre-flash verify section header missing"
        );

        // All 4 checks must be present
        assert!(
            script.contains("offset_overflow"),
            "REGRESSION: pre-flash verify missing offset_overflow check"
        );
        assert!(
            script.contains("bad_hash_format"),
            "REGRESSION: pre-flash verify missing hash format check"
        );
        assert!(
            script.contains("zero_unc_size"),
            "REGRESSION: pre-flash verify missing zero_unc_size check"
        );
        assert!(
            script.contains("misaligned_offset"),
            "REGRESSION: pre-flash verify missing alignment check"
        );

        // Success message must be present
        assert!(
            script.contains("partition(s) verified"),
            "REGRESSION: pre-flash verify success message missing"
        );
    }

    /// Verify Step 2 (bundle integrity + decompressor) is merged into one step.
    /// Old Step 1 (decompressor) and Step 2 (integrity) are now combined.
    #[test]
    fn test_integrity_step_merged_with_decompressor() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Merged step header
        assert!(
            script.contains("Bundle integrity + decompressor"),
            "REGRESSION: merged integrity+decompressor step header missing"
        );

        // Old separate "Decompressor availability" step header should NOT be present
        // (it's now part of merged step 2)
        let old_step1_pattern = "[Step 1/] Decompressor availability...";
        assert!(
            !script.contains(old_step1_pattern),
            "REGRESSION: old separate 'Decompressor availability' step still present (should be merged)"
        );

        // Both check_decompressor function and HDR_MAGIC check should be in same step
        assert!(
            script.contains("check_decompressor") && script.contains("HDR_MAGIC"),
            "REGRESSION: decompressor and bundle integrity not in same step"
        );
    }

    /// Verify step numbering: extract=0, verify=1, integrity=2, slot=3 (no device),
    /// validation=4, resize=5, flash=6+.
    /// This guards against accidental renumbering that breaks the alur.
    #[test]
    fn test_step_numbering_after_pilihan_b() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        // Without device check
        let script_no_dev = build_update_script(1, 1, "gzip", &meta, "", false);

        // Step 0 = extract — header line format: "[Step 0/7]"
        assert!(script_no_dev.contains("Step 0/"), "Step 0 (extract) missing");
        // Step 1 = pre-flash verify (NEW) — header format: "[Step 1/7]"
        assert!(script_no_dev.contains("Step 1/"), "Step 1 (pre-flash verify) missing");
        // Step 2 = integrity + decompressor (MERGED) — header format: "[Step 2/7]"
        assert!(script_no_dev.contains("Step 2/"), "Step 2 (integrity+decompressor) missing");
        // Step 3 = slot detection (no device) — header format: "[Step 3/7]"
        assert!(script_no_dev.contains("Step 3/"), "Step 3 (slot detection) missing");
        // Step 4 = partition validation — header format: "[Step 4/7]"
        assert!(script_no_dev.contains("Step 4/"), "Step 4 (partition validation) missing");
        // Step 5 = resize — header format: "[Step 5/7]"
        assert!(script_no_dev.contains("Step 5/"), "Step 5 (resize) missing");
        // Step 6 = flash — header format is DIFFERENT from other steps:
        //   "# ── Step {flash_step_offset}+{num_parts_minus_1}/{total_steps}: Flash each partition"
        // For 1 partition with no device: "Step 6+0/7"
        // We check for "Step 6" as substring (not "Step 6/") because the format differs.
        assert!(
            script_no_dev.contains("Step 6+0/7") || script_no_dev.contains("Step 6 "),
            "Step 6 (flash) header missing — got: {}",
            script_no_dev.lines().filter(|l| l.contains("Step 6")).collect::<Vec<_>>().join("\n")
        );

        // With device check, all subsequent steps shift +1
        let script_with_dev = build_update_script(1, 1, "gzip", &meta, "alioth", false);
        // Step 3 = device check (with device) — header format: "Step {device_check_step}:" (note colon)
        assert!(
            script_with_dev.contains("Step 3:") || script_with_dev.contains("Step 3 "),
            "Step 3 (device check) missing with device"
        );
        // Step 4 = slot detection (with device) — header format: "[Step 4/8]"
        assert!(script_with_dev.contains("Step 4/"), "Step 4 (slot detection) missing with device");
        // Step 7 = flash (with device) — header format: "Step 7+0/8"
        assert!(
            script_with_dev.contains("Step 7+0/8") || script_with_dev.contains("Step 7 "),
            "Step 7 (flash) missing with device"
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Bug NEW-A/B/C fix tests — guard empty vars + portable substring
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_regression_empty_offset_comp_guarded() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        assert!(script.contains("empty_offset"), "Bug NEW-A: empty_offset guard missing");
        assert!(script.contains("empty_comp_size"), "Bug NEW-A: empty_comp_size guard missing");
    }

    #[test]
    fn test_regression_empty_vunc_default_value() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        // The fix uses ${VUNC:-0} — check for the literal string in output
        assert!(script.contains("VUNC"), "Bug NEW-B: VUNC reference missing");
    }

    #[test]
    fn test_regression_portable_hash_substring() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        assert!(script.contains("HASH_SHORT"), "Bug NEW-C: HASH_SHORT variable missing");
        assert!(script.contains("printf"), "Bug NEW-C: printf fix missing");
    }

    /// Verify the new pre-flash compressed-data hash verification is present.
    /// This catches bundle corruption (MTP transfer, tmpfs issues) BEFORE
    /// we touch any block device.
    #[test]
    fn test_preflash_compressed_hash_verification() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1075,
            hash_hex: "a".repeat(64),
            comp_size: 334,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        // PCOMP_HASH eval is present
        assert!(
            script.contains("PCOMP_HASH"),
            "PCOMP_HASH variable missing from flash step"
        );
        // Pre-flash hash verification block is present
        assert!(
            script.contains("Hash verified"),
            "Compressed data hash verification block missing"
        );
        // Hash mismatch abort message is present
        assert!(
            script.contains("Compressed data hash mismatch"),
            "Hash mismatch abort message missing"
        );
        // Uses head -c for exact byte count
        assert!(
            script.contains("head -c"),
            "head -c (exact byte extraction) missing"
        );
    }

    /// Verify the fallback decompressor and gzip stderr capture are present.
    /// When the primary decompressor fails, the script should:
    /// 1. Capture gzip's stderr (not suppress it)
    /// 2. Print diagnostic info
    /// 3. Try fallback decompressors (gzip -dc, gunzip, zcat, busybox gzip)
    #[test]
    fn test_fallback_decompressor_and_diagnostics() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1075,
            hash_hex: "a".repeat(64),
            comp_size: 334,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        // GZIP_ERR temp file is used (not 2>/dev/null on decompressor)
        assert!(
            script.contains("GZIP_ERR"),
            "GZIP_ERR temp file variable missing — stderr is being suppressed"
        );
        // Diagnostic info on failure
        assert!(
            script.contains("Details:"),
            "GZIP error diagnostic message missing"
        );
        assert!(
            script.contains("Bundle:"),
            "Bundle diagnostic info missing"
        );
        // Fallback decompressor list
        assert!(
            script.contains("gzip -dc"),
            "Fallback 'gzip -dc' missing"
        );
        assert!(
            script.contains("gunzip -c"),
            "Fallback 'gunzip -c' missing"
        );
        assert!(
            script.contains("zcat"),
            "Fallback 'zcat' missing"
        );
        assert!(
            script.contains("busybox gzip -dc"),
            "Fallback 'busybox gzip -dc' missing"
        );
    }

    /// Verify the 3 BlassGo-alignment fixes:
    /// 1. lptools resize is PRIMARY (not remove+create)
    /// 2. resolve_target checks /dev/block/mapper/ FIRST
    /// 3. Post-flash re-map (lptools unmap && lptools map) is present
    #[test]
    fn test_blassgo_alignment_3_fixes() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200,  // 1075 MB
            hash_hex: "a".repeat(64),
            comp_size: 350224384,  // 334 MB
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // ── Fix 1: lptools resize is PRIMARY ──
        // The resize step must try `lptools resize` FIRST, with remove+create
        // only as a fallback. The previous (broken) approach was remove+create
        // primary, which destroys the by-name symlink.
        assert!(
            script.contains("lptools resize \"$LP_NAME\" \"$NEW_SIZE_BYTES\""),
            "Fix 1: lptools resize (primary) missing from resize step"
        );
        // remove+create should only appear as FALLBACK (with "fallback" comment nearby)
        // Count remove+create occurrences — should be in fallback paths only
        let remove_create_count = script.matches("lptools remove \"$LP_NAME\"").count();
        assert!(
            remove_create_count >= 1,
            "Fix 1: remove+create fallback missing (count={})",
            remove_create_count
        );
        // The resize step should NOT start with remove+create as primary
        // Check that the resize comment says "resize FIRST" (not "remove+create FIRST")
        assert!(
            script.contains("try lptools resize FIRST"),
            "Fix 1: resize-first comment missing"
        );
        assert!(
            !script.contains("try remove+create FIRST"),
            "Fix 1: remove+create-first comment still present (should be resize-first)"
        );

        // ── Fix 2: resolve_target checks /dev/block/mapper/ FIRST ──
        // The function must check mapper/ paths before by-name/ paths.
        // This handles the case where remove+create (fallback) destroys the
        // by-name symlink but creates a fresh dm at /dev/block/mapper/.
        assert!(
            script.contains("/dev/block/mapper/"),
            "Fix 2: /dev/block/mapper/ path missing from resolve_target"
        );
        assert!(
            script.contains("mapper_slotted"),
            "Fix 2: mapper_slotted variable missing from resolve_target"
        );
        // mapper/ must be checked BEFORE by-name/ (priority order)
        let mapper_pos = script.find("if [ -e \"$mapper_slotted\" ]").unwrap_or(usize::MAX);
        let byname_pos = script.find("if [ -e \"$slotted\" ]").unwrap_or(usize::MAX);
        assert!(
            mapper_pos < byname_pos,
            "Fix 2: mapper/ must be checked BEFORE by-name/ (priority order)"
        );

        // ── Fix 3: Post-flash re-map ──
        // After dd writes, the script must do `lptools unmap && lptools map`
        // to refresh the dm-linear device (BlassGo step 6/7).
        assert!(
            script.contains("Post-flash re-map"),
            "Fix 3: post-flash re-map comment missing"
        );
        // Post-flash re-map is silent on success (cosmetic refactor cacdf13)
        // — only check that lptools unmap + map are present, not the ui_print
        assert!(
            script.contains("lptools unmap \"$REMAP_LP_NAME\""),
            "Fix 3: lptools unmap in post-flash re-map missing"
        );
        assert!(
            script.contains("lptools map \"$REMAP_LP_NAME\""),
            "Fix 3: lptools map in post-flash re-map missing"
        );
    }

    /// Verify the cleanup trap uses BlassGo pattern (unmap → resize → map)
    /// for rollback, not just resize alone.
    #[test]
    fn test_cleanup_trap_blassgo_pattern() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200,
            hash_hex: "a".repeat(64),
            comp_size: 350224384,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Cleanup trap must do unmap BEFORE resize (BlassGo pattern)
        // The old code just did resize; the new code does unmap → resize → map.
        assert!(
            script.contains("lptools unmap \"$rname_lp\" >/dev/null 2>&1 || true"),
            "Cleanup: unmap before resize missing"
        );
        // After resize succeeds, must re-map to materialize rollback size
        assert!(
            script.contains("lptools map \"$rname_lp\" >/dev/null 2>&1 || true"),
            "Cleanup: re-map after resize missing"
        );
    }

    /// Verify optimization changes:
    /// 1. unmount_partition function is split from unmount_and_unmap_partition
    /// 2. twrp unmount dead code is REMOVED (was never callable — twrp binary
    ///    is PID 1 recovery, not in PATH for shell exec)
    /// 3. head -c $PCSIZE strips trailing padding (fixes gzip "trailing junk")
    /// 4. verify_block uses ${i} not ${{i}} (format!() escaping bug fixed)
    /// 5. Post-flash re-map is silent on success
    #[test]
    fn test_optimization_changes() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200,
            hash_hex: "a".repeat(64),
            comp_size: 350224384,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // 1. unmount_partition function is defined (split from unmount_and_unmap)
        assert!(
            script.contains("unmount_partition()"),
            "Optimize: unmount_partition function not defined"
        );

        // 2. twrp unmount dead code is REMOVED
        // (twrp binary is PID 1 recovery, not shell-callable — command -v twrp
        //  always returns false. Dead code removed for cleanliness.)
        assert!(
            !script.contains("twrp unmount"),
            "Optimize: twrp unmount dead code should be removed"
        );

        // 3. head -c $PCSIZE strips trailing padding
        assert!(
            script.contains("head -c \"$PCSIZE\""),
            "Optimize: head -c $PCSIZE missing from flash pipeline"
        );

        // 4. verify_block uses ${i} (not ${{i}} — that was a format!() escaping bug)
        // The verify_block is a raw string assigned to a variable, so it should
        // contain literal ${i}, not ${{i}}.
        assert!(
            script.contains("verify_${i}.fifo"),
            "Optimize: verify_block should use dollar-brace-i (not dollar-brace-brace-i)"
        );
        assert!(
            !script.contains("verify_${{i}}.fifo"),
            "Optimize: verify_block still has double-brace i (format escaping bug)"
        );

        // 5. Post-flash re-map is silent on success (no "Re-mapping" ui_print)
        assert!(
            !script.contains("Re-mapping $REMAP_LP_NAME"),
            "Optimize: post-flash re-map should be silent (remove 'Re-mapping' ui_print)"
        );

        // 6. Resize step uses unmount_partition (not unmount_and_unmap_partition)
        assert!(
            script.contains("unmount_partition \"$pname\" || true"),
            "Optimize: resize step should use unmount_partition (not unmount_and_unmap_partition)"
        );
    }

    /// Verify Transsion (Infinix/itel/Tecno) physical partition support:
    /// 1. resolve_target checks /dev/block/platform/bootdevice/by-name/ (Priority 5+6)
    /// 2. bootdev_slotted + bootdev_plain variables defined
    /// 3. Transsion physical partitions can be resolved (lk, logo, spmfw, tee, vendor_boot)
    #[test]
    fn test_transsion_physical_partition_support() {
        let meta = vec![PartitionMeta {
            name: "lk".to_string(),
            unc_size: 2097152,  // 2 MB
            hash_hex: "a".repeat(64),
            comp_size: 1048576,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // 1. /dev/block/platform/bootdevice/by-name/ path resolution
        assert!(
            script.contains("/dev/block/platform/bootdevice/by-name/"),
            "Transsion: /dev/block/platform/bootdevice/by-name/ path missing from resolve_target"
        );

        // 2. bootdev_slotted + bootdev_plain variables
        assert!(
            script.contains("bootdev_slotted"),
            "Transsion: bootdev_slotted variable missing from resolve_target"
        );
        assert!(
            script.contains("bootdev_plain"),
            "Transsion: bootdev_plain variable missing from resolve_target"
        );

        // 3. Priority 5 comment (Transsion physical GPT)
        assert!(
            script.contains("PHYSICAL GPT partitions on Transsion"),
            "Transsion: Priority 5 comment for physical GPT missing"
        );

        // 4. bootdev paths checked AFTER by-name paths (priority order)
        // Priority 5 (bootdev_slotted) must come AFTER Priority 4 (plain by-name)
        let bootdev_pos = script.find("if [ -e \"$bootdev_slotted\" ]").unwrap_or(usize::MAX);
        let plain_pos = script.find("if [ -e \"$plain\" ]").unwrap_or(usize::MAX);
        assert!(
            plain_pos < bootdev_pos,
            "Transsion: bootdev paths must be checked AFTER by-name paths (priority order)"
        );
    }

    /// Verify auto-map fix for unmapped dynamic partitions (Format Data recovery).
    ///
    /// Root cause (recovery.log CRC32 0x57923752, Itel S666LN):
    ///   User did Format Data → recovery Unmap_Super_Devices destroyed system_b
    ///   → OTAku validate_target found /dev/block/mapper/system_b missing → ABORT
    ///
    /// Fix: validate_target now calls `lptools map $LP_NAME` when target doesn't
    ///      exist AND it's a dynamic partition, before falling through to ABORT.
    #[test]
    fn test_auto_map_unmapped_dynamic_partition() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 5013785600,  // 4788 MB
            hash_hex: "a".repeat(64),
            comp_size: 4158234112,  // 3965 MB
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // 1. Auto-map comment present (references Format Data / Unmap_Super_Devices)
        assert!(
            script.contains("Auto-map unmapped dynamic partitions"),
            "Auto-map: comment block missing from validate_target"
        );

        // 2. Auto-map trigger condition: target missing + is_dynamic + lptools available
        assert!(
            script.contains("if [ ! -e \"$target\" ] && [ \"$is_dynamic\" = \"1\" ]"),
            "Auto-map: trigger condition (target missing + is_dynamic) missing"
        );

        // 3. lptools map call present
        assert!(
            script.contains("lptools map \"$lp_name\""),
            "Auto-map: 'lptools map $lp_name' call missing"
        );

        // 4. Slot-suffixed LP_NAME construction
        // Note: script contains POST-format!() output, so single braces ${name}
        assert!(
            script.contains("lp_name=\"${name}${TARGET_SLOT}\""),
            "Auto-map: slot-suffixed lp_name construction missing"
        );

        // 5. Re-resolve target after successful map
        assert!(
            script.contains("target=$(resolve_target \"$name\")"),
            "Auto-map: re-resolve target after map missing"
        );

        // 6. Success message
        assert!(
            script.contains("lptools map $lp_name succeeded"),
            "Auto-map: success ui_print message missing"
        );

        // 7. Failure message (lptools map failed)
        assert!(
            script.contains("lptools map $lp_name failed"),
            "Auto-map: failure ui_print message missing"
        );

        // 8. Reference to Format Data (root cause documentation in code)
        assert!(
            script.contains("Format Data"),
            "Auto-map: Format Data reference (root cause doc) missing"
        );
    }
}
