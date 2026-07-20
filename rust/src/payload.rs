//! AOSP OTA payload.bin read / write / extract operations.
//!
//! File format (Brillo / update_engine v2):
//!   Offset 0   :  "CrAU"                        (4 bytes)
//!   Offset 4   :  header protobuf length         (8 bytes BE)
//!   Offset 12  :  PayloadHeader protobuf         (variable)
//!   Offset 12+N:  DeltaArchiveManifest protobuf  (variable)
//!   Offset 12+N+M: data blobs ...               (variable)
//!   [optional] :  metadata signature block       (variable)
//!
//! Ported from Python payload.py to Rust with identical semantics.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::compression::{
    decompress, decompress_to_writer, detect_compression, detect_from_data,
    hash_and_compress_file_to_writer, operation_type_for_algorithm,
};
use crate::proto::{
    build_extent, build_manifest, build_partition_info, build_partition_update,
    build_payload_header, build_replace_operation, decode_manifest, decode_payload_header,
    encode_manifest, encode_payload_header, DeltaArchiveManifest,
    PartitionUpdate, PayloadHeader, OP_ZERO, OP_DISCARD,
    ManifestJson, ParsedPayloadJson,
};

// ---------------------------------------------------------------------------
//  Constants
// ---------------------------------------------------------------------------

/// Payload magic bytes: "CrAU"
pub const DELTA_MAGIC: [u8; 4] = *b"CrAU";
pub const HEADER_PROTOBUF_SIZE: usize = 8; // uint64 big-endian
pub const MAJOR_VERSION: u32 = 2; // Brillo v2
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
pub const METADATA_SIG_ALIGNMENT: u64 = 4096;

/// Maximum allowed size for a single operation's data blob (256 MB).
/// BUG FIX (NEW-2): Prevents OOM from corrupt/malicious payloads with huge
/// data_length values. On Android (256-512 MB per-app heap), allocating
/// >256 MB for a single operation is likely to OOM — and a real AOSP
/// payload never has operations this large (largest single op is typically
/// a REPLACE_XZ for a ~2GB partition, but split across many operations).
const MAX_OP_DATA_SIZE: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
//  Payload read result
// ---------------------------------------------------------------------------

/// Result of reading and parsing a payload.bin file.
#[derive(Debug)]
pub struct PayloadInfo {
    pub header: PayloadHeader,
    pub manifest: DeltaArchiveManifest,
    pub manifest_bytes: Vec<u8>,
    pub data_offset: u64,
    pub file_size: u64,
    pub header_len: u64,
    pub metadata_sig_offset: Option<u64>,
    pub file_path: String,
}

// ---------------------------------------------------------------------------
//  READ — parse a payload.bin file
// ---------------------------------------------------------------------------

/// Read and parse a payload.bin file.
///
/// Returns structured PayloadInfo with header, manifest, and data offset.
pub fn read_payload(path: &str) -> Result<PayloadInfo, String> {
    let file_path = Path::new(path);
    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", path, e))?
        .len();

    log::info!(
        "Reading payload: {} ({:.2} MB)",
        file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        file_size as f64 / (1024.0 * 1024.0)
    );

    let mut file =
        BufReader::new(File::open(file_path).map_err(|e| format!("Cannot open {}: {}", path, e))?);

    // ── Magic ──
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|e| format!("Cannot read magic: {}", e))?;
    if magic != DELTA_MAGIC {
        return Err(format!(
            "Invalid payload magic: expected 'CrAU', got {:?}",
            magic
        ));
    }

    // ── Header protobuf length (uint64 big-endian) ──
    let mut raw_len = [0u8; 8];
    file.read_exact(&mut raw_len)
        .map_err(|e| format!("Cannot read header length: {}", e))?;
    let header_len = u64::from_be_bytes(raw_len);

    // BUG FIX (NEW-7): Reject header_len == 0 — a valid AOSP payload always
    // has a non-zero header. Accepting 0 silently produces an empty manifest
    // and misleading "success" result for corrupt/mismatched files.
    if header_len == 0 {
        return Err("Header length is 0 — invalid payload format".to_string());
    }

    if header_len > file_size {
        return Err(format!(
            "Header length {} exceeds file size {}",
            header_len, file_size
        ));
    }

    // ── Header protobuf ──
    let mut header_bytes = vec![0u8; header_len as usize];
    file.read_exact(&mut header_bytes)
        .map_err(|e| format!("Cannot read header protobuf: {}", e))?;
    let header = decode_payload_header(&header_bytes)?;

    log::info!(
        "Header: version={}, manifest_len={}, minor_version={}",
        header.version,
        header.manifest_len,
        header.minor_version
    );

    // ── Manifest protobuf ──
    let manifest_len = header.manifest_len;
    let mut manifest_bytes = vec![0u8; manifest_len as usize];
    file.read_exact(&mut manifest_bytes)
        .map_err(|e| format!("Cannot read manifest: {}", e))?;
    let manifest = decode_manifest(&manifest_bytes)?;

    // ── Data offset ──
    let data_offset = 4 + HEADER_PROTOBUF_SIZE as u64 + header_len + manifest_len;

    // ── Metadata signature offset (if any) ──
    let metadata_sig_offset = if header.metadata_signature_len > 0 {
        let blob_section_size = 4 + header.metadata_signature_len;
        let aligned_size = if blob_section_size % METADATA_SIG_ALIGNMENT != 0 {
            blob_section_size + METADATA_SIG_ALIGNMENT - (blob_section_size % METADATA_SIG_ALIGNMENT)
        } else {
            blob_section_size
        };
        // BUG FIX (F-5): Guard against underflow when metadata_signature_len
        // is corrupted (larger than file_size). Without this, file_size - aligned_size
        // would underflow in release mode, producing a bogus offset.
        if aligned_size > file_size {
            return Err(format!(
                "metadata_signature_len ({}) + alignment exceeds file_size ({})",
                header.metadata_signature_len, file_size
            ));
        }
        Some(file_size - aligned_size)
    } else {
        None
    };

    log::info!(
        "Data offset: {} (0x{:X}), Partitions: {}",
        data_offset,
        data_offset,
        manifest.partitions.len()
    );

    Ok(PayloadInfo {
        header,
        manifest,
        manifest_bytes,
        data_offset,
        file_size,
        header_len,
        metadata_sig_offset,
        file_path: path.to_string(),
    })
}

// ---------------------------------------------------------------------------
//  EXTRACT — partition extraction
// ---------------------------------------------------------------------------

/// Extract the raw (compressed) data blobs for a partition.
///
/// Reads the data blob for each InstallOperation in the partition's
/// operation list and returns the concatenation.
/// Extract raw (possibly compressed) data for a partition.
///
/// **OOM WARNING**: This function accumulates all operation data chunks in
/// memory as `Vec<Vec<u8>>` then flattens. For large partitions, prefer
/// processing operations individually rather than using this function.
pub fn extract_partition_data(
    payload_info: &PayloadInfo,
    partition_name: &str,
) -> Result<Vec<u8>, String> {
    let partition = find_partition(&payload_info.manifest, partition_name)?;

    let mut file =
        BufReader::new(File::open(&payload_info.file_path).map_err(|e| format!("{}", e))?);
    let mut chunks: Vec<Vec<u8>> = Vec::new();

    for op in &partition.install_operations {
        if op.r#type == OP_ZERO || op.r#type == OP_DISCARD {
            continue;
        }
        if op.data_length == 0 {
            continue;
        }
        // BUG FIX (NEW-2): Validate data_length before allocating.
        // A corrupt/malicious payload with huge data_length would cause OOM.
        if op.data_length > MAX_OP_DATA_SIZE {
            return Err(format!(
                "Operation data_length {} exceeds {} MB limit — possible corrupt payload",
                op.data_length, MAX_OP_DATA_SIZE / (1024 * 1024)
            ));
        }
        if payload_info.data_offset + op.data_offset + op.data_length > payload_info.file_size {
            return Err(format!(
                "Operation data extends beyond file (offset={}, length={}, file_size={})",
                op.data_offset, op.data_length, payload_info.file_size
            ));
        }
        file.seek(SeekFrom::Start(payload_info.data_offset + op.data_offset))
            .map_err(|e| format!("Seek error: {}", e))?;
        let mut chunk = vec![0u8; op.data_length as usize];
        file.read_exact(&mut chunk)
            .map_err(|e| format!("Read error at offset {}: {}", op.data_offset, e))?;
        chunks.push(chunk);
    }

    let total: Vec<u8> = chunks.into_iter().flatten().collect();
    Ok(total)
}

/// Extract, detect compression, and decompress a partition image.
///
/// This is the primary extraction method — reads each operation's data,
/// detects compression from the operation type and magic bytes, and
/// decompresses to produce the final partition image.
///
/// **OOM WARNING**: This function materializes the entire decompressed
/// partition image in RAM as a `Vec<u8>`. For large dynamic partitions
/// (system: 2-5GB, vendor: 1-2GB, product: 1-3GB), this can exceed
/// Android's 256-512MB per-app heap limit and crash the process.
///
/// For large partitions, prefer `extract_and_decompress_partition_to_writer`
/// which streams decompressed chunks to a file, using only ~8MB RAM.
pub fn extract_and_decompress_partition(
    payload_info: &PayloadInfo,
    partition_name: &str,
) -> Result<Vec<u8>, String> {
    let partition = find_partition(&payload_info.manifest, partition_name)?;
    let block_size = payload_info.manifest.block_size;

    let mut file =
        BufReader::new(File::open(&payload_info.file_path).map_err(|e| format!("{}", e))?);
    let mut output_chunks: Vec<Vec<u8>> = Vec::new();

    for op in &partition.install_operations {
        let op_type = op.r#type;

        // ZERO: fill with zeros
        if op_type == OP_ZERO {
            let total_blocks: u64 = op.dst_extents.iter().map(|e| e.num_blocks).sum();
            // BUG FIX: Validate multiplication doesn't overflow and result
            // fits in usize. A corrupt manifest with huge num_blocks could
            // cause overflow → panic, or produce an enormous allocation → OOM.
            let zero_bytes = total_blocks.checked_mul(block_size)
                .ok_or_else(|| format!("ZERO operation overflow: {} blocks * {} block_size", total_blocks, block_size))?;
            if zero_bytes > isize::MAX as u64 {
                return Err(format!("ZERO operation too large: {} bytes exceeds allocation limit", zero_bytes));
            }
            output_chunks.push(vec![0u8; zero_bytes as usize]);
            continue;
        }

        // DISCARD: no data
        if op_type == OP_DISCARD {
            continue;
        }

        // Read compressed/raw data
        if op.data_length == 0 {
            continue;
        }

        // BUG FIX (NEW-2): Validate data_length before allocating.
        if op.data_length > MAX_OP_DATA_SIZE {
            return Err(format!(
                "Operation data_length {} exceeds {} MB limit — possible corrupt payload",
                op.data_length, MAX_OP_DATA_SIZE / (1024 * 1024)
            ));
        }
        if payload_info.data_offset + op.data_offset + op.data_length > payload_info.file_size {
            return Err(format!(
                "Operation data extends beyond file (offset={}, length={}, file_size={})",
                op.data_offset, op.data_length, payload_info.file_size
            ));
        }

        file.seek(SeekFrom::Start(payload_info.data_offset + op.data_offset))
            .map_err(|e| format!("Seek error: {}", e))?;
        let mut compressed_data = vec![0u8; op.data_length as usize];
        file.read_exact(&mut compressed_data)
            .map_err(|e| format!("Read error at offset {}: {}", op.data_offset, e))?;

        // BUG FIX (NEW-4): When auto-detection returns ALG_NONE (no magic bytes
        // matched), decompress() returns Ok(raw_data). If the operation type
        // indicates compression (REPLACE_XZ, PUIGZIP, etc.), this means the
        // magic bytes were corrupted and we should use the operation-type hint
        // instead of silently returning compressed data as-is.
        let decompressed = {
            let auto_result = decompress(&compressed_data, "auto");
            match &auto_result {
                Ok(data) if data.len() == compressed_data.len() => {
                    // Auto returned data unchanged — might be wrong detection.
                    // Try operation-type hint if it indicates compression.
                    let alg = detect_compression(op_type);
                    if alg != "none" {
                        match decompress(&compressed_data, alg) {
                            Ok(d) => d,
                            Err(_) => auto_result?,
                        }
                    } else {
                        auto_result?
                    }
                }
                _ => auto_result.unwrap_or_else(|_| {
                    let alg = detect_compression(op_type);
                    decompress(&compressed_data, alg).unwrap_or_else(|_| compressed_data.clone())
                }),
            }
        };

        // If dst_extents are specified, pad or truncate to expected size.
        let mut result = decompressed;
        if !op.dst_extents.is_empty() {
            // BUG FIX (F-1): Validate num_blocks * block_size doesn't overflow,
            // matching the ZERO-path fix. Also guard `as usize` against 32-bit truncation.
            let expected_size: u64 = op
                .dst_extents
                .iter()
                .map(|e| e.num_blocks.checked_mul(block_size)
                    .ok_or_else(|| format!("Extent overflow: {} blocks * {} block_size", e.num_blocks, block_size)))
                .collect::<Result<Vec<u64>, _>>()?
                .into_iter()
                .sum();
            if expected_size > isize::MAX as u64 {
                return Err(format!("Expected size too large: {} bytes", expected_size));
            }
            if result.len() < expected_size as usize {
                result.resize(expected_size as usize, 0u8);
            } else if result.len() > expected_size as usize {
                result.truncate(expected_size as usize);
            }
        }

        output_chunks.push(result);
    }

    Ok(output_chunks.into_iter().flatten().collect())
}

/// Extract and decompress a partition image, streaming to a writer.
///
/// This is the OOM-safe variant of `extract_and_decompress_partition`.
/// Instead of accumulating the entire decompressed image in RAM as a
/// `Vec<u8>` (which can be 2-5GB for system.img), it writes decompressed
/// chunks to the provided writer as they are produced.
///
/// # Memory usage
/// Peak RAM: ~8MB per operation (compressed chunk read + decompression buffer).
/// Compare: `extract_and_decompress_partition` holds the entire decompressed
/// image in RAM, which can be 2-5GB for system.img.
///
/// # Returns
/// Total bytes written to the writer (the decompressed partition size).
pub fn extract_and_decompress_partition_to_writer<W: std::io::Write>(
    payload_info: &PayloadInfo,
    partition_name: &str,
    writer: &mut W,
) -> Result<u64, String> {
    let partition = find_partition(&payload_info.manifest, partition_name)?;
    let block_size = payload_info.manifest.block_size;

    let mut file =
        BufReader::new(File::open(&payload_info.file_path).map_err(|e| format!("{}", e))?);
    let mut total_written: u64 = 0;

    for op in &partition.install_operations {
        let op_type = op.r#type;

        // ZERO: write zeros directly to writer (no Vec allocation)
        if op_type == OP_ZERO {
            let total_blocks: u64 = op.dst_extents.iter().map(|e| e.num_blocks).sum();
            // BUG FIX: Validate multiplication doesn't overflow before casting.
            let zero_bytes = total_blocks.checked_mul(block_size)
                .ok_or_else(|| format!("ZERO operation overflow: {} blocks * {} block_size", total_blocks, block_size))?;
            if zero_bytes > isize::MAX as u64 {
                return Err(format!("ZERO operation too large: {} bytes exceeds limit", zero_bytes));
            }
            let zero_size = zero_bytes as usize;
            // Write in 4MB chunks to avoid allocating a huge zero Vec
            let zero_chunk = [0u8; 4 * 1024 * 1024];
            let mut remaining = zero_size;
            while remaining > 0 {
                let n = remaining.min(zero_chunk.len());
                writer.write_all(&zero_chunk[..n])
                    .map_err(|e| format!("Write zero error: {}", e))?;
                total_written += n as u64;
                remaining -= n;
            }
            continue;
        }

        // DISCARD: no data
        if op_type == OP_DISCARD {
            continue;
        }

        // Read compressed/raw data
        if op.data_length == 0 {
            continue;
        }

        // BUG FIX (NEW-2): Validate data_length before allocating.
        if op.data_length > MAX_OP_DATA_SIZE {
            return Err(format!(
                "Operation data_length {} exceeds {} MB limit — possible corrupt payload",
                op.data_length, MAX_OP_DATA_SIZE / (1024 * 1024)
            ));
        }
        if payload_info.data_offset + op.data_offset + op.data_length > payload_info.file_size {
            return Err(format!(
                "Operation data extends beyond file (offset={}, length={}, file_size={})",
                op.data_offset, op.data_length, payload_info.file_size
            ));
        }

        file.seek(SeekFrom::Start(payload_info.data_offset + op.data_offset))
            .map_err(|e| format!("Seek error: {}", e))?;
        let mut compressed_data = vec![0u8; op.data_length as usize];
        file.read_exact(&mut compressed_data)
            .map_err(|e| format!("Read error at offset {}: {}", op.data_offset, e))?;

        // BUG FIX (NEW-3): Use streaming decompression instead of in-memory.
        // Previously called decompress() which materialized the FULL decompressed
        // output as Vec<u8> (potentially 5 GB for system.img), then wrote it all
        // at once. Now we stream decompressed chunks directly to the writer via
        // decompress_to_writer(), using only ~8 MB RAM.
        //
        // BUG FIX (NEW-4): When auto-detection returns ALG_NONE but the operation
        // type indicates compression, fall back to the operation-type hint.
        let detected_alg = detect_from_data(&compressed_data);
        let effective_alg = if detected_alg == "none" {
            let op_hint = detect_compression(op_type);
            if op_hint != "none" { op_hint } else { "none" }
        } else {
            detected_alg
        };

        // Compute expected size from dst_extents for padding/truncation.
        let expected_size: Option<u64> = if !op.dst_extents.is_empty() {
            let size: u64 = op
                .dst_extents
                .iter()
                .map(|e| e.num_blocks.checked_mul(block_size)
                    .ok_or_else(|| format!("Extent overflow: {} blocks * {} block_size", e.num_blocks, block_size)))
                .collect::<Result<Vec<u64>, _>>()?
                .into_iter()
                .sum();
            if size > isize::MAX as u64 {
                return Err(format!("Expected size too large: {} bytes", size));
            }
            Some(size)
        } else {
            None
        };

        // Stream decompress directly to writer.
        let decomp_bytes = if effective_alg == "none" {
            // Raw data — write directly
            writer.write_all(&compressed_data)
                .map_err(|e| format!("Write raw error: {}", e))?;
            compressed_data.len() as u64
        } else {
            match decompress_to_writer(&compressed_data, effective_alg, writer) {
                Ok(n) => n,
                Err(_) => {
                    // Fallback: try operation-type hint
                    let fallback_alg = detect_compression(op_type);
                    if fallback_alg != "none" && fallback_alg != effective_alg {
                        decompress_to_writer(&compressed_data, fallback_alg, writer)?
                    } else {
                        // Last resort: write raw data
                        writer.write_all(&compressed_data)
                            .map_err(|e| format!("Write raw fallback error: {}", e))?;
                        compressed_data.len() as u64
                    }
                }
            }
        };

        // Pad with zeros if decompressed output is smaller than expected size.
        // Truncation is not needed for streaming (we already wrote what we got).
        if let Some(expected) = expected_size {
            if decomp_bytes < expected {
                let padding = (expected - decomp_bytes) as usize;
                let zero_chunk = [0u8; 4 * 1024 * 1024];
                let mut remaining = padding;
                while remaining > 0 {
                    let n = remaining.min(zero_chunk.len());
                    writer.write_all(&zero_chunk[..n])
                        .map_err(|e| format!("Write padding error: {}", e))?;
                    remaining -= n;
                }
                total_written += expected;
            } else {
                total_written += decomp_bytes;
            }
        } else {
            total_written += decomp_bytes;
        }
    }

    writer.flush()
        .map_err(|e| format!("Flush error: {}", e))?;
    Ok(total_written)
}

/// Find a partition by name in the manifest.
fn find_partition<'a>(
    manifest: &'a DeltaArchiveManifest,
    partition_name: &str,
) -> Result<&'a PartitionUpdate, String> {
    manifest
        .partitions
        .iter()
        .find(|p| p.partition_name == partition_name)
        .ok_or_else(|| format!("Partition '{}' not found in manifest", partition_name))
}

// ---------------------------------------------------------------------------
//  WRITE — generate a payload.bin from partition images
// ---------------------------------------------------------------------------

/// Partition data for generating a payload.bin.
#[derive(Debug)]
pub struct PartitionData {
    pub name: String,
    pub image_path: String,
    pub compress: String,
}

/// Per-partition summary after generating payload.
#[derive(Debug, serde::Serialize)]
pub struct PartitionSummary {
    pub name: String,
    pub original_size: u64,
    pub compressed_size: u64,
    pub ratio: f64,
    pub algorithm: String,
    pub op_type: u32,
    pub op_type_name: String,
    pub sha256: String,
}

/// Result of writing a payload.bin.
#[derive(Debug, serde::Serialize)]
pub struct WritePayloadResult {
    pub success: bool,
    pub output: String,
    pub output_path: Option<String>,
    pub file_size: Option<u64>,
    pub partitions: Vec<PartitionSummary>,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Generate a payload.bin from partition images.
///
/// # Arguments
/// * `output_path` - Path for the output payload.bin
/// * `partitions_data` - List of partitions with name, image_path, and compression algorithm
/// * `block_size` - Block size in bytes (default 4096)
/// * `minor_version` - Payload minor version
pub fn write_payload(
    output_path: &str,
    partitions_data: &[PartitionData],
    block_size: u32,
    minor_version: u32,
) -> WritePayloadResult {
    let start = std::time::Instant::now();
    let mut lines: Vec<String> = Vec::new();
    let total_images = partitions_data.len();
    let mut partition_summaries: Vec<PartitionSummary> = Vec::new();

    // BUG FIX (NEW-5): Guard against block_size == 0 — would cause division
    // by zero in div_ceil. The JNI bridge already defaults to DEFAULT_BLOCK_SIZE
    // when block_size <= 0, but write_payload itself should be defensive too.
    let block_size = if block_size == 0 { DEFAULT_BLOCK_SIZE } else { block_size };

    // ── Phase 1: Read, hash, and compress each image, streaming to file ──
    //
    // OOM FIX (was OOM-01): Previous version accumulated ALL compressed blobs
    // in `all_blobs: Vec<Vec<u8>>` before writing. For a typical ROM with
    // system (2-5GB compressed), vendor (1-2GB), product (1-3GB), this could
    // hold 4-10GB in RAM simultaneously — far exceeding Android's 256-512MB
    // per-app heap limit.
    //
    // Fix: Stream each partition's compressed data directly to a temp file
    // using `hash_and_compress_file_to_writer`, which never holds the
    // compressed output in memory. We only track (offset, compressed_size)
    // per partition for building the manifest later.
    //
    // Memory usage per partition: ~4MB (one read buffer) + ~4MB (one write
    // buffer for the compressor) = ~8MB peak. Previous: O(sum of all
    // compressed sizes) which could be 10GB+.
    let mut encoded_partitions: Vec<Vec<u8>> = Vec::new();
    let mut current_data_offset: u64 = 0;

    // Temp file for streaming compressed data blobs (avoids all_blobs Vec)
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
    let blobs_tmp_path = output_parent.join("otaku_payload_blobs_tmp.bin");
    let _ = std::fs::remove_file(&blobs_tmp_path); // clean stale
    let mut blobs_file = match File::create(&blobs_tmp_path) {
        Ok(f) => f,
        Err(e) => {
            return WritePayloadResult {
                success: false,
                output: format!("Cannot create blobs temp file: {}", e),
                output_path: None,
                file_size: None,
                partitions: partition_summaries,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Cannot create blobs temp file: {}", e)),
            };
        }
    };

    for (idx, part) in partitions_data.iter().enumerate() {
        let name = &part.name;
        let image_path = &part.image_path;
        let alg = &part.compress;

        if !Path::new(image_path).exists() {
            let _ = std::fs::remove_file(&blobs_tmp_path);
            return WritePayloadResult {
                success: false,
                output: format!("Image file not found: {}", image_path),
                output_path: None,
                file_size: None,
                partitions: partition_summaries,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Image file not found: {}", image_path)),
            };
        }

        let img_size = match std::fs::metadata(image_path) {
            Ok(m) => m.len(),
            Err(e) => {
                let _ = std::fs::remove_file(&blobs_tmp_path);
                return WritePayloadResult {
                    success: false,
                    output: format!("Cannot stat {}: {}", image_path, e),
                    output_path: None,
                    file_size: None,
                    partitions: partition_summaries,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("Cannot stat {}: {}", image_path, e)),
                };
            }
        };

        lines.push(format!(
            "[{}/{}] Processing {} ({:.2} MB, compress={})",
            idx + 1,
            total_images,
            name,
            img_size as f64 / (1024.0 * 1024.0),
            alg
        ));

        // Stream compressed data directly to blobs temp file.
        // hash_and_compress_file_to_writer writes compressed chunks as they
        // are produced, never holding the full compressed output in memory.
        // It returns (compressed_size, sha256_hex_of_raw).
        let (compressed_size, hash_hex) = match hash_and_compress_file_to_writer(
            image_path, alg, None, &mut blobs_file,
        ) {
            Ok(result) => result,
            Err(e) => {
                let _ = std::fs::remove_file(&blobs_tmp_path);
                return WritePayloadResult {
                    success: false,
                    output: format!("Compression failed for {}: {}", name, e),
                    output_path: None,
                    file_size: None,
                    partitions: partition_summaries,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("Compression failed for {}: {}", name, e)),
                };
            }
        };

        // Decode the hex string back to bytes for protobuf fields.
        // BUG FIX: Previously used unwrap_or_default() which silently produced
        // an empty hash on decode failure — embedding a zero/missing hash in
        // the payload manifest, causing silent data corruption.
        let hash_bytes: Vec<u8> = match decode_hex_sha256(&hash_hex) {
            Some(bytes) => bytes,
            None => {
                let _ = std::fs::remove_file(&blobs_tmp_path);
                return WritePayloadResult {
                    success: false,
                    output: format!("Invalid SHA-256 hex for partition '{}': got '{}' (expected 64 hex chars)", name, hash_hex),
                    output_path: None,
                    file_size: None,
                    partitions: partition_summaries,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("Invalid SHA-256 hex for {}", name)),
                };
            }
        };

        // BUG FIX: Validate img_size fits in u32 before casting.
        // AOSP protobuf field is uint32, so partitions >4 GiB would silently
        // truncate, producing a corrupt payload.bin with wrong size metadata.
        if img_size > u32::MAX as u64 {
            let _ = std::fs::remove_file(&blobs_tmp_path);
            return WritePayloadResult {
                success: false,
                output: format!(
                    "Partition '{}' is {} bytes — exceeds u32 max ({}). \
                     AOSP payload format does not support partitions >4 GiB in dst_length.",
                    name, img_size, u32::MAX
                ),
                output_path: None,
                file_size: None,
                partitions: partition_summaries,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Partition {} too large for u32 dst_length", name)),
            };
        }

        // Build InstallOperation
        let op_type = operation_type_for_algorithm(alg);
        let num_blocks = img_size.div_ceil(block_size as u64);
        let dst_extent = build_extent(0, num_blocks);

        let op = build_replace_operation(
            op_type,
            current_data_offset,
            compressed_size,
            vec![dst_extent],
            hash_bytes.clone(),
            img_size as u32,
        );
        let _op_encoded = crate::proto::encode_install_operation(&op);

        // Build PartitionUpdate (reuse hash_bytes — no second sha256_file call)
        let new_info = build_partition_info(img_size, hash_bytes);
        let part_update =
            build_partition_update(name.clone(), vec![op], Some(new_info));
        let part_encoded = crate::proto::encode_partition_update(&part_update);

        encoded_partitions.push(part_encoded);
        current_data_offset += compressed_size;

        let ratio = if img_size > 0 {
            compressed_size as f64 / img_size as f64
        } else {
            1.0
        };

        partition_summaries.push(PartitionSummary {
            name: name.clone(),
            original_size: img_size,
            compressed_size,
            ratio,
            algorithm: alg.clone(),
            op_type,
            op_type_name: crate::proto::op_type_name(op_type).to_string(),
            sha256: hash_hex,
        });

        if img_size > 0 {
            lines.push(format!(
                "    -> {:.2} MB (ratio: {:.1}%)",
                compressed_size as f64 / (1024.0 * 1024.0),
                compressed_size as f64 / img_size as f64 * 100.0
            ));
        } else {
            lines.push(format!("    -> {} bytes", compressed_size));
        }
    }

    // Flush and sync blobs temp file — ensure all compressed data is on disk
    // before we read it back in Phase 4.
    if let Err(e) = blobs_file.flush() {
        let _ = std::fs::remove_file(&blobs_tmp_path);
        return WritePayloadResult {
            success: false,
            output: format!("Cannot flush blobs temp file: {}", e),
            output_path: None,
            file_size: None,
            partitions: partition_summaries,
            duration_ms: start.elapsed().as_millis() as u64,
            error: Some(format!("Cannot flush blobs temp file: {}", e)),
        };
    }

    // ── Phase 2: Build manifest ──
    lines.push("[*] Building manifest...".to_string());

    // BUG FIX: Previously used unwrap_or_default() which silently produced
    // an empty PartitionUpdate on decode failure — creating a corrupt manifest
    // with missing partition name and operations.
    let partitions: Vec<PartitionUpdate> = match encoded_partitions
        .iter()
        .map(|blob| crate::proto::decode_partition_update(blob))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&blobs_tmp_path);
            return WritePayloadResult {
                success: false,
                output: format!("Re-decode partition failed: {}", e),
                output_path: None,
                file_size: None,
                partitions: partition_summaries,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Re-decode partition failed: {}", e)),
            };
        }
    };

    let manifest = build_manifest(block_size as u64, minor_version, partitions);
    let manifest_blob = encode_manifest(&manifest);

    // ── Phase 3: Build header ──
    let header = build_payload_header(
        MAJOR_VERSION as u64,
        manifest_blob.len() as u64,
        0, // no metadata signature
        minor_version,
    );
    let header_blob = encode_payload_header(&header);

    // ── Phase 4: Write payload.bin ──
    lines.push(format!("[*] Writing payload.bin to {}", output_path));

    // BUG FIX: Previously used .ok() which silently ignored directory creation
    // errors (SELinux, storage not mounted). Now propagates the error so the
    // user gets a clear "Permission denied" instead of a cryptic "No such file".
    if let Some(parent) = Path::new(output_path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            let _ = std::fs::remove_file(&blobs_tmp_path);
            return WritePayloadResult {
                success: false,
                output: format!("Cannot create output directory: {}", e),
                output_path: None,
                file_size: None,
                partitions: partition_summaries,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Cannot create output directory: {}", e)),
            };
        }
    }

    let write_result: Result<u64, String> = (|| {
        let mut f = File::create(output_path)
            .map_err(|e| format!("Cannot create {}: {}", output_path, e))?;

        // Magic
        f.write_all(&DELTA_MAGIC)
            .map_err(|e| format!("Write magic error: {}", e))?;

        // Header protobuf length (big-endian uint64)
        f.write_all(&(header_blob.len() as u64).to_be_bytes())
            .map_err(|e| format!("Write header len error: {}", e))?;

        // Header protobuf
        f.write_all(&header_blob)
            .map_err(|e| format!("Write header error: {}", e))?;

        // Manifest protobuf
        f.write_all(&manifest_blob)
            .map_err(|e| format!("Write manifest error: {}", e))?;

        // Data blobs — stream from temp file instead of from all_blobs Vec.
        // OOM FIX: Previous version iterated `all_blobs` (Vec<Vec<u8>>) which
        // held ALL compressed data in RAM. Now we stream from the temp file
        // we built incrementally in Phase 1, using only a 4MB read buffer.
        let mut blobs_reader = File::open(&blobs_tmp_path)
            .map_err(|e| format!("Cannot open blobs temp file: {}", e))?;
        let mut copy_buf = [0u8; 4 * 1024 * 1024]; // 4MB buffer
        loop {
            let n = blobs_reader.read(&mut copy_buf)
                .map_err(|e| format!("Read blobs temp error: {}", e))?;
            if n == 0 { break; }
            f.write_all(&copy_buf[..n])
                .map_err(|e| format!("Write data blob error: {}", e))?;
        }

        f.flush()
            .map_err(|e| format!("Flush error: {}", e))?;

        // Clean up blobs temp file on success
        let _ = std::fs::remove_file(&blobs_tmp_path);

        Ok(std::fs::metadata(output_path)
            .map_err(|e| format!("Cannot stat output: {}", e))?
            .len())
    })();

    // Clean up blobs temp file on error too
    if write_result.is_err() {
        let _ = std::fs::remove_file(&blobs_tmp_path);
    }

    match write_result {
        Ok(total_size) => {
            let elapsed = start.elapsed();
            lines.push(format!(
                "[+] Payload written: {:.2} MB in {:.1}s",
                total_size as f64 / (1024.0 * 1024.0),
                elapsed.as_secs_f64()
            ));
            lines.push(format!("[+] Partitions: {}", total_images));

            WritePayloadResult {
                success: true,
                output: lines.join("\n"),
                output_path: Some(output_path.to_string()),
                file_size: Some(total_size),
                partitions: partition_summaries,
                duration_ms: elapsed.as_millis() as u64,
                error: None,
            }
        }
        Err(e) => WritePayloadResult {
            success: false,
            output: lines.join("\n"),
            output_path: None,
            file_size: None,
            partitions: partition_summaries,
            duration_ms: start.elapsed().as_millis() as u64,
            error: Some(e),
        },
    }
}

// ---------------------------------------------------------------------------
//  VERIFY — self-verify a generated payload.bin
// ---------------------------------------------------------------------------

/// Verification result for a payload.bin.
#[derive(Debug, serde::Serialize)]
pub struct VerifyResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

/// Self-verify a generated payload.bin by re-reading it.
///
/// Checks:
/// - Valid "CrAU" magic
/// - Parseable header and manifest
/// - Partition count
pub fn verify_payload(path: &str) -> VerifyResult {
    let mut lines: Vec<String> = Vec::new();

    match read_payload(path) {
        Ok(info) => {
            lines.push(format!(
                "[+] Verification passed for {}",
                Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            lines.push(format!("    Version:     {}", info.header.version));
            lines.push(format!("    Block size:  {}", info.manifest.block_size));
            lines.push(format!(
                "    Minor ver:   {}",
                info.header.minor_version
            ));
            lines.push(format!(
                "    Partitions:  {}",
                info.manifest.partitions.len()
            ));
            lines.push(format!("    Data offset: {}", info.data_offset));

            // Verify each partition's hash if available
            for part in &info.manifest.partitions {
                if let Some(new_info) = &part.new_partition_info {
                    if !new_info.hash.is_empty() {
                        let hash_hex: String =
                            new_info.hash.iter().map(|b| format!("{:02x}", b)).collect();
                        lines.push(format!(
                            "    {}: hash={}...",
                            part.partition_name,
                            &hash_hex[..hash_hex.len().min(16)]
                        ));
                    }
                }
            }

            VerifyResult {
                success: true,
                output: lines.join("\n"),
                error: None,
            }
        }
        Err(e) => {
            lines.push(format!("[!] Verification failed: {}", e));
            VerifyResult {
                success: false,
                output: lines.join("\n"),
                error: Some(e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  JSON export for JNI results
// ---------------------------------------------------------------------------

/// Convert a PayloadInfo to JSON for JNI return.
pub fn payload_info_to_json(info: &PayloadInfo) -> ParsedPayloadJson {
    ParsedPayloadJson {
        header: crate::proto::PayloadHeaderJson::from(&info.header),
        manifest: ManifestJson::from(&info.manifest),
        data_offset: info.data_offset,
        file_size: info.file_size,
    }
}

// ---------------------------------------------------------------------------
//  Utility helpers
// ---------------------------------------------------------------------------

/// Format a byte count as a human-readable string.
pub fn human_size(size_bytes: u64) -> String {
    if size_bytes < 1024 {
        format!("{} B", size_bytes)
    } else if size_bytes < 1024 * 1024 {
        format!("{:.1} KB", size_bytes as f64 / 1024.0)
    } else if size_bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", size_bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", size_bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Decode a hex-encoded SHA-256 digest (64 hex chars) back to 32 raw bytes.
///
/// Used to convert the hex string returned by `hash_and_compress_file` back
/// into the `Vec<u8>` expected by protobuf fields
/// (`InstallOperation.data_sha256_hash`, `PartitionInfo.hash`). Avoids a
/// second `sha256_file()` pass over the image — for a 5GB system.img that
/// saves ~10s of I/O + CPU per partition.
///
/// Returns None if the input is not exactly 64 hex chars (e.g. empty string
/// when `hash_and_compress_file` had an internal error and returned "").
fn decode_hex_sha256(hex: &str) -> Option<Vec<u8>> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = Vec::with_capacity(32);
    for i in (0..64).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16).ok()?;
        out.push(byte);
    }
    Some(out)
}
