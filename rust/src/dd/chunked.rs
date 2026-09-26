//! dd/chunked.rs — DDBU v2 chunked-xz encoder + chunk-table format (T51).
//!
//! WHY: xz is by far the slowest encoder we ship (5–20 MB/s per core), which
//! makes in-app bundle builds painful on multi-core devices. The xz FORMAT
//! cannot be parallelized within a single stream, but a bundle can be split
//! into independently-compressed chunks and reassembled — IF the chunk
//! boundaries are known by construction. This module defines exactly that:
//!
//! * The encoder splits each partition image into fixed-size chunks (64 MB in
//!   production, injectable for tests), compresses them on a worker pool, and
//!   writes them to the bundle in order, 4096-aligned.
//! * A chunk TABLE (24 bytes per entry) is appended to the bundle as a
//!   trailer, and the DDBU header is patched to v2 pointing at it.
//! * The bundled helper (`otaku-decomp --flash-chunked`, see decomp.rs) reads
//!   the table back at flash time and decodes chunks on its own pool —
//!   ordered pipe by default, direct pwrite to the block device opt-in.
//!
//! FORMAT (DDBU v2 — chunked xz only; every other algorithm stays v1 with a
//! byte-identical layout, so old bundles and the classic flash path are
//! untouched):
//!
//! ```text
//! Header (4096 B, v1 fields unchanged at 0..16):
//!   @16  chunk_table_offset  u64 LE   absolute file offset of the trailer
//!   @24  chunk_count_total   u32 LE   total entries across all partitions
//!   @28  part_counts[n]      u16 LE   chunk count per partition, in order
//! Trailer (4096-aligned, after the last partition):
//!   entry × N, 24 B each:
//!     comp_offset  u64 LE   absolute offset of this chunk's compressed bytes
//!     comp_len     u64 LE   compressed length in bytes
//!     decomp_len   u64 LE   uncompressed length (= 64 MB except the last)
//!   Entries are grouped by partition in partition order. The decompressed
//!   offset of entry k is the running sum of prior decomp_len values, so it
//!   is derivable and not stored (keeps the entry lean at 24 B).
//! ```
//!
//! COMPATIBILITY (three-way, locked decision):
//! * new app + v1 bundle      → classic single-stream path, unchanged
//! * new helper + v1 bundle   → classic `-a` path, unchanged
//! * OLD app/template + v2 bundle → the pre-T51 flasher already aborts on
//!   `HDR_VERSION != 1` ("Unsupported bundle version") — honest refusal, no
//!   half-parse. That gate predates T51; v2 simply trips it.
//!
//! ERROR MODEL: any worker/read/write error or user cancellation (T36
//! sentinel) is funneled to a single `Err(String)`; the temp bundle file is
//! cleaned up by run_dd_build's existing outer error path.
//!
//! MEMORY MODEL: peak RAM ≈ workers × (64 MB input buffer + compressed
//! output in flight). The worker cap is 4 (not the zstd 6): the dominant
//! cost here is the 64 MB in-flight chunk buffer, so 4 keeps the worst case
//! around ~300 MB on top of the writer's own buffers — predictable on
//! armv7's ~3 GB address space.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::compression::{resolve_level, StreamCompressResult, ALG_XZ};

/// Production chunk size (locked decision T51: 64 MB).
pub(crate) const CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Serialized size of one chunk-table entry (3 × u64 LE).
pub(crate) const CHUNK_ENTRY_SIZE: usize = 24;

/// Header field offsets inside the 4096-byte DDBU header (v2).
pub(crate) const HDR_V2_TABLE_OFFSET_FIELD: usize = 16;
pub(crate) const HDR_V2_COUNT_FIELD: usize = 24;
pub(crate) const HDR_V2_PART_COUNTS_FIELD: usize = 28;

/// DDBU format version written for chunked (xz) bundles.
pub(crate) const DDBUNDLE_VERSION_V2: u16 = 2;

/// Progress poll interval in the writer loop (ms). Keeps the callback at
/// 4 MB granularity even when chunk completions are rare (small partitions).
const PROGRESS_POLL_MS: u64 = 200;

// ---------------------------------------------------------------------------
//  Chunk table types + (de)serialization
// ---------------------------------------------------------------------------

/// One chunk-table entry. `comp_offset` is ABSOLUTE within otaku.bin (header
/// included); the decompressed offset is the running sum of prior entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkEntry {
    pub comp_offset: u64,
    pub comp_len: u64,
    pub decomp_len: u64,
}

/// Serialize chunk entries (24 B each, all fields u64 LE).
pub(crate) fn encode_chunk_table(entries: &[ChunkEntry]) -> Vec<u8> {
    let mut v = Vec::with_capacity(entries.len() * CHUNK_ENTRY_SIZE);
    for e in entries {
        v.extend_from_slice(&e.comp_offset.to_le_bytes());
        v.extend_from_slice(&e.comp_len.to_le_bytes());
        v.extend_from_slice(&e.decomp_len.to_le_bytes());
    }
    v
}

/// Parse `bytes` as a chunk table. Rejects any length not a multiple of the
/// entry size — a torn trailer must fail loudly, never half-parse.
pub(crate) fn decode_chunk_table(bytes: &[u8]) -> Result<Vec<ChunkEntry>, String> {
    if !bytes.len().is_multiple_of(CHUNK_ENTRY_SIZE) {
        return Err(format!(
            "chunk table length {} is not a multiple of {}",
            bytes.len(),
            CHUNK_ENTRY_SIZE
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / CHUNK_ENTRY_SIZE);
    for i in (0..bytes.len()).step_by(CHUNK_ENTRY_SIZE) {
        let f = |off: usize| {
            u64::from_le_bytes(
                bytes[i + off..i + off + 8]
                    .try_into()
                    .expect("chunk entry slice is exactly 8 bytes"),
            )
        };
        out.push(ChunkEntry {
            comp_offset: f(0),
            comp_len: f(8),
            decomp_len: f(16),
        });
    }
    Ok(out)
}

/// Header v2 fields, reader side (used by the helper's flash path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkedHeader {
    pub table_offset: u64,
    pub total_chunks: u32,
    pub part_counts: Vec<u16>,
}

/// Parse the v2 fields out of a 4096-byte header slice.
///
/// `num_parts` must be passed in (it lives in the v1 field area the caller
/// has already validated) so the per-part counts can be sliced safely.
pub(crate) fn parse_header_v2(hdr: &[u8], num_parts: u16) -> Result<ChunkedHeader, String> {
    if hdr.len() < HDR_V2_PART_COUNTS_FIELD + num_parts as usize * 2 {
        return Err("header too short for v2 part counts".to_string());
    }
    let rd_u64 = |off: usize| {
        u64::from_le_bytes(
            hdr[off..off + 8]
                .try_into()
                .expect("header slice is exactly 8 bytes"),
        )
    };
    let rd_u32 = |off: usize| {
        u32::from_le_bytes(
            hdr[off..off + 4]
                .try_into()
                .expect("header slice is exactly 4 bytes"),
        )
    };
    let rd_u16 = |off: usize| {
        u16::from_le_bytes(
            hdr[off..off + 2]
                .try_into()
                .expect("header slice is exactly 2 bytes"),
        )
    };
    let part_counts: Vec<u16> = (0..num_parts as usize)
        .map(|i| rd_u16(HDR_V2_PART_COUNTS_FIELD + i * 2))
        .collect();
    let total_chunks = rd_u32(HDR_V2_COUNT_FIELD);
    let sum: u32 = part_counts.iter().map(|&c| c as u32).sum();
    if sum != total_chunks {
        return Err(format!(
            "chunk count mismatch: header says {} but per-part counts sum to {}",
            total_chunks, sum
        ));
    }
    Ok(ChunkedHeader {
        table_offset: rd_u64(HDR_V2_TABLE_OFFSET_FIELD),
        total_chunks,
        part_counts,
    })
}

/// Patch the v2 fields INTO a 4096-byte header (writer side).
///
/// The header must already carry version 2 (the caller sets it); this only
/// fills the previously-zero padding area.
pub(crate) fn patch_header_v2(hdr: &mut [u8], table_offset: u64, part_counts: &[u16]) {
    hdr[HDR_V2_TABLE_OFFSET_FIELD..HDR_V2_TABLE_OFFSET_FIELD + 8]
        .copy_from_slice(&table_offset.to_le_bytes());
    let total: u32 = part_counts.iter().map(|&c| c as u32).sum();
    hdr[HDR_V2_COUNT_FIELD..HDR_V2_COUNT_FIELD + 4].copy_from_slice(&total.to_le_bytes());
    for (i, &c) in part_counts.iter().enumerate() {
        let off = HDR_V2_PART_COUNTS_FIELD + i * 2;
        hdr[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
}

/// Record the first error (first-wins) and flag the pool to wind down.
fn record_error(
    error: &Arc<Mutex<Option<String>>>,
    failed: &AtomicBool,
    msg: String,
) {
    let mut slot = error.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(msg);
    }
    failed.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
//  Worker pool sizing
// ---------------------------------------------------------------------------

/// Worker count for the chunked xz encoder.
///
/// Capped at 4 — below the zstd cap (6, T50) — because each worker here holds
/// a 64 MB in-flight chunk buffer plus its compressed output; 4 keeps peak
/// RAM predictable (~300 MB) where zstd's per-worker cost is only a window.
pub(crate) fn xz_pool_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(1)
}

// ---------------------------------------------------------------------------
//  Trailer writer (called by run_dd_build after the partition loop)
// ---------------------------------------------------------------------------

/// Append the chunk-table trailer at the current (aligned) position.
///
/// Aligns to 4096 first, writes all partitions' entries concatenated in
/// partition order, flushes, and returns the absolute trailer offset.
pub(crate) fn write_chunk_table_trailer(
    out: &mut File,
    entries_per_part: &[Vec<ChunkEntry>],
) -> Result<u64, String> {
    let pos = out
        .stream_position()
        .map_err(|e| format!("chunk trailer: stream_position failed: {}", e))?;
    let aligned = super::align_up(pos as usize, super::ALIGN) as u64;
    if aligned > pos {
        let pad = (aligned - pos) as usize;
        out.write_all(&vec![0u8; pad])
            .map_err(|e| format!("chunk trailer: padding write failed: {}", e))?;
    }
    let table_offset = aligned;
    let mut bytes = Vec::new();
    for entries in entries_per_part {
        bytes.extend_from_slice(&encode_chunk_table(entries));
    }
    out.write_all(&bytes)
        .map_err(|e| format!("chunk trailer: write failed: {}", e))?;
    out.flush()
        .map_err(|e| format!("chunk trailer: flush failed: {}", e))?;
    Ok(table_offset)
}

// ---------------------------------------------------------------------------
//  Chunked xz encoder
// ---------------------------------------------------------------------------

/// Compress `file_path` as independent xz chunks on a worker pool, writing
/// chunks to `out` in order (each 4096-aligned), and returning the chunk
/// table plus the same result triple the classic streaming path produces.
///
/// Architecture (deadlock-audited):
/// * READER thread — sequential hash pass over the input (unc_hash) + user
///   cancellation checks. Sends nothing; it can always terminate.
/// * WORKERS × N — each pulls chunk indices from an atomic counter, reads its
///   chunk from its own file handle in 4 MB sub-reads (feeding the shared
///   `fed_bytes` progress counter), xz-encodes it, sends `(idx, result)` on a
///   bounded channel. No input channel exists, so no producer can block on
///   vanished consumers.
/// * MAIN thread — the writer: receives results out of order into a BTreeMap,
///   flushes in order, hashes the compressed bytes (comp_hash), counts
///   `comp_size`, records entries, and drives the (non-Send) progress
///   callback by polling `fed_bytes` between `recv_timeout` waits.
///
/// Error funnel: first error (I/O, encoder, cancel) wins; the shared `failed`
/// flag makes every thread exit fast; a dropped writer side makes worker
/// `send` fail, which exits them — no thread can block forever.
///
/// `chunk_size` is the production 64 MB; tests inject smaller sizes. An
/// empty input still produces exactly one (empty) chunk so the flash side
/// never sees a zero-entry partition.
pub(crate) fn compress_xz_chunked_with_progress(
    file_path: &str,
    level: Option<i32>,
    mut out: File,
    chunk_size: u64,
    mut on_progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(StreamCompressResult, Vec<ChunkEntry>, File), String> {
    use sha2::{Digest, Sha256};

    let file_size = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot stat {}: {}", file_path, e))?
        .len();
    let level_clamped = resolve_level(ALG_XZ, level).clamp(0, 9) as u32;

    let num_chunks = file_size.div_ceil(chunk_size).max(1) as usize;
    let chunk_decomp_len =
        |idx: usize| -> u64 { chunk_size.min(file_size.saturating_sub(idx as u64 * chunk_size)) };

    let workers = xz_pool_workers().max(1);
    let failed = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Progress: total input bytes fed into encoders so far (4 MB granularity
    // across all workers). The writer polls it and drives the callback.
    let fed_bytes = Arc::new(AtomicU64::new(0));
    let next_idx = Arc::new(AtomicUsize::new(0));
    let unc_hash_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let (tx_out, rx_out) = mpsc::sync_channel::<(usize, Result<Vec<u8>, String>)>(workers + 2);

    let (entries, comp_size, comp_hash_hex) = std::thread::scope(|scope| {
        // ── Reader: sequential hash + cancellation pass ──
        {
            let failed = Arc::clone(&failed);
            let error = Arc::clone(&error);
            let unc_cell = Arc::clone(&unc_hash_cell);
            let path = file_path.to_string();
            scope.spawn(move || {
                let mut f = match File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        record_error(&error, &failed, format!("Cannot open {}: {}", path, e));
                        return;
                    }
                };
                let mut hasher = Sha256::new();
                let mut buf = vec![0u8; 4 * 1024 * 1024];
                loop {
                    if failed.load(Ordering::SeqCst) {
                        return;
                    }
                    if crate::cancel_requested() {
                        record_error(&error, &failed, crate::CANCEL_SENTINEL.to_string());
                        return;
                    }
                    let n = match f.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) => {
                            record_error(&error, &failed, format!("Read error: {}", e));
                            return;
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                let hex: String =
                    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
                *unc_cell.lock().unwrap_or_else(|p| p.into_inner()) = Some(hex);
            });
        }

        // ── Workers: read + xz-encode chunks, dispatch via atomic counter ──
        for _ in 0..workers {
            let failed = Arc::clone(&failed);
            let error = Arc::clone(&error);
            let fed_bytes = Arc::clone(&fed_bytes);
            let next_idx = Arc::clone(&next_idx);
            let tx_out = tx_out.clone();
            let path = file_path.to_string();
            scope.spawn(move || {
                let f = match File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        record_error(&error, &failed, format!("Cannot open {}: {}", path, e));
                        return;
                    }
                };
                // Per-worker reusable chunk buffer (grown once, reused).
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    if failed.load(Ordering::SeqCst) {
                        return;
                    }
                    let idx = next_idx.fetch_add(1, Ordering::SeqCst);
                    if idx >= num_chunks {
                        return;
                    }
                    let len = chunk_decomp_len(idx);
                    if (buf.len() as u64) < len {
                        buf.resize(len as usize, 0u8);
                    }
                    let data = &mut buf[..len as usize];
                    // Read in 4 MB sub-reads: progress granularity + a
                    // cancellation checkpoint inside big chunks.
                    let mut off: u64 = 0;
                    while off < len {
                        if failed.load(Ordering::SeqCst) {
                            return;
                        }
                        if crate::cancel_requested() {
                            let _ = tx_out.send((idx, Err(crate::CANCEL_SENTINEL.to_string())));
                            return;
                        }
                        let end = (off + 4 * 1024 * 1024).min(len);
                        let slice = &mut data[off as usize..end as usize];
                        if let Err(e) = f.read_exact_at(slice, idx as u64 * chunk_size + off) {
                            let _ = tx_out.send((idx, Err(format!("Read error: {}", e))));
                            return;
                        }
                        fed_bytes.fetch_add(end - off, Ordering::SeqCst);
                        off = end;
                    }
                    // Independent xz stream per chunk (same level clamp as
                    // the classic path).
                    let mut enc = xz2::write::XzEncoder::new(Vec::new(), level_clamped);
                    let comp = match enc.write_all(data).and_then(|_| enc.finish()) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx_out.send((idx, Err(format!("xz encode error: {}", e))));
                            return;
                        }
                    };
                    if tx_out.send((idx, Ok(comp))).is_err() {
                        return; // writer gone — exit fast
                    }
                }
            });
        }
        drop(tx_out); // workers hold clones; the channel closes when they exit

        // ── Writer (main thread): ordered flush + progress + hashes ──
        let mut comp_hasher = Sha256::new();
        let mut comp_size: u64 = 0;
        let mut entries: Vec<ChunkEntry> = Vec::with_capacity(num_chunks);
        let mut pending: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        let mut next_write = 0usize;
        let mut received = 0usize;
        let mut writer_err: Option<String> = None;
        let mut last_pct: i32 = -1;

        let mut report_progress = |fed: u64| {
            if let Some(ref mut cb) = on_progress {
                let shown = fed.min(file_size);
                let pct = if file_size > 0 {
                    (shown as f64 / file_size as f64 * 100.0) as i32
                } else {
                    100
                };
                if pct != last_pct {
                    last_pct = pct;
                    cb(shown, file_size);
                }
            }
        };

        while received < num_chunks {
            let evt = if writer_err.is_some() {
                None
            } else {
                match rx_out.recv_timeout(Duration::from_millis(PROGRESS_POLL_MS)) {
                    Ok(v) => Some(v),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        report_progress(fed_bytes.load(Ordering::SeqCst));
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                }
            };
            let (idx, res) = match evt {
                Some(v) => v,
                None => {
                    if writer_err.is_none() {
                        writer_err =
                            Some("chunked encoder: worker pool exited unexpectedly".to_string());
                    }
                    break;
                }
            };
            received += 1;
            match res {
                Ok(data) => {
                    pending.insert(idx, data);
                }
                Err(e) => {
                    writer_err = Some(e);
                    break;
                }
            }
            // Flush the in-order prefix.
            while writer_err.is_none() {
                let data = match pending.remove(&next_write) {
                    Some(d) => d,
                    None => break,
                };
                let comp_offset = match out.stream_position() {
                    Ok(p) => p,
                    Err(e) => {
                        writer_err =
                            Some(format!("chunked encoder: stream_position failed: {}", e));
                        break;
                    }
                };
                if let Err(e) = out.write_all(&data) {
                    writer_err = Some(format!("chunked encoder: write failed: {}", e));
                    break;
                }
                let pos = match out.stream_position() {
                    Ok(p) => p,
                    Err(e) => {
                        writer_err =
                            Some(format!("chunked encoder: stream_position failed: {}", e));
                        break;
                    }
                };
                let aligned = super::align_up(pos as usize, super::ALIGN) as u64;
                if aligned > pos {
                    let pad = vec![0u8; (aligned - pos) as usize];
                    if let Err(e) = out.write_all(&pad) {
                        writer_err =
                            Some(format!("chunked encoder: padding write failed: {}", e));
                        break;
                    }
                }
                comp_hasher.update(&data);
                comp_size += data.len() as u64;
                entries.push(ChunkEntry {
                    comp_offset,
                    comp_len: data.len() as u64,
                    decomp_len: chunk_decomp_len(next_write),
                });
                next_write += 1;
                report_progress(fed_bytes.load(Ordering::SeqCst));
            }
            if writer_err.is_some() {
                break;
            }
        }
        drop(rx_out);

        if writer_err.is_none() && next_write != num_chunks {
            writer_err = Some("chunked encoder: not all chunks were written".to_string());
        }
        if let Some(e) = writer_err {
            record_error(&error, &failed, e);
        }

        let hex: String = comp_hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        (entries, comp_size, hex)
    });

    // Funnel any recorded error (I/O, encoder, cancel) out of the pool.
    if let Some(e) = error.lock().unwrap_or_else(|p| p.into_inner()).take() {
        return Err(e);
    }
    let unc_hash_hex = unc_hash_cell
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .ok_or_else(|| "chunked encoder: hash pass did not complete".to_string())?;

    Ok((
        StreamCompressResult {
            comp_size,
            unc_hash_hex,
            comp_hash_hex,
        },
        entries,
        out,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn tmp_path(name: &str) -> String {
        let p = std::env::temp_dir().join(name);
        p.to_string_lossy().to_string()
    }

    #[test]
    fn test_chunk_table_roundtrip() {
        let entries = vec![
            ChunkEntry { comp_offset: 4096, comp_len: 1000, decomp_len: 64 * 1024 * 1024 },
            ChunkEntry { comp_offset: 8192, comp_len: 20, decomp_len: 123 },
        ];
        let bytes = encode_chunk_table(&entries);
        assert_eq!(bytes.len(), 2 * CHUNK_ENTRY_SIZE);
        let back = decode_chunk_table(&bytes).unwrap();
        assert_eq!(back, entries);
        // Torn trailer must be rejected, never half-parsed.
        assert!(decode_chunk_table(&bytes[..23]).is_err());
    }

    #[test]
    fn test_header_v2_patch_parse_roundtrip() {
        let mut hdr = vec![0u8; 4096];
        let counts = vec![3u16, 0u16, 12u16];
        patch_header_v2(&mut hdr, 987654321u64, &counts);
        let parsed = parse_header_v2(&hdr, 3).unwrap();
        assert_eq!(parsed.table_offset, 987654321);
        assert_eq!(parsed.part_counts, counts);
        assert_eq!(parsed.total_chunks, 15);
        // Count mismatch (header vs per-part sums) must fail loudly.
        let mut bad = hdr.clone();
        bad[HDR_V2_COUNT_FIELD..HDR_V2_COUNT_FIELD + 4].copy_from_slice(&16u32.to_le_bytes());
        assert!(parse_header_v2(&bad, 3).is_err());
        // Short header rejected.
        assert!(parse_header_v2(&hdr[..30], 3).is_err());
    }

    #[test]
    fn test_xz_pool_workers_envelope() {
        let n = xz_pool_workers();
        assert!((1..=4).contains(&n), "xz pool workers must be 1..=4, got {}", n);
    }

    /// End-to-end format round-trip: dummy header → chunked encode → trailer
    /// → header patch → parse back → per-chunk decode == original slices.
    /// 600 KB across 3 chunks of 256 KB, level 1 for speed.
    #[test]
    fn test_compress_xz_chunked_full_bundle_roundtrip() {
        use sha2::{Digest, Sha256};

        let data: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
        let in_path = tmp_path("t51_chunked_in.bin");
        let out_path = tmp_path("t51_chunked_bundle.bin");
        std::fs::write(&in_path, &data).unwrap();

        let mut calls = 0usize;
        let mut out = File::create(&out_path).unwrap();
        // Dummy 4096-byte header placeholder (production writes it later).
        out.write_all(&vec![0u8; 4096]).unwrap();

        let (result, entries, mut out) = compress_xz_chunked_with_progress(
            &in_path,
            Some(1),
            out,
            256 * 1024,
            Some(&mut |_read, _total| {
                calls += 1;
            }),
        )
        .expect("chunked encode");

        // 600 KB / 256 KB → 3 chunks (last partial).
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|e| e.decomp_len).sum::<u64>(),
            data.len() as u64
        );
        // Monotonic, 4096-aligned offsets, first chunk right after header.
        assert_eq!(entries[0].comp_offset, 4096);
        for w in entries.windows(2) {
            assert!(w[1].comp_offset > w[0].comp_offset);
        }
        for e in &entries {
            assert_eq!(e.comp_offset % 4096, 0, "chunk data must be 4096-aligned");
        }
        // Hashes must match manual computation.
        assert_eq!(
            result.unc_hash_hex,
            Sha256::digest(&data).iter().map(|b| format!("{:02x}", b)).collect::<String>()
        );
        assert!(!result.comp_hash_hex.is_empty());
        assert!(result.comp_size > 0);
        assert!(calls > 0, "progress callback must fire");

        // Trailer + v2 header, mirroring run_dd_build's post-loop steps.
        let table_offset =
            write_chunk_table_trailer(&mut out, &[entries.clone()]).expect("trailer");
        let mut hdr = vec![0u8; 4096];
        hdr[..4].copy_from_slice(b"DDBU");
        hdr[4..6].copy_from_slice(&DDBUNDLE_VERSION_V2.to_le_bytes());
        hdr[6..8].copy_from_slice(&3u16.to_le_bytes()); // compress_id = xz
        hdr[8..10].copy_from_slice(&1u16.to_le_bytes()); // num_parts
        hdr[10..12].copy_from_slice(&4096u16.to_le_bytes()); // header_size
        patch_header_v2(&mut hdr, table_offset, &[entries.len() as u16]);
        out.seek(SeekFrom::Start(0)).unwrap();
        out.write_all(&hdr).unwrap();
        drop(out);

        // Parse back and per-chunk decode.
        let bundle = std::fs::read(&out_path).unwrap();
        let parsed = parse_header_v2(&bundle[..4096], 1).unwrap();
        assert_eq!(parsed.table_offset, table_offset);
        assert_eq!(parsed.total_chunks as usize, entries.len());
        let table_bytes = &bundle[parsed.table_offset as usize..];
        let back = decode_chunk_table(table_bytes).unwrap();
        assert_eq!(back, entries);

        let mut decomp_pos = 0usize;
        for e in &back {
            let comp =
                &bundle[e.comp_offset as usize..(e.comp_offset + e.comp_len) as usize];
            let mut dec = xz2::read::XzDecoder::new(comp);
            let mut out_buf = Vec::new();
            dec.read_to_end(&mut out_buf).unwrap();
            assert_eq!(out_buf.len(), e.decomp_len as usize);
            assert_eq!(
                &out_buf[..],
                &data[decomp_pos..decomp_pos + e.decomp_len as usize]
            );
            decomp_pos += e.decomp_len as usize;
        }
        assert_eq!(decomp_pos, data.len());

        let _ = std::fs::remove_file(&in_path);
        let _ = std::fs::remove_file(&out_path);
    }

    /// Empty input → exactly one empty chunk (flash side never sees zero).
    #[test]
    fn test_compress_xz_chunked_empty_input() {
        let in_path = tmp_path("t51_chunked_empty.bin");
        let out_path = tmp_path("t51_chunked_empty_out.bin");
        std::fs::write(&in_path, b"").unwrap();
        let out = File::create(&out_path).unwrap();
        let (result, entries, _out) =
            compress_xz_chunked_with_progress(&in_path, None, out, 256 * 1024, None)
                .expect("empty encode");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].decomp_len, 0);
        assert_eq!(
            result.unc_hash_hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_file(&in_path);
        let _ = std::fs::remove_file(&out_path);
    }
}
