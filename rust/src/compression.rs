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
fn is_alg(algorithm: &str, target: &str) -> bool {
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
    if data.len() >= 6 {
        if data[..6] == [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00] {
            return ALG_XZ;
        }
    }

    // Brotli: no reliable magic, try trial decompression
    if data.len() >= 3 {
        use brotli::Decompressor;
        let mut dec = Decompressor::new(data, 4096);
        let mut probe = [0u8; 1];
        if dec.read(&mut probe).is_ok() {
            return ALG_BROTLI;
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

fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::read::GzDecoder;

    let mut decoder = GzDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| format!("gzip decompress error: {}", e))?;
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

    let mut decoder = BzDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| format!("bzip2 decompress error: {}", e))?;
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
    let mut decoder = xz2::read::XzDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| format!("xz decompress error: {}", e))?;
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
    let mut decoder = brotli::Decompressor::new(data, 4096);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| format!("brotli decompress error: {}", e))?;
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
    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", file_path, e))?
        .len();

    let mut file =
        File::open(file_path).map_err(|e| format!("Cannot open {}: {}", file_path, e))?;
    let mut hasher = Sha256::new();
    let chunk_size = 4 * 1024 * 1024; // 4 MB chunks
    let mut buf = vec![0u8; chunk_size];

    if is_alg(algorithm, ALG_NONE) {
        // No compression: just hash and return raw bytes via streaming copy.
        let mut raw_buf = Vec::with_capacity(file_size as usize);
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
}
