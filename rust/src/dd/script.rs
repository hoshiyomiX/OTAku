//! Update-binary flasher script builder (TWRP/OrangeFox-compatible shell template).
//!
//! Secara mekanis dipindah dari dd.rs Fase-1 (zero behavior change):
//! seluruh logika template identik byte-per-byte dengan versi pra-split.

use super::{decomp_cmd_for_id, shell_escape_dq, PartitionMeta};

// ---------------------------------------------------------------------------
//  Update-binary script builder
// ---------------------------------------------------------------------------

/// Flasher script version/branding string.
/// Displayed in the update-binary header and flash_info.txt.
const SCRIPT_VERSION: &str = "Custom Payload Maker";

/// Build the META-INF/com/google/android/update-binary shell script.
///
/// This is a TWRP/OrangeFox-compatible flasher that:
/// 1. Opens payload (direct ZIP read or fallback extract to /tmp)
/// 2. Checks decompressor availability
/// 3. Validates bundle integrity (DDBU magic, version, compress, partition count)
/// 4. Checks device compatibility (if device specified)
/// 5. Detects A/B slot
/// 6. Validates partition block devices (size check, unmount)
/// 7. Flashes each partition (direct read → decompress → dd write → optional verify)
pub(super) fn build_update_script(
    num_parts: usize,
    compress_id: u16,
    compress_name: &str,
    partitions_meta: &[PartitionMeta],
    total_unc_size: u64,
    device: &str,
    skip_verify: bool,
) -> String {
    let decomp_cmd = decomp_cmd_for_id(compress_id);

    // Build partition variable assignments
    // BUG FIX (NEW-1): Shell-escape partition names to prevent command injection.
    // Previously, raw partition names were interpolated into double-quoted shell
    // strings, allowing `$(cmd)` or backtick injection to execute arbitrary
    // commands with root privileges in recovery shell.
    let mut part_vars = String::new();
    for (i, p) in partitions_meta.iter().enumerate() {
        part_vars.push_str(&format!(
            "PART_{}_NAME=\"{}\"\n\
             PART_{}_UNC_SIZE=\"{}\"\n\
             PART_{}_HASH=\"{}\"\n\
             PART_{}_COMP_SIZE=\"{}\"\n\
             PART_{}_DATA_OFFSET=\"{}\"\n\
             PART_{}_COMP_HASH=\"{}\"\n",
            i, shell_escape_dq(&p.name), i, p.unc_size, i, p.hash_hex, i, p.comp_size, i, p.data_offset,
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
    let free_space_step = resize_step + 1;  // Pre-flash free space check (NEW)
    let flash_step_offset = free_space_step + 1;
    let total_steps = num_parts + flash_step_offset;

    // Device check step
    let device_check_step = integrity_step + 1; // = 3
    // BUG FIX (NEW-1): Shell-escape device codename to prevent injection
    let escaped_device = shell_escape_dq(device);
    let device_check_block = if has_device {
        format!(
            r#"
# ── Step {device_check_step}: Device compatibility ──────────────────
TARGET_DEVICE="{escaped_device}"
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

        # F6 fix (T25 audit): the old interactive confirm was a dead feature:
        #   - `choose` does not exist in TWRP/busybox (verified against the
        #     TWRP android-12.1 tree and the busybox applet list)
        #   - `read` reads STDIN — in recovery that is the update-binary
        #     protocol pipe, not a console: it hangs or consumes protocol
        #     data. (`command -v read` is always true anyway — builtin.)
        # New policy, no fake interactivity:
        #   - Detection FAILED (CURRENT_DEVICE empty) → warn + proceed.
        #     Codename detection has real false negatives (OEM props absent
        #     in minimal recoveries); blocking the user on a failed probe
        #     would prevent flashing the CORRECT device. Partition
        #     validation (existence + size) remains the real safety gate.
        #   - Genuinely DIFFERENT device detected → ABORT (matches the old
        #     de-facto behavior, since both confirm branches always failed).
        if [ -z "$CURRENT_DEVICE" ]; then
            ui_print "  ! Device codename could not be detected on this recovery."
            ui_print "  ! Continuing — partition validation still gates the flash."
        else
            ui_print "! ABORT: Refusing to flash a bundle built for $TARGET_DEVICE"
            ui_print "!  onto this device ($CURRENT_DEVICE)."
            ui_print "!  Rebuild the bundle with the correct device selected,"
            ui_print "!  or flash it on the intended device."
            exit 1
        fi
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
    // BUG FIX (O-2): Sanitize header_info to prevent shell injection via newlines.
    // This string is interpolated into a shell comment in the flasher script.
    // Without sanitization, a device codename containing \n breaks out of the
    // comment, allowing arbitrary command execution with root privileges.
    header_info = header_info.replace(['\n', '\r'], " ");

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
        # we cover the full partition, then trim to EXACTLY PSIZE bytes using
        # dd-based verify_trim() instead of head -c (1-byte-at-a-time syscall).
        #
        # Without verify_trim, if PSIZE is not 1MB-aligned (common for Android
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
        # ── verify_trim: dd-based replacement for head -c "$PSIZE" ──
        # head -c reads 1 byte per syscall — O(n) syscalls for n bytes.
        # For a 5120 MB partition, that's 5.12 billion syscalls → extremely slow.
        # verify_trim uses dd bs=4096 (4KB blocks) for bulk reads, then
        # dd bs=1 for the <4096 byte remainder. This is O(n/4096) syscalls —
        # 4096x fewer than head -c.
        VERIFY_FULL_BLOCKS=$(( PSIZE / 4096 ))
        VERIFY_REMAINDER=$(( PSIZE % 4096 ))
        verify_trim() {
            if [ "$VERIFY_REMAINDER" -eq 0 ]; then
                dd bs=4096 count=$VERIFY_FULL_BLOCKS 2>/dev/null
            else
                dd bs=4096 count=$VERIFY_FULL_BLOCKS 2>/dev/null
                dd bs=1 count=$VERIFY_REMAINDER 2>/dev/null
            fi
        }
        dd if="$PTARGET" bs=1048576 count=$VERIFY_BLOCKS 2>/dev/null | verify_trim | tee "$VERIFY_FIFO" >/dev/null

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
    # Use verify_trim (dd-based) to ensure we hash EXACTLY PSIZE bytes.
    # verify_trim uses dd bs=4096 for bulk reads — 4096x fewer syscalls
    # than head -c (1-byte-at-a-time). Same approach as flash trim_pipe.
    VERIFY_BLOCKS=$(( (PSIZE + 1048575) / 1048576 ))
    VERIFY_FULL_BLOCKS=$(( PSIZE / 4096 ))
    VERIFY_REMAINDER=$(( PSIZE % 4096 ))
    verify_trim() {
        if [ "$VERIFY_REMAINDER" -eq 0 ]; then
            dd bs=4096 count=$VERIFY_FULL_BLOCKS 2>/dev/null
        else
            dd bs=4096 count=$VERIFY_FULL_BLOCKS 2>/dev/null
            dd bs=1 count=$VERIFY_REMAINDER 2>/dev/null
        fi
    }
    VERIFY_HASH=$(dd if="$PTARGET" bs=1048576 count=$VERIFY_BLOCKS 2>/dev/null | verify_trim | sha256sum | cut -d' ' -f1)
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
            # Build slot-suffixed name for lptools (same as resize step)
            pname_lp="$pname"
            if [ -n "$TARGET_SLOT" ]; then
                pname_lp="${{pname}}${{TARGET_SLOT}}"
            fi
            # Skip if already mapped — no need to touch it.
            # Check both plain and slot-suffixed paths: on A/B devices the
            # dm-linear device is named "vendor_a", not just "vendor".
            if [ -e "/dev/mapper/$pname" ] || [ -e "/dev/block/by-name/$pname" ] || \
               [ -e "/dev/mapper/$pname_lp" ] || [ -e "/dev/block/by-name/$pname_lp" ]; then
                continue
            fi
            # Not mapped — try to map.  Use unmount_partition + lptools unmap
            # separately so that lptools unmap receives the slot-suffixed name
            # (unmount_and_unmap_partition hardcodes plain $pname for lptools).
            unmount_partition "$pname" >/dev/null 2>&1 || true
            lptools unmap "$pname_lp" >/dev/null 2>&1 || true
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

# ── Direct ZIP reading (primary) vs extract-to-/tmp (fallback) ──
# BUG FIX: Previously, the entire otaku.bin (potentially 4+ GB) was extracted
# to /tmp (tmpfs, RAM-backed). On devices with limited RAM, this fails with
# "Failed to extract otaku.bin" when otaku.bin exceeds available /tmp space.
# New approach: read otaku.bin data directly from the ZIP file using dd with
# a computed offset (ZIP_DATA_OFFSET). This eliminates the /tmp extraction
# entirely for the primary path — no tmpfs usage regardless of bundle size.
# The ZIP stores otaku.bin with CompressionMethod::Stored (no compression),
# so the bytes inside the ZIP are identical to the standalone file.
BUNDLE=""           # Set after offset computation or fallback extraction
ZIP_DATA_OFFSET=0   # Byte offset of otaku.bin data within the ZIP
BUNDLE_SIZE=0       # Size of otaku.bin data (not the ZIP file size)
TOTAL_FLASH_SIZE={total_unc_size}   # Total uncompressed size of all partitions (for free space check)
{part_vars}NUM_PARTS={num_parts}
COMPRESS_ID={compress_id}
SKIP_VERIFY={skip_verify_flag}   # F2: 1 = no post-flash hash; dd failures must then prove themselves

ui_print "======================================"
ui_print "  OTAku — {script_version}"
ui_print "        by hoshiyomiX"
ui_print "======================================"
"#,
        script_version = SCRIPT_VERSION,
        header_info = header_info,
        part_vars = part_vars,
        total_unc_size = total_unc_size,
        num_parts = num_parts,
        compress_id = compress_id,
        skip_verify_flag = if skip_verify { 1 } else { 0 },
    ));

    // ── Step 0: Open payload (direct ZIP reading or fallback extract) ──
    // BUG FIX: Previously, the entire otaku.bin (potentially 4+ GB) was
    // extracted to /tmp (tmpfs, RAM-backed). On devices with limited RAM
    // (e.g. Infinix-X6871 with 8 GB RAM, /tmp ~2-4 GB), this fails with
    // "Failed to extract otaku.bin" for bundles exceeding /tmp capacity.
    // Recovery log evidence: "Size: 3798 MB" → "✗ Error: Failed to extract"
    //
    // New approach: read otaku.bin data DIRECTLY from the ZIP file using dd
    // with a computed offset. Since otaku.bin is stored with
    // CompressionMethod::Stored (no ZIP-level compression), the bytes inside
    // the ZIP are identical to the standalone file. This eliminates /tmp usage
    // entirely for the primary path — no tmpfs exhaustion regardless of bundle
    // size. The old extract-to-/tmp path is retained as a fallback for edge
    // cases (e.g. ZIP on a FUSE filesystem where dd skip is unreliable).
    script.push_str(&format!(
        r#"# ── Step {extract_step}/{total_steps}: Open payload ──────────────────
ui_print "> Opening payload..."

if [ ! -f "$ZIPFILE" ]; then
    ui_print "✗ Error: ZIP file not found"
    ui_print "  Path: $ZIPFILE"
    exit 1
fi

# ── Primary path: Compute ZIP_DATA_OFFSET from local file header ──
# The ZIP format stores each file entry as:
#   [Local file header: 30 bytes fixed + filename + extra field]
#   [File data]
# We parse the first local file header to find where otaku.bin data starts.
# Since otaku.bin is always the first entry in our ZIPs, it starts at byte 0.
#
# Local file header layout (little-endian):
#   Offset 0:  Signature (4B) = 0x04034b50
#   Offset 26: Filename length (2B)
#   Offset 28: Extra field length (2B)
#   Data starts at: 30 + filename_length + extra_field_length
#
# With ZIP64 (large_file=true), the extra field contains:
#   ZIP64 extended info: header_id(2B) + data_size(2B) + orig_size(8B) + comp_size(8B)
#   = 20 bytes extra (typical for our ZIPs with 9-byte filename "otaku.bin")
#   Total offset = 30 + 9 + 20 = 59 bytes
DIRECT_READ_OK=0
ZIP_LFH_SIG=$(od -A n -t x1 -N 4 "$ZIPFILE" 2>/dev/null | tr -d '[:space:]')
if [ "$ZIP_LFH_SIG" = "504b0304" ]; then
    FNAME_LEN=$(od -A n -t u2 -j 26 -N 2 "$ZIPFILE" 2>/dev/null | tr -d '[:space:]')
    EXTRA_LEN=$(od -A n -t u2 -j 28 -N 2 "$ZIPFILE" 2>/dev/null | tr -d '[:space:]')
    if [ -n "$FNAME_LEN" ] && [ -n "$EXTRA_LEN" ]; then
        ZIP_DATA_OFFSET=$(( 30 + FNAME_LEN + EXTRA_LEN ))
        # Verify: read the filename and confirm it's "otaku.bin"
        FNAME_READ=$(dd if="$ZIPFILE" bs=1 skip=30 count=$FNAME_LEN 2>/dev/null | tr -d '\0')
        if [ "$FNAME_READ" = "otaku.bin" ]; then
            DIRECT_READ_OK=1
        else
            ui_print "  Note: First ZIP entry is '$FNAME_READ', not 'otaku.bin'"
        fi
    fi
fi

# Query ZIP central directory for expected otaku.bin size (used for validation).
EXPECTED_BUNDLE_SIZE=0
ZIP_LIST_OK=0
ZIP_LIST_NAME_PATTERN='otaku[.]bin$'

try_zip_listing() {{
    local unzip_cmd="$1"
    local listing
    listing=$($unzip_cmd -l "$ZIPFILE" 2>/dev/null | tr -d '\r' | awk '{{print $1, $NF}}' | grep "$ZIP_LIST_NAME_PATTERN")
    [ -z "$listing" ] && return 1
    EXPECTED_BUNDLE_SIZE=$(echo "$listing" | awk '{{print $1}}' | tr -d ' ')
    case "$EXPECTED_BUNDLE_SIZE" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$EXPECTED_BUNDLE_SIZE" -gt 0 ] 2>/dev/null || return 1
    return 0
}}

if which unzip >/dev/null 2>&1; then
    try_zip_listing "unzip" && ZIP_LIST_OK=1
fi
if [ "$ZIP_LIST_OK" = "0" ] && busybox --list 2>/dev/null | grep -q "^unzip$"; then
    try_zip_listing "busybox unzip" && ZIP_LIST_OK=1
fi
if [ "$ZIP_LIST_OK" = "0" ] && toybox unzip --help >/dev/null 2>&1; then
    try_zip_listing "toybox unzip" && ZIP_LIST_OK=1
fi

if [ "$DIRECT_READ_OK" = "1" ]; then
    # ── Primary: Direct ZIP reading ──
    BUNDLE="$ZIPFILE"
    ZIP_FILE_SIZE=$(wc -c < "$ZIPFILE" | tr -d ' ')

    ui_print "  Mode: Direct ZIP read (no /tmp extraction needed)"
    ui_print "  ZIP data offset: $ZIP_DATA_OFFSET bytes"

    # ── BUNDLE_SIZE computation ──
    # ZIP_FILE_SIZE - ZIP_DATA_OFFSET gives the size of ALL bytes from the
    # otaku.bin data start to the END of the ZIP file — but the ZIP file also
    # contains the central directory and EOCD record AFTER otaku.bin's data.
    # These trailing bytes inflate the computed size, causing a mismatch against
    # the actual otaku.bin size reported by unzip -l.
    # When unzip -l is available (ZIP_LIST_OK=1), use EXPECTED_BUNDLE_SIZE
    # directly — it gives the exact otaku.bin uncompressed size, which equals
    # the stored (CompressionMethod=Stored) data size inside the ZIP.
    # When unzip -l is unavailable, use ZIP_FILE_SIZE - ZIP_DATA_OFFSET as a
    # best-effort estimate (slightly over-counts, but unavoidable without listing).
    if [ "$ZIP_LIST_OK" = "1" ] && [ -n "$EXPECTED_BUNDLE_SIZE" ] && [ "$EXPECTED_BUNDLE_SIZE" != "0" ]; then
        BUNDLE_SIZE=$EXPECTED_BUNDLE_SIZE
        ui_print "  Size: $(( BUNDLE_SIZE / 1048576 )) MB ✓ (verified via ZIP listing)"
    else
        BUNDLE_SIZE=$(( ZIP_FILE_SIZE - ZIP_DATA_OFFSET ))
        ui_print "  Size: $(( BUNDLE_SIZE / 1048576 )) MB (listing unavailable — size includes ZIP trailer)"
    fi
    ui_print "  ✓ Direct read ready"
else
    # ── Fallback: Extract otaku.bin to /tmp ──
    # This path is used when:
    #   - ZIP local file header parsing failed (unusual ZIP structure)
    #   - First ZIP entry is not otaku.bin
    #   - od command not available
    # WARNING: This requires enough /tmp space for the entire otaku.bin!
    ui_print "  Note: Direct ZIP read unavailable — falling back to /tmp extraction"
    if [ "$ZIP_LIST_OK" = "1" ] && [ -n "$EXPECTED_BUNDLE_SIZE" ] && [ "$EXPECTED_BUNDLE_SIZE" != "0" ]; then
        ui_print "  Size: $(( EXPECTED_BUNDLE_SIZE / 1048576 )) MB"
    else
        ui_print "  Note: cannot query ZIP listing — size check will be skipped."
    fi

    # Pre-extraction /tmp space check (if df available)
    if [ "$ZIP_LIST_OK" = "1" ] && [ -n "$EXPECTED_BUNDLE_SIZE" ] && command -v df >/dev/null 2>&1; then
        TMP_FREE_KB=$(df /tmp 2>/dev/null | tail -1 | awk '{{print $4}}')
        NEEDED_KB=$(( (EXPECTED_BUNDLE_SIZE + 1023) / 1024 ))
        if [ -n "$TMP_FREE_KB" ] && [ "$TMP_FREE_KB" -lt "$NEEDED_KB" ] 2>/dev/null; then
            ui_print "✗ Error: Not enough /tmp space for extraction"
            ui_print "  Needed: $(( NEEDED_KB / 1024 )) MB"
            ui_print "  Available: $(( TMP_FREE_KB / 1024 )) MB"
            ui_print "  Hint: Use a smaller bundle or free up /tmp space"
            exit 1
        fi
    fi

    BUNDLE="/tmp/otaku.bin"
    rm -f "$BUNDLE"
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
        if command -v df >/dev/null 2>&1; then
            df -h /tmp 2>/dev/null | tail -1 | awk '{{print "    total="$2" used="$3" free="$4}}' 2>/dev/null
        fi
        exit 1
    fi

    BUNDLE_EXTRACT_SIZE=$(wc -c < "$BUNDLE" | tr -d ' ')
    BUNDLE_SIZE=$BUNDLE_EXTRACT_SIZE

    # Post-extract size verification
    if [ "$ZIP_LIST_OK" = "1" ] && [ -n "$EXPECTED_BUNDLE_SIZE" ] && [ "$EXPECTED_BUNDLE_SIZE" != "0" ]; then
        if [ "$BUNDLE_EXTRACT_SIZE" != "$EXPECTED_BUNDLE_SIZE" ]; then
            ui_print "✗ Error: otaku.bin size mismatch"
            ui_print "  Expected: $EXPECTED_BUNDLE_SIZE bytes"
            ui_print "  Actual:   $BUNDLE_EXTRACT_SIZE bytes"
            ui_print "  Hint: tmpfs full or ZIP CRC error"
            if command -v df >/dev/null 2>&1; then
                TMP_FREE=$(df /tmp 2>/dev/null | tail -1 | awk '{{print $4}}')
                if [ -n "$TMP_FREE" ]; then
                    ui_print "    Free: $TMP_FREE (1K-blocks)"
                fi
            else
                ui_print "    (df not available in this recovery)"
            fi
            rm -f "$BUNDLE"
            exit 1
        fi
        ui_print "  ✓ Size verified"
    fi

    ui_print "  ✓ Extracted ($(( BUNDLE_EXTRACT_SIZE / 1048576 )) MB)"
fi
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

# BUNDLE_SIZE is set in Step 0 (either computed from ZIP size - offset,
# or from wc -c of the extracted file). Use it directly for verification.
# For direct-read mode, wc -c < $BUNDLE would give the entire ZIP file
# size (not otaku.bin size), so we MUST use the pre-computed BUNDLE_SIZE.
BUNDLE_VERIFY_SIZE=$BUNDLE_SIZE
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

if [ "$COMPRESS_ID" = "0" ]; then
    # ALG_NONE: no decompression needed — use plain cat (no -d flag).
    # BUG FIX (NEW-F): Previously used "$DECOMP_CMD -d" which expands
    # to "cat -d" — neither GNU coreutils cat, busybox cat, nor toybox
    # cat supports -d, causing the flash to fail with "invalid option".
    DECOMP_CMD="cat"
    DECOMP_PIPE="cat"
else
    if ! check_decompressor "{decomp_cmd}"; then
        ui_print "! ABORT: {decomp_cmd} not found."
        ui_print "! Available tools:"
        which gzip bzip2 xz lz4 zstd zstdcat 2>/dev/null || echo "  (none found)"
        busybox --list 2>/dev/null | head -5
        ui_print "! Rebuild bundle with an available compressor."
        ui_print "! Recommended: --compress lz4 (fastest) or --compress gzip"
        exit 1
    fi
    DECOMP_PIPE="$DECOMP_CMD -d"
    # BUG FIX: lz4 requires explicit -c flag for stdout output when piped.
    # gzip/bzip2/xz auto-detect pipe and write to stdout, but lz4 -d
    # without -c may attempt to write to a file (especially older versions
    # or busybox lz4). The fallback chain already uses "lz4 -dc" as the
    # first fallback, so the primary pipe should match.
    if [ "$COMPRESS_ID" = "5" ]; then
        DECOMP_PIPE="$DECOMP_CMD -dc"
    fi
    # ZSTD also requires -c for stdout output when piped (like lz4).
    # zstd -d without -c writes to a file with .zst removed by default.
    if [ "$COMPRESS_ID" = "6" ]; then
        DECOMP_PIPE="$DECOMP_CMD -dc"
    fi
    # Brotli also requires -c for stdout output when piped (like lz4/zstd).
    # On OrangeFox, brotli is only available via busybox, and busybox brotli -d
    # without -c attempts to write to a file (removing .br extension) instead of
    # stdout. The fallback chain already uses "busybox brotli -dc", so the
    # primary pipe should match.
    if [ "$COMPRESS_ID" = "4" ]; then
        DECOMP_PIPE="$DECOMP_CMD -dc"
    fi

    # ── Multi-threaded decompressor upgrade ──
    # OrangeFox recovery availability (default build, no optional flags):
    #   gzip/bzip2/xz: ✅ via toybox (single-threaded only)
    #   nproc:         ✅ via toybox
    #   pigz/pbzip2:   ❌ REMOVED — no OrangeFox build flag exists; never available
    #   brotli:        ❌ REMOVED — no OrangeFox build flag exists; never available
    #   xz -T0:        ⚠️  Only if FOX_USE_XZ_UTILS=1 enabled by device maintainer
    #   lz4:           ⚠️  Only if FOX_USE_LZ4_BINARY=1 enabled by device maintainer
    #   zstd:          ⚠️  Only if FOX_USE_ZSTD_BINARY=1 enabled by device maintainer
    #
    # pigz/pbzip2/brotli MT upgrade branches removed — they were dead code
    # (command -v always fails on OrangeFox). Removing them shrinks the
    # generated script and eliminates misleading "try pigz" messages.
    NPROC=$(nproc 2>/dev/null || echo 1)
    if [ "$NPROC" -gt 1 ]; then
        case "$COMPRESS_ID" in
            3) # xz → try xz -T0 (multi-threaded xz, liblzma 5.2+)
                # xz -T0 uses all available threads; -T1 = single-threaded (default)
                if xz -T0 --help >/dev/null 2>&1; then
                    DECOMP_PIPE="xz -T0 -dc"
                    ui_print "  ✓ Multi-threaded: xz -T0 ($NPROC cores)"
                fi
                ;;
            5) # lz4 → lz4 is already extremely fast single-threaded,
                # but lz4 -T0 can use multiple threads for marginal gain.
                # Only enable if the lz4 binary supports -T flag.
                if lz4 -T0 --help >/dev/null 2>&1; then
                    DECOMP_PIPE="lz4 -dc -T0"
                    ui_print "  ✓ Multi-threaded: lz4 -T0 ($NPROC cores)"
                fi
                ;;
            6) # zstd → zstd -T0 uses all available threads.
                # zstd decompression is already fast (~400 MB/s single-thread),
                # but multi-threaded decompress can help on very large partitions.
                if zstd -T0 --help >/dev/null 2>&1; then
                    DECOMP_PIPE="zstd -dc -T0"
                    ui_print "  ✓ Multi-threaded: zstd -T0 ($NPROC cores)"
                fi
                ;;
        esac
    fi
fi
ui_print "  ✓ Decompressor: $DECOMP_PIPE"

# ── Bundle integrity ──
# BUNDLE_SIZE is already set from Step 0 (either computed from ZIP or extracted file).
# For direct-read mode, reading the header requires dd with skip=$ZIP_DATA_OFFSET
# because $BUNDLE points to the ZIP file (not the extracted otaku.bin).
# For fallback mode, $BUNDLE = /tmp/otaku.bin and ZIP_DATA_OFFSET = 0, so dd is
# equivalent to reading the file directly.
#
# Helper: read bytes from otaku.bin at a given offset and length.
# Uses dd with bs=1 for precision (header reads are small — < 20 bytes).
# For direct-read mode, adds ZIP_DATA_OFFSET to the skip.
read_bundle_bytes() {{
    local offset=$1
    local count=$2
    dd if="$BUNDLE" bs=1 skip=$(( ZIP_DATA_OFFSET + offset )) count=$count 2>/dev/null
}}

HDR_MAGIC=$(read_bundle_bytes 0 4 | od -A n -t x1 | tr -d '[:space:]')
if [ "$HDR_MAGIC" != "44444255" ]; then
    ui_print "! ABORT: Invalid bundle magic (expected DDBU, got $(echo $HDR_MAGIC | sed 's/\(..\)/\\x\1/g'))"
    exit 1
fi

HDR_VERSION=$(read_bundle_bytes 4 2 | od -A n -t u2 | tr -d '[:space:]')
# HDR_COMPRESS is u16 LE (2 bytes) — read with -t u2 -N 2 to match the
# header writer (build_header() writes compress_id as u16 LE).
# Previous code used -t u1 -N 1 which only read the low byte; this worked
# by accident for compress_id 0-4 (high byte = 0) but would silently
# truncate if compress_id ever exceeded 255.
HDR_COMPRESS=$(read_bundle_bytes 6 2 | od -A n -t u2 | tr -d '[:space:]')
HDR_NUM_PARTS=$(read_bundle_bytes 8 2 | od -A n -t u2 | tr -d '[:space:]')
HDR_HDR_SIZE=$(read_bundle_bytes 10 2 | od -A n -t u2 | tr -d '[:space:]')

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

# F8 fix (T25): cross-check the bundle header against the script's own
# constant. A mismatch means this update-binary and this otaku.bin come
# from DIFFERENT builds (e.g. someone re-packed an old bundle into a new
# ZIP shell) — the offsets/hashes baked into the script would not match
# the bundle layout, flashing garbage at wrong offsets.
if [ "$HDR_NUM_PARTS" != "$NUM_PARTS" ]; then
    ui_print "! ABORT: Header/script partition count mismatch"
    ui_print "!  otaku.bin header says : $HDR_NUM_PARTS"
    ui_print "!  update-binary says    : $NUM_PARTS"
    ui_print "!  This ZIP mixes bundles from different builds — re-download or rebuild."
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
    # F7 fix (T25): ANCHOR the device-name match — an unanchored device
    # name matched dm-5 inside dm-55 (and any path containing the
    # substring), unmounting the WRONG partition's mounts. Anchor:
    # preceded by start-or-slash, followed by space-or-EOL. The retry
    # probe below uses grep -qF (fixed string) so mount-point text is
    # never treated as a regex.
    if [ -z "$real_dev" ] || [ -z "$dev_name" ]; then
        return 0
    fi
    mount_points=$(mount 2>/dev/null | grep -E "(^|/)($real_dev|$dev_name)([[:space:]]|\$)" | awk '{{print $3}}')
    for mp in $mount_points; do
        ui_print "    unmount $pname from $mp"
        umount "$mp" 2>/dev/null
        # dm-verity / dm-crypt mounts sometimes need a moment to release;
        # retry once with lazy unmount as a safety net. Lazy unmount detaches
        # the mount immediately even if a process still has open fds — safe
        # here because we are about to destroy the underlying dm device.
        if mount 2>/dev/null | grep -qF " $mp "; then
            sleep 1
            umount -l "$mp" 2>/dev/null
        fi
    done
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
    # F7 fix (T25): anchored match — see the identical fix in the unmount
    # helper. Unanchored device names matched dm-5 inside dm-55 and matched
    # device names appearing inside unrelated mount paths.
    MOUNT_POINT=$(mount 2>/dev/null | grep -E "(^|/)($real_dev|$DEV_NAME)([[:space:]]|\$)" | awk '{{print $3}}' | head -1)
    if [ -n "$MOUNT_POINT" ]; then
        ui_print "  Unmounting $name from $MOUNT_POINT..."
        umount "$MOUNT_POINT" 2>/dev/null
        # Verify umount took effect; retry once with sleep if still mounted.
        # Most umounts are instant — the previous unconditional `sleep 1`
        # wasted 1s per partition (10 partitions × 1s = 10s of pure stall).
        # dm-verity and dm-crypt mounts sometimes need a moment to release,
        # so we keep a single retry as a safety net.
        if mount 2>/dev/null | grep -qF " $MOUNT_POINT "; then
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

    // ── Pre-flash free space check ──
    // After partition validation and resize, verify that the target device
    // has enough free space to hold the total uncompressed data. This check
    // uses TOTAL_FLASH_SIZE (computed during build and saved in the ZIP) to
    // compare against available storage on the target device.
    //
    // For devices with dynamic partitions, lptools free is used (super partition).
    // For devices with non-dynamic partitions, df on /data or the data partition
    // is used as a rough heuristic. The check is skipped gracefully if neither
    // tool is available — the per-partition size validation already ensures each
    // partition can hold its individual image.
    script.push_str(&format!(
        r#"# ── Step {free_space_step}/{total_steps}: Pre-flash free space check ──────────────────
ui_print "> Checking available storage space..."

# ── TOTAL_FLASH_SIZE vs available space ──
# TOTAL_FLASH_SIZE is the sum of all partition uncompressed sizes, computed
# during the build and embedded in flash_info.txt + the script header.
# It represents the minimum storage capacity needed to flash this bundle.
# If available space is less than this, the flash will almost certainly fail.
#
# This is a SUPPLEMENTARY check — the per-partition size validation in the
# previous step already ensures each partition can hold its individual image.
# This check catches scenarios where:
#   1. The data partition is nearly full and can't hold system/vendor/etc.
#   2. The super partition doesn't have enough free space for dynamic resize.
#   3. The overall device storage is critically low.
#
# The check is skipped gracefully if df/lptools free is unavailable — some
# minimal recovery builds lack these tools.
FLASH_SPACE_CHECK=0

# ── Method 1: lptools free (dynamic partitions) ──
# This is the most reliable method for devices with dynamic partitions.
# lptools free reports actual available bytes in the super partition metadata.
if [ "$HAS_LPTOOLS" = "1" ] && [ -n "$RESIZE_NEEDED" ]; then
    LP_FREE=$(lptools free 2>/dev/null | grep -o 'Free space: [0-9]*' | awk '{{print $3}}')
    if [ -n "$LP_FREE" ]; then
        # For dynamic partitions, we need: existing partition sizes + resize delta.
        # But TOTAL_FLASH_SIZE includes ALL partitions (dynamic + non-dynamic).
        # The resize step already verified LP_FREE ≥ RESIZE_TOTAL.
        # Here we just report informational comparison.
        ui_print "  Super free space: $(( LP_FREE / 1048576 )) MB"
        ui_print "  Total flash size: $(( TOTAL_FLASH_SIZE / 1048576 )) MB"
        if [ "$LP_FREE" -lt "$RESIZE_TOTAL" ]; then
            # Already caught by resize step, but double-check for safety
            ui_print "✗ Error: Insufficient free space in super partition."
            ui_print "  Need: $(( RESIZE_TOTAL / 1048576 )) MB, available: $(( LP_FREE / 1048576 )) MB"
            exit 1
        fi
        FLASH_SPACE_CHECK=1
    fi
fi

# ── Method 2: df on data partition (non-dynamic / general check) ──
# For non-dynamic partitions, we check free space on the data partition.
# This is a heuristic — the actual flash writes to individual block devices
# (system, vendor, boot, etc.), not /data. But if /data is critically low,
# the device likely can't handle the flash either (recovery needs /data for
# temporary files, and a nearly-full device may have other issues).
# We also check the partition that each image will be written to.
if [ "$FLASH_SPACE_CHECK" = "0" ] && command -v df >/dev/null 2>&1; then
    # Check /data free space as a general health indicator
    DATA_FREE_KB=$(df /data 2>/dev/null | tail -1 | awk '{{print $4}}')
    if [ -n "$DATA_FREE_KB" ] && [ "$DATA_FREE_KB" -gt 0 ] 2>/dev/null; then
        DATA_FREE_BYTES=$(( DATA_FREE_KB * 1024 ))
        ui_print "  /data free space: $(( DATA_FREE_BYTES / 1048576 )) MB"
        ui_print "  Total flash size: $(( TOTAL_FLASH_SIZE / 1048576 )) MB"
        # /data doesn't need to hold the full TOTAL_FLASH_SIZE (partitions
        # are flashed directly to block devices), but if /data has < 100 MB
        # free, the device is critically low and flashing may fail.
        MIN_DATA_FREE=$(( 100 * 1048576 ))  # 100 MB minimum
        if [ "$DATA_FREE_BYTES" -lt "$MIN_DATA_FREE" ]; then
            ui_print "! WARNING: /data has less than 100 MB free — flash may fail."
            ui_print "!  Free up space on /data before flashing."
            # This is a WARNING, not an ABORT — the per-partition size check
            # is the authoritative gate. Low /data may just mean the device
            # is full, but recovery can still flash directly to block devices.
        fi
        FLASH_SPACE_CHECK=1
    fi
fi

# ── Method 3: Sum per-partition available space ──
# For each non-dynamic partition, check that PART_SIZE ≥ UNC_SIZE.
# This was already done in the validation step, but we report the total
# here for informational purposes.
if [ "$FLASH_SPACE_CHECK" = "0" ]; then
    ui_print "  Note: Cannot check free space (df/lptools unavailable)."
    ui_print "  Per-partition size validation passed — proceeding with flash."
    ui_print "  Total flash size: $(( TOTAL_FLASH_SIZE / 1048576 )) MB"
fi

ui_print "  ✓ Free space check complete"

# ── Flash each partition ──"#,
        free_space_step = free_space_step,
        total_steps = total_steps,
    ));

    // ── Flash each partition ──
    // BUG FIX (NEW-G): Generate algorithm-specific fallback decompressors instead
    // of hardcoding gzip-only fallbacks. Previously, all compression types used
    // gzip fallbacks, which guaranteed failure for bzip2/xz/brotli partitions.
    let fallback_decompressors = match compress_id {
        0 => "",  // ALG_NONE — no decompression, no fallback needed
        1 => r#""gzip -dc" "gunzip -c" "zcat" "busybox gzip -dc""#,
        2 => r#""bzip2 -dc" "bzcat" "busybox bzip2 -dc""#,
        3 => r#""xz -T0 -dc" "xz -dc" "xzcat" "busybox xz -dc""#,
        4 => "",  // brotli removed — no fallback available
        5 => r#""lz4 -dc" "lz4 -d" "busybox lz4 -dc""#,
        6 => r#""zstd -dc" "zstdcat" "busybox zstd -dc""#,
        _ => "",
    };
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

    # Bug NEW-A/B fix (flash step): guard empty variables before arithmetic.
    # The verify step already has these guards, but the flash step was missing
    # them. Empty POFFSET/PCSIZE/PSIZE in $(( )) causes syntax errors in
    # POSIX sh (dash) — the Android recovery default shell.
    if [ -z "$POFFSET" ] || [ -z "$PCSIZE" ] || [ -z "$PSIZE" ]; then
        ui_print "! ABORT: Missing partition metadata for $PNAME"
        ui_print "!  POFFSET=${{POFFSET:-(empty)}} PCSIZE=${{PCSIZE:-(empty)}} PSIZE=${{PSIZE:-(empty)}}"
        ui_print "!  Bundle is corrupt or was built with incompatible OTAku version."
        exit 1
    fi

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
    # Instead, we write WITHOUT conv= flags; a non-zero dd exit is judged
    # by dd_failure_verdict (dd's own byte-count / post-flash hash).
    EXTRACT_SKIP=$(( ZIP_DATA_OFFSET + DATA_OFFSET + POFFSET ))
    SKIP_BLOCKS=$(( EXTRACT_SKIP / 4096 ))
    SKIP_REMAINDER=$(( EXTRACT_SKIP % 4096 ))
    READ_COUNT=$(( (PCSIZE + 4095) / 4096 ))

    # ── dd_if_bundle: read PCSIZE bytes from BUNDLE at EXTRACT_SKIP ──
    # BUG FIX: When ZIP_DATA_OFFSET is not 4096-aligned (e.g. 59 bytes for
    # ZIP64 local file header), the old code used dd bs=4096 skip=$SKIP_BLOCKS
    # which truncates the remainder — reading from the WRONG offset. This caused
    # compressed data hash mismatches and wrong data being flashed.
    # Fix: read the full blocks, then skip the remainder bytes using tail -c.
    # This is efficient (bs=4096 for bulk read) and correct (tail handles remainder).
    dd_if_bundle() {{
        if [ "$SKIP_REMAINDER" -gt 0 ]; then
            dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$(( READ_COUNT + 1 )) 2>/dev/null | \
                tail -c +$(( SKIP_REMAINDER + 1 ))
        else
            dd if="$BUNDLE" bs=4096 skip=$SKIP_BLOCKS count=$READ_COUNT 2>/dev/null
        fi
    }}

    # ── OPTIMIZATION: Replace head -c with dd-based exact byte count ──
    # head -c $PCSIZE reads 1 byte at a time via read() syscall — extremely
    # slow for large payloads (e.g. 5120 MB = 5.12 billion syscalls).
    # Replace with dd bs=4096 which reads in 4KB blocks (8x-16x faster).
    #
    # head -c was needed to strip trailing alignment padding before the
    # decompressor. We now compute the exact block count and use dd to
    # read only the compressed data (PCSIZE bytes) without the padding.
    #
    # For the common case where dd_if_bundle already reads the exact count
    # (SKIP_REMAINDER=0 and READ_COUNT*4096 == PCSIZE), no truncation is
    # needed at all — dd already provides the exact bytes.
    #
    # When truncation IS needed (alignment padding present), we use
    # dd bs=1M count=... instead of head -c for the final trim.
    PCSIZE_BLOCKS=$(( PCSIZE / 4096 ))
    PCSIZE_REMAINDER=$(( PCSIZE % 4096 ))
    # If dd_if_bundle reads exactly PCSIZE bytes (no padding), skip trim.
    NEED_TRIM=0
    if [ "$SKIP_REMAINDER" -gt 0 ] || [ "$PCSIZE_REMAINDER" -ne 0 ]; then
        NEED_TRIM=1
    fi

    # ── Trim pipeline: replaces head -c $PCSIZE ──
    # When NEED_TRIM=0, dd_if_bundle already outputs exactly PCSIZE bytes.
    # When NEED_TRIM=1, we need to strip trailing padding. Instead of
    # head -c (1-byte-at-a-time), use dd with computed block count.
$HARUKA_PARSER_CHANGE_LINE    # dd bs=4096 count=$PCSIZE_BLOCKS reads the full blocks, then if
    # PCSIZE_REMAINDER > 0 we append the remaining bytes with a second dd.
    # This avoids head -c entirely — all reads are in 4KB blocks.
    trim_pipe() {{
        if [ "$NEED_TRIM" = "0" ]; then
            # No trim needed — pass through (dd_if_bundle is exact)
            cat
        else
            # Trim to exactly PCSIZE bytes using dd (block-aligned reads).
            # dd bs=4096 count=N is much faster than head -c (bulk reads).
            if [ "$PCSIZE_REMAINDER" -eq 0 ]; then
                dd bs=4096 count=$PCSIZE_BLOCKS 2>/dev/null
            else
                # Read full blocks + remainder: use dd bs=1 for the last
                # partial block. This is still faster than head -c for
                # the bulk (bs=4096) — only the last <4096 bytes use bs=1.
                dd bs=4096 count=$PCSIZE_BLOCKS 2>/dev/null
                dd bs=1 count=$PCSIZE_REMAINDER 2>/dev/null
            fi
        fi
    }}

    # ── F2 policy: dd failure verdict (proof, never proxy) ──
    # dd exited nonzero. The old excuse compared the PARTITION's total size
    # (blockdev --getsize64) against the expected image size — but capacity
    # says nothing about how many bytes dd actually wrote, so a REAL write
    # failure (EIO/ENOSPC/EINVAL) was excused as "busybox ftruncate — data
    # OK". With SKIP_VERIFY=1 that meant a silent brick (T25 finding F2).
    #
    # New policy:
    #   1. dd's own stderr byte-count line ("N bytes ... copied") >= expected
    #      → busybox ftruncate quirk on block devices: full write PROVEN.
    #   2. Verification enabled → defer to the Step-C post-flash hash, which
    #      reads the data back and compares against the embedded SHA-256.
    #   3. SKIP_VERIFY=1 + no byte-count proof → ABORT (fail-stop).
    # Args: $1 = dd exit status, $2 = dd stderr file, $3 = partition name,
    #       $4 = expected (uncompressed) size in bytes.
    dd_failure_verdict() {{
        DF_STATUS=$1; DF_ERR=$2; DF_NAME=$3; DF_EXPECTED=$4
        # busybox/toybox/GNU dd all print "N bytes (...) copied" to stderr.
        # tr -d ' bytes' strips the unit letters from the matched text.
        DF_COPIED=$(grep -o '^[0-9][0-9]* bytes' "$DF_ERR" 2>/dev/null | head -1 | tr -d ' bytes')
        if [ -n "$DF_COPIED" ] && [ "$DF_COPIED" -ge "$DF_EXPECTED" ]; then
            ui_print "  Note: dd status=$DF_STATUS (busybox ftruncate quirk — $DF_COPIED bytes fully written, data OK)"
            return 0
        fi
        if [ "$SKIP_VERIFY" != "1" ]; then
            ui_print "  ! dd status=$DF_STATUS (copied ${{DF_COPIED:-0}} of $DF_EXPECTED bytes) — verdict deferred to post-flash hash verify"
            return 0
        fi
        ui_print "! ABORT: dd write failed for $DF_NAME (status=$DF_STATUS, copied ${{DF_COPIED:-0}} of $DF_EXPECTED bytes)"
        ui_print "!  dd stderr: $(head -3 "$DF_ERR" 2>/dev/null | tr '\n' ' ')"
        return 1
    }}

    # ── Pre-flash compressed-data hash verification (streaming) ──
    # Compute SHA-256 of the compressed partition data and compare to the
    # expected hash stored in PART_i_COMP_HASH. This catches bundle
    # corruption (MTP transfer errors, tmpfs issues, ZIP CRC errors) BEFORE
    # we touch any block device — preventing partial writes that would
    # leave the partition in a broken state.
    #
    # PERFORMANCE: When PCOMP_HASH is set AND FIFO write is available,
    # we COMBINE hash verification with the decompression pipeline using
    # dual FIFOs + tee. This reads the compressed data ONCE instead of
    # TWICE — for a 5120 MB compressed system partition, this saves
    # ~5120 MB of redundant I/O, reducing flash time by ~30-40%.
    #
    # Dual-FIFO pipeline:
    #   dd_if_bundle | head -c $PCSIZE → tee $HASH_FIFO → $DECOMP > $WRITE_FIFO
    #                                           ↓
    #                              sha256sum (bg) → hash file
    #
    # If FIFO is not available (very rare), fall back to separate hash
    # pass (old behavior — reads data twice).
    #
    # Backward compat: old bundles built before this feature don't have
    # PART_i_COMP_HASH (empty string) — skip the check entirely.
    HASH_FIFO="/tmp/ddpart_${{i}}_hash.fifo"
    HASH_FILE="/tmp/ddpart_${{i}}_hash.val"
    rm -f "$HASH_FIFO" "$HASH_FILE"
    HASH_VERIFIED=0

    if [ -n "$PCOMP_HASH" ]; then
        ui_print "  Verifying compressed data integrity..."
        # Try streaming dual-FIFO path (reads data once, hashes while decompressing)
        if mkfifo "$HASH_FIFO" 2>/dev/null; then
            # Start background sha256sum on the hash FIFO
            sha256sum < "$HASH_FIFO" > "$HASH_FILE" 2>/dev/null &
            HASH_PID=$!
            HASH_VERIFIED=1
            # The tee + decompression happens in the write block below.
            # We set STREAMING_HASH=1 so the write pipeline uses tee.
            STREAMING_HASH=1
        else
            # FIFO unavailable — fall back to separate hash pass (reads data twice)
            COMP_HASH_ACTUAL=$(dd_if_bundle | trim_pipe | sha256sum 2>/dev/null | awk '{{print $1}}')
            if [ -z "$COMP_HASH_ACTUAL" ]; then
                ui_print "! ABORT: Cannot compute compressed data hash for $PNAME"
                ui_print "!  Bundle may be unreadable or sha256sum not available."
                ui_print "!  Bundle size: $BUNDLE_SIZE bytes"
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
                ui_print "!  Bundle size: $BUNDLE_SIZE bytes"
                exit 1
            fi
            ui_print "  ✓ Hash verified"
            STREAMING_HASH=0
        fi
    else
        STREAMING_HASH=0
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
        # OPTIMIZATION: Replace head -c $PCSIZE with trim_pipe.
        # trim_pipe uses dd bs=4096 (bulk reads) instead of head -c
        # (1-byte-at-a-time). For NEED_TRIM=0 (common case), it's a
        # no-op (cat) — zero overhead. For NEED_TRIM=1, it reads in
        # 4KB blocks — 8x-16x faster than head -c for large payloads.
        #
        # PERFORMANCE (streaming hash): When STREAMING_HASH=1, we pipe
        # through `tee $HASH_FIFO` so sha256sum (started above) hashes
        # the compressed data IN PARALLEL with decompression. This reads
        # compressed data ONCE instead of TWICE — ~30-40% faster for
        # large partitions (e.g. 5120 MB system).
        if [ "$STREAMING_HASH" = "1" ]; then
            dd_if_bundle | \
                trim_pipe | \
                tee "$HASH_FIFO" 2>/dev/null | \
                $DECOMP_PIPE > "$TMP_FIFO" 2>"$GZIP_ERR" &
        else
            dd_if_bundle | \
                trim_pipe | \
                $DECOMP_PIPE > "$TMP_FIFO" 2>"$GZIP_ERR" &
        fi
        DECOMP_PID=$!

        # ── OPTIMIZATION: O_DIRECT for block device writes ──
        # oflag=direct bypasses the Linux page cache for writes to block
        # devices (eMMC/UFS). This avoids double-buffering (kernel copies
        # data to page cache, then writes to flash), reducing memory
        # pressure and improving write throughput by 10-30% on eMMC.
        #
        # Requirements:
        #   - Target must be a block device (test -b)
        #   - Write size must be sector-aligned (bs=4096 minimum)
        #   - dd must support oflag=direct (busybox 1.33+ or GNU coreutils)
        #
        # For non-block devices (regular files, tmpfs), skip O_DIRECT —
        # it's invalid and would cause EINVAL errors.
        #
        # BUG FIX: Probe O_DIRECT on the ACTUAL target device, not /dev/null.
        # The old probe `dd oflag=direct if=/dev/zero of=/dev/null` always
        # succeeds because /dev/null accepts any flags — it's a no-op write.
        # This gave false positives on devices where the underlying block
        # device doesn't support O_DIRECT (e.g., some dm-linear targets
        # on older kernels, or F2FS on certain eMMC chips).
        #
        # New probe: write 1 sector to the target with oflag=direct, then
        # immediately read it back. If the write succeeds, O_DIRECT works.
        # If it fails (EINVAL/EOPNOTSUPP), fall back to buffered writes.
        # The 1-sector write is harmless — we're about to overwrite the
        # entire partition anyway, and 4096 bytes is a single flash page.
        # F2: capture dd's stderr — its byte-count line ("N bytes ... copied")
        # is the only honest proof of how many bytes reached the device.
        DD_ERR="/tmp/dderr_$$_${{i}}"
        rm -f "$DD_ERR"
        DD_OFLAG=""
        if [ -b "$PTARGET" ]; then
            # Probe O_DIRECT on the actual target device.
            # Write 4096 bytes (1 sector) with oflag=direct — if it fails,
            # the device doesn't support O_DIRECT and we fall back.
            # Use a temp file for the probe input (dd if=pipe doesn't work
            # reliably with oflag=direct on all busybox builds).
            _OD_PROBE="/tmp/od_probe_$$_${{i}}"
            dd if=/dev/zero of="$_OD_PROBE" bs=4096 count=1 2>/dev/null
            if dd oflag=direct if="$_OD_PROBE" of="$PTARGET" bs=4096 count=1 conv=notrunc 2>/dev/null; then
                DD_OFLAG="oflag=direct"
            fi
            rm -f "$_OD_PROBE"
        fi

        # No conv= flags — busybox dd ftruncate() on dm-linear is a false-failure
        # (the false-failure itself is handled by dd_failure_verdict below)
        if [ -n "$DD_OFLAG" ]; then
            dd of="$PTARGET" bs=1048576 if="$TMP_FIFO" $DD_OFLAG 2>"$DD_ERR"
        else
            dd of="$PTARGET" bs=1048576 if="$TMP_FIFO" 2>"$DD_ERR"
        fi
        DD_STATUS=$?

        wait $DECOMP_PID 2>/dev/null
        DECOMP_STATUS=$?

        rm -f "$TMP_FIFO"

        # ── Check streaming hash result ──
        # If we used the dual-FIFO streaming path, now verify the hash.
        # The background sha256sum has been reading from HASH_FIFO via tee.
        # We must wait for it and check the result before proceeding.
        if [ "$STREAMING_HASH" = "1" ] && [ "$HASH_VERIFIED" = "1" ]; then
            # Close hash FIFO and wait for sha256sum to finish
            rm -f "$HASH_FIFO"
            wait $HASH_PID 2>/dev/null
            if [ -s "$HASH_FILE" ]; then
                COMP_HASH_ACTUAL=$(cut -d' ' -f1 < "$HASH_FILE")
            else
                COMP_HASH_ACTUAL=""
            fi
            rm -f "$HASH_FILE"
            HASH_VERIFIED=0
            if [ -z "$COMP_HASH_ACTUAL" ]; then
                ui_print "! ABORT: Cannot compute compressed data hash for $PNAME"
                ui_print "!  Bundle may be unreadable or sha256sum not available."
                ui_print "!  Bundle size: $BUNDLE_SIZE bytes"
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
                ui_print "!  Bundle size: $BUNDLE_SIZE bytes"
                exit 1
            fi
            ui_print "  ✓ Hash verified"
        fi

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
            ui_print "  Decompressor: $DECOMP_PIPE (status=$DECOMP_STATUS)"
            ui_print "  Details: $GZIP_ERR_MSG"
            ui_print "  Bundle: $BUNDLE_SIZE bytes | Compressed: $PCSIZE | Uncompressed: $PSIZE"

            # If compressed-data hash was verified above (PCOMP_HASH non-empty),
            # the compressed data IS intact — the issue is with the decompressor
            # itself (e.g., busybox gzip quirk). Try fallback decompressors.
            if [ -n "$PCOMP_HASH" ]; then
                ui_print "  → Hash OK, trying fallback decompressors..."
                FALLBACK_OK=0
                for FB_DECOMP in {fallback_decompressors}; do
                    ui_print "  → Trying: $FB_DECOMP..."
                    TMP_FIFO2="/tmp/ddpart_${{i}}.fifo2"
                    rm -f "$TMP_FIFO2" "$GZIP_ERR"
                    mkfifo "$TMP_FIFO2" 2>/dev/null
                    if [ $? -ne 0 ]; then
                        ui_print "    FIFO creation failed — skipping $FB_DECOMP"
                        continue
                    fi
                    dd_if_bundle | \
                        trim_pipe | \
                        $FB_DECOMP > "$TMP_FIFO2" 2>"$GZIP_ERR" &
                    FB_PID=$!
                    dd of="$PTARGET" bs=1048576 if="$TMP_FIFO2" $DD_OFLAG 2>"$DD_ERR"
                    FB_DD_STATUS=$?
                    wait $FB_PID 2>/dev/null
                    FB_DECOMP_STATUS=$?
                    rm -f "$TMP_FIFO2"
                    if [ $FB_DECOMP_STATUS -eq 0 ]; then
                        ui_print "  ✓ $FB_DECOMP succeeded!"
                        FALLBACK_OK=1
                        # Check dd write status — F2 policy (proof, never proxy)
                        if [ $FB_DD_STATUS -ne 0 ]; then
                            if ! dd_failure_verdict "$FB_DD_STATUS" "$DD_ERR" "$PNAME" "$PSIZE"; then
                                rm -f "$GZIP_ERR" "$DD_ERR"
                                exit 1
                            fi
                        fi
                        rm -f "$GZIP_ERR" "$DD_ERR"
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
                    ui_print "  Hint: Rebuild with --compress lz4 (fastest) or --compress gzip"
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
            # even when all data was written — but that must be PROVEN, not
            # assumed. F2 policy: dd's own byte-count, or the post-flash hash
            # (Step C). Partition capacity is not evidence of written bytes.
            if [ $DD_STATUS -ne 0 ]; then
                if ! dd_failure_verdict "$DD_STATUS" "$DD_ERR" "$PNAME" "$PSIZE"; then
                    rm -f "$DD_ERR"
                    exit 1
                fi
            fi
            rm -f "$DD_ERR"
        fi
    else
        # FIFO not available (very rare) — fall back to direct 3-pipeline.
        # We lose decompressor error detection (sh $? = last pipe stage only),
        # but this avoids tmpfs exhaustion by never writing a temp file.
        # trim_pipe replaces head -c (dd-based block reads instead of 1-byte-at-a-time).
        # O_DIRECT: apply if DD_OFLAG was set by the probe above. In the
        # 3-pipeline, dd is the last stage so we can add oflag=direct.
        GZIP_ERR="/tmp/ddpart_${{i}}.err"
        DD_ERR="/tmp/dderr_$$_${{i}}"
        rm -f "$GZIP_ERR" "$DD_ERR"
        # F5 fix: reset DD_OFLAG unconditionally for THIS partition. A stale
        # "oflag=direct" from a previous partition (whose device supported
        # O_DIRECT) must not leak here — this partition's device may reject
        # it with EINVAL, and the old `-z "$DD_OFLAG"` guard skipped the
        # re-probe precisely when the flag was stale.
        DD_OFLAG=""
        if [ -b "$PTARGET" ]; then
            # Re-probe O_DIRECT for this path. Same probe as above.
            _OD_PROBE="/tmp/od_probe_$$_${{i}}_nf"
            dd if=/dev/zero of="$_OD_PROBE" bs=4096 count=1 2>/dev/null
            if dd oflag=direct if="$_OD_PROBE" of="$PTARGET" bs=4096 count=1 conv=notrunc 2>/dev/null; then
                DD_OFLAG="oflag=direct"
            fi
            rm -f "$_OD_PROBE"
        fi
        dd_if_bundle | \
            trim_pipe | \
            $DECOMP_PIPE 2>"$GZIP_ERR" | \
            dd of="$PTARGET" bs=1048576 $DD_OFLAG 2>"$DD_ERR"
        DD_STATUS=$?

        if [ $DD_STATUS -ne 0 ]; then
            GZIP_ERR_MSG=$(cat "$GZIP_ERR" 2>/dev/null | tr -d '\r' | head -1)
            rm -f "$GZIP_ERR"
            # F2 policy: proof via dd byte-count or post-flash hash — never
            # via partition size (the old silent-brick path, T25 finding F2).
            if ! dd_failure_verdict "$DD_STATUS" "$DD_ERR" "$PNAME" "$PSIZE"; then
                ui_print "!  Decompressor stderr: $GZIP_ERR_MSG"
                rm -f "$DD_ERR"
                exit 1
            fi
            rm -f "$DD_ERR"
        else
            rm -f "$GZIP_ERR" "$DD_ERR"
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
            # F4 fix: bootctl set-active-boot-slot takes a NUMBER (0=a, 1=b).
            # Passing the letter made strtoul() parse it as 0 = slot A — the
            # OPPOSITE of the intended slot on every _b device. fastboot
            # set_active still expects the letter, so keep it as-is there.
            SLOT_NUM=$(echo "$SLOT_LETTER" | tr 'ab' '01')
            bootctl set-active-boot-slot $SLOT_NUM 2>/dev/null || \
                fastboot set_active $SLOT_LETTER 2>/dev/null || true
        fi
    else
        ui_print "  ✓ Slot: $TARGET_SLOT"
    fi
fi

# ── Done ────────────────────────────────────────────────────
# Clean up extracted bundle (only present in fallback mode —
# direct-read mode has BUNDLE=$ZIPFILE, not /tmp/otaku.bin).
if [ "$BUNDLE" = "/tmp/otaku.bin" ] && [ -f "$BUNDLE" ]; then
    rm -f "$BUNDLE"
fi
ui_print "======================================"
ui_print "  Flash complete — $NUM_PARTS partition(s)"
ui_print "======================================"
exit 0
"#,
        flash_step_offset = flash_step_offset,
        num_parts_minus_1 = if num_parts > 0 { num_parts - 1 } else { 0 },
        total_steps = total_steps,
        verify_block = verify_block.trim(),
        fallback_decompressors = fallback_decompressors,
    ));

    script
}
