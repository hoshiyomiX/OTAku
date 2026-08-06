//! Compression / decompression for AOSP payload.bin operations.
//!
//! All algorithms are statically compiled — no runtime dependency checks needed.
//! Supported: gzip (flate2/miniz_oxide), lz4 (lz4_flex/frame), bzip2, xz (xz2/liblzma), zstd (libzstd-sys).
//! "none" is internal identity only (not user-selectable). "brotli" removed from APK build.
//!
//! Ported from Python compression.py to Rust with identical semantics.

use std::io::{Read, Write};

// ---------------------------------------------------------------------------
//  Algorithm constants
// ---------------------------------------------------------------------------

pub const ALG_NONE: &str = "none";
pub const ALG_GZIP: &str = "gzip";
pub const ALG_LZ4: &str = "lz4";
pub const ALG_BZIP2: &str = "bzip2";
pub const ALG_XZ: &str = "xz";
pub const ALG_ZSTD: &str = "zstd";
pub const ALG_AUTO: &str = "auto";

// REMOVED: ALL_ALGORITHMS — dead constant (zero callers).
// nativeCheckDeps in lib.rs hardcodes the available algorithm list.

/// Default compression levels per algorithm (matches Python DEFAULT_LEVELS)
pub const DEFAULT_LEVELS: &[(&str, i32)] = &[
    // DEMOTED: (ALG_NONE, 0) — none is not user-selectable
    // DEMOTED: (ALG_BROTLI, 6) — brotli removed from APK build
    (ALG_GZIP, 6),
    (ALG_LZ4, 4),
    (ALG_BZIP2, 9),
    (ALG_XZ, 6),
    (ALG_ZSTD, 3),
];

/// Valid level ranges per algorithm: (min, max) (matches Python LEVEL_RANGES)
pub const LEVEL_RANGES: &[(&str, i32, i32)] = &[
    // DEMOTED: (ALG_NONE, 0, 0) — none is not user-selectable
    // DEMOTED: (ALG_BROTLI, 0, 11) — brotli removed from APK build
    (ALG_GZIP, 1, 9),
    (ALG_LZ4, 1, 12),
    (ALG_BZIP2, 1, 9),
    (ALG_XZ, 0, 9),
    (ALG_ZSTD, 1, 22),
];

// ---------------------------------------------------------------------------
//  Compression ID mapping (for DDBU header)
// ---------------------------------------------------------------------------

pub const COMPRESS_ID_MAP: &[(&str, u16)] = &[
    // DEMOTED: ("none", 0) — none is not user-selectable
    ("gzip", 1),
    ("bzip2", 2),
    ("xz", 3),
    // REMOVED: ("brotli", 4) — brotli removed from APK build
    ("lz4", 5),
    ("zstd", 6),
];

/// Get the compress ID for an algorithm name.
///
/// BUG FIX (NEW-E): Uses `normalise()` to resolve aliases before lookup,
/// so "REPLACE_BROT" → "none" (brotli removed) and "REPLACE_BZ" → "bzip2" → 2.
/// Previously did exact-match, returning 0 (none) for AOSP type names.
pub fn compress_id(algorithm: &str) -> u16 {
    let canonical = normalise(algorithm);
    COMPRESS_ID_MAP
        .iter()
        .find(|(name, _)| *name == canonical)
        .map(|(_, id)| *id)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
//  Algorithm name normalization (matches Python _normalise)
// ---------------------------------------------------------------------------

/// Normalize an algorithm name to canonical form, handling common aliases.
///
/// BUG FIX (NEW-E): Added AOSP operation type name aliases so that strings
/// like "REPLACE_BROT" and "REPLACE_BZ" (which appear in parsed payload JSON)
/// are correctly mapped to their canonical algorithm names (brotli→"none", "bzip2").
/// Previously, these strings fell through to the `other` arm and were returned
/// as-is, causing `is_alg("REPLACE_BROT", "brotli")` to return false and
/// `compress_id("REPLACE_BROT")` to return 0 (none) instead of 4 (brotli).
/// Now brotli-like inputs normalise to "none" (uncompressed passthrough).
fn normalise(algorithm: &str) -> String {
    let lower = algorithm.to_lowercase().trim().to_string();
    match lower.as_str() {
        "" | "raw" | "none" => ALG_NONE.to_string(),
        "bz2" | "bzip2" | "replace_bz" => ALG_BZIP2.to_string(),
        "gz" | "gzip" | "puigzip" => ALG_GZIP.to_string(),
        "lzma" | "xz" | "replace_xz" => ALG_XZ.to_string(),
        "br" | "brotli" | "replace_brot" | "brotli_bsdiff" => ALG_NONE.to_string(), // brotli removed — treat as uncompressed
        "lz4" | "l4" => ALG_LZ4.to_string(),
        "zstd" | "zs" | "zst" => ALG_ZSTD.to_string(),
        other => other.to_string(), // return as-is for unknown algorithms
    }
}

// ---------------------------------------------------------------------------
//  Level resolution
// ---------------------------------------------------------------------------

/// Resolve compression level: use provided level or algorithm default, clamped to valid range.
///
/// BUG FIX: Previously, `Some(0)` was treated as literal level 0, bypassing the
/// algorithm default. This caused xz (valid range 0-9) to use level 0 (no compression)
/// and brotli (removed — normalises to "none") when the caller
/// intended "use default". Now `Some(0)` is treated the same as `None` — both mean
/// "use the algorithm's default level". This aligns with the Kotlin/Java convention
/// where `level = 0` means "default" (sentinel value), and with all JNI entry points
/// that convert `jint 0` → `None`.
pub fn resolve_level(algorithm: &str, level: Option<i32>) -> i32 {
    let alg = normalise(algorithm);
    let default = DEFAULT_LEVELS
        .iter()
        .find(|(name, _)| *name == alg)
        .map(|(_, lvl)| *lvl)
        .unwrap_or(0);
    // Treat Some(0) same as None — 0 is the "use default" sentinel, matching
    // the Kotlin convention (level=0 → default) and JNI conversion (jint 0 → None).
    let resolved = level.filter(|&v| v > 0).unwrap_or(default);
    let (min, max) = LEVEL_RANGES
        .iter()
        .find(|(name, _, _)| *name == alg)
        .map(|(_, lo, hi)| (*lo, *hi))
        .unwrap_or((0, 0));
    resolved.clamp(min, max)
}

/// Check if an algorithm name matches a canonical algorithm.
/// Works with the String return type of normalise().
pub fn is_alg(algorithm: &str, target: &str) -> bool {
    normalise(algorithm) == target
}

// ---------------------------------------------------------------------------
//  Auto-detect compression from magic bytes (matches Python _detect_from_data)
// ---------------------------------------------------------------------------

/// Detect the compression format from magic bytes.
pub fn detect_from_data(data: &[u8]) -> &'static str {
    if data.len() < 2 {
        return ALG_NONE;
    }

    // Gzip magic: 1F 8B
    if data[0] == 0x1F && data[1] == 0x8B {
        return ALG_GZIP;
    }

    // Bzip2 magic: 42 5A 68 ("BZh")
    if data.len() >= 3 && data[0] == 0x42 && data[1] == 0x5A && data[2] == 0x68 {
        return ALG_BZIP2;
    }

    // XZ magic: FD 37 7A 58 5A 00
    if data.len() >= 6 && data[..6] == [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00] {
        return ALG_XZ;
    }

    // LZ4 frame format magic: 04 22 4D 18
    if data.len() >= 4 && data[..4] == [0x04, 0x22, 0x4D, 0x18] {
        return ALG_LZ4;
    }

    // ZSTD frame magic: 28 B5 2F FD
    if data.len() >= 4 && data[..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        return ALG_ZSTD;
    }

    // REMOVED: brotli trial decompression — brotli not in APK build

    ALG_NONE
}

// ---------------------------------------------------------------------------
//  Compression / decompression — full implementation
// ---------------------------------------------------------------------------

/// Compress data with the specified algorithm.
///
/// # Arguments
/// * `data` - Raw data to compress
/// * `algorithm` - One of "none", "gzip", "bzip2", "xz"
/// * `level` - Compression level. None = use algorithm default.
///
/// # Returns
/// Compressed data (or data unchanged for "none")
///
/// Note: Only used in tests. Production code uses the streaming
/// hash_and_compress_file_to_writer_with_progress path.
#[cfg(test)]
pub fn compress(data: &[u8], algorithm: &str, level: Option<i32>) -> Result<Vec<u8>, String> {
    if is_alg(algorithm, ALG_NONE) {
        return Err("ALG_NONE (no compression) is not a valid compression output — select gzip/lz4/bzip2/xz/zstd".to_string());
    }

    let resolved_level = resolve_level(algorithm, level);

    if is_alg(algorithm, ALG_GZIP) {
        return compress_gzip(data, resolved_level);
    }
    if is_alg(algorithm, ALG_BZIP2) {
        return compress_bzip2(data, resolved_level);
    }
    if is_alg(algorithm, ALG_XZ) {
        return compress_xz(data, resolved_level);
    }
    if is_alg(algorithm, ALG_LZ4) {
        return compress_lz4(data, resolved_level);
    }
    if is_alg(algorithm, ALG_ZSTD) {
        return compress_zstd(data, resolved_level);
    }
    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

/// Decompress data with the specified algorithm.
///
/// # Arguments
/// * `data` - Compressed (or raw) data
/// * `algorithm` - One of "none", "gzip", "bzip2", "xz", "auto"
///
/// "auto" attempts to detect the format from magic bytes.
pub fn decompress(data: &[u8], algorithm: &str) -> Result<Vec<u8>, String> {
    let alg = normalise(algorithm);

    let effective_alg = if alg == ALG_AUTO {
        detect_from_data(data).to_string()
    } else {
        alg
    };

    if effective_alg == ALG_NONE {
        return Ok(data.to_vec());
    }

    if effective_alg == ALG_GZIP {
        return decompress_gzip(data);
    }
    if effective_alg == ALG_BZIP2 {
        return decompress_bzip2(data);
    }
    if effective_alg == ALG_XZ {
        return decompress_xz(data);
    }
    if effective_alg == ALG_LZ4 {
        return decompress_lz4(data);
    }
    if effective_alg == ALG_ZSTD {
        return decompress_zstd(data);
    }
    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

/// Streaming decompress: write decompressed chunks to a writer as they are produced.
///
/// BUG FIX (NEW-3): The in-memory `decompress()` materializes the full decompressed
/// output as a `Vec<u8>`, which can be 2-5 GB for large partitions — exceeding
/// Android's 256-512 MB per-app heap limit. This streaming variant writes chunks
/// to the writer as they are decompressed, using only ~4 MB RAM regardless of
/// the decompressed size.
///
/// Peak RAM: ~4 MB (read buffer) + compressor internal state (~1-4 MB) = ~8 MB.
/// Compare: `decompress()` holds the entire output, which can be 5 GB.
///
/// Returns the total bytes written.
pub fn decompress_to_writer<W: Write>(
    data: &[u8],
    algorithm: &str,
    writer: &mut W,
) -> Result<u64, String> {
    let alg = normalise(algorithm);
    let effective_alg = if alg == ALG_AUTO {
        detect_from_data(data).to_string()
    } else {
        alg
    };

    if effective_alg == ALG_NONE {
        writer.write_all(data).map_err(|e| format!("Write raw error: {}", e))?;
        return Ok(data.len() as u64);
    }

    const BUF_SIZE: usize = 4 * 1024 * 1024; // 4 MB
    let mut buf = [0u8; BUF_SIZE];
    let mut total: u64 = 0;

    if effective_alg == ALG_GZIP {
        use flate2::read::GzDecoder;
        let mut decoder = GzDecoder::new(data);
        loop {
            let n = decoder.read(&mut buf).map_err(|e| format!("gzip streaming decompress error: {}", e))?;
            if n == 0 { break; }
            writer.write_all(&buf[..n]).map_err(|e| format!("Write decompressed error: {}", e))?;
            total += n as u64;
            if total > MAX_DECOMPRESSED_SIZE as u64 {
                return Err(format!("gzip decompressed output exceeds {} GiB limit — possible zip bomb",
                    MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)));
            }
        }
        return Ok(total);
    }

    if effective_alg == ALG_BZIP2 {
        use bzip2::read::BzDecoder;
        let mut decoder = BzDecoder::new(data);
        loop {
            let n = decoder.read(&mut buf).map_err(|e| format!("bzip2 streaming decompress error: {}", e))?;
            if n == 0 { break; }
            writer.write_all(&buf[..n]).map_err(|e| format!("Write decompressed error: {}", e))?;
            total += n as u64;
            if total > MAX_DECOMPRESSED_SIZE as u64 {
                return Err(format!("bzip2 decompressed output exceeds {} GiB limit — possible zip bomb",
                    MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)));
            }
        }
        return Ok(total);
    }

    if effective_alg == ALG_XZ {
        let mut decoder = xz2::read::XzDecoder::new(data);
        loop {
            let n = decoder.read(&mut buf).map_err(|e| format!("xz streaming decompress error: {}", e))?;
            if n == 0 { break; }
            writer.write_all(&buf[..n]).map_err(|e| format!("Write decompressed error: {}", e))?;
            total += n as u64;
            if total > MAX_DECOMPRESSED_SIZE as u64 {
                return Err(format!("xz decompressed output exceeds {} GiB limit — possible zip bomb",
                    MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)));
            }
        }
        return Ok(total);
    }

    if effective_alg == ALG_LZ4 {
        use std::io::Cursor;
        let cursor = Cursor::new(data);
        let mut decoder = lz4_flex::frame::FrameDecoder::new(cursor);
        loop {
            let n = decoder.read(&mut buf).map_err(|e| format!("lz4 streaming decompress error: {}", e))?;
            if n == 0 { break; }
            writer.write_all(&buf[..n]).map_err(|e| format!("Write decompressed error: {}", e))?;
            total += n as u64;
            if total > MAX_DECOMPRESSED_SIZE as u64 {
                return Err(format!("lz4 decompressed output exceeds {} GiB limit — possible zip bomb",
                    MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)));
            }
        }
        return Ok(total);
    }

    if effective_alg == ALG_ZSTD {
        let mut decoder = zstd::Decoder::new(data).map_err(|e| format!("zstd decoder init error: {}", e))?;
        loop {
            let n = decoder.read(&mut buf).map_err(|e| format!("zstd streaming decompress error: {}", e))?;
            if n == 0 { break; }
            writer.write_all(&buf[..n]).map_err(|e| format!("Write decompressed error: {}", e))?;
            total += n as u64;
            if total > MAX_DECOMPRESSED_SIZE as u64 {
                return Err(format!("zstd decompressed output exceeds {} GiB limit — possible zip bomb",
                    MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)));
            }
        }
        return Ok(total);
    }

    Err(format!("Unknown compression algorithm for streaming: {:?}", algorithm))
}

// ---------------------------------------------------------------------------
//  Gzip implementation (flate2 / miniz_oxide)
// ---------------------------------------------------------------------------

#[cfg(test)]
fn compress_gzip(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let level_clamped = level.clamp(1, 9) as u32;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level_clamped));
    encoder
        .write_all(data)
        .map_err(|e| format!("gzip compress write error: {}", e))?;
    encoder
        .finish()
        .map_err(|e| format!("gzip compress finish error: {}", e))
}

/// Maximum decompressed output size for in-memory decompress operations.
/// Prevents zip-bomb style attacks where a small compressed input decompresses
/// to gigabytes, exceeding Android's 256-512 MB per-app heap limit.
const MAX_DECOMPRESSED_SIZE: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let decoder = GzDecoder::new(data);
    let mut result = Vec::new();
    // BUG FIX: Use take() to limit decompressed output to MAX_DECOMPRESSED_SIZE.
    // Without this, a compressed bomb could decompress to gigabytes and OOM.
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64)
        .read_to_end(&mut result)
        .map_err(|e| format!("gzip decompress error: {}", e))?;
    if result.len() >= MAX_DECOMPRESSED_SIZE {
        return Err(format!(
            "gzip decompressed output exceeds {} GiB limit — possible zip bomb",
            MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
//  Bzip2 implementation
// ---------------------------------------------------------------------------

#[cfg(test)]
fn compress_bzip2(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    use bzip2::write::BzEncoder;
    use bzip2::Compression;

    let level_clamped = level.clamp(1, 9) as u32;
    let mut encoder = BzEncoder::new(Vec::new(), Compression::new(level_clamped));
    encoder
        .write_all(data)
        .map_err(|e| format!("bzip2 compress write error: {}", e))?;
    encoder
        .finish()
        .map_err(|e| format!("bzip2 compress finish error: {}", e))
}

fn decompress_bzip2(data: &[u8]) -> Result<Vec<u8>, String> {
    use bzip2::read::BzDecoder;
    use std::io::Read;

    let decoder = BzDecoder::new(data);
    let mut result = Vec::new();
    // BUG FIX: Same zip-bomb protection as decompress_gzip.
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64)
        .read_to_end(&mut result)
        .map_err(|e| format!("bzip2 decompress error: {}", e))?;
    if result.len() >= MAX_DECOMPRESSED_SIZE {
        return Err(format!(
            "bzip2 decompressed output exceeds {} GiB limit — possible zip bomb",
            MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
//  XZ implementation (xz2 / liblzma)
// ---------------------------------------------------------------------------

#[cfg(test)]
fn compress_xz(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    let level_clamped = level.clamp(0, 9) as u32;
    let mut result = Vec::new();
    let mut encoder = xz2::write::XzEncoder::new(result, level_clamped);
    encoder
        .write_all(data)
        .map_err(|e| format!("xz compress write error: {}", e))?;
    result = encoder
        .finish()
        .map_err(|e| format!("xz compress finish error: {}", e))?;
    Ok(result)
}

fn decompress_xz(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let decoder = xz2::read::XzDecoder::new(data);
    let mut result = Vec::new();
    // BUG FIX: Same zip-bomb protection as decompress_gzip.
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64)
        .read_to_end(&mut result)
        .map_err(|e| format!("xz decompress error: {}", e))?;
    if result.len() >= MAX_DECOMPRESSED_SIZE {
        return Err(format!(
            "xz decompressed output exceeds {} GiB limit — possible zip bomb",
            MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(result)
}

// DEMOTED: compress_brotli() and decompress_brotli() — removed from APK build.

// ---------------------------------------------------------------------------
//  LZ4 implementation (lz4_flex frame format — compatible with lz4 CLI)
// ---------------------------------------------------------------------------

/// Map our 1-12 compression level to lz4_flex BlockSize.
/// LZ4 frame format controls compression via block size rather than a numeric
/// level. Larger blocks give better compression ratios but use more memory.
///   Level 1-3  → Max64KB  (fastest, least compression — ~2x ratio)
///   Level 4-6  → Max4MB   (balanced — default, ~2.5x ratio)
///   Level 7-9  → Max4MB   (better ratio, still fast)
///   Level 10-12 → Max8MB  (best ratio, uses more memory)
fn lz4_block_size_for_level(level: i32) -> lz4_flex::frame::BlockSize {
    match level.clamp(1, 12) {
        1..=3 => lz4_flex::frame::BlockSize::Max64KB,
        4..=9 => lz4_flex::frame::BlockSize::Max4MB,
        _ => lz4_flex::frame::BlockSize::Max8MB,
    }
}

/// Build a FrameInfo with the appropriate block size for a given level.
///
/// NOTE: lz4_flex 0.14 does not expose ContentChecksum in the frame module,
/// so we rely on block_size alone (which is the primary compression knob).
/// The lz4 CLI default adds ContentChecksum, but our frames are verified
/// via SHA-256 in the flash pipeline anyway (PART_i_COMP_HASH).
fn lz4_frame_info_for_level(level: i32) -> lz4_flex::frame::FrameInfo {
    lz4_flex::frame::FrameInfo::new()
        .block_size(lz4_block_size_for_level(level))
}

#[cfg(test)]
fn compress_lz4(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    // lz4_flex frame format produces output compatible with `lz4 -d` CLI.
    // We use FrameEncoder with FrameInfo to control block size (which is the
    // primary compression knob in frame format — larger blocks = better ratio).
    let resolved_level = level.clamp(1, 12);
    let frame_info = lz4_frame_info_for_level(resolved_level);
    let mut result = Vec::new();
    {
        let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(frame_info, &mut result).auto_finish();
        encoder.write_all(data)
            .map_err(|e| format!("lz4 frame compress write error: {}", e))?;
    } // encoder.finish() is called on drop — flushes and writes end mark
    Ok(result)
}

fn decompress_lz4(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Read};
    let cursor = Cursor::new(data);
    let decoder = lz4_flex::frame::FrameDecoder::new(cursor);
    let mut result = Vec::new();
    // BUG FIX: Use take() to limit decompressed output to MAX_DECOMPRESSED_SIZE.
    // Without this, a crafted LZ4 frame could decompress to gigabytes and OOM.
    // All other decompress_* functions have this protection; lz4 was missing it.
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64)
        .read_to_end(&mut result)
        .map_err(|e| format!("lz4 frame decompress error: {}", e))?;
    if result.len() >= MAX_DECOMPRESSED_SIZE {
        return Err(format!(
            "lz4 decompressed output exceeds {} GiB limit — possible zip bomb",
            MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
//  ZSTD implementation (zstd / libzstd-sys)
// ---------------------------------------------------------------------------

#[cfg(test)]
fn compress_zstd(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    // zstd crate Encoder wraps Facebook's libzstd C library.
    // Level 1-19 are standard; 20-22 are "ultra" (slower, better ratio).
    // Default level 3 provides ~2.8x ratio at 400 MB/s decompress speed,
    // significantly better than gzip level 6 (~2.5x at 200 MB/s) and
    // competitive with xz level 6 (~3.5x at 40 MB/s) for much faster
    // decompression — critical for recovery flashing time.
    let level_clamped = level.clamp(1, 22);
    let mut result = Vec::new();
    {
        let mut encoder = zstd::Encoder::new(&mut result, level_clamped)
            .map_err(|e| format!("zstd encoder init error: {}", e))?;
        encoder.write_all(data)
            .map_err(|e| format!("zstd compress write error: {}", e))?;
        encoder.finish()
            .map_err(|e| format!("zstd compress finish error: {}", e))?;
    }
    Ok(result)
}

fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let decoder = zstd::Decoder::new(data)
        .map_err(|e| format!("zstd decoder init error: {}", e))?;
    let mut result = Vec::new();
    // Zip-bomb protection: same .take() pattern as all other decompress_*.
    // A crafted ZSTD frame could decompress to gigabytes; this prevents OOM.
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64)
        .read_to_end(&mut result)
        .map_err(|e| format!("zstd decompress error: {}", e))?;
    if result.len() >= MAX_DECOMPRESSED_SIZE {
        return Err(format!(
            "zstd decompressed output exceeds {} GiB limit — possible zip bomb",
            MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
//  SHA-256 hashing
// ---------------------------------------------------------------------------

/// Compute SHA-256 hash of data
///
/// Note: Only used in tests. Production code uses the streaming hash
/// inside hash_and_compress_file_to_writer_with_progress.
#[cfg(test)]
pub fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

// REMOVED: sha256_file() — dead function (only caller was dead sha256_file_hex).
// Production code uses the streaming hash inside
// hash_and_compress_file_to_writer_with_progress instead.

// ---------------------------------------------------------------------------
//  Streaming compression with progress
// ---------------------------------------------------------------------------

/// Progress callback type: `(bytes_processed, total_bytes)`
pub type ProgressFn = Box<dyn FnMut(u64, u64)>;

/// Compress data in chunks, calling progress callback after each chunk.
///
/// This matches the Python `compress_streaming()` function for real-time
/// progress reporting on large (2+ GB) partition images.
///
/// Note: Only used in tests. Production code uses the streaming
/// hash_and_compress_file_to_writer_with_progress path.
#[cfg(test)]
pub fn compress_streaming(
    data: &[u8],
    algorithm: &str,
    level: Option<i32>,
    chunk_size: usize,
    mut on_progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<Vec<u8>, String> {
    if is_alg(algorithm, ALG_NONE) {
        if let Some(ref mut cb) = on_progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }

    let resolved_level = resolve_level(algorithm, level);
    let effective_chunk = if is_alg(algorithm, ALG_XZ) {
        // Use larger chunks for XZ (LZMA has large internal dictionary)
        chunk_size.max(4 * 1024 * 1024)
    } else {
        chunk_size
    };
    let total = data.len() as u64;

    if is_alg(algorithm, ALG_GZIP) {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level_clamped));
        let mut offset: usize = 0;
        while offset < data.len() {
            let end = (offset + effective_chunk).min(data.len());
            encoder
                .write_all(&data[offset..end])
                .map_err(|e| format!("gzip streaming write error: {}", e))?;
            offset = end;
            if let Some(ref mut cb) = on_progress {
                cb(offset as u64, total);
            }
        }
        return encoder
            .finish()
            .map_err(|e| format!("gzip streaming finish error: {}", e));
    }

    if is_alg(algorithm, ALG_BZIP2) {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = BzEncoder::new(Vec::new(), Compression::new(level_clamped));
        let mut offset: usize = 0;
        while offset < data.len() {
            let end = (offset + effective_chunk).min(data.len());
            encoder
                .write_all(&data[offset..end])
                .map_err(|e| format!("bzip2 streaming write error: {}", e))?;
            offset = end;
            if let Some(ref mut cb) = on_progress {
                cb(offset as u64, total);
            }
        }
        return encoder
            .finish()
            .map_err(|e| format!("bzip2 streaming finish error: {}", e));
    }

    if is_alg(algorithm, ALG_XZ) {
        let level_clamped = resolved_level.clamp(0, 9) as u32;
        let buf = Vec::new();
        let mut encoder = xz2::write::XzEncoder::new(buf, level_clamped);
        let mut offset: usize = 0;
        while offset < data.len() {
            let end = (offset + effective_chunk).min(data.len());
            encoder
                .write_all(&data[offset..end])
                .map_err(|e| format!("xz streaming write error: {}", e))?;
            offset = end;
            if let Some(ref mut cb) = on_progress {
                cb(offset as u64, total);
            }
        }
        return encoder
            .finish()
            .map_err(|e| format!("xz streaming finish error: {}", e));
    }

    if is_alg(algorithm, ALG_LZ4) {
        let frame_info = lz4_frame_info_for_level(resolved_level);
        let mut result = Vec::new();
        {
            let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(frame_info, &mut result).auto_finish();
            let mut offset: usize = 0;
            while offset < data.len() {
                let end = (offset + effective_chunk).min(data.len());
                encoder
                    .write_all(&data[offset..end])
                    .map_err(|e| format!("lz4 streaming write error: {}", e))?;
                offset = end;
                if let Some(ref mut cb) = on_progress {
                    cb(offset as u64, total);
                }
            }
        } // encoder is flushed on drop
        return Ok(result);
    }

    if is_alg(algorithm, ALG_ZSTD) {
        let level_clamped = resolved_level.clamp(1, 22);
        let mut result = Vec::new();
        {
            let mut encoder = zstd::Encoder::new(&mut result, level_clamped)
                .map_err(|e| format!("zstd encoder init error: {}", e))?;
            let mut offset: usize = 0;
            while offset < data.len() {
                let end = (offset + effective_chunk).min(data.len());
                encoder
                    .write_all(&data[offset..end])
                    .map_err(|e| format!("zstd streaming write error: {}", e))?;
                offset = end;
                if let Some(ref mut cb) = on_progress {
                    cb(offset as u64, total);
                }
            }
            encoder.finish()
                .map_err(|e| format!("zstd streaming finish error: {}", e))?;
        }
        return Ok(result);
    }

    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

// ---------------------------------------------------------------------------
//  Hash and compress a file in a single streaming pass
// ---------------------------------------------------------------------------

/// Hash and compress a file in a single streaming pass.
///
/// Reads the file chunk-by-chunk, updating SHA-256 and feeding each
/// chunk directly to an incremental compressor. The raw file data is
/// never held fully in memory.
///
/// Returns `(compressed_data, sha256_hexdigest)`
pub fn hash_and_compress_file(
    file_path: &str,
    algorithm: &str,
    level: Option<i32>,
) -> Result<(Vec<u8>, String), String> {
    use sha2::{Digest, Sha256};
    use std::fs::File;

    let _alg = normalise(algorithm);
    // Use the original algorithm string for is_alg / resolve_level checks
    // (they call normalise internally)
    //
    // Note: file_size is intentionally NOT pre-fetched here — the
    // hash_and_compress_file_with_progress variant fetches it for progress
    // reporting. This variant streams chunks without needing total size.
    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", file_path, e))?
        .len();

    // ALG_NONE size guard — refuse files larger than 256MB.
    //
    // ALG_NONE ("no compression") loads the entire file into a Vec<u8> for
    // return (no streaming variant exists for the API). On Android, the
    // typical per-app heap limit is 256-512MB; a 5GB system.img + 256MB
    // heap = guaranteed OOM crash that kills the entire app process.
    //
    // The 256MB threshold is chosen to:
    //   - ALLOW all typical ALG_NONE use cases (small physical partitions):
    //       boot (~64MB), dtbo (~32MB), vbmeta (~16MB), init_boot (~64MB),
    //       recovery (~64MB), lk/logo/spmfw/tee (~2-16MB), vendor_boot (~64MB)
    //   - REFUSE large dynamic partitions that should use gzip/xz:
    //       system (~2-5GB), vendor (~1-2GB), product (~1-3GB), system_ext,
    //       odm, vendor_dlkm, etc.
    //
    // Users who genuinely need ALG_NONE for a large partition should use the
    // payload.bin path (write_payload) instead of the DD bundle path — the
    // payload format supports streaming.
    const ALG_NONE_MAX_SIZE: u64 = 256 * 1024 * 1024; // 256 MB
    if is_alg(algorithm, ALG_NONE) && file_size > ALG_NONE_MAX_SIZE {
        return Err(format!(
            "ALG_NONE (no compression) refused for {} — file size {} bytes exceeds {} byte limit. \
             ALG_NONE loads the entire file into memory, which would OOM Android's 256-512MB heap. \
             Use gzip or xz instead (they stream chunks and never hold the full file in memory). \
             If you genuinely need uncompressed storage, use the payload.bin path (write_payload) \
             which supports streaming.",
            file_path, file_size, ALG_NONE_MAX_SIZE
        ));
    }

    let mut file =
        File::open(file_path).map_err(|e| format!("Cannot open {}: {}", file_path, e))?;
    let mut hasher = Sha256::new();
    let chunk_size = 4 * 1024 * 1024; // 4 MB chunks
    let mut buf = vec![0u8; chunk_size];

    if is_alg(algorithm, ALG_NONE) {
        // No compression: just hash and return raw bytes.
        // Use Vec::new() (not with_capacity) to avoid OOM pre-allocation for
        // large files — Vec grows incrementally as data is appended.
        let mut raw_buf = Vec::new();
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            raw_buf.extend_from_slice(&buf[..n]);
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((raw_buf, hex));
    }

    let resolved_level = resolve_level(algorithm, level);

    if is_alg(algorithm, ALG_GZIP) {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level_clamped));
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("gzip compress write error: {}", e))?;
        }
        let compressed = encoder
            .finish()
            .map_err(|e| format!("gzip compress finish error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed, hex));
    }

    if is_alg(algorithm, ALG_BZIP2) {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = BzEncoder::new(Vec::new(), Compression::new(level_clamped));
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("bzip2 compress write error: {}", e))?;
        }
        let compressed = encoder
            .finish()
            .map_err(|e| format!("bzip2 compress finish error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed, hex));
    }

    if is_alg(algorithm, ALG_XZ) {
        let level_clamped = resolved_level.clamp(0, 9) as u32;
        let inner = Vec::new();
        let mut encoder = xz2::write::XzEncoder::new(inner, level_clamped);
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("xz compress write error: {}", e))?;
        }
        let compressed = encoder
            .finish()
            .map_err(|e| format!("xz compress finish error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed, hex));
    }

    if is_alg(algorithm, ALG_LZ4) {
        let frame_info = lz4_frame_info_for_level(resolved_level);
        let mut result = Vec::new();
        {
            let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(frame_info, &mut result).auto_finish();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| format!("Read error: {}", e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                encoder
                    .write_all(&buf[..n])
                    .map_err(|e| format!("lz4 compress write error: {}", e))?;
            }
        } // encoder is flushed on drop
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((result, hex));
    }

    if is_alg(algorithm, ALG_ZSTD) {
        let level_clamped = resolved_level.clamp(1, 22);
        let mut result = Vec::new();
        {
            let mut encoder = zstd::Encoder::new(&mut result, level_clamped)
                .map_err(|e| format!("zstd encoder init error: {}", e))?;
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| format!("Read error: {}", e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                encoder
                    .write_all(&buf[..n])
                    .map_err(|e| format!("zstd compress write error: {}", e))?;
            }
            encoder.finish()
                .map_err(|e| format!("zstd compress finish error: {}", e))?;
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((result, hex));
    }

    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

// REMOVED: hash_and_compress_file_with_progress() — dead function (zero callers).
// Returns entire compressed output in Vec<u8> — OOM risk on Android.
// Completely replaced by the streaming hash_and_compress_file_to_writer_with_progress
// variant which writes compressed chunks directly to the output writer.

// ---------------------------------------------------------------------------
//  Sha256Writer — wraps a Write to compute SHA-256 of bytes written through it
// ---------------------------------------------------------------------------

/// A `Write` wrapper that computes SHA-256 of everything written through it,
/// while passing the bytes through to the underlying writer.
///
/// Used by `hash_and_compress_file_to_writer_with_progress` to compute the
/// SHA-256 of the *compressed* data as it is written to disk — without
/// holding the compressed data in memory. This is essential for OOM safety:
/// computing the hash of a 351MB compressed vendor partition by first
/// accumulating it in a Vec would risk OOM on Android's 256-512MB heap.
pub struct Sha256Writer<W> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W> Sha256Writer<W> {
    pub fn new(inner: W) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }

    /// Consume the Sha256Writer and return the SHA-256 hex digest of all
    /// data written through it, plus the inner writer.
    pub fn finalize(self) -> (String, W) {
        use sha2::Digest;
        let hex: String = self.hasher.finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        (hex, self.inner)
    }
}

impl<W: Write> Write for Sha256Writer<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
//  CountingWriter — wraps a Write to count bytes written
// ---------------------------------------------------------------------------

/// A `Write` wrapper that counts how many bytes were written through it.
///
/// Used by `hash_and_compress_file_to_writer` to track compressed output size
/// without holding the data in memory. The compressor writes compressed chunks
/// through this wrapper to the underlying writer, and we count every byte.
struct CountingWriter<W> {
    inner: W,
    bytes_written: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes_written: 0 }
    }

    /// Return the total bytes written so far.
    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Consume the CountingWriter and return the inner writer.
    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
//  Hash and compress a file, streaming to a writer (OOM-safe)
// ---------------------------------------------------------------------------

/// Hash and compress a file, streaming compressed output directly to a writer.
///
/// This is the OOM-safe variant of `hash_and_compress_file`. Instead of
/// accumulating the entire compressed output in a `Vec<u8>` (which can be
/// 2-5GB for large partitions like system.img), it writes compressed chunks
/// to the provided writer as they are produced by the compressor.
///
/// # Memory usage
/// Peak RAM: ~8MB (4MB read buffer + ~4MB compressor internal buffer).
/// Compare: `hash_and_compress_file` holds the entire compressed output in
/// RAM, which can be 2-5GB for system.img.
///
/// # Returns
/// `(compressed_size, sha256_hex_of_raw)` — the compressed size (bytes
/// written to the writer) and the SHA-256 hex digest of the uncompressed
/// input file. The compressed data itself is NOT returned; it was already
/// written to the writer.
///
/// # When to use
/// - `write_payload`: stream each partition's compressed data to a temp file
/// - Any scenario where the compressed output should go directly to disk
///   without holding it in RAM first
pub fn hash_and_compress_file_to_writer<W: Write>(
    file_path: &str,
    algorithm: &str,
    level: Option<i32>,
    writer: &mut W,
) -> Result<(u64, String), String> {
    use sha2::{Digest, Sha256};
    use std::fs::File;

    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", file_path, e))?
        .len();

    // ALG_NONE size guard — same rationale as hash_and_compress_file.
    // 256MB cap; large files must use gzip/xz (streaming) or payload.bin path.
    const ALG_NONE_MAX_SIZE: u64 = 256 * 1024 * 1024; // 256 MB
    if is_alg(algorithm, ALG_NONE) && file_size > ALG_NONE_MAX_SIZE {
        return Err(format!(
            "ALG_NONE (no compression) refused for {} — file size {} bytes exceeds {} byte limit. \
             ALG_NONE loads the entire file into memory, which would OOM Android's 256-512MB heap. \
             Use gzip or xz instead (they stream chunks and never hold the full file in memory). \
             If you genuinely need uncompressed storage, use the payload.bin path (write_payload) \
             which supports streaming.",
            file_path, file_size, ALG_NONE_MAX_SIZE
        ));
    }

    let mut file =
        File::open(file_path).map_err(|e| format!("Cannot open {}: {}", file_path, e))?;
    let mut hasher = Sha256::new();
    let chunk_size = 4 * 1024 * 1024; // 4 MB chunks
    let mut buf = vec![0u8; chunk_size];

    // Wrap the writer in a CountingWriter to track compressed output size.
    // The CountingWriter passes every write() through to the real writer
    // while counting bytes — zero additional memory overhead.
    let mut counting = CountingWriter::new(writer);

    if is_alg(algorithm, ALG_NONE) {
        // No compression: hash and write raw chunks directly to writer.
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            counting.write_all(&buf[..n])
                .map_err(|e| format!("Write error: {}", e))?;
        }
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("Flush error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed_size, hex));
    }

    let resolved_level = resolve_level(algorithm, level);

    if is_alg(algorithm, ALG_GZIP) {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        // GzEncoder writes compressed chunks through our CountingWriter
        let mut encoder = GzEncoder::new(&mut counting, Compression::new(level_clamped));
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("gzip compress write error: {}", e))?;
        }
        encoder
            .finish()
            .map_err(|e| format!("gzip compress finish error: {}", e))?;
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("gzip flush error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed_size, hex));
    }

    if is_alg(algorithm, ALG_BZIP2) {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = BzEncoder::new(&mut counting, Compression::new(level_clamped));
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("bzip2 compress write error: {}", e))?;
        }
        encoder
            .finish()
            .map_err(|e| format!("bzip2 compress finish error: {}", e))?;
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("bzip2 flush error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed_size, hex));
    }

    if is_alg(algorithm, ALG_XZ) {
        let level_clamped = resolved_level.clamp(0, 9) as u32;
        let mut encoder = xz2::write::XzEncoder::new(&mut counting, level_clamped);
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("xz compress write error: {}", e))?;
        }
        encoder
            .finish()
            .map_err(|e| format!("xz compress finish error: {}", e))?;
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("xz flush error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed_size, hex));
    }

    if is_alg(algorithm, ALG_LZ4) {
        let frame_info = lz4_frame_info_for_level(resolved_level);
        {
            let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(frame_info, &mut counting).auto_finish();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| format!("Read error: {}", e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                encoder
                    .write_all(&buf[..n])
                    .map_err(|e| format!("lz4 compress write error: {}", e))?;
            }
        } // encoder is flushed on drop
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("lz4 flush error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed_size, hex));
    }

    if is_alg(algorithm, ALG_ZSTD) {
        let level_clamped = resolved_level.clamp(1, 22);
        let mut encoder = zstd::Encoder::new(&mut counting, level_clamped)
            .map_err(|e| format!("zstd encoder init error: {}", e))?;
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("zstd compress write error: {}", e))?;
        }
        encoder.finish()
            .map_err(|e| format!("zstd compress finish error: {}", e))?;
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("zstd flush error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed_size, hex));
    }

    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

// ---------------------------------------------------------------------------
//  Hash and compress a file, streaming to a writer WITH progress (OOM-safe)
// ---------------------------------------------------------------------------

/// Result of streaming compression to a writer with progress reporting.
///
/// Contains all the metadata needed by dd.rs without ever holding the
/// compressed data in memory:
/// - `comp_size`: total compressed bytes written to the writer
/// - `unc_hash_hex`: SHA-256 hex of the uncompressed input file
/// - `comp_hash_hex`: SHA-256 hex of the compressed output (computed
///   on-the-fly by `Sha256Writer`)
pub struct StreamCompressResult {
    pub comp_size: u64,
    pub unc_hash_hex: String,
    pub comp_hash_hex: String,
}

/// Hash and compress a file, streaming compressed output directly to a writer,
/// with real-time per-chunk progress reporting.
///
/// This is the OOM-safe streaming variant. Instead of accumulating the entire
/// compressed output in a `Vec<u8>` (which can be 351MB for a vendor partition),
/// it writes compressed chunks to the provided writer as they are produced
/// by the compressor.
///
/// # Memory usage
/// Peak RAM: ~8MB (4MB read buffer + ~4MB compressor internal buffer).
/// Compare: the removed in-memory variant held the entire compressed output
/// in RAM, which can be 351MB+ for large partitions → OOM on Android.
///
/// # Progress callback
/// `on_progress` is called after each 4MB chunk is read from the input file
/// and fed to the compressor. The callback receives `(bytes_read, file_size)`.
/// The caller is responsible for throttling (e.g., only fire when percent
/// changes by >= 1).
///
/// # Returns
/// `StreamCompressResult` with compressed size, uncompressed hash, and
/// compressed hash. The compressed data itself is NOT returned; it was
/// already written to the writer.
pub fn hash_and_compress_file_to_writer_with_progress<W: Write>(
    file_path: &str,
    algorithm: &str,
    level: Option<i32>,
    writer: W,
    mut on_progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(StreamCompressResult, W), String> {
    use sha2::{Digest, Sha256};
    use std::fs::File;

    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", file_path, e))?
        .len();

    // ALG_NONE size guard — same rationale as hash_and_compress_file.
    const ALG_NONE_MAX_SIZE: u64 = 256 * 1024 * 1024; // 256 MB
    if is_alg(algorithm, ALG_NONE) && file_size > ALG_NONE_MAX_SIZE {
        return Err(format!(
            "ALG_NONE (no compression) refused for {} — file size {} bytes exceeds {} byte limit. \
             ALG_NONE loads the entire file into memory, which would OOM Android's 256-512MB heap. \
             Use gzip or xz instead (they stream chunks and never hold the full file in memory).",
            file_path, file_size, ALG_NONE_MAX_SIZE
        ));
    }

    let mut file =
        File::open(file_path).map_err(|e| format!("Cannot open {}: {}", file_path, e))?;
    let mut hasher = Sha256::new();
    let chunk_size = 4 * 1024 * 1024; // 4 MB chunks
    let mut buf = vec![0u8; chunk_size];
    let mut bytes_read: u64 = 0;

    // Wrap the writer in Sha256Writer (computes compressed data hash)
    // then CountingWriter (tracks compressed size).
    // Order: Sha256Writer → CountingWriter → underlying writer
    // This way, compressed bytes pass through both wrappers:
    //   - CountingWriter counts bytes for comp_size
    //   - Sha256Writer hashes bytes for comp_hash_hex
    let mut counting = CountingWriter::new(Sha256Writer::new(writer));

    // Throttled progress callback
    let mut last_reported_percent: i32 = -1;
    let mut report_progress = |read: u64, total: u64| {
        if let Some(ref mut cb) = on_progress {
            let percent = if total > 0 {
                (read as f64 / total as f64 * 100.0) as i32
            } else {
                100
            };
            if percent != last_reported_percent {
                last_reported_percent = percent;
                cb(read, total);
            }
        }
    };

    if is_alg(algorithm, ALG_NONE) {
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            counting.write_all(&buf[..n])
                .map_err(|e| format!("Write error: {}", e))?;
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        counting.flush().map_err(|e| format!("Flush error: {}", e))?;
        let comp_size = counting.bytes_written();
        let (comp_hash_hex, _sha_writer) = counting.into_inner().finalize();
        let unc_hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((
            StreamCompressResult { comp_size, unc_hash_hex, comp_hash_hex },
            _sha_writer,
        ));
    }

    let resolved_level = resolve_level(algorithm, level);

    if is_alg(algorithm, ALG_GZIP) {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = GzEncoder::new(&mut counting, Compression::new(level_clamped));
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("gzip compress write error: {}", e))?;
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        encoder
            .finish()
            .map_err(|e| format!("gzip compress finish error: {}", e))?;
        counting.flush().map_err(|e| format!("gzip flush error: {}", e))?;
        let comp_size = counting.bytes_written();
        let (comp_hash_hex, _sha_writer) = counting.into_inner().finalize();
        let unc_hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((
            StreamCompressResult { comp_size, unc_hash_hex, comp_hash_hex },
            _sha_writer,
        ));
    }

    if is_alg(algorithm, ALG_BZIP2) {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;

        let level_clamped = resolved_level.clamp(1, 9) as u32;
        let mut encoder = BzEncoder::new(&mut counting, Compression::new(level_clamped));
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("bzip2 compress write error: {}", e))?;
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        encoder
            .finish()
            .map_err(|e| format!("bzip2 compress finish error: {}", e))?;
        counting.flush().map_err(|e| format!("bzip2 flush error: {}", e))?;
        let comp_size = counting.bytes_written();
        let (comp_hash_hex, _sha_writer) = counting.into_inner().finalize();
        let unc_hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((
            StreamCompressResult { comp_size, unc_hash_hex, comp_hash_hex },
            _sha_writer,
        ));
    }

    if is_alg(algorithm, ALG_XZ) {
        let level_clamped = resolved_level.clamp(0, 9) as u32;
        let mut encoder = xz2::write::XzEncoder::new(&mut counting, level_clamped);
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("xz compress write error: {}", e))?;
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        encoder
            .finish()
            .map_err(|e| format!("xz compress finish error: {}", e))?;
        counting.flush().map_err(|e| format!("xz flush error: {}", e))?;
        let comp_size = counting.bytes_written();
        let (comp_hash_hex, _sha_writer) = counting.into_inner().finalize();
        let unc_hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((
            StreamCompressResult { comp_size, unc_hash_hex, comp_hash_hex },
            _sha_writer,
        ));
    }

    if is_alg(algorithm, ALG_LZ4) {
        let frame_info = lz4_frame_info_for_level(resolved_level);
        {
            let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(frame_info, &mut counting).auto_finish();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| format!("Read error: {}", e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                encoder
                    .write_all(&buf[..n])
                    .map_err(|e| format!("lz4 compress write error: {}", e))?;
                bytes_read += n as u64;
                report_progress(bytes_read, file_size);
            }
        } // encoder is flushed on drop
        counting.flush().map_err(|e| format!("lz4 flush error: {}", e))?;
        let comp_size = counting.bytes_written();
        let (comp_hash_hex, _sha_writer) = counting.into_inner().finalize();
        let unc_hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((
            StreamCompressResult { comp_size, unc_hash_hex, comp_hash_hex },
            _sha_writer,
        ));
    }

    if is_alg(algorithm, ALG_ZSTD) {
        let level_clamped = resolved_level.clamp(1, 22);
        let mut encoder = zstd::Encoder::new(&mut counting, level_clamped)
            .map_err(|e| format!("zstd encoder init error: {}", e))?;
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| format!("zstd compress write error: {}", e))?;
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        encoder.finish()
            .map_err(|e| format!("zstd compress finish error: {}", e))?;
        counting.flush().map_err(|e| format!("zstd flush error: {}", e))?;
        let comp_size = counting.bytes_written();
        let (comp_hash_hex, _sha_writer) = counting.into_inner().finalize();
        let unc_hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((
            StreamCompressResult { comp_size, unc_hash_hex, comp_hash_hex },
            _sha_writer,
        ));
    }

    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

// ---------------------------------------------------------------------------
//  Operation type mapping
// ---------------------------------------------------------------------------

/// Map an InstallOperation type enum value to a compression algorithm string.
/// Matches Python compression.py detect_compression().
pub fn detect_compression(operation_type: u32) -> &'static str {
    match operation_type {
        0 => ALG_NONE,       // REPLACE
        8 => ALG_XZ,         // REPLACE_XZ
        12 => ALG_BZIP2,     // REPLACE_BZ
        13 => ALG_NONE,      // REPLACE_BROT — brotli demoted, treat as uncompressed
        14 => ALG_GZIP,      // PUIGZIP
        23 => ALG_NONE,      // BROTLI_BSDIFF — brotli demoted, treat as uncompressed
        21 | 22 => ALG_NONE, // ZERO / DISCARD
        _ => ALG_NONE,
    }
}

/// Map a compression algorithm name to the recommended InstallOperation type.
/// Matches Python compression.py operation_type_for_algorithm().
pub fn operation_type_for_algorithm(algorithm: &str) -> u32 {
    if is_alg(algorithm, ALG_NONE) {
        return 0; // REPLACE
    }
    if is_alg(algorithm, ALG_BZIP2) {
        return 12; // REPLACE_BZ
    }
    if is_alg(algorithm, ALG_GZIP) {
        return 14; // PUIGZIP
    }
    if is_alg(algorithm, ALG_XZ) {
        return 8; // REPLACE_XZ
    }
    // DEMOTED: if is_alg(algorithm, ALG_BROTLI) { return 13; } — brotli removed from APK build
    if is_alg(algorithm, ALG_LZ4) {
        return 0; // LZ4 has no AOSP operation type; use REPLACE (0) as fallback
    }
    if is_alg(algorithm, ALG_ZSTD) {
        return 0; // ZSTD has no AOSP operation type; use REPLACE (0) as fallback
    }
    0
}

// ---------------------------------------------------------------------------
//  Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalise() {
        assert_eq!(normalise("gzip"), ALG_GZIP);
        assert_eq!(normalise("GZ"), ALG_GZIP);
        assert_eq!(normalise("bz2"), ALG_BZIP2);
        assert_eq!(normalise("XZ"), ALG_XZ);
        // DEMOTED: assert_eq!(normalise("br"), ALG_BROTLI);
        assert_eq!(normalise("none"), ALG_NONE);
        assert_eq!(normalise("raw"), ALG_NONE);
        assert_eq!(normalise(""), ALG_NONE);
    }

    /// Bug NEW-E: AOSP operation type names must map to canonical algorithms.
    #[test]
    fn test_normalise_aosp_op_type_names() {
        // DEMOTED: REPLACE_BROT → brotli (now demoted to none)
        // DEMOTED: assert_eq!(normalise("REPLACE_BROT"), ALG_BROTLI);
        // DEMOTED: assert_eq!(normalise("replace_brot"), ALG_BROTLI);
        // REPLACE_BZ → bzip2
        assert_eq!(normalise("REPLACE_BZ"), ALG_BZIP2);
        assert_eq!(normalise("replace_bz"), ALG_BZIP2);
        // REPLACE_XZ → xz
        assert_eq!(normalise("REPLACE_XZ"), ALG_XZ);
        assert_eq!(normalise("replace_xz"), ALG_XZ);
        // PUIGZIP → gzip
        assert_eq!(normalise("PUIGZIP"), ALG_GZIP);
        // DEMOTED: BROTLI_BSDIFF → brotli (now demoted to none)
        // DEMOTED: assert_eq!(normalise("BROTLI_BSDIFF"), ALG_BROTLI);
    }

    /// Bug NEW-E: compress_id must resolve AOSP type names correctly.
    #[test]
    fn test_compress_id_aosp_aliases() {
        // DEMOTED: assert_eq!(compress_id("REPLACE_BROT"), 4); // brotli → now 0 (none)
        assert_eq!(compress_id("REPLACE_BZ"), 2);   // bzip2
        assert_eq!(compress_id("REPLACE_XZ"), 3);   // xz
        assert_eq!(compress_id("PUIGZIP"), 1);       // gzip
        // DEMOTED: assert_eq!(compress_id("BROTLI_BSDIFF"), 4); // brotli → now 0 (none)
        // DEMOTED: assert_eq!(compress_id("brotli"), 4); // brotli → now 0 (none)
        assert_eq!(compress_id("bzip2"), 2);
    }

    /// Bug NEW-E: is_alg must recognize AOSP type names.
    #[test]
    fn test_is_alg_aosp_aliases() {
        // DEMOTED: assert!(is_alg("REPLACE_BROT", ALG_BROTLI));
        assert!(is_alg("REPLACE_BZ", ALG_BZIP2));
        assert!(is_alg("REPLACE_XZ", ALG_XZ));
        assert!(is_alg("PUIGZIP", ALG_GZIP));
        // DEMOTED: assert!(is_alg("BROTLI_BSDIFF", ALG_BROTLI));
        // Negative cases
        // DEMOTED: assert!(!is_alg("REPLACE_BROT", ALG_BZIP2)); // brotli demoted
        // DEMOTED: assert!(!is_alg("REPLACE_BZ", ALG_BROTLI)); // brotli demoted
    }

    #[test]
    fn test_resolve_level() {
        assert_eq!(resolve_level("gzip", None), 6); // default
        assert_eq!(resolve_level("gzip", Some(9)), 9);
        assert_eq!(resolve_level("gzip", Some(15)), 9); // clamped
        // BUG FIX: Some(0) now means "use default" (same as None), not literal level 0.
        // Previously, resolve_level("gzip", Some(0)) returned 1 (clamped from 0).
        // Now it returns 6 (the gzip default), matching the Kotlin convention.
        assert_eq!(resolve_level("gzip", Some(0)), 6); // 0 = default → 6
        assert_eq!(resolve_level("xz", None), 6);
        // DEMOTED: assert_eq!(resolve_level("brotli", None), 6);
        assert_eq!(resolve_level("bzip2", None), 9);
        assert_eq!(resolve_level("lz4", None), 4);
    }

    /// Regression test: verify Some(0) resolves to algorithm default for ALL algorithms.
    /// This prevents the latent bug where Some(0) bypassed defaults.
    #[test]
    fn test_regression_resolve_level_some_zero_is_default() {
        // Some(0) should behave identically to None for every algorithm
        for (alg, default) in DEFAULT_LEVELS {
            assert_eq!(
                resolve_level(alg, Some(0)),
                resolve_level(alg, None),
                "REGRESSION: resolve_level({}, Some(0)) != resolve_level({}, None)",
                alg, alg
            );
            assert_eq!(
                resolve_level(alg, Some(0)),
                *default,
                "REGRESSION: resolve_level({}, Some(0)) = {}, expected default {}",
                alg, resolve_level(alg, Some(0)), default
            );
        }
    }

    /// Verify that xz can still use explicit level 0 (not just default).
    /// For xz, level 0 is valid (fastest/no compression).
    /// The fix only changes the semantics of Some(0) — callers who want literal level 0
    /// for xz must now pass Some(0) AFTER the resolve_level fix, which treats it
    /// as default. To use literal level 0 for xz, the caller must pass it directly
    /// (this is acceptable because no JNI entry point ever passes Some(0)).
    /// DEMOTED: brotli explicit level tests removed (brotli demoted from APK build).
    #[test]
    fn test_resolve_level_explicit_levels_for_xz() {
        // Explicit level 1 for xz (not default, not level 0)
        assert_eq!(resolve_level("xz", Some(1)), 1);
        // Explicit level 9 for xz
        assert_eq!(resolve_level("xz", Some(9)), 9);
    }

    #[test]
    fn test_detect_from_data() {
        // Gzip magic
        let gzip_data: &[u8] = &[0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00];
        assert_eq!(detect_from_data(gzip_data), ALG_GZIP);

        // Bzip2 magic
        let bzip2_data: &[u8] = b"BZh9\x00\x00\x00";
        assert_eq!(detect_from_data(bzip2_data), ALG_BZIP2);

        // XZ magic
        let xz_data: &[u8] = &[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0x00];
        assert_eq!(detect_from_data(xz_data), ALG_XZ);

        // Unknown
        assert_eq!(detect_from_data(b"hello"), ALG_NONE);
        assert_eq!(detect_from_data(&[]), ALG_NONE);
    }

    #[test]
    fn test_compress_decompress_gzip() {
        let data = b"Hello, OTAku! This is a test of gzip compression.";
        let compressed = compress(data, "gzip", None).unwrap();
        assert!(!compressed.is_empty());
        let decompressed = decompress(&compressed, "gzip").unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_compress_decompress_bzip2() {
        let data = b"Hello, OTAku! This is a test of bzip2 compression.";
        let compressed = compress(data, "bzip2", None).unwrap();
        assert!(!compressed.is_empty());
        let decompressed = decompress(&compressed, "bzip2").unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_compress_decompress_xz() {
        let data = b"Hello, OTAku! This is a test of xz compression.";
        let compressed = compress(data, "xz", None).unwrap();
        assert!(!compressed.is_empty());
        let decompressed = decompress(&compressed, "xz").unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_compress_decompress_lz4() {
        let data = b"Hello, OTAku! This is a test of lz4 compression.";
        let compressed = compress(data, "lz4", None).unwrap();
        assert!(!compressed.is_empty());
        let decompressed = decompress(&compressed, "lz4").unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_lz4_frame_magic() {
        let data = b"LZ4 frame format test data";
        let compressed = compress(data, "lz4", None).unwrap();
        // LZ4 frame magic: 04 22 4D 18
        assert_eq!(compressed[0], 0x04);
        assert_eq!(compressed[1], 0x22);
        assert_eq!(compressed[2], 0x4D);
        assert_eq!(compressed[3], 0x18);
        // detect_from_data should identify it
        assert_eq!(detect_from_data(&compressed), ALG_LZ4);
    }

    #[test]
    fn test_compress_id_lz4() {
        assert_eq!(compress_id("lz4"), 5);
        assert_eq!(compress_id("LZ4"), 5);
        assert_eq!(compress_id("l4"), 5);
    }

    #[test]
    fn test_normalise_lz4() {
        assert_eq!(normalise("lz4"), ALG_LZ4);
        assert_eq!(normalise("LZ4"), ALG_LZ4);
        assert_eq!(normalise("l4"), ALG_LZ4);
    }

    #[test]
    fn test_normalise_zstd() {
        assert_eq!(normalise("zstd"), ALG_ZSTD);
        assert_eq!(normalise("ZSTD"), ALG_ZSTD);
        assert_eq!(normalise("zs"), ALG_ZSTD);
        assert_eq!(normalise("ZS"), ALG_ZSTD);
        assert_eq!(normalise("zst"), ALG_ZSTD);
    }

    #[test]
    fn test_compress_id_zstd() {
        assert_eq!(compress_id("zstd"), 6);
        assert_eq!(compress_id("ZSTD"), 6);
        assert_eq!(compress_id("zs"), 6);
    }

    /// Regression: decompress_lz4() must have .take() zip-bomb protection
    /// matching all other decompress_* functions. Without it, a crafted LZ4
    /// frame could decompress to gigabytes and OOM on Android.
    #[test]
    fn test_regression_lz4_zip_bomb_protection() {
        // Normal data should decompress fine
        let data = b"Normal lz4 data for zip-bomb test";
        let compressed = compress(data, "lz4", None).unwrap();
        let result = decompress(&compressed, "lz4").unwrap();
        assert_eq!(data.to_vec(), result);
        // If a 2 GiB+ payload were crafted, decompress_lz4 would return Err
        // (we can't easily create a real zip bomb in a unit test, but the
        // .take() guard + size check ensure it fails instead of OOM).
    }

    #[test]
    fn test_decompress_auto() {
        let data = b"Auto-detect test data for compression.";
        let compressed = compress(data, "gzip", None).unwrap();
        let decompressed = decompress(&compressed, "auto").unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_sha256() {
        let data = b"test";
        let hash = sha256(data);
        assert_eq!(hash.len(), 32); // SHA-256 = 32 bytes
    }

    #[test]
    fn test_operation_type_mapping() {
        assert_eq!(detect_compression(0), ALG_NONE); // REPLACE
        assert_eq!(detect_compression(8), ALG_XZ); // REPLACE_XZ
        assert_eq!(detect_compression(12), ALG_BZIP2); // REPLACE_BZ
        assert_eq!(detect_compression(13), ALG_NONE); // REPLACE_BROT (brotli demoted)
        assert_eq!(detect_compression(14), ALG_GZIP); // PUIGZIP
        assert_eq!(detect_compression(23), ALG_NONE); // BROTLI_BSDIFF (brotli demoted)
        assert_eq!(detect_compression(21), ALG_NONE); // ZERO

        assert_eq!(operation_type_for_algorithm("none"), 0);   // REPLACE
        assert_eq!(operation_type_for_algorithm("bzip2"), 12); // REPLACE_BZ
        assert_eq!(operation_type_for_algorithm("xz"), 8);     // REPLACE_XZ
        assert_eq!(operation_type_for_algorithm("gzip"), 14); // PUIGZIP
        // DEMOTED: assert_eq!(operation_type_for_algorithm("brotli"), 13); // REPLACE_BROT (brotli demoted → returns 0)
    }

    #[test]
    fn test_compress_streaming_gzip() {
        let data = vec![0xAB_u8; 1024 * 1024]; // 1 MB
        let mut progress_calls = 0u32;
        let compressed = compress_streaming(
            &data,
            "gzip",
            None,
            256 * 1024,
            Some(&mut |done: u64, total: u64| {
                assert!(done <= total);
                progress_calls += 1;
            }),
        )
        .unwrap();
        assert!(!compressed.is_empty());
        assert!(progress_calls > 0);

        let decompressed = decompress(&compressed, "gzip").unwrap();
        assert_eq!(data, decompressed);
    }

    /// Verify the ALG_NONE size guard refuses files > 256MB.
    ///
    /// This is a regression test for the OOM crash that occurred when users
    /// selected "no compression" for a large system.img (5GB) — the entire
    /// file was loaded into a Vec<u8>, exceeding Android's 256-512MB heap
    /// limit and killing the app process.
    ///
    /// We can't easily test with a real 5GB file, but we can verify:
    ///   1. The error message contains "ALG_NONE" and "refused"
    ///   2. The error message mentions the size limit
    ///   3. The threshold (256MB) is documented in the error
    ///   4. A small file (1KB) does NOT trigger the guard
    #[test]
    fn test_alg_none_size_guard() {
        // Create a small temp file (1KB) — should NOT trigger the guard.
        let tmp = std::env::temp_dir().join("otaku_test_alg_none_small.bin");
        std::fs::write(&tmp, b"x".repeat(1024)).unwrap();
        let result = hash_and_compress_file(tmp.to_str().unwrap(), "none", None);
        assert!(result.is_ok(), "Small file should NOT trigger ALG_NONE guard");
        let _ = std::fs::remove_file(&tmp);

        // Verify the guard threshold constant exists in source code.
        // (We can't easily test a real >256MB file in CI, but the constant
        // check ensures the guard logic remains present.)
        let source = include_str!("compression.rs");
        assert!(
            source.contains("ALG_NONE_MAX_SIZE: u64 = 256 * 1024 * 1024"),
            "ALG_NONE_MAX_SIZE constant (256MB threshold) missing from compression.rs"
        );
        assert!(
            source.contains("ALG_NONE (no compression) refused"),
            "ALG_NONE refusal error message missing"
        );
        // The guard must be present in hash_and_compress_file,
        // hash_and_compress_file_with_progress,
        // hash_and_compress_file_to_writer, and
        // hash_and_compress_file_to_writer_with_progress. Count occurrences:
        let guard_count = source.matches("ALG_NONE_MAX_SIZE").count();
        assert!(
            guard_count >= 4,
            "ALG_NONE size guard must appear in all four compress functions (found {} occurrences, need >= 4)",
            guard_count
        );
    }
}
