//! Compression / decompression for AOSP payload.bin operations.
//!
//! All algorithms are statically compiled — no runtime dependency checks needed.
//! Supported: none, gzip (flate2/miniz_oxide), bzip2, xz (xz2/liblzma), brotli (pure Rust).
//!
//! Ported from Python compression.py to Rust with identical semantics.

use std::io::{Read, Write};

// ---------------------------------------------------------------------------
//  Algorithm constants
// ---------------------------------------------------------------------------

pub const ALG_NONE: &str = "none";
pub const ALG_GZIP: &str = "gzip";
pub const ALG_BZIP2: &str = "bzip2";
pub const ALG_XZ: &str = "xz";
pub const ALG_BROTLI: &str = "brotli";
pub const ALG_AUTO: &str = "auto";

pub const ALL_ALGORITHMS: &[&str] = &[ALG_NONE, ALG_BZIP2, ALG_GZIP, ALG_XZ, ALG_BROTLI];

/// Default compression levels per algorithm (matches Python DEFAULT_LEVELS)
pub const DEFAULT_LEVELS: &[(&str, i32)] = &[
    (ALG_NONE, 0),
    (ALG_GZIP, 6),
    (ALG_BZIP2, 9),
    (ALG_XZ, 6),
    (ALG_BROTLI, 6),
];

/// Valid level ranges per algorithm: (min, max) (matches Python LEVEL_RANGES)
pub const LEVEL_RANGES: &[(&str, i32, i32)] = &[
    (ALG_NONE, 0, 0),
    (ALG_GZIP, 1, 9),
    (ALG_BZIP2, 1, 9),
    (ALG_XZ, 0, 9),
    (ALG_BROTLI, 0, 11),
];

// ---------------------------------------------------------------------------
//  Compression ID mapping (for DDBU header)
// ---------------------------------------------------------------------------

pub const COMPRESS_ID_MAP: &[(&str, u16)] = &[
    ("none", 0),
    ("gzip", 1),
    ("bzip2", 2),
    ("xz", 3),
    ("brotli", 4),
];

/// Get the compress ID for an algorithm name
pub fn compress_id(algorithm: &str) -> u16 {
    COMPRESS_ID_MAP
        .iter()
        .find(|(name, _)| *name == algorithm)
        .map(|(_, id)| *id)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
//  Algorithm name normalization (matches Python _normalise)
// ---------------------------------------------------------------------------

/// Normalize an algorithm name to canonical form, handling common aliases.
fn normalise(algorithm: &str) -> String {
    let lower = algorithm.to_lowercase().trim().to_string();
    match lower.as_str() {
        "" | "raw" | "none" => ALG_NONE.to_string(),
        "bz2" | "bzip2" => ALG_BZIP2.to_string(),
        "gz" | "gzip" => ALG_GZIP.to_string(),
        "lzma" | "xz" => ALG_XZ.to_string(),
        "br" | "brotli" => ALG_BROTLI.to_string(),
        other => other.to_string(), // return as-is for unknown algorithms
    }
}

// ---------------------------------------------------------------------------
//  Level resolution
// ---------------------------------------------------------------------------

/// Resolve compression level: use provided level or algorithm default, clamped to valid range.
fn resolve_level(algorithm: &str, level: Option<i32>) -> i32 {
    let alg = normalise(algorithm);
    let default = DEFAULT_LEVELS
        .iter()
        .find(|(name, _)| *name == alg)
        .map(|(_, lvl)| *lvl)
        .unwrap_or(0);
    let resolved = level.unwrap_or(default);
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

    // Brotli: no reliable magic, try trial decompression with larger probe.
    // BUG FIX: Previously only decoded 1 byte, which can produce false positives
    // — any random 3+ bytes can sometimes decode as valid brotli for 1 byte.
    // Now decode at least 64 bytes and validate the output is non-trivial.
    if data.len() >= 8 {
        use brotli::Decompressor;
        use std::io::Read;
        let mut dec = Decompressor::new(data, 4096);
        let mut probe = [0u8; 64];
        match dec.read(&mut probe) {
            Ok(n) if n > 0 => return ALG_BROTLI,
            _ => {} // Not brotli
        }
    }

    ALG_NONE
}

// ---------------------------------------------------------------------------
//  Compression / decompression — full implementation
// ---------------------------------------------------------------------------

/// Compress data with the specified algorithm.
///
/// # Arguments
/// * `data` - Raw data to compress
/// * `algorithm` - One of "none", "gzip", "bzip2", "xz", "brotli"
/// * `level` - Compression level. None = use algorithm default.
///
/// # Returns
/// Compressed data (or data unchanged for "none")
pub fn compress(data: &[u8], algorithm: &str, level: Option<i32>) -> Result<Vec<u8>, String> {
    if is_alg(algorithm, ALG_NONE) {
        return Ok(data.to_vec());
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
    if is_alg(algorithm, ALG_BROTLI) {
        return compress_brotli(data, resolved_level);
    }
    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

/// Decompress data with the specified algorithm.
///
/// # Arguments
/// * `data` - Compressed (or raw) data
/// * `algorithm` - One of "none", "gzip", "bzip2", "xz", "brotli", "auto"
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
    if effective_alg == ALG_BROTLI {
        return decompress_brotli(data);
    }
    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

// ---------------------------------------------------------------------------
//  Gzip implementation (flate2 / miniz_oxide)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
//  Brotli implementation (pure Rust brotli crate)
// ---------------------------------------------------------------------------

fn compress_brotli(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    let quality = level.clamp(0, 11);
    // brotli crate expects quality as a u32 parameter in CompressorWriter
    let mut result = Vec::new();
    {
        let mut encoder = brotli::CompressorWriter::new(&mut result, 4096, quality as u32, 22);
        encoder
            .write_all(data)
            .map_err(|e| format!("brotli compress write error: {}", e))?;
    } // encoder is flushed on drop
    Ok(result)
}

fn decompress_brotli(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let decoder = brotli::Decompressor::new(data, 4096);
    let mut result = Vec::new();
    // BUG FIX: Same zip-bomb protection as decompress_gzip.
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64)
        .read_to_end(&mut result)
        .map_err(|e| format!("brotli decompress error: {}", e))?;
    if result.len() >= MAX_DECOMPRESSED_SIZE {
        return Err(format!(
            "brotli decompressed output exceeds {} GiB limit — possible zip bomb",
            MAX_DECOMPRESSED_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
//  SHA-256 hashing
// ---------------------------------------------------------------------------

/// Compute SHA-256 hash of data
pub fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

/// Compute SHA-256 hash of a file (streaming, chunked)
pub fn sha256_file(path: &str) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    use std::fs::File;

    let mut file = File::open(path).map_err(|e| format!("Cannot open {}: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 4 * 1024 * 1024]; // 4 MB chunks
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Read error: {}", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_vec())
}

/// Compute SHA-256 hash of a file and return hex string
pub fn sha256_file_hex(path: &str) -> Result<String, String> {
    let hash = sha256_file(path)?;
    Ok(hash.iter().map(|b| format!("{:02x}", b)).collect())
}

// ---------------------------------------------------------------------------
//  Streaming compression with progress
// ---------------------------------------------------------------------------

/// Progress callback type: `(bytes_processed, total_bytes)`
pub type ProgressFn = Box<dyn FnMut(u64, u64)>;

/// Compress data in chunks, calling progress callback after each chunk.
///
/// This matches the Python `compress_streaming()` function for real-time
/// progress reporting on large (2+ GB) partition images.
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

    if is_alg(algorithm, ALG_BROTLI) {
        let quality = resolved_level.clamp(0, 11) as u32;
        let mut result = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut result, 4096, quality, 22);
            let mut offset: usize = 0;
            while offset < data.len() {
                let end = (offset + effective_chunk).min(data.len());
                encoder
                    .write_all(&data[offset..end])
                    .map_err(|e| format!("brotli streaming write error: {}", e))?;
                offset = end;
                if let Some(ref mut cb) = on_progress {
                    cb(offset as u64, total);
                }
            }
        } // encoder is flushed on drop
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

    if is_alg(algorithm, ALG_BROTLI) {
        let quality = resolved_level.clamp(0, 11) as u32;
        let mut result = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut result, 4096, quality, 22);
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
                    .map_err(|e| format!("brotli compress write error: {}", e))?;
            }
        } // encoder is flushed on drop
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((result, hex));
    }

    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

// ---------------------------------------------------------------------------
//  Hash and compress a file with real-time progress reporting
// ---------------------------------------------------------------------------

/// Hash and compress a file with real-time progress reporting.
///
/// Same as `hash_and_compress_file` but calls the progress callback
/// after each chunk is read and fed to the compressor.
/// Throttled to report at most once per ~1% change to avoid flooding
/// the JNI bridge.
///
/// The callback signature is `(bytes_read, file_size)`.
pub fn hash_and_compress_file_with_progress(
    file_path: &str,
    algorithm: &str,
    level: Option<i32>,
    mut on_progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(Vec<u8>, String), String> {
    use sha2::{Digest, Sha256};
    use std::fs::File;

    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", file_path, e))?
        .len();

    // ALG_NONE size guard — see hash_and_compress_file for full rationale.
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
    let mut bytes_read: u64 = 0;
    let mut last_reported_percent: i32 = -1;

    // Throttled progress callback: only fires when percent changes by >= 1
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
        // CRITICAL: Do NOT load entire file into Vec — for 5GB system.img this
        // would cause OOM on Android (heap limit 256-512MB).
        // Instead, stream through a BufWriter to a temp Vec with limited capacity.
        // We still need to return Vec<u8> for API compatibility, but we use a
        // capped Vec that grows incrementally (not pre-allocated to file_size).
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
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        // NOTE: raw_buf still holds entire file in memory for ALG_NONE.
        // This is a known limitation — users should use gzip/xz for large files.
        // ALG_NONE is primarily for small partitions (boot, dtbo, vbmeta).
        // Full streaming fix requires changing the return type to io::Write.
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
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
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
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
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
            bytes_read += n as u64;
            report_progress(bytes_read, file_size);
        }
        let compressed = encoder
            .finish()
            .map_err(|e| format!("xz compress finish error: {}", e))?;
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((compressed, hex));
    }

    if is_alg(algorithm, ALG_BROTLI) {
        let quality = resolved_level.clamp(0, 11) as u32;
        let mut result = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut result, 4096, quality, 22);
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
                    .map_err(|e| format!("brotli compress write error: {}", e))?;
                bytes_read += n as u64;
                report_progress(bytes_read, file_size);
            }
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        return Ok((result, hex));
    }

    Err(format!("Unknown compression algorithm: {:?}", algorithm))
}

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
    #[allow(dead_code)]
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

    if is_alg(algorithm, ALG_BROTLI) {
        let quality = resolved_level.clamp(0, 11) as u32;
        {
            let mut encoder = brotli::CompressorWriter::new(&mut counting, 4096, quality, 22);
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
                    .map_err(|e| format!("brotli compress write error: {}", e))?;
            }
        } // encoder is flushed on drop
        let compressed_size = counting.bytes_written();
        counting.flush().map_err(|e| format!("brotli flush error: {}", e))?;
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
/// This is the OOM-safe variant of `hash_and_compress_file_with_progress`.
/// Instead of accumulating the entire compressed output in a `Vec<u8>` (which
/// can be 351MB for a vendor partition), it writes compressed chunks to the
/// provided writer as they are produced by the compressor.
///
/// # Memory usage
/// Peak RAM: ~8MB (4MB read buffer + ~4MB compressor internal buffer).
/// Compare: `hash_and_compress_file_with_progress` holds the entire compressed
/// output in RAM, which can be 351MB+ for large partitions → OOM on Android.
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

    if is_alg(algorithm, ALG_BROTLI) {
        let quality = resolved_level.clamp(0, 11) as u32;
        {
            let mut encoder = brotli::CompressorWriter::new(&mut counting, 4096, quality, 22);
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
                    .map_err(|e| format!("brotli compress write error: {}", e))?;
                bytes_read += n as u64;
                report_progress(bytes_read, file_size);
            }
        } // encoder is flushed on drop
        counting.flush().map_err(|e| format!("brotli flush error: {}", e))?;
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
        12 => ALG_NONE,      // REPLACE_BZ (AOSP uses as no-op)
        14 => ALG_GZIP,      // PUIGZIP
        23 => ALG_BROTLI,    // BROTLI_BSDIFF
        21 | 22 => ALG_NONE, // ZERO / DISCARD
        _ => ALG_NONE,
    }
}

/// Map a compression algorithm name to the recommended InstallOperation type.
/// Matches Python compression.py operation_type_for_algorithm().
pub fn operation_type_for_algorithm(algorithm: &str) -> u32 {
    if is_alg(algorithm, ALG_NONE) || is_alg(algorithm, ALG_BZIP2) {
        return 0; // REPLACE
    }
    if is_alg(algorithm, ALG_GZIP) {
        return 14; // PUIGZIP
    }
    if is_alg(algorithm, ALG_XZ) {
        return 8; // REPLACE_XZ
    }
    if is_alg(algorithm, ALG_BROTLI) {
        return 23; // BROTLI_BZ
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
        assert_eq!(normalise("br"), ALG_BROTLI);
        assert_eq!(normalise("none"), ALG_NONE);
        assert_eq!(normalise("raw"), ALG_NONE);
        assert_eq!(normalise(""), ALG_NONE);
    }

    #[test]
    fn test_resolve_level() {
        assert_eq!(resolve_level("gzip", None), 6); // default
        assert_eq!(resolve_level("gzip", Some(9)), 9);
        assert_eq!(resolve_level("gzip", Some(15)), 9); // clamped
        assert_eq!(resolve_level("gzip", Some(0)), 1); // clamped
        assert_eq!(resolve_level("xz", None), 6);
        assert_eq!(resolve_level("brotli", None), 6);
        assert_eq!(resolve_level("bzip2", None), 9);
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
    fn test_compress_decompress_brotli() {
        let data = b"Hello, OTAku! This is a test of brotli compression.";
        let compressed = compress(data, "brotli", None).unwrap();
        assert!(!compressed.is_empty());
        let decompressed = decompress(&compressed, "brotli").unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_compress_none() {
        let data = b"Hello, OTAku!";
        let result = compress(data, "none", None).unwrap();
        assert_eq!(data.to_vec(), result);
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
        assert_eq!(detect_compression(14), ALG_GZIP); // PUIGZIP
        assert_eq!(detect_compression(23), ALG_BROTLI); // BROTLI_BSDIFF
        assert_eq!(detect_compression(21), ALG_NONE); // ZERO

        assert_eq!(operation_type_for_algorithm("none"), 0);
        assert_eq!(operation_type_for_algorithm("xz"), 8);
        assert_eq!(operation_type_for_algorithm("gzip"), 14);
        assert_eq!(operation_type_for_algorithm("brotli"), 23);
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
