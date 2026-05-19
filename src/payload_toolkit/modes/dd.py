"""
modes/dd.py — Generate a ddbundle-format flashable ZIP from partition images.

Called from Kotlin via Chaquopy:
    payload_toolkit.modes.dd.run(
        images={"odm_dlkm": "/path/to/odm_dlkm.img"},
        compress="gzip",
        output_path="/path/to/flashable_dd.zip",
        device="S666LN-OP",
    )

ddbundle format:
    ddbundle.bin  — DDBU header (4096 bytes, padded) + compressed partition data
                    Header: magic "DDBU" (4B) + version (u16) + compress_id (u16)
                          + num_parts (u16) + header_size (u16) + padding to 4096
                    Data:   each partition compressed, padded to 4096 alignment
    flash_info.txt — Human-readable metadata
    META-INF/com/google/android/update-binary — TWRP/OrangeFox flasher
    META-INF/com/google/android/updater-script — Stub ("#Mtk client script")

Compress IDs (stored in header + used by flasher):
    0 = none,  1 = gzip,  2 = bzip2,  3 = xz
"""

import hashlib
import io
import os
import struct
import tempfile
import time
import zipfile

from .. import _report_progress
from ..compression import compress

# ── Constants ────────────────────────────────────────────────────────────────

DDBUNDLE_MAGIC = b"DDBU"
DDBUNDLE_VERSION = 1
HEADER_SIZE = 4096          # Fixed header allocation (padded)
ALIGN = 4096

# Compress algorithm -> ddbundle numeric ID
COMPRESS_ID_MAP = {
    "none": 0,
    "gzip": 1,
    "bzip2": 2,
    "xz": 3,
}

# ddbundle numeric ID -> shell decompressor command
COMPRESS_CMD_MAP = {
    0: "cat",
    1: "gzip",
    2: "bzip2",
    3: "xz",
}

# ddbundle numeric ID -> file extension for temp files
COMPRESS_EXT_MAP = {
    0: ".raw",
    1: ".gz",
    2: ".bz2",
    3: ".xz",
}

# Slant ASCII art for "Renuked v3" — TWRP console banner
BANNER = r"""    ____              __  __
   / __ \____  ____  / /_/ /_  ____
  / / / / __ \/ __ \/ __/ __ \/ __ \
 / /_/ / /_/ / / / / /_/ / / / /_/ /
 \____/\____/_/ /_/\__/_/ /_/\____/  v3"""


# ── Helpers ──────────────────────────────────────────────────────────────────

def _align_up(offset, alignment=ALIGN):
    """Round *offset* up to the next multiple of *alignment*."""
    remainder = offset % alignment
    if remainder:
        return offset + alignment - remainder
    return offset


def _human_size(size_bytes):
    """Return a human-readable size string."""
    if size_bytes < 1024:
        return f"{size_bytes} bytes"
    if size_bytes < 1048576:
        return f"{size_bytes / 1024:.1f} KB"
    return f"{size_bytes / 1048576:.1f} MB"


def _build_header(compress_id, num_parts):
    """Build the 4096-byte ddbundle.bin header.

    Header layout (first 12 bytes, all little-endian):
        Offset  Size  Field
        0       4     Magic "DDBU"
        4       2     Version (uint16) = 1
        6       2     Compress ID (uint16): 0=none, 1=gzip, 2=bzip2, 3=xz
        8       2     Number of partitions (uint16)
        10      2     Header data size (uint16) = 4096
        12      4084  Zero padding (to reach 4096)
    """
    hdr = struct.pack(
        "<4sHHHH",
        DDBUNDLE_MAGIC,
        DDBUNDLE_VERSION,
        compress_id,
        num_parts,
        HEADER_SIZE,
    )
    # Pad to HEADER_SIZE
    hdr += b"\x00" * (HEADER_SIZE - len(hdr))
    return hdr


def _build_update_script(num_parts, compress_id, compress_name, partitions_meta, device=""):
    """Build the META-INF/com/google/android/update-binary shell script.

    partitions_meta: list of dicts with keys:
        name, unc_size, hash_hex, comp_size, data_offset
    device: device codename string (optional, for target validation)
    """
    # Build partition variable assignments
    part_vars = []
    for i, p in enumerate(partitions_meta):
        part_vars.append(
            f'PART_{i}_NAME="{p["name"]}"\n'
            f'PART_{i}_UNC_SIZE="{p["unc_size"]}"\n'
            f'PART_{i}_HASH="{p["hash_hex"]}"\n'
            f'PART_{i}_COMP_SIZE="{p["comp_size"]}"\n'
            f'PART_{i}_DATA_OFFSET="{p["data_offset"]}"'
        )

    decomp_cmd = COMPRESS_CMD_MAP.get(compress_id, "cat")
    decomp_ext = COMPRESS_EXT_MAP.get(compress_id, ".raw")

    # Device check step — only emitted when device is set
    device_check_block = ""
    if device:
        device_check_block = f'''
# ── Step 2: Device compatibility ──────────────────────────
TARGET_DEVICE="{device}"
CURRENT_DEVICE=$(getprop ro.product.device 2>/dev/null || getprop ro.build.product 2>/dev/null)

if [ -n "$TARGET_DEVICE" ]; then
    if [ "$CURRENT_DEVICE" != "$TARGET_DEVICE" ]; then
        ui_print ""
        ui_print "  WARNING: Device mismatch!"
        ui_print "  Expected : $TARGET_DEVICE"
        ui_print "  Current  : $CURRENT_DEVICE"
        ui_print ""
        ui_print "  Flashing on wrong device may BRICK it."
        ui_print "  Press Power to continue, Vol- to abort."
        ui_print ""
        # Wait for key press (Power = continue, Vol- = abort)
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
'''

    # Calculate step numbers dynamically based on whether device check is enabled
    # With device:    Step 0 (decompressor) + 1 (integrity) + 2 (device) + 3 (slot) + 4 (validation) + N flash
    # Without device: Step 0 (decompressor) + 1 (integrity) + 2 (slot) + 3 (validation) + N flash
    slot_step = 3 if device else 2
    validation_step = 4 if device else 3
    flash_step_offset = 5 if device else 4
    total_steps = num_parts + flash_step_offset

    # Header info line
    header_info_parts = ", ".join(p["name"] for p in partitions_meta)
    header_info = f"Partitions: {header_info_parts}"
    if device:
        header_info += f" | Device: {device}"
    header_info += f" | Compress: {compress_name}"

    script = f'''#!/sbin/sh
# {BANNER}
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

BUNDLE="/tmp/ddbundle.bin"
{chr(10).join(part_vars)}
NUM_PARTS={num_parts}
COMPRESS_ID={compress_id}

ui_print ""
{chr(10).join(line.strip() for line in BANNER.strip().splitlines())}
ui_print ""

# ── Step 0: Decompressor availability ──────────────────────
ui_print "[Step 0/{total_steps}] Decompressor availability..."

DECOMP_CMD=""
check_decompressor() {{
    local cmd="$1"
    # Method 1: standalone binary
    if which "$cmd" >/dev/null 2>&1; then
        DECOMP_CMD="$cmd"
        return 0
    fi
    # Method 2: busybox applet
    if busybox --list 2>/dev/null | grep -q "^${{cmd}}$"; then
        DECOMP_CMD="busybox $cmd"
        return 0
    fi
    # Method 3: toybox (some recoveries use toybox instead of busybox)
    if toybox --help >/dev/null 2>&1 && toybox "$cmd" --help >/dev/null 2>&1; then
        DECOMP_CMD="toybox $cmd"
        return 0
    fi
    # Method 4: check common alternative paths
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
    which gzip bzip2 xz 2>/dev/null || echo "  (none found)"
    busybox --list 2>/dev/null | head -5
    ui_print "! Rebuild bundle with a available compressor."
    ui_print "! Recommended: --compress gzip"
    exit 1
fi
ui_print "  Decompressor: $DECOMP_CMD"
ui_print ""

# ── Step 1: Bundle integrity ───────────────────────────────
ui_print "[Step 1/{total_steps}] Bundle integrity..."

if [ ! -f "$BUNDLE" ]; then
    ui_print "! ABORT: $BUNDLE not found"
    exit 1
fi

BUNDLE_SIZE=$(wc -c < "$BUNDLE")

# Read 64-byte header via od
HDR_MAGIC=$(od -A n -t x1 -N 4 "$BUNDLE" | tr -d ' \\n')
if [ "$HDR_MAGIC" != "44444255" ]; then
    ui_print "! ABORT: Invalid bundle magic (expected DDBU, got $(echo $HDR_MAGIC | sed 's/\\(..\\)/\\\\x\\1/g'))"
    exit 1
fi

# Parse header fields (little-endian uint16 at offsets 4,6,8,10)
HDR_VERSION=$(od -A n -t u2 -j 4 -N 2 "$BUNDLE" | tr -d ' ')
HDR_COMPRESS=$(od -A n -t u1 -j 6 -N 1 "$BUNDLE" | tr -d ' ')
HDR_NUM_PARTS=$(od -A n -t u2 -j 8 -N 2 "$BUNDLE" | tr -d ' ')
HDR_HDR_SIZE=$(od -A n -t u2 -j 10 -N 2 "$BUNDLE" | tr -d ' ')

# Validate header fields
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
# Align to 4096
REMAINDER=$(( DATA_OFFSET % 4096 ))
if [ "$REMAINDER" -ne 0 ]; then
    DATA_OFFSET=$(( DATA_OFFSET + 4096 - REMAINDER ))
fi

ui_print "  Version=$HDR_VERSION Compress=$HDR_COMPRESS Parts=$HDR_NUM_PARTS"
ui_print "  Header=$HDR_HDR_SIZE DataOffset=$DATA_OFFSET"
ui_print ""
{device_check_block}
# ── Step {slot_step}: Slot detection ──────────────────────────
ui_print "[Step {slot_step}/{total_steps}] Slot detection..."

TARGET_SLOT=""

# Method 1: /proc/cmdline androidboot.slot_suffix
CMDLINE_SLOT=$(cat /proc/cmdline 2>/dev/null | tr ' ' '\\n' | grep -o 'androidboot.slot_suffix=[^ ]*' | cut -d= -f2)
if [ -n "$CMDLINE_SLOT" ]; then
    TARGET_SLOT="$CMDLINE_SLOT"
fi

# Method 2: /proc/cmdline androidboot.slot (Unisoc/SPD variant)
if [ -z "$TARGET_SLOT" ]; then
    CMDLINE_SLOT_RAW=$(cat /proc/cmdline 2>/dev/null | tr ' ' '\\n' | grep -o 'androidboot.slot=[^ ]*' | cut -d= -f2)
    if [ -n "$CMDLINE_SLOT_RAW" ]; then
        TARGET_SLOT="_$CMDLINE_SLOT_RAW"
    fi
fi

# Method 3: getprop fallback
if [ -z "$TARGET_SLOT" ]; then
    PROP_SLOT=$(getprop ro.boot.slot_suffix 2>/dev/null)
    if [ -n "$PROP_SLOT" ]; then
        TARGET_SLOT="$PROP_SLOT"
    fi
fi

# Normalize: ensure _a or _b prefix
case "$TARGET_SLOT" in
    _a|_b) ;;
    a)  TARGET_SLOT="_a" ;;
    b)  TARGET_SLOT="_b" ;;
    *)   TARGET_SLOT="" ;;
esac

ui_print "  Active slot: ${{TARGET_SLOT:-none (non-A/B device)}}"
ui_print ""

# Shared partitions that NEVER get slot suffix
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

# ── Step {validation_step}: Partition validation ─────────────────────
ui_print "[Step {validation_step}/{total_steps}] Partition validation..."

validate_target() {{
    local target="$1"
    local min_size="$2"
    local name="$3"

    # Check block device exists
    if [ ! -e "$target" ]; then
        ui_print "! ABORT: $target not found for partition '$name'"
        return 1
    fi

    # Check it's a block device
    if [ ! -b "$target" ]; then
        ui_print "! ABORT: $target is not a block device"
        return 1
    fi

    # Check if mounted — unmount if so
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

    # Get partition size
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

# Validate all partitions
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

# ── Step {flash_step_offset}+: Flash each partition ────────────────────
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

    # Step C: Post-verify — read back and hash compare
    ui_print "  Verifying $PNAME..."
    VERIFY_HASH=""
    FULL_BLOCKS=$(( PSIZE / 4096 ))
    REMAINDER_BYTES=$(( PSIZE % 4096 ))

    if [ "$REMAINDER_BYTES" -eq 0 ]; then
        VERIFY_HASH=$(dd if="$PTARGET" bs=4096 count=$FULL_BLOCKS 2>/dev/null | sha256sum | cut -d' ' -f1)
    else
        VERIFY_HASH={{
            dd if="$PTARGET" bs=4096 count=$FULL_BLOCKS 2>/dev/null
            dd if="$PTARGET" bs=1 skip=$(( FULL_BLOCKS * 4096 )) count=$REMAINDER_BYTES 2>/dev/null
        }} | sha256sum | cut -d' ' -f1
    fi

    if [ "$VERIFY_HASH" = "$PHASH" ]; then
        ui_print "  $PNAME: VERIFIED OK"
    else
        ui_print "! ABORT: Hash mismatch for $PNAME!"
        ui_print "  Expected: $PHASH"
        ui_print "  Got:      $VERIFY_HASH"
        exit 1
    fi
    ui_print ""
done

# ── Done ────────────────────────────────────────────────────
ui_print "──────────────────────────────────────────"
ui_print " All $NUM_PARTS partition(s) flashed"
ui_print " and verified successfully!"
ui_print "──────────────────────────────────────────"
ui_print ""
exit 0
'''
    return script


def _build_flash_info(version, compress_name, bundle_size, num_parts, partitions_meta, device=""):
    """Build the flash_info.txt human-readable metadata."""
    lines = [
        "Renuked v3 — dd-based partition flasher",
        f"Generated: {time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime())}",
        f"Compression: {compress_name}",
        f"Bundle size: {bundle_size:,} bytes ({_human_size(bundle_size)})",
        f"Partitions: {num_parts}",
    ]
    if device:
        lines.append(f"Target device: {device}")
    lines.append("")
    for p in partitions_meta:
        lines.append(f"  [{p['name']}]")
        lines.append(f"    Uncompressed: {p['unc_size']:,} bytes ({_human_size(p['unc_size'])})")
        lines.append(f"    Compressed:   {p['comp_size']:,} bytes ({_human_size(p['comp_size'])})")
        lines.append(f"    SHA-256:      {p['hash_hex']}")
        lines.append(f"    Data offset:  {p['data_offset']}")
        lines.append("")
    return "\n".join(lines)


# ── Public API ───────────────────────────────────────────────────────────────

def _parse_args(args, kwargs):
    """Normalise Chaquopy dict-arg vs direct keyword-arg calling convention."""
    if args and isinstance(args[0], dict):
        return args[0]
    return kwargs


def run(*args, **kwargs):
    """Generate a ddbundle-format flashable ZIP from partition images.

    Parameters (via dict or kwargs)
    -------------------------------
    images      : dict  — {partition_name: image_file_path, ...}
    compress    : str   — Compression algorithm (default "gzip")
    output_path : str   — Path for output .zip file
    device      : str   — Device codename (optional, informational)

    Returns
    -------
    dict
        success     : bool
        output      : str
        zip_path    : str
        zip_size    : int
    """
    params = _parse_args(args, kwargs)
    images = params.get("images", {})
    compress_alg = str(params.get("compress", "gzip")).lower()
    output_path = str(params.get("output_path", "flashable_dd.zip"))
    device = str(params.get("device", ""))

    lines = []
    t0 = time.time()

    # ── Validate inputs ──
    if not images:
        return {"success": False, "output": "[!] Error: no images specified",
                "error": "no images specified"}

    if not output_path:
        return {"success": False, "output": "[!] Error: output_path is required",
                "error": "output_path is required"}

    # Normalise images dict
    if isinstance(images, dict):
        images = {str(k): str(v) for k, v in images.items()}

    # Validate compression
    if compress_alg not in COMPRESS_ID_MAP:
        return {"success": False,
                "output": f"[!] Error: unsupported compression '{compress_alg}'. "
                          f"Supported: {', '.join(COMPRESS_ID_MAP.keys())}",
                "error": f"unsupported compression: {compress_alg}"}

    # Validate image files
    for name, path in images.items():
        if not os.path.isfile(path):
            return {"success": False,
                    "output": f"[!] Image not found: {name} -> {path}",
                    "error": f"image file not found: {path}"}

    try:
        compress_id = COMPRESS_ID_MAP[compress_alg]
        compress_name = compress_alg
        decomp_cmd = COMPRESS_CMD_MAP[compress_id]
        num_parts = len(images)

        lines.append("\u2550" * 50)
        lines.append("REPACK: Generate flashable OTA ZIP")
        lines.append(f"Time: {time.strftime('%Y-%m-%d %H:%M:%S')}")
        lines.append("\u2500" * 50)
        lines.append("")
        lines.append(f"Partitions ({num_parts}):")
        for name, path in images.items():
            size = os.path.getsize(path)
            lines.append(f"  {name} ({_human_size(size)})")
        lines.append(f"Compression: {compress_name}")
        lines.append(f"Output: {os.path.basename(output_path)}")
        if device:
            lines.append(f"Device: {device}")
        lines.append("")

        # ── Step 1: Build ddbundle.bin ──
        _report_progress(1, 3, "Building ddbundle.bin")
        lines.append(f"[Step 1] Building ddbundle.bin...")
        lines.append(f"  Compressing {num_parts} partition(s) with {compress_name}...")

        # Build header
        header = _build_header(compress_id, num_parts)

        # Compress each partition and build data section
        partitions_meta = []
        data_blobs = bytearray()

        for i, (name, path) in enumerate(images.items()):
            _report_progress(1 + i, 2 + num_parts, f"Compressing {name}")

            raw_data = open(path, "rb").read()
            unc_size = len(raw_data)
            hash_hex = hashlib.sha256(raw_data).hexdigest()

            if compress_id == 0:
                comp_data = raw_data
            else:
                comp_data = compress(raw_data, compress_alg)

            comp_size = len(comp_data)
            data_offset = len(data_blobs)

            partitions_meta.append({
                "name": name,
                "unc_size": unc_size,
                "hash_hex": hash_hex,
                "comp_size": comp_size,
                "data_offset": data_offset,
            })

            data_blobs.extend(comp_data)

            # Pad to 4096 alignment
            aligned = _align_up(len(data_blobs), ALIGN)
            if aligned > len(data_blobs):
                data_blobs.extend(b"\x00" * (aligned - len(data_blobs)))

            lines.append(f"    {name}: {unc_size:,} -> {comp_size:,} bytes "
                         f"({100 * comp_size / max(unc_size, 1):.1f}%)")

        bundle_data = header + bytes(data_blobs)
        bundle_size = len(bundle_data)
        lines.append(f"  Bundle size: {_human_size(bundle_size)}")
        lines.append("")

        # ── Step 2: Build flasher scripts ──
        _report_progress(1 + num_parts, 2 + num_parts, "Building flasher scripts")
        lines.append("[Step 2] Building flasher scripts...")

        update_binary = _build_update_script(
            num_parts, compress_id, compress_name, partitions_meta, device=device
        )
        updater_script = "#Mtk client script\n"
        flash_info = _build_flash_info(
            "3", compress_name, bundle_size, num_parts, partitions_meta, device=device
        )

        lines.append(f"  update-binary: {len(update_binary):,} bytes")
        lines.append(f"  flash_info.txt: {len(flash_info):,} bytes")
        lines.append("")

        # ── Step 3: Write output ZIP ──
        _report_progress(2 + num_parts, 2 + num_parts, "Writing output ZIP")
        lines.append(f"[Step 3] Writing {os.path.basename(output_path)}...")

        os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)

        with zipfile.ZipFile(output_path, "w", compression=zipfile.ZIP_STORED) as zf:
            zf.writestr("ddbundle.bin", bundle_data)
            zf.writestr("flash_info.txt", flash_info)
            zf.writestr("META-INF/com/google/android/update-binary", update_binary)
            zf.writestr("META-INF/com/google/android/updater-script", updater_script)

        zip_size = os.path.getsize(output_path)
        lines.append(f"  ZIP size: {_human_size(zip_size)}")
        lines.append("")

        # ── Summary ──
        elapsed = time.time() - t0
        lines.append("\u2550" * 50)
        lines.append(f"SUCCESS in {elapsed * 1000:.0f}ms")
        lines.append(f"Output: {output_path}")
        lines.append(f"ZIP size: {_human_size(zip_size)}")
        lines.append("\u2550" * 50)

        output = "\n".join(lines)
        print(output)

        return {
            "success": True,
            "output": output,
            "zip_path": output_path,
            "zip_size": zip_size,
            "bundle_size": bundle_size,
        }

    except Exception as exc:
        err_msg = f"[!] Error: {exc}"
        lines.append(err_msg)
        import traceback
        lines.append(traceback.format_exc())
        output = "\n".join(lines)
        print(output)
        return {"success": False, "output": output, "error": str(exc)}
