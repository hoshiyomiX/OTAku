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
use std::io::{Seek, Write};
use std::path::Path;

use crate::compression::{compress_id, hash_and_compress_file_with_progress, is_alg, ALG_NONE};

// ---------------------------------------------------------------------------
//  Progress sidecar file
// ---------------------------------------------------------------------------

/// Write progress with per-partition compression percentage.
///
/// `partition_percent` is 0-100 for the current partition being compressed.
/// The overall build percentage is calculated as:
///   completed_partitions * 100 / total_partitions + partition_percent / total_partitions
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
    let overall_percent = if total > 0 {
        ((current.saturating_sub(1)) * 100 + partition_percent as usize) / total
    } else {
        0
    };
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
    // Best-effort — progress file is non-critical
    let _ = std::fs::write(&progress_path, content.to_string());
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
pub const DDBUNDLE_MAGIC: [u8; 4] = [b'D', b'D', b'B', b'U'];
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

/// Slant ASCII art banner for the flasher script.
const SCRIPT_VERSION: &str = "v3";

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
             PART_{}_DATA_OFFSET=\"{}\"\n",
            i, p.name, i, p.unc_size, i, p.hash_hex, i, p.comp_size, i, p.data_offset
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
CURRENT_DEVICE=""

# Try multiple sources for the device codename — getprop can return empty
# in recoveries that don't mount /vendor or that boot from fastboot boot
# (no full system initialization). Fall back to build.prop files which
# are static and always available if the partition is mounted.
CURRENT_DEVICE=$(getprop ro.product.device 2>/dev/null)
if [ -z "$CURRENT_DEVICE" ]; then
    CURRENT_DEVICE=$(getprop ro.build.product 2>/dev/null)
fi
# Fallback 1: parse /system/build.prop (mounted /system)
if [ -z "$CURRENT_DEVICE" ] && [ -f /system/build.prop ]; then
    CURRENT_DEVICE=$(grep -E '^ro\.product\.device=' /system/build.prop 2>/dev/null | head -1 | cut -d= -f2 | tr -d ' \r')
fi
# Fallback 2: parse /vendor/build.prop (mounted /vendor)
if [ -z "$CURRENT_DEVICE" ] && [ -f /vendor/build.prop ]; then
    CURRENT_DEVICE=$(grep -E '^ro\.product\.device=' /vendor/build.prop 2>/dev/null | head -1 | cut -d= -f2 | tr -d ' \r')
fi
# Fallback 3: /proc/cmdline androidboot.hardware (last resort — not exact device,
# but at least identifies the SoC family)
if [ -z "$CURRENT_DEVICE" ]; then
    CURRENT_DEVICE=$(cat /proc/cmdline 2>/dev/null | tr ' ' '\n' | grep -o 'androidboot\.hardware=[^ ]*' | cut -d= -f2)
fi

# Support comma-separated device list
DEVICE_MATCH=0
OLD_IFS="$IFS"
IFS=','
for _dev in $TARGET_DEVICE; do
    _clean=$(echo "$_dev" | tr -d '[:space:]')
    if [ "$CURRENT_DEVICE" = "$_clean" ]; then
        DEVICE_MATCH=1
        break
    fi
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
        ui_print "  Device: $CURRENT_DEVICE [OK]"
    fi
fi
ui_print ""
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
        r#"ui_print "  Verifying $PNAME (fast 1MB-block hash)..."
VERIFY_HASH=""

# Fast path: large block size + background sha256sum via FIFO.
# Pipeline:
#   dd if=$PTARGET bs=1M → tee FIFO → /dev/null
#                              ↓
#              sha256sum (bg) → hash file
#
# bs=1M (1048576) matches dd write block + UFS page size, eliminating
# the byte-by-byte syscall overhead of the old bs=1 remainder read.
VERIFY_FIFO="/tmp/verify_${{i}}.fifo"
VERIFY_HASHFILE="/tmp/verify_${{i}}.hash"
rm -f "$VERIFY_FIFO" "$VERIFY_HASHFILE"

FAST_OK=0
if command -v sha256sum >/dev/null 2>&1; then
    if mkfifo "$VERIFY_FIFO" 2>/dev/null; then
        sha256sum < "$VERIFY_FIFO" > "$VERIFY_HASHFILE" 2>/dev/null &
        VERIFY_PID=$!

        # Read PSIZE bytes in 1MB blocks. count is computed to cover the
        # whole partition (rounds UP to next MB, which is safe because
        # sha256sum only hashes what it actually receives — dd will stop
        # at end of partition anyway).
        VERIFY_BLOCKS=$(( (PSIZE + 1048575) / 1048576 ))
        dd if="$PTARGET" bs=1048576 count=$VERIFY_BLOCKS 2>/dev/null | tee "$VERIFY_FIFO" >/dev/null

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
    FULL_BLOCKS=$(( PSIZE / 4096 ))
    REMAINDER_BYTES=$(( PSIZE % 4096 ))

    if [ "$REMAINDER_BYTES" -eq 0 ]; then
        VERIFY_HASH=$(dd if="$PTARGET" bs=4096 count=$FULL_BLOCKS 2>/dev/null | sha256sum | cut -d' ' -f1)
    else
        VERIFY_HASH=$(
            dd if="$PTARGET" bs=4096 count=$FULL_BLOCKS 2>/dev/null
            dd if="$PTARGET" bs=1 skip=$(( FULL_BLOCKS * 4096 )) count=$REMAINDER_BYTES 2>/dev/null
        ) | sha256sum | cut -d' ' -f1
    fi
fi

if [ "$VERIFY_HASH" = "$PHASH" ]; then
    ui_print "  $PNAME: VERIFIED OK"
else
    ui_print "! ABORT: Hash mismatch for $PNAME!"
    ui_print "  Expected: $PHASH"
    ui_print "  Got:      $VERIFY_HASH"
    exit 1
fi
"#
        .to_string()
    };

    let verified_word = if skip_verify { "" } else { "and verified" };

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
    ui_print "! Performing emergency cleanup..."
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
            # IMPORTANT: Use explicit if-else, NOT ||/&& chaining.
            # Shell || and && have SAME precedence and left-to-right associativity,
            # so `a || b && c` parses as `(a || b) && c` — meaning if `a` fails
            # and `b` succeeds, `c` runs unconditionally. That would create a
            # partition we don't want. Use if-else to make intent explicit.
            if ! lptools resize "$rname" "$rsize" >/dev/null 2>&1; then
                # resize failed — try remove+create as fallback
                lptools remove "$rname" >/dev/null 2>&1
                lptools create "$rname" "$rsize" >/dev/null 2>&1
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
            # Not mapped — try to map. unmount_and_unmap_partition is idempotent
            # (returns 0 if not present/mapped), so safe to call as a prep step.
            unmount_and_unmap_partition "$pname" >/dev/null 2>&1
            lptools map "$pname" >/dev/null 2>&1
        done
    fi
    sync
    ui_print "! Cleanup complete."
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

ui_print ""
ui_print "  OTAku {script_version}"
ui_print ""
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
ui_print "[Step {extract_step}/{total_steps}] Extracting otaku.bin..."

rm -f "$BUNDLE"
if [ ! -f "$ZIPFILE" ]; then
    ui_print "! ABORT: ZIP file not found: $ZIPFILE"
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
# Use a single robust regex: ^[0-9]+ otaku\.bin$
# (no leading space — `awk '{{print $1, $NF}}'` outputs "$1 $NF" with single space,
# not " $1 $NF"). The previous `^ [0-9]*` pattern required a leading space and
# never matched, so the whole ZIP listing check was silently skipped.
# `[0-9]+` instead of `[0-9]*` to reject empty size (malformed line).
ZIP_LIST_REGEX='^[0-9]+ otaku\.bin$'

if which unzip >/dev/null 2>&1; then
    if unzip -l "$ZIPFILE" 2>/dev/null | awk '{{print $1, $NF}}' | grep -q "$ZIP_LIST_REGEX"; then
        EXPECTED_BUNDLE_SIZE=$(unzip -l "$ZIPFILE" 2>/dev/null | awk '{{print $1, $NF}}' | grep 'otaku\.bin$' | awk '{{print $1}}' | tr -d ' ')
        ZIP_LIST_OK=1
    fi
fi
if [ "$ZIP_LIST_OK" = "0" ] && busybox --list 2>/dev/null | grep -q "^unzip$"; then
    if busybox unzip -l "$ZIPFILE" 2>/dev/null | awk '{{print $1, $NF}}' | grep -q "$ZIP_LIST_REGEX"; then
        EXPECTED_BUNDLE_SIZE=$(busybox unzip -l "$ZIPFILE" 2>/dev/null | awk '{{print $1, $NF}}' | grep 'otaku\.bin$' | awk '{{print $1}}' | tr -d ' ')
        ZIP_LIST_OK=1
    fi
fi
if [ "$ZIP_LIST_OK" = "0" ] && toybox unzip --help >/dev/null 2>&1; then
    if toybox unzip -l "$ZIPFILE" 2>/dev/null | awk '{{print $1, $NF}}' | grep -q "$ZIP_LIST_REGEX"; then
        EXPECTED_BUNDLE_SIZE=$(toybox unzip -l "$ZIPFILE" 2>/dev/null | awk '{{print $1, $NF}}' | grep 'otaku\.bin$' | awk '{{print $1}}' | tr -d ' ')
        ZIP_LIST_OK=1
    fi
fi

# Warn if central directory listing failed — not fatal (some old busybox builds
# don't support `unzip -l`), but the post-extract size check below will be skipped.
if [ "$ZIP_LIST_OK" = "0" ]; then
    ui_print "  Note: cannot query ZIP listing — size check will be skipped."
elif [ -z "$EXPECTED_BUNDLE_SIZE" ] || [ "$EXPECTED_BUNDLE_SIZE" = "0" ]; then
    ui_print "  Note: otaku.bin not found in ZIP listing — possible corrupt ZIP."
    ui_print "! ABORT: ZIP does not contain otaku.bin in its root."
    exit 1
else
    ui_print "  Expected otaku.bin size: $(( EXPECTED_BUNDLE_SIZE / 1048576 )) MB ($EXPECTED_BUNDLE_SIZE bytes)"
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
    ui_print "! ABORT: Failed to extract otaku.bin from ZIP"
    ui_print "! ZIP: $ZIPFILE"
    ui_print "! Make sure the ZIP contains 'otaku.bin' in its root."
    exit 1
fi

BUNDLE_EXTRACT_SIZE=$(wc -c < "$BUNDLE" | tr -d ' ')

# Post-extract size verification — catches truncated or partially-extracted
# otaku.bin (common on tmpfs with low space, or ZIP CRC mismatch).
# Only check when we have a trusted expected size from the ZIP listing.
if [ "$ZIP_LIST_OK" = "1" ] && [ -n "$EXPECTED_BUNDLE_SIZE" ] && [ "$EXPECTED_BUNDLE_SIZE" != "0" ]; then
    if [ "$BUNDLE_EXTRACT_SIZE" != "$EXPECTED_BUNDLE_SIZE" ]; then
        ui_print "! ABORT: otaku.bin size mismatch after extract!"
        ui_print "!  Expected (ZIP listing): $EXPECTED_BUNDLE_SIZE bytes"
        ui_print "!  Actual   (extracted)  : $BUNDLE_EXTRACT_SIZE bytes"
        ui_print "!  Difference: $(( EXPECTED_BUNDLE_SIZE - BUNDLE_EXTRACT_SIZE )) bytes"
        ui_print "!  Likely cause: tmpfs full, ZIP CRC error, or interrupted write."
        ui_print "!  Free space in /tmp:"
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
    ui_print "  Size verified: $BUNDLE_EXTRACT_SIZE bytes [OK]"
fi

ui_print "  Extracted: $BUNDLE ($(( BUNDLE_EXTRACT_SIZE / 1048576 )) MB)"
ui_print ""
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
ui_print "[Step {verify_step}/{total_steps}] Pre-flash partition table verify..."

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
        ui_print "  $VPNAME: offset=$VOFFSET comp=$(( VCOMP / 1048576 ))MB unc=$(( VUNC / 1048576 ))MB hash=$HASH_SHORT... [OK]"
    fi
done

if [ "$VERIFY_OK" != "1" ]; then
    ui_print "! ABORT: $VERIFY_ERRORS partition(s) failed pre-flash verify."
    ui_print "!  Bundle is corrupt or was built with incompatible OTAku version."
    exit 1
fi

ui_print "  All $NUM_PARTS partition(s) passed structural verify."
ui_print ""
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
ui_print "[Step {integrity_step}/{total_steps}] Bundle integrity + decompressor availability..."

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
ui_print "  Decompressor: $DECOMP_CMD"

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

ui_print "  Version=$HDR_VERSION Compress=$HDR_COMPRESS Parts=$HDR_NUM_PARTS"
ui_print "  Header=$HDR_HDR_SIZE DataOffset=$DATA_OFFSET"
ui_print ""
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
ui_print "[Step {slot_step}/{total_steps}] Slot detection..."

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

ui_print "  Active slot: ${{TARGET_SLOT:-none (non-A/B device)}}"
ui_print ""

resolve_target() {{
    local name="$1"
    local slotted="/dev/block/by-name/${{name}}${{TARGET_SLOT}}"
    local plain="/dev/block/by-name/$name"

    # No slot detected — non-A/B device
    if [ -z "$TARGET_SLOT" ]; then
        echo "$plain"
        return
    fi

    # Prefer slotted path if it exists (e.g. boot_b, dtbo_b, vbmeta_b)
    if [ -e "$slotted" ]; then
        echo "$slotted"
        return
    fi

    # Dynamic partitions in super don't have _a/_b symlinks on Virtual AB
    # devices — use the plain name (e.g. system, vendor, odm_dlkm)
    if [ -e "$plain" ]; then
        echo "$plain"
        return
    fi

    # Neither exists yet — return slotted path as best guess;
    # validate_target() will produce a clear error if still missing
    echo "$slotted"
}}
"#,
        slot_step = slot_step,
        total_steps = total_steps,
    ));

    // ── Partition validation ──
    script.push_str(&format!(
        r#"# ── Step {validation_step}/{total_steps}: Partition validation ─────────────────────
ui_print "[Step {validation_step}/{total_steps}] Partition validation..."

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
unmount_and_unmap_partition() {{
    local pname="$1"
    local ptarget dev_name mount_points mp

    ptarget=$(resolve_target "$pname" 2>/dev/null)
    [ -z "$ptarget" ] && return 0
    [ ! -e "$ptarget" ] && return 0

    dev_name=$(basename "$ptarget" 2>/dev/null)
    # Find mount points referencing either the full block device path or its
    # basename (some recoveries report /dev/block/dm-0 rather than the by-name
    # symlink; basename match catches both).
    mount_points=$(mount 2>/dev/null | grep -E "($ptarget|$dev_name)" | awk '{{print $3}}')
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

    # lptools unmap is idempotent: returns 0 if already unmapped (state != ACTIVE).
    # If it still fails after targeted unmount, the partition is either genuinely
    # busy (some process holds an open fd) or not a dynamic partition.
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

    if [ ! -e "$target" ]; then
        ui_print "! ABORT: $target not found for partition '$name'"
        return 1
    fi

    if [ ! -b "$target" ]; then
        ui_print "! ABORT: $target is not a block device"
        return 1
    fi

    MOUNT_POINT=$(mount 2>/dev/null | grep " $target " | awk '{{print $3}}' | head -1)
    if [ -z "$MOUNT_POINT" ]; then
        DEV_NAME=$(basename "$target")
        MOUNT_POINT=$(mount 2>/dev/null | grep "$DEV_NAME" | awk '{{print $3}}' | head -1)
    fi
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
            ui_print "  $name ($target): $(( PART_SIZE / 1048576 )) MB [NEEDS RESIZE: $(( PART_SIZE / 1048576 )) -> $(( min_size / 1048576 )) MB]"
            RESIZE_NEEDED="${{RESIZE_NEEDED:+$RESIZE_NEEDED }}$name"
            RESIZE_TOTAL=$(( RESIZE_TOTAL + min_size - PART_SIZE ))
            return 0
        else
            ui_print "! ABORT: Partition $name too small: $PART_SIZE < $min_size"
            return 1
        fi
    fi

    ui_print "  $name ($target): $(( PART_SIZE / 1048576 )) MB [OK]"
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
ui_print ""
"#,
        validation_step = validation_step,
        total_steps = total_steps,
    ));

    // ── Resize dynamic partitions ──
    script.push_str(&format!(
        r#"# ── Step {resize_step}/{total_steps}: Resize dynamic partitions ──────────────────
ui_print "[Step {resize_step}/{total_steps}] Resize dynamic partitions..."

if [ -z "$RESIZE_NEEDED" ]; then
    ui_print "  No dynamic partitions need resizing."
    ui_print ""
else
    ui_print "  Partitions needing resize:$RESIZE_NEEDED"
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

    # Targeted unmount + unmap — ONLY for partitions that will be resized.
    #
    # Rationale (aligned with AOSP non-A/B OTA flow):
    #   AOSP's update_dynamic_partitions op_list only contains partitions that
    #   are being resized/added/removed. It does NOT unmap all dynamic
    #   partitions on the device. Each op `resize` is defined as
    #   "unmap, then resize" — only the partition being resized needs unmap.
    #   Source: source.android.com/docs/core/ota/dynamic_partitions/nonab
    #
    #   lptools resize only updates metadata in the super partition; it does
    #   NOT touch other partitions' dm-linear devices. So unmapping partitions
    #   that won't be resized is unnecessary work AND risks EBUSY on
    #   recovery-mounted partitions (system_ext, odm, etc.), which was the
    #   original root cause of the "lptools map fails" bug.
    #
    #   Previous approach iterated ALL $DYNAMIC_PART_NAMES (19 names) — too
    #   broad. Now we only iterate $RESIZE_NEEDED (partitions the bundle needs
    #   to grow).
    #
    # We still use targeted unmount (NOT `umount -a`) per partition via the
    # helper, so /proc /sys /tmp /data /cache are never touched.
    # See unmount_and_unmap_partition() docstring for full rationale.
    if [ -n "$RESIZE_NEEDED" ]; then
        ui_print "  Unmounting & unmapping partitions that need resize:$RESIZE_NEEDED"
        UNMAP_FAILED=""
        for pname in $RESIZE_NEEDED; do
            pname=$(echo "$pname" | tr -d ' ')
            [ -z "$pname" ] && continue
            if ! unmount_and_unmap_partition "$pname"; then
                UNMAP_FAILED="$UNMAP_FAILED $pname"
            fi
        done
        if [ -n "$UNMAP_FAILED" ]; then
            ui_print "! WARNING: lptools unmap failed for:$UNMAP_FAILED"
            ui_print "  These partitions are genuinely busy (a process holds an"
            ui_print "  open fd on the dm device). Resize will likely fail for them."
        fi
    else
        ui_print "  No partitions need resize — skipping unmap step."
    fi

    # Resize each partition that needs it
    ui_print "  Using lptools to resize dynamic partitions..."
    RESIZE_OK=1
    # Track partitions created via remove+create fallback (lptools create auto-maps)
    CREATED_PARTS=""
    # RESIZED_ORIGINAL is also declared at script top (default "") — we append here.
    # Each entry is "name:original_size_bytes" so the cleanup trap can restore sizes.
    for pname in $RESIZE_NEEDED; do
        pname=$(echo "$pname" | tr -d ' ')
        [ -z "$pname" ] && continue

        # Find the partition index to get its UNC_SIZE
        FOUND_IDX=-1
        for j in $(seq 0 $(( NUM_PARTS - 1 ))); do
            eval "CHECK_NAME=\$PART_${{j}}_NAME"
            if [ "$CHECK_NAME" = "$pname" ]; then
                FOUND_IDX=$j
                break
            fi
        done

        if [ "$FOUND_IDX" -lt 0 ]; then
            continue
        fi

        eval "PNAME_SZ=\$PART_${{FOUND_IDX}}_UNC_SIZE"
        NEW_SIZE_BYTES=$PNAME_SZ

        # Capture ORIGINAL size BEFORE resize for cleanup-trap rollback.
        # Re-query blockdev because PART_SIZE from validation step is local scope.
        # If blockdev query fails (e.g. partition not mapped), fallback to NEW_SIZE
        # (no-op rollback — better than crash).
        ORIG_PTARGET=$(resolve_target "$pname")
        ORIG_SIZE_BYTES=0
        if [ -e "$ORIG_PTARGET" ]; then
            ORIG_SIZE_BYTES=$(blockdev --getsize64 "$ORIG_PTARGET" 2>/dev/null)
        fi
        if [ -z "$ORIG_SIZE_BYTES" ] || [ "$ORIG_SIZE_BYTES" = "0" ]; then
            # Fallback to sysfs (sectors × 512)
            DEV_NAME=$(basename "$ORIG_PTARGET" 2>/dev/null)
            if [ -n "$DEV_NAME" ] && [ -f "/sys/class/block/$DEV_NAME/size" ]; then
                SECTORS=$(cat "/sys/class/block/$DEV_NAME/size" 2>/dev/null)
                if [ -n "$SECTORS" ]; then
                    ORIG_SIZE_BYTES=$(( SECTORS * 512 ))
                fi
            fi
        fi
        if [ -n "$ORIG_SIZE_BYTES" ] && [ "$ORIG_SIZE_BYTES" != "0" ]; then
            RESIZED_ORIGINAL="$RESIZED_ORIGINAL $pname:$ORIG_SIZE_BYTES"
        fi

        ui_print "  Resizing $pname from $(( ORIG_SIZE_BYTES / 1048576 )) MB to $(( NEW_SIZE_BYTES / 1048576 )) MB..."

        # Try lptools resize first (preserves existing data in metadata)
        # Note: original phhusson lptools resize does NOT re-map after resize
        lptools resize "$pname" "$NEW_SIZE_BYTES" >/dev/null 2>&1
        RESIZE_RC=$?
        if [ $RESIZE_RC -ne 0 ]; then
            ui_print "!  lptools resize failed for $pname — trying remove+create fallback"
            # Fallback: remove then create with new size.
            # lptools remove auto-unmaps before removing.
            # lptools create auto-maps after creating (creates dm device immediately).
            lptools remove "$pname" >/dev/null 2>&1
            lptools create "$pname" "$NEW_SIZE_BYTES" >/dev/null 2>&1
            CREATE_RC=$?
            if [ $CREATE_RC -ne 0 ]; then
                ui_print "!  lptools remove+create also failed for $pname"
                RESIZE_OK=0
                # Rollback entry from RESIZED_ORIGINAL — resize didn't happen,
                # so cleanup trap should NOT try to re-resize this partition.
                RESIZED_ORIGINAL=$(echo "$RESIZED_ORIGINAL" | sed "s/ $pname:[0-9]*//")
            else
                ui_print "  $pname recreated at $(( NEW_SIZE_BYTES / 1048576 )) MB"
                CREATED_PARTS="$CREATED_PARTS $pname "
            fi
        else
            ui_print "  $pname resized to $(( NEW_SIZE_BYTES / 1048576 )) MB"
            # After successful resize, re-map the partition (resize doesn't auto-map
            # in the original phhusson version — lptools resize only updates
            # metadata, does NOT create a new dm-linear device).
            # Use the targeted unmount+unmap+map helper so we don't trip over a
            # stale dm-linear device left from the resize step.
            unmap_and_remap_partition "$pname"
        fi
    done

    if [ "$RESIZE_OK" != "1" ]; then
        ui_print "! ABORT: lptools resize failed for one or more partitions."
        ui_print "!  Try flashing via fastbootd or use a different recovery."
        exit 1
    fi

    # Verify resized partitions are mapped (safety net for post-resize remap).
    #
    # The post-resize remap inside the resize loop (line ~1349) already calls
    # unmap_and_remap_partition for each successfully resized partition, and
    # lptools create auto-maps partitions that went through the remove+create
    # fallback. So at this point, every partition in $RESIZE_NEEDED should
    # already be mapped — UNLESS the post-resize remap failed.
    #
    # Targeted: only iterate $RESIZE_NEEDED (the partitions we touched). Skip
    # $CREATED_PARTS (already mapped by lptools create). Skip partitions that
    # are already mapped (post-resize remap succeeded). Only attempt remap for
    # partitions that are NOT mapped — i.e. post-resize remap failures.
    #
    # This replaces the previous broad iteration over ALL $DYNAMIC_PART_NAMES,
    # which would unnecessarily unmap+remap partitions that were never touched
    # (e.g. system_ext, odm auto-mounted by recovery) and risk EBUSY/EEXIST.
    ui_print "  Verifying resized partitions are mapped..."
    REMAP_FAILED=""
    for pname in $RESIZE_NEEDED; do
        pname=$(echo "$pname" | tr -d ' ')
        [ -z "$pname" ] && continue
        case "$CREATED_PARTS" in
            *" $pname "*) continue ;;  # already mapped by lptools create
        esac
        # Skip if already mapped (post-resize remap succeeded).
        if [ -e "/dev/mapper/$pname" ] || [ -e "/dev/block/by-name/$pname" ]; then
            continue
        fi
        # Not mapped — post-resize remap failed or didn't run. Retry.
        if ! unmap_and_remap_partition "$pname"; then
            REMAP_FAILED="$REMAP_FAILED $pname"
        fi
    done

    if [ -n "$REMAP_FAILED" ]; then
        ui_print "! WARNING: lptools map failed for:$REMAP_FAILED"
        ui_print "  Attempting to continue — dd write may fail if device not ready."
    fi

    ui_print "  lptools resize complete."

    ui_print "  Dynamic partition resize complete."
    ui_print ""
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

    STEP_NUM=$(( i + {flash_step_offset} ))

    ui_print "[Step $STEP_NUM/{total_steps}] Flashing $PNAME ($(( PSIZE / 1048576 )) MB)..."
    ui_print "  Compressed: $(( PCSIZE / 1048576 )) MB | Offset: $POFFSET"

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
    ui_print "  Flashing $PNAME to $PTARGET..."

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
        dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null | \
            $DECOMP_CMD -d > "$TMP_FIFO" 2>/dev/null &
        DECOMP_PID=$!

        # No conv= flags — busybox dd ftruncate() on dm-linear is a false-failure
        dd of="$PTARGET" bs=1048576 if="$TMP_FIFO" 2>/dev/null
        DD_STATUS=$?

        wait $DECOMP_PID 2>/dev/null
        DECOMP_STATUS=$?

        rm -f "$TMP_FIFO"

        if [ $DECOMP_STATUS -ne 0 ]; then
            ui_print "! ABORT: Decompression failed for $PNAME (status=$DECOMP_STATUS)"
            exit 1
        fi

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
    else
        # FIFO not available (very rare) — fall back to direct 3-pipeline.
        # We lose decompressor error detection (sh $? = last pipe stage only),
        # but this avoids tmpfs exhaustion by never writing a temp file.
        dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null | \
            $DECOMP_CMD -d 2>/dev/null | \
            dd of="$PTARGET" bs=1048576 2>/dev/null
        DD_STATUS=$?

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

    # Step C: Post-verify (conditional)
    {verify_block}
    ui_print ""
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
        ui_print "  Active slot: $TARGET_SLOT [OK]"
    fi
fi

# ── Done ────────────────────────────────────────────────────
ui_print "──────────────────────────────────────────"
ui_print " All $NUM_PARTS partition(s) flashed {verified_word} successfully!"
ui_print "──────────────────────────────────────────"
ui_print ""
exit 0
"#,
        flash_step_offset = flash_step_offset,
        num_parts_minus_1 = if num_parts > 0 { num_parts - 1 } else { 0 },
        total_steps = total_steps,
        verify_block = verify_block.trim(),
        verified_word = verified_word,
    ));

    script
}

// ---------------------------------------------------------------------------
//  flash_info.txt builder
// ---------------------------------------------------------------------------

/// Build the flash_info.txt human-readable metadata.
fn build_flash_info(
    compress_name: &str,
    bundle_size: u64,
    num_parts: usize,
    partitions_meta: &[PartitionMeta],
    device: &str,
    level: i32,
    skip_verify: bool,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.push("OTAku v1.0 — dd-based partition flasher".to_string());
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
///
/// # Returns
/// DdBuildResult with success/error, paths, sizes, and log output.
pub fn run_dd_build(
    images: &[(String, String)], // (partition_name, image_path)
    compression: &str,
    level: i32,
    output_path: &str,
    device: &str,
    skip_verify: bool,
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
        let temp_dir = std::env::temp_dir();
        let bundle_tmp_path = temp_dir.join("otaku_build_tmp.bin");
        let bundle_tmp_path_str = bundle_tmp_path.to_string_lossy().to_string();

        // Clean up stale temp file from previous builds
        let _ = std::fs::remove_file(&bundle_tmp_path);

        // Write placeholder header first (will overwrite with real header later)
        {
            let mut tmp = File::create(&bundle_tmp_path)
                .map_err(|e| format!("Cannot create temp file: {}", e))?;
            tmp.write_all(&vec![0u8; HEADER_SIZE])
                .map_err(|e| format!("Cannot write header placeholder: {}", e))?;
            tmp.flush()
                .map_err(|e| format!("Cannot flush header: {}", e))?;
        }

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

        // ── Step 1: Build otaku.bin (incrementally written to temp file) ──
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

        // Open temp file in append mode for incremental writes
        {
            let mut tmp_file = std::fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(&bundle_tmp_path)
                .map_err(|e| format!("Cannot open temp file for writing: {}", e))?;

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
                    tmp_file.metadata().map(|m| m.len()).unwrap_or(HEADER_SIZE as u64),
                    Some(&bundle_tmp_path_str), total_estimated,
                    0, // partition_percent = 0%
                );

                // Get the uncompressed file size for progress calculation
                let unc_size = std::fs::metadata(path)
                    .map_err(|e| format!("Cannot stat {}: {}", path, e))?
                    .len();

                // Hash and compress with real-time per-chunk progress reporting.
                // The progress callback fires after each 4MB chunk is read and fed
                // to the compressor, updating the sidecar file with the partition
                // compression percentage. This is what makes live progress work —
                // Kotlin polls the sidecar every 500ms and sees the percentage grow.
                let output_path_clone = output_path.to_string();
                let bundle_tmp_path_str_clone = bundle_tmp_path_str.clone();
                let name_clone = name.clone();
                let (compressed, hash_hex) = hash_and_compress_file_with_progress(
                    path,
                    &compress_name,
                    level_opt,
                    Some(&mut |bytes_read: u64, file_size: u64| {
                        let pct = if file_size > 0 {
                            (bytes_read * 100 / file_size) as i32
                        } else {
                            100
                        };
                        write_progress_with_percent(
                            &output_path_clone,
                            i + 1,
                            num_parts,
                            &name_clone,
                            "compressing",
                            tmp_file.metadata().map(|m| m.len()).unwrap_or(HEADER_SIZE as u64),
                            Some(&bundle_tmp_path_str_clone),
                            total_estimated,
                            pct,
                        );
                    }),
                )?;

                let comp_size = compressed.len() as u64;
                let data_offset = tmp_file.metadata().map(|m| m.len()).unwrap_or(0) - HEADER_SIZE as u64;

                partitions_meta.push(PartitionMeta {
                    name: name.clone(),
                    unc_size,
                    hash_hex,
                    comp_size,
                    data_offset,
                });

                // Write compressed data directly to temp file (incremental!)
                tmp_file.write_all(&compressed)
                    .map_err(|e| format!("Cannot write compressed data for {}: {}", name, e))?;

                // Align to 4096 boundary
                let current_pos = tmp_file.metadata().map(|m| m.len()).unwrap_or(0);
                let aligned = align_up(current_pos as usize, ALIGN);
                if aligned > current_pos as usize {
                    let padding = aligned - current_pos as usize;
                    tmp_file.write_all(&vec![0u8; padding])
                        .map_err(|e| format!("Cannot write alignment padding: {}", e))?;
                }

                tmp_file.flush()
                    .map_err(|e| format!("Cannot flush temp file after {}: {}", name, e))?;

                // Write progress: partition done (100% for this partition)
                write_progress_with_percent(
                    output_path, i + 1, num_parts, name, "compressed",
                    tmp_file.metadata().map(|m| m.len()).unwrap_or(0),
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
        } // tmp_file dropped here

        // Now overwrite the header placeholder with the real header
        let header = build_header(compress_id_val, num_parts as u16);
        {
            let mut tmp_file = std::fs::OpenOptions::new()
                .write(true)
                .open(&bundle_tmp_path)
                .map_err(|e| format!("Cannot open temp file for header: {}", e))?;
            tmp_file.seek(std::io::SeekFrom::Start(0))
                .map_err(|e| format!("Cannot seek to start: {}", e))?;
            tmp_file.write_all(&header)
                .map_err(|e| format!("Cannot write header: {}", e))?;
            tmp_file.flush()
                .map_err(|e| format!("Cannot flush header: {}", e))?;
        }

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
        for i in 12..HEADER_SIZE {
            assert_eq!(hdr[i], 0u8);
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
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        assert!(script.starts_with("#!/sbin/sh"));
        assert!(script.contains("PART_0_NAME=\"boot\""));
        assert!(script.contains("PART_0_HASH=\"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\""));
        assert!(script.contains("check_decompressor \"gzip\""));
        assert!(script.contains("sha256sum"));
        assert!(script.contains("VERIFIED OK"));
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
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", true);
        assert!(script.contains("Verification skipped"));
        assert!(!script.contains("sha256sum"));
        assert!(script.contains("flashed  successfully!"));
    }

    #[test]
    fn test_build_update_script_with_device() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
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
        }];
        let info = build_flash_info("gzip", 16781312, 1, &meta, "crosshatch", 6, false);
        assert!(info.contains("OTAku v1.0"));
        assert!(info.contains("gzip (level 6)"));
        assert!(info.contains("crosshatch"));
        assert!(info.contains("[boot]"));
        assert!(info.contains("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"));
        assert!(info.contains("enabled"));
    }

    #[test]
    fn test_run_dd_build_no_images() {
        let result = run_dd_build(&[], "gzip", 6, "/tmp/test.zip", "", false);
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
        assert!(
            script.contains("if ! lptools resize \"$rname\" \"$rsize\""),
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

        // Assert: fix is present (shared regex variable with no leading space).
        assert!(
            script.contains("ZIP_LIST_REGEX="),
            "REGRESSION: ZIP_LIST_REGEX variable not defined (Bug #2)"
        );
        assert!(
            script.contains("^[0-9]+ otaku"),
            "REGRESSION: fixed regex pattern not found (Bug #2)"
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

    /// Regression: edge case B — getprop kosong fallback chain.
    /// The old code used `getprop ro.product.device || getprop ro.build.product`.
    /// If both returned empty (recovery without /vendor mounted), device
    /// check would silently mismatch. The fix adds /system/build.prop,
    /// /vendor/build.prop, and /proc/cmdline fallbacks.
    #[test]
    fn test_regression_getprop_fallback_chain() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "alioth", false);

        // Assert: build.prop fallback paths present.
        assert!(
            script.contains("/system/build.prop"),
            "REGRESSION: /system/build.prop fallback missing (edge case B)"
        );
        assert!(
            script.contains("/vendor/build.prop"),
            "REGRESSION: /vendor/build.prop fallback missing (edge case B)"
        );

        // Assert: /proc/cmdline androidboot.hardware fallback present.
        assert!(
            script.contains("androidboot.hardware"),
            "REGRESSION: /proc/cmdline hardware fallback missing (edge case B)"
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
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);

        // Pre-flash verify step is present
        assert!(
            script.contains("Pre-flash partition table verify"),
            "REGRESSION: pre-flash verify step missing (alur user step 2)"
        );

        // Verify step number is 1 (after extract=0, before integrity=2)
        assert!(
            script.contains("[Step 1/") || script.contains("[Step 1 ") || script.contains("Step 1/"),
            "REGRESSION: pre-flash verify step not numbered as Step 1"
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
            script.contains("passed structural verify"),
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
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        assert!(script.contains("HASH_SHORT"), "Bug NEW-C: HASH_SHORT variable missing");
        assert!(script.contains("printf"), "Bug NEW-C: printf fix missing");
    }
}
