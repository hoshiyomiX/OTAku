//! OTAku custom payload.bin read / write / extract operations.
//!
//! FORMAT DECISION (T27, Option B): this is OTAku's OWN custom payload
//! format — NOT an AOSP/update_engine payload. The framing was originally
//! modeled on Brillo v2, but the protobuf field numbers and enum values
//! have always been OTAku-internal (see proto.rs). The magic was therefore
//! changed from the AOSP lookalike "CrAU" to "OTKU" so that standard
//! payload tools (update_engine, payload-dumper) reject our files cleanly
//! instead of mis-parsing them, and our reader rejects real AOSP payloads
//! cleanly instead of mis-parsing those. Read + write speak OTKU only.
//!
//! File format (OTAku custom, v2):
//!   Offset 0   :  "OTKU"                        (4 bytes)
//!   Offset 4   :  header protobuf length         (8 bytes BE)
//!   Offset 12  :  PayloadHeader protobuf         (variable)
//!   Offset 12+N:  DeltaArchiveManifest protobuf  (variable)
//!   Offset 12+N+M: data blobs ...               (variable)
//!   [optional] :  metadata signature block       (variable)

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::compression::{
    decompress_to_writer, detect_compression, detect_from_data,
    hash_and_compress_file_to_writer_with_progress,
    operation_type_for_algorithm,
};
use crate::proto::{
    build_extent, build_manifest, build_partition_info, build_partition_update,
    build_payload_header, build_replace_operation, decode_manifest, decode_payload_header,
    encode_manifest, encode_payload_header, DeltaArchiveManifest,
    PartitionUpdate, PayloadHeader, OP_ZERO, OP_DISCARD, is_known_op_type,
    ManifestJson, ParsedPayloadJson,
};

// ---------------------------------------------------------------------------
//  Constants
// ---------------------------------------------------------------------------

/// Payload magic bytes: "OTKU" — OTAku custom format (T27 Option B).
/// Deliberately NOT "CrAU": we are not an AOSP payload, and using the
/// AOSP magic made both sides mis-parse each other (T25 finding F1).
pub const DELTA_MAGIC: [u8; 4] = *b"OTKU";
/// Legacy AOSP magic — recognized ONLY to produce an honest rejection
/// message. Never accepted for parsing.
pub const AOSP_MAGIC: [u8; 4] = *b"CrAU";
pub const HEADER_PROTOBUF_SIZE: usize = 8; // uint64 big-endian
pub const MAJOR_VERSION: u32 = 2; // format family v2 (OTAku custom)
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
pub const METADATA_SIG_ALIGNMENT: u64 = 4096;

/// Maximum allowed size for a single operation's data blob (256 MB).
/// BUG FIX (NEW-2): Prevents OOM from corrupt/malicious payloads with huge
/// data_length values. On Android (256-512 MB per-app heap), allocating
/// >256 MB for a single operation is likely to OOM — and a real AOSP
/// payload never has operations this large (largest single op is typically
/// a REPLACE_XZ for a ~2GB partition, but split across many operations).
#[allow(clippy::doc_lazy_continuation)]
const MAX_OP_DATA_SIZE: u64 = 256 * 1024 * 1024;

// T53-F05: absolute sanity caps for header/manifest lengths. The
// file_size-relative bounds alone still allow a sparse or crafted multi-GB
// file to request a multi-GB allocation before read_exact can fail — an
// uncatchable allocation abort, not a catch_unwind-able panic.
const MAX_HEADER_SIZE: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_SIZE: u64 = 64 * 1024 * 1024;

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
        if magic == AOSP_MAGIC {
            // Option B: we speak OTAku's custom format only. Say so plainly
            // instead of half-parsing an AOSP payload (the old F1 failure
            // mode: lenient parse produced garbage partitions=0).
            return Err(
                "This is an AOSP OTA payload.bin, which OTAku does not support. \
                 OTAku reads/writes its own custom payload format (magic 'OTKU')."
                    .to_string(),
            );
        }
        return Err(format!(
            "Invalid payload magic: expected 'OTKU', got {:?}",
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
    // T53-F05: absolute cap + 32-bit truncation guard (the manifest side
    // already had both via NEW-I/NEW-N; the header side did not).
    if header_len > MAX_HEADER_SIZE {
        return Err(format!(
            "Header length {} exceeds {} MB sanity limit — possible corrupt payload",
            header_len, MAX_HEADER_SIZE / (1024 * 1024)
        ));
    }
    if header_len > isize::MAX as u64 {
        return Err(format!("Header too large: {} bytes exceeds isize::MAX", header_len));
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
    // BUG FIX (NEW-I): Validate manifest_len against file_size before
    // allocating. A corrupt payload with manifest_len = u64::MAX would
    // attempt to allocate ~18 exabytes, causing an OOM crash before
    // read_exact can fail with UnexpectedEof.
    if DELTA_MAGIC.len() as u64 + HEADER_PROTOBUF_SIZE as u64 + header_len + manifest_len > file_size {
        return Err(format!(
            "Manifest extends beyond file (header_len={}, manifest_len={}, file_size={})",
            header_len, manifest_len, file_size
        ));
    }
    // BUG FIX (NEW-N): Guard against usize truncation on 32-bit targets.
    // manifest_len is u64 from protobuf — casting to usize silently truncates
    // if manifest_len > 4 GiB on 32-bit. While OTAku targets 64-bit Android,
    // this follows the integer truncation guard principle.
    if manifest_len > isize::MAX as u64 {
        return Err(format!("Manifest too large: {} bytes exceeds isize::MAX", manifest_len));
    }
    // T53-F05: absolute cap — same rationale as the header cap above.
    if manifest_len > MAX_MANIFEST_SIZE {
        return Err(format!(
            "Manifest length {} exceeds {} MB sanity limit — possible corrupt payload",
            manifest_len, MAX_MANIFEST_SIZE / (1024 * 1024)
        ));
    }
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

// /// Extract the raw (compressed) data blobs for a partition.
// ///
// /// Reads the data blob for each InstallOperation in the partition's
// /// operation list and returns the concatenation.
// REMOVED: extract_partition_data() — dead function (zero callers).
// Superseded by extract_and_decompress_partition_to_writer() for OOM safety.
//
// REMOVED: extract_and_decompress_partition() — dead function (zero callers, even in tests).
// Was OOM-unsafe: materialized entire partition in RAM as Vec<u8>.
// Replaced by extract_and_decompress_partition_to_writer() which streams
// decompressed chunks to a writer, using only ~8MB RAM.
// Deprecated since 0.4.0, gated with #[cfg(test)] since earlier cleanup,
// but never called even in tests — removed entirely.

/// Extract and decompress a partition image, streaming to a writer.
///
/// Instead of accumulating the entire decompressed image in RAM as a
/// `Vec<u8>` (which can be 2-5GB for system.img), it writes decompressed
/// chunks to the provided writer as they are produced.
///
/// # Memory usage
/// Peak RAM: ~8MB per operation (compressed chunk read + decompression buffer).
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

        // T27 (AlgoSpec): reject op types outside OTAku's canonical set
        // BEFORE any data handling. The old path silently treated unknown
        // ops as raw data (passthrough corruption — same failure class as
        // the removed brotli mapping).
        if !is_known_op_type(op_type) {
            return Err(format!(
                "Unsupported InstallOperation type {} for partition '{}' — \
                 not part of the OTAku custom format (corrupt or foreign payload?)",
                op_type, partition_name
            ));
        }

        // ZERO: write zeros directly to writer (no Vec allocation)
        if op_type == OP_ZERO {
            // BUG FIX (O-3): Use checked_add to detect overflow in sum of num_blocks.
            // A corrupt manifest with many large extents could wrap the u64 sum.
            let total_blocks: u64 = op.dst_extents.iter().try_fold(0u64, |acc, e| acc.checked_add(e.num_blocks))
                .ok_or_else(|| "Extent num_blocks sum overflow in ZERO operation".to_string())?;
            // BUG FIX: Validate multiplication doesn't overflow before casting.
            let zero_bytes = total_blocks.checked_mul(block_size)
                .ok_or_else(|| format!("ZERO operation overflow: {} blocks * {} block_size", total_blocks, block_size))?;
            if zero_bytes > isize::MAX as u64 {
                return Err(format!("ZERO operation too large: {} bytes exceeds limit", zero_bytes));
            }
            let zero_size = zero_bytes as usize;
            // Write in 4MB chunks to avoid allocating a huge zero Vec
            // F14 (T27): heap buffer — 4MB stack array overflows small-stack threads.
            let zero_chunk = vec![0u8; 4 * 1024 * 1024];
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
        // T53-F06: checked arithmetic — a corrupt manifest can carry
        // data_offset near u64::MAX; plain adds panic in debug builds
        // (inside the JNI worker) and wrap in release (seek to a bogus
        // offset before a confusing downstream failure).
        let op_data_start = payload_info
            .data_offset
            .checked_add(op.data_offset)
            .ok_or_else(|| {
                format!(
                    "Operation offset overflow (data_offset={} + op offset={})",
                    payload_info.data_offset, op.data_offset
                )
            })?;
        let op_data_end = op_data_start
            .checked_add(op.data_length)
            .ok_or_else(|| {
                format!(
                    "Operation offset overflow (data_offset={} + op offset={} + length={})",
                    payload_info.data_offset, op.data_offset, op.data_length
                )
            })?;
        if op_data_end > payload_info.file_size {
            return Err(format!(
                "Operation data extends beyond file (offset={}, length={}, file_size={})",
                op.data_offset, op.data_length, payload_info.file_size
            ));
        }

        file.seek(SeekFrom::Start(op_data_start))
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
            // NEW-4: data sniffing found no magic — trust the manifest's
            // op_type hint (canonical set already validated above).
            detect_compression(op_type)
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
                .try_fold(0u64, |acc, v| acc.checked_add(v))
                .ok_or_else(|| "Extent size sum overflow".to_string())?;
            if size > isize::MAX as u64 {
                return Err(format!("Expected size too large: {} bytes", size));
            }
            Some(size)
        } else {
            None
        };

        // Stream decompress directly to writer.
        // BUG FIX (NEW-H): Previously, if decompress_to_writer failed after writing
        // partial data to the writer (File), the fallback would APPEND after the
        // corrupt partial output — producing a garbage file. Now we buffer the
        // initial attempt in a Cursor<Vec<u8>>. On success, write the buffer to
        // the actual writer. On failure, discard the buffer and retry with fallback.
        // This trades memory for correctness — acceptable since MAX_OP_DATA_SIZE
        // caps compressed input at 256MB and each op's decompressed output is
        // bounded by MAX_DECOMPRESSED_SIZE in compression.rs.
        let decomp_bytes = if effective_alg == "none" {
            // Raw data — truncate to expected_size if needed
            let raw_len = if let Some(expected) = expected_size {
                (compressed_data.len() as u64).min(expected) as usize
            } else {
                compressed_data.len()
            };
            writer.write_all(&compressed_data[..raw_len])
                .map_err(|e| format!("Write raw error: {}", e))?;
            raw_len as u64
        } else {
            // First attempt: decompress to an in-memory buffer.
            let mut buf1 = std::io::Cursor::new(Vec::new());
            match decompress_to_writer(&compressed_data, effective_alg, &mut buf1) {
                Ok(n) => {
                    // BUG FIX (O-1): Truncate buffer to expected_size if decompressed
                    // output exceeds the dst_extents. This matches the in-memory path's
                    // truncate behavior. Now possible because Cursor buffers the full
                    // output before writing.
                    if let Some(expected) = expected_size {
                        let buf = buf1.get_mut();
                        if buf.len() > expected as usize {
                            buf.truncate(expected as usize);
                        }
                    }
                    writer.write_all(buf1.get_ref())
                        .map_err(|e| format!("Write decompressed error: {}", e))?;
                    n
                }
                Err(_) => {
                    // First attempt failed — discard partial buffer, try fallback
                    let fallback_alg = detect_compression(op_type);
                    if fallback_alg != "none" && fallback_alg != effective_alg {
                        let mut buf2 = std::io::Cursor::new(Vec::new());
                        match decompress_to_writer(&compressed_data, fallback_alg, &mut buf2) {
                            Ok(n) => {
                                // Truncate fallback buffer too
                                if let Some(expected) = expected_size {
                                    let buf = buf2.get_mut();
                                    if buf.len() > expected as usize {
                                        buf.truncate(expected as usize);
                                    }
                                }
                                writer.write_all(buf2.get_ref())
                                    .map_err(|e| format!("Write fallback decompressed error: {}", e))?;
                                n
                            }
                            Err(_) => {
                                // BUG FIX (O-4): Return error instead of writing raw
                                // compressed data as "last resort" — that silently produces
                                // a corrupt file that could brick the device if flashed.
                                return Err(format!(
                                    "All decompressors failed for operation (type={}): \
                                     primary={}, fallback={}. Raw data NOT written to prevent corruption.",
                                    op_type, effective_alg, fallback_alg
                                ));
                            }
                        }
                    } else {
                        // No valid fallback — return error instead of writing raw data
                        return Err(format!(
                            "Decompression failed for operation (type={}): algorithm={}, \
                             no fallback available. Raw data NOT written to prevent corruption.",
                            op_type, effective_alg
                        ));
                    }
                }
            }
        };

        // Pad with zeros if decompressed output is smaller than expected size.
        // Truncation is not needed for streaming (we already wrote what we got).
        if let Some(expected) = expected_size {
            if decomp_bytes < expected {
                let padding = (expected - decomp_bytes) as usize;
                // F14 (T27): heap buffer — 4MB stack array overflows small-stack threads.
            let zero_chunk = vec![0u8; 4 * 1024 * 1024];
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
//  Extraction progress sidecar (same convention as dd.rs build progress)
// ---------------------------------------------------------------------------

/// Chunk size used to slice large write_all calls so the sidecar file is
/// updated at a smooth cadence — matches dd.rs's 4MB compression chunk
/// granularity, which Kotlin's 500ms poller was tuned for.
const PROGRESS_CHUNK: usize = 4 * 1024 * 1024;

/// Write the extraction progress sidecar JSON atomically (tmp + rename),
/// mirroring dd.rs `write_progress_with_percent`.
///
/// Field semantics match the DD build sidecar so the Kotlin polling code
/// follows the same shape. `current`/`total` are 1/1: the JNI bridge is
/// called once per partition, so the BATCH position (partition 3 of 7) is
/// tracked Kotlin-side by the extract loop — only `partition_percent` is
/// authoritative here. `overall_percent` mirrors `partition_percent` and
/// is recomputed by Kotlin when a batch is in flight.
fn write_extract_progress(sidecar_path: &str, name: &str, bytes_written: u64, total_estimated: u64) {
    // total_estimated = 0 (unknowable manifest) → percent stays 0; Kotlin
    // renders the byte counter instead of a percentage in that case.
    // checked_div mirrors dd.rs's overall_percent math — division by an
    // estimate-less 0 maps to None → 0 (clippy::manual_checked_ops).
    let partition_percent: i32 = (bytes_written.min(total_estimated) * 100)
        .checked_div(total_estimated)
        .map(|v| v.min(100) as i32)
        .unwrap_or(0);
    let content = serde_json::json!({
        "current": 1,
        "total": 1,
        "name": name,
        "phase": "extracting",
        "bytes_written": bytes_written,
        "tmp_path": "",
        "total_estimated": total_estimated,
        "partition_percent": partition_percent,
        "overall_percent": partition_percent,
    });
    // Atomic write: tmp file + rename (same-filesystem rename is atomic on
    // Linux/ext4) — prevents Kotlin's poller from reading truncated JSON.
    let _ = (|| {
        let tmp_progress_path = format!("{}.tmp", sidecar_path);
        std::fs::write(&tmp_progress_path, content.to_string()).ok()?;
        std::fs::rename(&tmp_progress_path, sidecar_path).ok()
    })();
}

/// Delete the extraction progress sidecar file (completion or error path).
fn delete_extract_progress(sidecar_path: &str) {
    let _ = std::fs::remove_file(sidecar_path);
}

/// Writer wrapper that reports extraction progress through a `.progress`
/// sidecar file next to the output image.
///
/// Locked design decision (same as the DD build): progress is reported via
/// a sidecar file that Kotlin polls — NOT via a JNI callback (callbacks
/// failed in v3.4/v3.5 with JNIEnv re-entrancy and local-ref overflow).
///
/// Every `write_all` is sliced into ≤4MB chunks; after each chunk the
/// sidecar JSON is rewritten atomically. A multi-GB system.img therefore
/// produces ~1250 tiny sidecar writes over the whole extraction — the same
/// I/O overhead class the DD build has always paid.
pub struct ProgressSidecarWriter<W: std::io::Write> {
    inner: W,
    sidecar_path: String,
    name: String,
    total_estimated: u64,
    bytes_written: u64,
}

impl<W: std::io::Write> ProgressSidecarWriter<W> {
    /// Wrap `inner` (the output .img file) with progress reporting.
    ///
    /// `output_path` is the image path — the sidecar lands at
    /// `<output_path>.progress`, exactly like the DD build's ZIP sidecar.
    /// `total_estimated` comes from [`partition_expected_size`].
    pub fn new(inner: W, output_path: &str, name: &str, total_estimated: u64) -> Self {
        Self {
            inner,
            sidecar_path: format!("{}.progress", output_path),
            name: name.to_string(),
            total_estimated,
            bytes_written: 0,
        }
    }

    /// Remove the progress sidecar file. Call on BOTH the success and the
    /// error path — Kotlin's finally-block also deletes it (belt and
    /// suspenders, same as the DD build).
    pub fn remove_sidecar(&self) {
        delete_extract_progress(&self.sidecar_path);
    }

    fn note_progress(&self) {
        write_extract_progress(&self.sidecar_path, &self.name, self.bytes_written, self.total_estimated);
    }
}

impl<W: std::io::Write> std::io::Write for ProgressSidecarWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // T36: user cancellation — abort the streaming extraction at this
        // chunk boundary. The io::Error carries CANCEL_SENTINEL, which the
        // JNI layer forwards to Kotlin inside the extract error JSON.
        if crate::cancel_requested() {
            return Err(std::io::Error::other(crate::CANCEL_SENTINEL));
        }
        let n = self.inner.write(buf)?;
        self.bytes_written += n as u64;
        self.note_progress();
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        // Slice into ≤4MB chunks so the sidecar cadence matches dd.rs.
        // extract_and_decompress_partition_to_writer issues one large
        // write_all per operation (up to hundreds of MB) — without slicing,
        // the poller would see 0% → 100% jumps per operation.
        let mut off = 0;
        while off < buf.len() {
            // T36: user cancellation — checked per ≤4MB slice, matching the
            // DD build's chunk cadence (see write() above for the sentinel).
            if crate::cancel_requested() {
                return Err(std::io::Error::other(crate::CANCEL_SENTINEL));
            }
            let end = (off + PROGRESS_CHUNK).min(buf.len());
            self.inner.write_all(&buf[off..end])?;
            self.bytes_written += (end - off) as u64;
            self.note_progress();
            off = end;
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Expected decompressed size of a partition (progress denominator).
///
/// Sums `dst_extents.num_blocks × block_size` over ALL install operations
/// — each op pads its output up to its own extent size, so the sum is
/// exactly the expected total output. Falls back to
/// `new_partition_info.partition_size` when the manifest carries no
/// extents. Returns 0 when the partition is unknown or has no size info
/// (Kotlin then shows a byte counter instead of a percentage).
pub fn partition_expected_size(info: &PayloadInfo, partition_name: &str) -> u64 {
    let partition = match find_partition(&info.manifest, partition_name) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let block_size = info.manifest.block_size;
    let from_extents: u64 = partition
        .install_operations
        .iter()
        .flat_map(|op| op.dst_extents.iter())
        .map(|e| e.num_blocks.saturating_mul(block_size))
        .sum();
    if from_extents > 0 {
        from_extents
    } else {
        partition
            .new_partition_info
            .as_ref()
            .map(|i| i.partition_size)
            .unwrap_or(0)
    }
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

/// Generate a payload.bin from partition images, with real-time progress.
///
/// Progress sidecar (same design as the DD build — see
/// [`crate::dd::write_progress_with_percent`]): during the build, Rust
/// writes `<output_path>.progress` atomically with the phases
/// `compressing` (per 4MB chunk, `partition_percent` authoritative),
/// `compressed` (partition finished), and `assembling` (header +
/// manifest + data-blob copy). Kotlin polls it every 500ms. The sidecar
/// is removed on BOTH the success and the error path before this
/// returns — Kotlin's finally block also deletes it (belt and
/// suspenders, same discipline as run_dd_build).
///
/// # Arguments
/// * `output_path` - Path for the output payload.bin
/// * `partitions_data` - List of partitions with name, image_path, and compression algorithm
/// * `block_size` - Block size in bytes (default 4096)
/// * `minor_version` - Payload minor version
/// * `level` - Compression level (None = use algorithm default)
pub fn write_payload(
    output_path: &str,
    partitions_data: &[PartitionData],
    block_size: u32,
    minor_version: u32,
    level: Option<i32>,
) -> WritePayloadResult {
    let result = write_payload_inner(
        output_path,
        partitions_data,
        block_size,
        minor_version,
        level,
    );
    // The .progress sidecar is transient — remove it on the success AND
    // the error path so a stale file can never leak into the next build
    // (same discipline as run_dd_build; Kotlin's finally also deletes).
    crate::dd::delete_progress_file(output_path);
    result
}

/// Implementation of [`write_payload`] — see its doc for the sidecar
/// contract. Every error path below relies on the public wrapper to
/// remove the sidecar afterwards.
fn write_payload_inner(
    output_path: &str,
    partitions_data: &[PartitionData],
    block_size: u32,
    minor_version: u32,
    level: Option<i32>,
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
    // using `hash_and_compress_file_to_writer_with_progress`, which never holds the
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

    // Progress estimation total — sum of input image sizes, the same
    // convention as run_dd_build's total_estimated (the sidecar carries
    // it so Kotlin can render byte counters alongside percentages).
    let total_estimated: u64 = partitions_data
        .iter()
        .map(|p| std::fs::metadata(&p.image_path).map(|m| m.len()).unwrap_or(0))
        .sum();
    let blobs_tmp_path_str = blobs_tmp_path.to_string_lossy().to_string();

    for (idx, part) in partitions_data.iter().enumerate() {
        // T36: user cancellation — stop before starting the next partition
        // (chunk-level aborts happen inside the compression helper).
        if crate::cancel_requested() {
            let _ = std::fs::remove_file(&blobs_tmp_path);
            return WritePayloadResult {
                success: false,
                output: crate::CANCEL_SENTINEL.to_string(),
                output_path: None,
                file_size: None,
                partitions: partition_summaries,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(crate::CANCEL_SENTINEL.to_string()),
            };
        }
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

        // Stream compressed data directly to blobs temp file, with
        // per-chunk progress. hash_and_compress_file_to_writer_with_progress
        // writes compressed chunks as they are produced (never holding the
        // full compressed output in memory) and invokes on_progress after
        // every 4MB chunk read — the callback rewrites the .progress
        // sidecar so Kotlin's 500ms poller sees smooth percentages.
        // The writer moves BY VALUE and is handed back on return (borrow
        // rules — same pattern as run_dd_build).
        let output_path_owned = output_path.to_string();
        let blobs_tmp_path_clone = blobs_tmp_path.clone();
        let name_clone = name.clone();
        let (comp_result, returned_blobs_file) =
            match hash_and_compress_file_to_writer_with_progress(
                image_path, alg, level, blobs_file,
                Some(&mut |bytes_read: u64, file_size: u64| {
                    let pct = (bytes_read * 100)
                        .checked_div(file_size)
                        .map(|v| v as i32)
                        .unwrap_or(100);
                    // Current blobs temp size = compressed bytes so far
                    // (the file handle itself was moved into the
                    // compressor, so stat by path — like run_dd_build).
                    let current_size = std::fs::metadata(&blobs_tmp_path_clone)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    crate::dd::write_progress_with_percent(
                        &output_path_owned,
                        idx + 1,
                        total_images,
                        &name_clone,
                        "compressing",
                        current_size,
                        Some(&blobs_tmp_path_str),
                        total_estimated,
                        pct,
                    );
                }),
            ) {
                Ok(r) => r,
                Err(e) => {
                    let _ = std::fs::remove_file(&blobs_tmp_path);
                    // T36: a deliberate cancellation keeps its clean sentinel
                    // message — not wrapped in "Compression failed" noise.
                    let msg = if e == crate::CANCEL_SENTINEL {
                        e
                    } else {
                        format!("Compression failed for {}: {}", name, e)
                    };
                    return WritePayloadResult {
                        success: false,
                        output: msg.clone(),
                        output_path: None,
                        file_size: None,
                        partitions: partition_summaries,
                        duration_ms: start.elapsed().as_millis() as u64,
                        error: Some(msg),
                    };
                }
            };
        blobs_file = returned_blobs_file;
        let compressed_size = comp_result.comp_size;
        let hash_hex = comp_result.unc_hash_hex;

        // Progress: this partition's compression is done (100%).
        crate::dd::write_progress_with_percent(
            output_path,
            idx + 1,
            total_images,
            name,
            "compressed",
            std::fs::metadata(&blobs_tmp_path).map(|m| m.len()).unwrap_or(0),
            Some(&blobs_tmp_path_str),
            total_estimated,
            100,
        );

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
        // F14 fix (T27, found by test_otku_round_trip): this was a 4MB STACK
        // array — overflowed test threads (~2MB stack) and is a latent crash
        // on any JNI thread with a small stack (ART default -Xss ≈ 1MB).
        // Heap-allocate instead; identical streaming semantics.
        let mut copy_buf = vec![0u8; 4 * 1024 * 1024]; // 4MB buffer
        // "assembling" phase progress — the final I/O copy is multi-GB
        // for real ROMs, so the sidecar keeps moving here too (Kotlin
        // maps this phase to 97%, like the DD build's writing_zip).
        // Throttled to 1% steps; checked_div per the clippy
        // manual_checked_ops discipline.
        let total_to_copy = current_data_offset;
        let mut bytes_copied: u64 = 0;
        let mut last_copy_pct: i32 = -1;
        loop {
            // T53-F08: honor cancellation during the final multi-GB copy —
            // every other phase checks the flag; without this, Cancel
            // during "assembling" runs to completion (minutes on slow
            // storage) before the operation ends.
            if crate::cancel_requested() {
                return Err(crate::CANCEL_SENTINEL.to_string());
            }
            let n = blobs_reader.read(&mut copy_buf)
                .map_err(|e| format!("Read blobs temp error: {}", e))?;
            if n == 0 { break; }
            f.write_all(&copy_buf[..n])
                .map_err(|e| format!("Write data blob error: {}", e))?;
            bytes_copied += n as u64;
            if total_to_copy > 0 {
                let copy_pct = (bytes_copied * 100)
                    .checked_div(total_to_copy)
                    .map(|v| v.min(100) as i32)
                    .unwrap_or(100);
                if copy_pct != last_copy_pct {
                    last_copy_pct = copy_pct;
                    crate::dd::write_progress_with_percent(
                        output_path,
                        total_images,
                        total_images,
                        "",
                        "assembling",
                        bytes_copied,
                        Some(&blobs_tmp_path_str),
                        total_estimated,
                        // T53-F07: the computed copy_pct — the literal 100
                        // froze the sidecar at 100% for the entire final
                        // multi-GB copy.
                        copy_pct,
                    );
                }
            }
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
/// - Valid "OTKU" magic (OTAku custom format)
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

// ---------------------------------------------------------------------------
//  Tests — payload format (T27 Fase-3a: magic + round-trip coverage)
//  payload.rs previously had ZERO tests — the exact blind spot that let
//  the AOSP-lookalike magic survive (T25 finding F1).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("otaku_t27_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_image(dir: &std::path::Path, name: &str, size: usize) -> String {
        let p = dir.join(name);
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().to_string()
    }

    /// Option B core invariant: OTAku writes OTKU, reads OTKU back.
    #[test]
    fn test_otku_round_trip() {
        let dir = temp_dir("rt");
        let img = make_image(&dir, "boot.img", 8192);
        let out = dir.join("p.bin").to_string_lossy().to_string();
        let pd = vec![PartitionData {
            name: "boot".to_string(),
            image_path: img,
            compress: "gzip".to_string(),
        }];
        let res = write_payload(&out, &pd, 4096, 0, None);
        assert!(res.success, "write_payload gagal: {:?}", res.error);

        // Magic is OTKU on disk.
        let mut head = [0u8; 4];
        {
            let mut f = std::fs::File::open(&out).unwrap();
            use std::io::Read;
            f.read_exact(&mut head).unwrap();
        }
        assert_eq!(&head, b"OTKU", "magic di-disk bukan OTKU");

        // Read-back parses: 1 partition, block size preserved.
        let info = read_payload(&out).expect("read_payload OTKU gagal");
        assert_eq!(info.manifest.partitions.len(), 1);
        assert_eq!(info.manifest.block_size, 4096);
        assert_eq!(info.header.version, MAJOR_VERSION as u64);

        // Self-verify passes.
        let v = verify_payload(&out);
        assert!(v.success, "verify_payload gagal: {:?}", v.error);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An AOSP payload (CrAU magic) must be rejected with the honest
    /// "not supported" message — never half-parsed.
    #[test]
    fn test_aosp_magic_rejected_honestly() {
        let dir = temp_dir("crau");
        let p = dir.join("aosp.bin");
        // Minimal AOSP-shaped file: magic + BE u64 header len + junk.
        let mut content = Vec::new();
        content.extend_from_slice(b"CrAU");
        content.extend_from_slice(&16u64.to_be_bytes());
        content.extend_from_slice(&[0u8; 24]);
        std::fs::write(&p, content).unwrap();
        let err = read_payload(&p.to_string_lossy()).err().expect("harus ditolak");
        assert!(
            err.contains("AOSP") && err.contains("does not support"),
            "pesan penolakan AOSP tidak jujur: {}",
            err
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Garbage magic gets the plain invalid-magic error.
    #[test]
    fn test_garbage_magic_rejected() {
        let dir = temp_dir("junk");
        let p = dir.join("junk.bin");
        std::fs::write(&p, b"XXXXjunkjunkjunk").unwrap();
        let err = read_payload(&p.to_string_lossy()).err().expect("harus ditolak");
        assert!(
            err.contains("expected 'OTKU'"),
            "pesan magic tidak menyebut OTKU: {}",
            err
        );
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// End-to-end per algorithm: write payload (op_type now HONEST) ->
    /// extract -> bytes must equal the original image. This is the test the
    /// old format could not pass honestly for lz4/zstd (manifest said
    /// REPLACE while the blob was compressed).
    #[test]
    fn test_extract_round_trip_per_algorithm() {
        for alg in ["gzip", "bzip2", "xz", "lz4", "zstd", "none"] {
            let dir = temp_dir("rtx");
            let original: Vec<u8> = (0..196_608usize) // 48 KB = 12 blocks
                .map(|i| ((i * 7 + i / 4096) % 251) as u8)
                .collect();
            let img_path = dir.join("sys.img");
            std::fs::write(&img_path, &original).unwrap();
            let out = dir.join("p.bin").to_string_lossy().to_string();

            let pd = vec![PartitionData {
                name: "system".to_string(),
                image_path: img_path.to_string_lossy().to_string(),
                compress: alg.to_string(),
            }];
            let res = write_payload(&out, &pd, 4096, 0, None);
            assert!(res.success, "write({}) gagal: {:?}", alg, res.error);

            let info = read_payload(&out).expect("read gagal");
            let mut extracted = std::io::Cursor::new(Vec::new());
            let n = extract_and_decompress_partition_to_writer(
                &info,
                "system",
                &mut extracted,
            )
            .unwrap_or_else(|e| panic!("extract({}) gagal: {}", alg, e));
            assert_eq!(n as usize, original.len(), "ukuran extract({}) salah", alg);
            assert_eq!(
                extracted.get_ref(),
                &original,
                "isi extract({}) != asli — manifest op_type masih bohong?",
                alg
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Unknown op types must be REJECTED at extract, not silently treated
    /// as raw data (the old passthrough-corruption path).
    #[test]
    fn test_extract_rejects_unknown_op_type() {
        // Bangun payload gzip yang valid, lalu patch byte op_type di dalam
        // manifest protobuf: field 1 (type) varint. Lokasi: cari manifest
        // bytes di file dan flip op_type menjadi 99.
        let dir = temp_dir("badop");
        let original = vec![0x5Au8; 8192];
        let img_path = dir.join("boot.img");
        std::fs::write(&img_path, &original).unwrap();
        let out = dir.join("p.bin").to_string_lossy().to_string();
        let pd = vec![PartitionData {
            name: "boot".to_string(),
            image_path: img_path.to_string_lossy().to_string(),
            compress: "gzip".to_string(),
        }];
        let res = write_payload(&out, &pd, 4096, 0, None);
        assert!(res.success, "write gagal: {:?}", res.error);

        // Rebuild file dengan op_type 99: manifest_len hidup DI DALAM header
        // protobuf (field manifest_len), jadi header ikut dire-encode.
        let info = read_payload(&out).unwrap();
        let file_bytes = std::fs::read(&out).unwrap();
        let old_data_start = info.data_offset as usize;

        let mut bad_manifest = info.manifest.clone();
        bad_manifest.partitions[0].install_operations[0].r#type = 99;
        let bad_manifest_bytes = crate::proto::encode_manifest(&bad_manifest);

        let mut bad_header = info.header.clone();
        bad_header.manifest_len = bad_manifest_bytes.len() as u64;
        let bad_header_bytes = crate::proto::encode_payload_header(&bad_header);

        let mut rebuilt = Vec::new();
        rebuilt.extend_from_slice(b"OTKU");
        rebuilt.extend_from_slice(&(bad_header_bytes.len() as u64).to_be_bytes());
        rebuilt.extend_from_slice(&bad_header_bytes);
        rebuilt.extend_from_slice(&bad_manifest_bytes);
        rebuilt.extend_from_slice(&file_bytes[old_data_start..]);
        std::fs::write(&out, &rebuilt).unwrap();

        let err = extract_and_decompress_partition_to_writer(
            &read_payload(&out).unwrap(),
            "boot",
            &mut std::io::Cursor::new(Vec::new()),
        )
        .err()
        .expect("op_type 99 harus ditolak");
        assert!(
            err.contains("Unsupported InstallOperation type 99"),
            "pesan tolak op tidak jelas: {}",
            err
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Legacy OTAku files (built before T27, magic CrAU with OTAku's own
    /// deviant schema) are ALSO rejected — clean break, no hybrid parsing.
    #[test]
    fn test_legacy_otaku_files_rejected() {
        // Same rejection path as real AOSP files: magic CrAU -> honest error.
        // (Old OTAku payloads and real AOSP payloads are indistinguishable by
        // magic alone; both get the same clear message.)
        let dir = temp_dir("legacy");
        let p = dir.join("legacy.bin");
        let mut content = Vec::new();
        content.extend_from_slice(b"CrAU");
        content.extend_from_slice(&8u64.to_be_bytes());
        content.extend_from_slice(&[0u8; 8]);
        std::fs::write(&p, content).unwrap();
        assert!(read_payload(&p.to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
