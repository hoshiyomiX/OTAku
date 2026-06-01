//! Compression / decompression for AOSP payload.bin operations.
//!
//! Phase 2 will implement full compression with streaming and progress.
//! Phase 1: Stub module that compiles.

use std::io::Read;

// ---------------------------------------------------------------------------
//  Algorithm constants
// ---------------------------------------------------------------------------

pub const ALG_NONE: &str = "none";
pub const ALG_GZIP: &str = "gzip";
pub const ALG_BZIP2: &str = "bzip2";
pub const ALG_XZ: &str = "xz";
pub const ALG_BROTLI: &str = "brotli";

pub const ALL_ALGORITHMS: &[&str] = &[ALG_NONE, ALG_BZIP2, ALG_GZIP, ALG_XZ, ALG_BROTLI];

/// Default compression levels per algorithm
pub const DEFAULT_LEVELS: &[(&str, i32)] = &[
    (ALG_NONE, 0),
    (ALG_GZIP, 6),
    (ALG_BZIP2, 9),
    (ALG_XZ, 6),
    (ALG_BROTLI, 6),
];

/// Valid level ranges per algorithm: (min, max)
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
//  Compression / decompression stubs (Phase 2 implementation)
// ---------------------------------------------------------------------------

/// Compress data with the specified algorithm.
/// Phase 2: Full implementation with all algorithms and levels.
pub fn compress(data: &[u8], _algorithm: &str, _level: Option<i32>) -> Result<Vec<u8>, String> {
    // Phase 1 stub: just return data as-is
    Ok(data.to_vec())
}

/// Decompress data with the specified algorithm.
/// Phase 2: Full implementation with auto-detect.
pub fn decompress(data: &[u8], _algorithm: &str) -> Result<Vec<u8>, String> {
    // Phase 1 stub: just return data as-is
    Ok(data.to_vec())
}

/// Compute SHA-256 hash of data
pub fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

/// Compute SHA-256 hash of a file (streaming, chunked)
pub fn sha256_file(path: &str) -> Result<Vec<u8>, String> {
    use sha2::{Sha256, Digest};
    use std::fs::File;

    let mut file = File::open(path).map_err(|e| format!("Cannot open {}: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 4 * 1024 * 1024]; // 4 MB chunks
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("Read error: {}", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_vec())
}

/// Hash and compress a file in a single streaming pass.
/// Returns (compressed_data, sha256_hex)
/// Phase 2: Full streaming implementation with progress callback.
pub fn hash_and_compress_file(
    _file_path: &str,
    _algorithm: &str,
    _level: Option<i32>,
) -> Result<(Vec<u8>, String), String> {
    // Phase 1 stub
    Err("hash_and_compress_file not yet implemented (Phase 2)".to_string())
}

/// Map an InstallOperation type enum value to a compression algorithm string.
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
pub fn operation_type_for_algorithm(algorithm: &str) -> u32 {
    match algorithm {
        ALG_NONE | ALG_BZIP2 => 0, // REPLACE
        ALG_GZIP => 14,            // PUIGZIP
        ALG_XZ => 8,               // REPLACE_XZ
        ALG_BROTLI => 23,          // BROTLI_BZ
        _ => 0,
    }
}
