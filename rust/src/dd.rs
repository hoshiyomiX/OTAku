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
use std::io::Write;
use std::path::Path;

use crate::compression::{compress_id, hash_and_compress_file, is_alg, ALG_NONE};

// ---------------------------------------------------------------------------
//  Progress sidecar file
// ---------------------------------------------------------------------------

/// Write a progress sidecar file next to the output ZIP.
///
/// The progress file is `<output_path>.progress` and contains a single JSON line:
///   `{"current":1,"total":3,"name":"boot","phase":"compressing","bytes_written":12345678}`
///
/// Kotlin polls this file every 500ms to drive live progress UI.
/// The file is deleted when the build completes (success or failure).
fn write_progress_file(output_path: &str, current: usize, total: usize, name: &str, phase: &str, bytes_written: u64) {
    let progress_path = format!("{}.progress", output_path);
    let content = serde_json::json!({
        "current": current,
        "total": total,
        "name": name,
        "phase": phase,
        "bytes_written": bytes_written
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
const BANNER: &str = r#"    ____              __  __
   / __ \____  ____  / /_/ /_  ____
  / / / / __ \/ __ \/ __/ __ \/ __ \
 / /_/ / /_/ / / / / /_/ / / / /_/ /
 \____/\____/_/ /_/\__/_/ /_/\____/  v3"#;

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
    let decomp_ext = decomp_ext_for_id(compress_id);

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
    let decomp_step = 1;
    let integrity_step = 2;
    let slot_step = if has_device { 4 } else { 3 };
    let validation_step = if has_device { 5 } else { 4 };
    let flash_step_offset = if has_device { 6 } else { 5 };
    let total_steps = num_parts + flash_step_offset;

    // Device check step
    let device_check_step = integrity_step + 1; // = 3
    let device_check_block = if has_device {
        format!(
            r#"
# ── Step {device_check_step}: Device compatibility ──────────────────
TARGET_DEVICE="{device}"
CURRENT_DEVICE=$(getprop ro.product.device 2>/dev/null || getprop ro.build.product 2>/dev/null)

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
        ui_print "  Current  : $CURRENT_DEVICE"
        ui_print ""
        ui_print "  Flashing on wrong device may BRICK it."
        ui_print "  Press Power to continue, Vol- to abort."
        ui_print ""
        choose -t 30 "Continue?" "Yes" "No"
        if [ $? -ne 0 ]; then
            ui_print "! ABORT: User cancelled (device mismatch)"
            exit 1
        fi
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

    // Verification block
    let verify_block = if skip_verify {
        r#"ui_print "  Verification skipped"
"#
        .to_string()
    } else {
        r#"ui_print "  Verifying $PNAME..."
VERIFY_HASH=""
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
# {banner}
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

BUNDLE="/tmp/otaku.bin"
{part_vars}NUM_PARTS={num_parts}
COMPRESS_ID={compress_id}

ui_print ""
{banner_trimmed}
ui_print ""
"#,
        banner = BANNER,
        header_info = header_info,
        part_vars = part_vars,
        num_parts = num_parts,
        compress_id = compress_id,
        banner_trimmed = BANNER.trim(),
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

BUNDLE_EXTRACT_SIZE=$(wc -c < "$BUNDLE")
ui_print "  Extracted: $BUNDLE ($(( BUNDLE_EXTRACT_SIZE / 1048576 )) MB)"
ui_print ""
"#,
        extract_step = extract_step,
        total_steps = total_steps,
    ));

    // ── Step 1: Decompressor availability ──
    script.push_str(&format!(
        r#"# ── Step {decomp_step}/{total_steps}: Decompressor availability ────────────
ui_print "[Step {decomp_step}/{total_steps}] Decompressor availability..."

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
ui_print ""
"#,
        decomp_step = decomp_step,
        total_steps = total_steps,
        decomp_cmd = decomp_cmd,
    ));

    // ── Step 2: Bundle integrity ──
    script.push_str(&format!(
        r#"# ── Step {integrity_step}/{total_steps}: Bundle integrity ───────────────────
ui_print "[Step {integrity_step}/{total_steps}] Bundle integrity..."

if [ ! -f "$BUNDLE" ]; then
    ui_print "! ABORT: $BUNDLE not found"
    exit 1
fi

BUNDLE_SIZE=$(wc -c < "$BUNDLE")

HDR_MAGIC=$(od -A n -t x1 -N 4 "$BUNDLE" | tr -d ' \\n')
if [ "$HDR_MAGIC" != "44444255" ]; then
    ui_print "! ABORT: Invalid bundle magic (expected DDBU, got $(echo $HDR_MAGIC | sed 's/\(..\)/\\x\\1/g'))"
    exit 1
fi

HDR_VERSION=$(od -A n -t u2 -j 4 -N 2 "$BUNDLE" | tr -d ' ')
HDR_COMPRESS=$(od -A n -t u1 -j 6 -N 1 "$BUNDLE" | tr -d ' ')
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

if [ "$HDR_HDR_SIZE" -lt 64 ] || [ "$HDR_HDR_SIZE" -gt "$BUNDLE_SIZE" ]; then
    ui_print "! ABORT: Invalid header size: $HDR_HDR_SIZE"
    exit 1
fi

DATA_OFFSET=$(( HDR_HDR_SIZE ))
REMAINDER=$(( DATA_OFFSET % 4096 ))
if [ "$REMAINDER" -ne 0 ]; then
    DATA_OFFSET=$(( DATA_OFFSET + 4096 - REMAINDER ))
fi

ui_print "  Version=$HDR_VERSION Compress=$HDR_COMPRESS Parts=$HDR_NUM_PARTS"
ui_print "  Header=$HDR_HDR_SIZE DataOffset=$DATA_OFFSET"
ui_print ""
"#,
        integrity_step = integrity_step,
        total_steps = total_steps,
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

is_shared_partition() {{
    case "$1" in
        modem|bluetooth|dsp|cnss|fvb|persist|keystore|provision) return 0 ;;
        *) return 1 ;;
    esac
}}

resolve_target() {{
    local name="$1"
    if is_shared_partition "$name"; then
        echo "/dev/block/by-name/$name"
    elif [ -n "$TARGET_SLOT" ]; then
        echo "/dev/block/by-name/${{name}}${{TARGET_SLOT}}"
    else
        echo "/dev/block/by-name/$name"
    fi
}}
"#,
        slot_step = slot_step,
        total_steps = total_steps,
    ));

    // ── Partition validation ──
    script.push_str(&format!(
        r#"# ── Step {validation_step}/{total_steps}: Partition validation ─────────────────────
ui_print "[Step {validation_step}/{total_steps}] Partition validation..."

validate_target() {{
    local target="$1"
    local min_size="$2"
    local name="$3"

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
        sleep 1
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
        ui_print "! ABORT: Partition $name too small: $PART_SIZE < $min_size"
        return 1
    fi

    ui_print "  $name ($target): $(( PART_SIZE / 1048576 )) MB [OK]"
    return 0
}}

for i in $(seq 0 $(( NUM_PARTS - 1 ))); do
    eval "PNAME=\\$PART_${{i}}_NAME"
    eval "PSIZE=\\$PART_${{i}}_UNC_SIZE"
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

    // ── Flash each partition ──
    script.push_str(&format!(
        r#"# ── Step {flash_step_offset}+{num_parts_minus_1}/{total_steps}: Flash each partition ────────────────────
for i in $(seq 0 $(( NUM_PARTS - 1 ))); do
    eval "PNAME=\\$PART_${{i}}_NAME"
    eval "PSIZE=\\$PART_${{i}}_UNC_SIZE"
    eval "PHASH=\\$PART_${{i}}_HASH"
    eval "PCSIZE=\\$PART_${{i}}_COMP_SIZE"
    eval "POFFSET=\\$PART_${{i}}_DATA_OFFSET"

    STEP_NUM=$(( i + {flash_step_offset} ))

    ui_print "[Step $STEP_NUM/{total_steps}] Flashing $PNAME ($(( PSIZE / 1048576 )) MB)..."
    ui_print "  Compressed: $(( PCSIZE / 1048576 )) MB | Offset: $POFFSET"

    PTARGET=$(resolve_target "$PNAME")
    TMP_COMP="/tmp/ddpart_${{i}}{decomp_ext}"

    # Step A: Extract compressed data from bundle
    ui_print "  Extracting compressed data..."
    dd if="$BUNDLE" of="$TMP_COMP" bs=1 skip=$(( DATA_OFFSET + POFFSET )) count="$PCSIZE" 2>/dev/null
    if [ $? -ne 0 ]; then
        ui_print "! ABORT: Failed to extract compressed data for $PNAME"
        exit 1
    fi

    # Step B: Pipe decompress directly to dd
    ui_print "  Writing to $PTARGET..."
    $DECOMP_CMD -d < "$TMP_COMP" | dd of="$PTARGET" bs=4096 conv=fsync 2>/dev/null
    DD_STATUS=$?
    rm -f "$TMP_COMP"

    if [ $DD_STATUS -ne 0 ]; then
        ui_print "! ABORT: dd write failed for $PNAME (status=$DD_STATUS)"
        exit 1
    fi

    # Step C: Post-verify (conditional)
    {verify_block}
    ui_print ""
done

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
        decomp_ext = decomp_ext,
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

    lines.push("Renuked v3 — dd-based partition flasher".to_string());
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

        // ── Step 1: Build otaku.bin ──
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

        let header = build_header(compress_id_val, num_parts as u16);

        let mut partitions_meta: Vec<PartitionMeta> = Vec::new();
        let mut data_blobs: Vec<u8> = Vec::new();

        for (i, (name, path)) in images.iter().enumerate() {
            log::info!(
                "[{}/{}] Compressing {} ({})",
                i + 1,
                num_parts,
                name,
                path
            );

            // Write progress: starting this partition
            write_progress_file(output_path, i + 1, num_parts, name, "compressing", data_blobs.len() as u64);

            // Hash and compress the image in a single streaming pass
            let level_opt = if level > 0 { Some(level) } else { None };
            let (compressed, hash_hex) = hash_and_compress_file(path, &compress_name, level_opt)?;

            // Get uncompressed size
            let unc_size = std::fs::metadata(path)
                .map_err(|e| format!("Cannot stat {}: {}", path, e))?
                .len();

            let comp_size = compressed.len() as u64;
            let data_offset = data_blobs.len() as u64;

            partitions_meta.push(PartitionMeta {
                name: name.clone(),
                unc_size,
                hash_hex,
                comp_size,
                data_offset,
            });

            data_blobs.extend_from_slice(&compressed);
            drop(compressed); // free compressed data

            // Align data_blobs to ALIGN boundary
            let aligned = align_up(data_blobs.len(), ALIGN);
            if aligned > data_blobs.len() {
                let padding = aligned - data_blobs.len();
                data_blobs.extend_from_slice(&vec![0u8; padding]);
            }

            // Write progress: partition done
            write_progress_file(output_path, i + 1, num_parts, name, "compressed", data_blobs.len() as u64);

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

        // Combine header + data blobs
        let mut bundle_data = header;
        bundle_data.append(&mut data_blobs);
        let bundle_size = bundle_data.len() as u64;
        lines.push(format!("  Bundle size  : {}", human_size(bundle_size)));
        lines.push(String::new());

        // ── Step 2: Build flasher scripts ──
        lines.push("[2/3] Building flasher scripts".to_string());
        write_progress_file(output_path, num_parts, num_parts, "scripts", "building_scripts", bundle_size);

        let update_binary = build_update_script(
            num_parts,
            compress_id_val,
            &compress_name,
            &partitions_meta,
            device,
            skip_verify,
        );
        let updater_script = "#Mtk client script\n";
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
        write_progress_file(output_path, num_parts, num_parts, "writing_zip", "writing_zip", bundle_size);

        // Ensure output directory exists
        if let Some(parent) = Path::new(output_path).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create output dir: {}", e))?;
        }

        // Write otaku.bin to temp file first, then add to ZIP.
        // This avoids double-copying the bundle data in memory.
        let temp_dir = std::env::temp_dir();
        let bundle_tmp_path = temp_dir.join("otaku_build_tmp.bin");

        {
            let mut bundle_tmp =
                File::create(&bundle_tmp_path).map_err(|e| format!("Cannot create temp file: {}", e))?;
            bundle_tmp
                .write_all(&bundle_data)
                .map_err(|e| format!("Cannot write temp bundle: {}", e))?;
            bundle_tmp
                .flush()
                .map_err(|e| format!("Cannot flush temp bundle: {}", e))?;
        }
        drop(bundle_data); // free memory before ZIP creation

        // Create the output ZIP with ZIP_STORED (no compression — the data is already compressed)
        {
            let zip_file =
                File::create(output_path).map_err(|e| format!("Cannot create ZIP: {}", e))?;
            let mut zip = zip::ZipWriter::new(zip_file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            // Add otaku.bin
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
            hash_hex: "abcdef1234567890".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        }];
        let script = build_update_script(1, 1, "gzip", &meta, "", false);
        assert!(script.starts_with("#!/sbin/sh"));
        assert!(script.contains("PART_0_NAME=\"boot\""));
        assert!(script.contains("PART_0_HASH=\"abcdef1234567890\""));
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
            hash_hex: "deadbeef".to_string(),
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
            hash_hex: "abc".to_string(),
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
            hash_hex: "abcdef1234567890".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        }];
        let info = build_flash_info("gzip", 16781312, 1, &meta, "crosshatch", 6, false);
        assert!(info.contains("Renuked v3"));
        assert!(info.contains("gzip (level 6)"));
        assert!(info.contains("crosshatch"));
        assert!(info.contains("[boot]"));
        assert!(info.contains("abcdef1234567890"));
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
}
