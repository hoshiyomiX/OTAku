//! AOSP OTA payload.bin read / write operations.
//!
//! Phase 2 will implement full payload parsing and generation.
//! Phase 1: Stub module that compiles.

// Phase 1 stub — full implementation in Phase 2

/// Payload magic bytes: "CrAU"
pub const DELTA_MAGIC: [u8; 4] = [b'C', b'r', b'A', b'U'];
pub const HEADER_PROTOBUF_SIZE: usize = 8; // uint64 big-endian
pub const MAJOR_VERSION: u32 = 2; // Brillo v2
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Read and parse a payload.bin file.
/// Phase 2: Full implementation.
pub fn read_payload(_path: &str) -> Result<(), String> {
    Err("read_payload not yet implemented (Phase 2)".to_string())
}

/// Generate a payload.bin from partition images.
/// Phase 2: Full implementation.
pub fn write_payload(
    _output_path: &str,
    _partitions_data: &[()],
    _block_size: u32,
    _minor_version: u32,
) -> Result<(), String> {
    Err("write_payload not yet implemented (Phase 2)".to_string())
}
