//! decomp.rs — universal bundled decompressor engine (T49).
//!
//! The flashable ZIP carries its own decompressor (`otaku-decomp`, a thin
//! CLI wrapper over this module) so flashing never depends on recovery-
//! provided gzip/bzip2/xz/lz4/zstd binaries — recoveries vary wildly
//! (OrangeFox ships lz4/zstd only behind device-maintainer flags
//! FOX_USE_LZ4_BINARY / FOX_USE_ZSTD_BINARY; TWRP busybox applets differ
//! per build).
//!
//! Zero new dependencies: every codec is already a crate dependency used
//! by the in-app payload pipeline (flate2 / bzip2 / xz2 / lz4_flex / zstd),
//! and ZIP reading reuses the `zip` crate that assembles the flashable
//! ZIP itself.
//!
//! Threading note (T49): the `zstdmt` feature is compiled into libzstd
//! (multi-thread capable). Decompression of a single zstd/lz4 frame is
//! inherently single-thread — MT engages only for multi-frame streams;
//! xz2 0.1 does not expose liblzma's threaded decoder, so XZ decode stays
//! single-threaded here (documented deviation from the "MT helper" ideal;
//! the old recovery-side `xz -T0` upgrade path only helped multi-block
//! streams too, so no capability is lost vs the pre-T49 template).
//!
//! T51 closes that gap at the FORMAT level: chunked-xz bundles (DDBU v2)
//! split each partition into independently-compressed 64 MB chunks, so the
//! `flash_chunked` pool below decodes them in parallel — ordered pipe to
//! stdout by default, direct pwrite to the block device opt-in. Classic
//! single-stream bundles (v1) keep the `decompress_stream` path above,
//! untouched.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Version of the bundled helper (surfaced by `otaku-decomp --version`).
/// T51: 1.1.0 — adds the chunked flash path (`--flash-chunked`).
pub const DECOMP_VERSION: &str = "1.1.0";

/// I/O buffer size for streaming pipelines (1 MiB).
const BUF: usize = 1 << 20;

/// Compression algorithms supported by the bundled decompressor.
/// Maps 1:1 onto the DDBU bundle COMPRESS_ID values (1/2/3/5/6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    Gzip,
    Bzip2,
    Xz,
    Lz4,
    Zstd,
}

impl Alg {
    /// Parse a CLI algorithm name (`-a gzip` etc.).
    pub fn from_name(s: &str) -> Option<Alg> {
        match s {
            "gzip" => Some(Alg::Gzip),
            "bzip2" => Some(Alg::Bzip2),
            "xz" => Some(Alg::Xz),
            "lz4" => Some(Alg::Lz4),
            "zstd" => Some(Alg::Zstd),
            _ => None,
        }
    }

    /// Canonical name (also the `-a` flag value and the DDBU alg name).
    pub fn name(&self) -> &'static str {
        match self {
            Alg::Gzip => "gzip",
            Alg::Bzip2 => "bzip2",
            Alg::Xz => "xz",
            Alg::Lz4 => "lz4",
            Alg::Zstd => "zstd",
        }
    }

    /// Map a DDBU bundle COMPRESS_ID to the algorithm.
    /// 0 (ALG_NONE) and legacy 4 (brotli) intentionally map to None — the
    /// flasher template handles id 0 with plain `cat` and aborts on id 4.
    pub fn from_compress_id(id: u16) -> Option<Alg> {
        match id {
            1 => Some(Alg::Gzip),
            2 => Some(Alg::Bzip2),
            3 => Some(Alg::Xz),
            5 => Some(Alg::Lz4),
            6 => Some(Alg::Zstd),
            _ => None,
        }
    }
}

/// Copy a decoder's output into a buffered writer, then flush.
fn pipe<D: Read, W: Write>(mut decoder: D, writer: W) -> io::Result<u64> {
    let mut writer = BufWriter::with_capacity(BUF, writer);
    let n = io::copy(&mut decoder, &mut writer)?;
    writer.flush()?;
    Ok(n)
}

/// Decompress `reader` into `writer`. Returns the number of decompressed
/// bytes written. Streaming: memory use is bounded by the internal buffer
/// regardless of the decompressed size (multi-GB partitions flash through
/// a pipe, never fully materialized).
pub fn decompress_stream<R: Read, W: Write>(alg: Alg, reader: R, writer: W) -> io::Result<u64> {
    match alg {
        Alg::Gzip => pipe(
            flate2::read::GzDecoder::new(BufReader::with_capacity(BUF, reader)),
            writer,
        ),
        Alg::Bzip2 => pipe(
            bzip2::read::BzDecoder::new(BufReader::with_capacity(BUF, reader)),
            writer,
        ),
        Alg::Xz => pipe(
            xz2::read::XzDecoder::new(BufReader::with_capacity(BUF, reader)),
            writer,
        ),
        Alg::Lz4 => pipe(
            lz4_flex::frame::FrameDecoder::new(BufReader::with_capacity(BUF, reader)),
            writer,
        ),
        Alg::Zstd => {
            let decoder = zstd::Decoder::new(BufReader::with_capacity(BUF, reader))?;
            pipe(decoder, writer)
        }
    }
}

/// Extract a single entry from a ZIP archive to `out_path`, verifying the
/// entry's CRC in the process (the `zip` crate checks CRC on read).
///
/// This replaces the flasher's old reliance on recovery-provided
/// `unzip`/`busybox unzip`/`toybox unzip` for pulling `otaku.bin` out of
/// the flashable ZIP.
///
/// Returns the extracted size in bytes.
pub fn unzip_entry(zip_path: &str, entry_name: &str, out_path: &str) -> Result<u64, String> {
    let file = File::open(zip_path)
        .map_err(|e| format!("cannot open ZIP '{}': {}", zip_path, e))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("ZIP open error ('{}'): {}", zip_path, e))?;
    let mut entry = archive
        .by_name(entry_name)
        .map_err(|e| format!("entry '{}' not found in ZIP: {}", entry_name, e))?;

    // Ensure the output directory exists (best-effort — /tmp always does).
    if let Some(parent) = Path::new(out_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let out = File::create(out_path)
        .map_err(|e| format!("cannot create '{}': {}", out_path, e))?;
    let mut out = BufWriter::with_capacity(BUF, out);
    let n = io::copy(&mut entry, &mut out)
        .map_err(|e| format!("extract '{}' failed (CRC or I/O error): {}", entry_name, e))?;
    out.flush()
        .map_err(|e| format!("flush '{}' failed: {}", out_path, e))?;
    Ok(n)
}

/// Compress a small in-memory buffer with the given algorithm.
///
/// Only used by `selftest` (and unit tests) to produce known-good
/// compressed input for the decode path — the real bundle encoder lives
/// in `compression.rs`.
fn compress_bytes(alg: Alg, data: &[u8]) -> Result<Vec<u8>, String> {
    match alg {
        Alg::Gzip => {
            use flate2::write::GzEncoder;
            let mut e = GzEncoder::new(Vec::new(), flate2::Compression::new(6));
            e.write_all(data).map_err(|e| e.to_string())?;
            e.finish().map_err(|e| e.to_string())
        }
        Alg::Bzip2 => {
            use bzip2::write::BzEncoder;
            let mut e = BzEncoder::new(Vec::new(), bzip2::Compression::new(1));
            e.write_all(data).map_err(|e| e.to_string())?;
            e.finish().map_err(|e| e.to_string())
        }
        Alg::Xz => {
            let mut e = xz2::write::XzEncoder::new(Vec::new(), 6);
            e.write_all(data).map_err(|e| e.to_string())?;
            e.finish().map_err(|e| e.to_string())
        }
        Alg::Lz4 => {
            // Same auto_finish + scope-drop pattern as compression.rs.
            let mut out = Vec::new();
            {
                let mut e = lz4_flex::frame::FrameEncoder::new(&mut out).auto_finish();
                e.write_all(data).map_err(|e| e.to_string())?;
            }
            Ok(out)
        }
        Alg::Zstd => {
            let mut out = Vec::new();
            let mut e = zstd::Encoder::new(&mut out, 3).map_err(|e| e.to_string())?;
            e.write_all(data).map_err(|e| e.to_string())?;
            e.finish().map_err(|e| e.to_string())?;
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
//  T51: chunked flash path (DDBU v2)
// ---------------------------------------------------------------------------

/// Where the chunked flash path writes its decompressed output.
///
/// * `Stdout` — ordered stream to stdout (pipe mode, the flasher's default:
///   the existing `dd of=<partition>` machinery consumes it unchanged).
/// * `File`   — ordered stream to a file (selftest / debugging).
/// * `Pwrite` — each finished chunk is written DIRECTLY to the block device
///   at its decompressed offset via `pwrite` — no FIFO, no `dd`, and no
///   ordering constraint (chunks are independent). Opt-in from the flasher
///   template via `OTAKU_WRITE_MODE=pwrite`.
#[derive(Debug, Clone)]
pub enum ChunkOut {
    Stdout,
    File(String),
    Pwrite(String),
}

/// Flash (decode) one partition of a DDBU v2 chunked bundle on a worker pool.
///
/// `base_off` shifts every table offset — non-zero when `bundle` is the
/// flashable ZIP itself in the flasher's direct-read mode (table offsets are
/// absolute within otaku.bin; the ZIP local-file-header shift is added on
/// top). Pass 0 for an extracted otaku.bin.
///
/// Verification order mirrors the classic flasher: if `expected_comp_hash`
/// is provided, the partition's compressed bytes are hashed and compared
/// BEFORE anything is decoded or written — a corrupt bundle aborts while
/// the partition is still untouched.
///
/// Returns the total decompressed bytes written.
pub fn flash_chunked(
    bundle: &str,
    base_off: u64,
    part_idx: usize,
    out: ChunkOut,
    expected_comp_hash: Option<&str>,
) -> Result<u64, String> {
    use crate::dd::chunked::{decode_chunk_table, parse_header_v2, ChunkEntry};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::FileExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    let hdr_size = 4096usize;
    let mut f = File::open(bundle)
        .map_err(|e| format!("chunked flash: cannot open '{}': {}", bundle, e))?;
    f.seek(SeekFrom::Start(base_off))
        .map_err(|e| format!("chunked flash: seek failed: {}", e))?;
    let mut hdr = vec![0u8; hdr_size];
    f.read_exact(&mut hdr)
        .map_err(|e| format!("chunked flash: header read failed: {}", e))?;

    // Magic + version + compress id gates (defense in depth — the flasher
    // template gates on the baked constants too; a mixed/corrupt ZIP must
    // fail here before any device write).
    if hdr[..4] != *b"DDBU" {
        return Err("chunked flash: bad bundle magic (expected DDBU)".to_string());
    }
    let version = u16::from_le_bytes([hdr[4], hdr[5]]);
    if version != 2 {
        return Err(format!(
            "chunked flash: bundle version {} is not a chunked (v2) bundle — \
             rebuild with a current OTAku xz build",
            version
        ));
    }
    let compress_id = u16::from_le_bytes([hdr[6], hdr[7]]);
    if compress_id != 3 {
        return Err(format!(
            "chunked flash: compress id {} is not xz (chunked bundles are xz-only)",
            compress_id
        ));
    }
    let num_parts = u16::from_le_bytes([hdr[8], hdr[9]]);
    if part_idx >= num_parts as usize {
        return Err(format!(
            "chunked flash: partition index {} out of range (bundle has {})",
            part_idx, num_parts
        ));
    }

    let info = parse_header_v2(&hdr, num_parts)?;
    let start = info
        .part_counts
        .iter()
        .take(part_idx)
        .map(|&c| c as usize)
        .sum::<usize>();
    let count = info.part_counts[part_idx] as usize;
    if count == 0 {
        // The encoder never emits zero-entry partitions (empty images get
        // one empty chunk) — a zero here means a torn/corrupt header.
        return Err(format!(
            "chunked flash: partition {} has zero chunk entries — corrupt header?",
            part_idx
        ));
    }

    // Read this partition's slice of the trailer table.
    let mut table_bytes = vec![0u8; count * crate::dd::chunked::CHUNK_ENTRY_SIZE];
    f.seek(SeekFrom::Start(base_off + info.table_offset + (start * crate::dd::chunked::CHUNK_ENTRY_SIZE) as u64))
        .map_err(|e| format!("chunked flash: table seek failed: {}", e))?;
    f.read_exact(&mut table_bytes)
        .map_err(|e| format!("chunked flash: table read failed: {}", e))?;
    let entries: Vec<ChunkEntry> = decode_chunk_table(&table_bytes)?;

    // T53-F13: sanity-cap table fields BEFORE any allocation — a torn or
    // hand-edited table must fail with a clean error, never an uncatchable
    // OOM abort (vec![0u8; comp_len] / Vec::with_capacity(decomp_len) used
    // to trust attacker-controlled u64s). comp data must lie inside the
    // bundle file; decomp_len is bounded by the encoder's chunk size.
    let file_len = f
        .metadata()
        .map_err(|e| format!("chunked flash: stat failed: {}", e))?
        .len();
    for e in &entries {
        let comp_end = e
            .comp_offset
            .checked_add(e.comp_len)
            .and_then(|o| base_off.checked_add(o));
        match comp_end {
            Some(end) if end <= file_len => {}
            _ => {
                return Err(format!(
                    "chunked flash: chunk table entry out of bounds \
                     (comp_offset={}, comp_len={}, bundle={})",
                    e.comp_offset, e.comp_len, file_len
                ));
            }
        }
        if e.decomp_len > crate::dd::chunked::CHUNK_SIZE {
            return Err(format!(
                "chunked flash: chunk decomp_len {} exceeds chunk size {} — corrupt table",
                e.decomp_len,
                crate::dd::chunked::CHUNK_SIZE
            ));
        }
    }

    // Decompressed offset of each entry (running sum) — needed for pwrite
    // and for sanity checks.
    let mut decomp_offsets: Vec<u64> = Vec::with_capacity(entries.len());
    let mut acc: u64 = 0;
    for e in &entries {
        decomp_offsets.push(acc);
        acc = acc
            .checked_add(e.decomp_len)
            .ok_or_else(|| "chunked flash: decomp_len overflow in chunk table".to_string())?;
    }

    // ── Pre-verify the compressed data hash BEFORE decoding/writing ──
    if let Some(expected) = expected_comp_hash {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20]; // 1 MiB reusable read buffer
        for e in &entries {
            let mut off: u64 = 0;
            while off < e.comp_len {
                let n = ((1u64) << 20).min(e.comp_len - off) as usize;
                f.read_exact_at(&mut buf[..n], base_off + e.comp_offset + off)
                    .map_err(|err| format!("chunked flash: comp read failed: {}", err))?;
                hasher.update(&buf[..n]);
                off += n as u64;
            }
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        if hex != expected {
            return Err(format!(
                "chunked flash: compressed data hash mismatch for partition {} — \
                 bundle is corrupt; rebuild and re-transfer",
                part_idx
            ));
        }
    }

    // ── Decode pool ──
    // Same envelope as the encoder: min(available_parallelism, 4) workers —
    // each holds one in-flight chunk (≤ 64 MB input + output).
    let workers = crate::dd::chunked::xz_pool_workers().max(1).min(entries.len().max(1));
    let f = Arc::new(f);
    let next_idx = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel::<(usize, Result<Vec<u8>, String>)>(workers + 2);

    let total_written: u64 = std::thread::scope(|scope| {
        for _ in 0..workers {
            let f = Arc::clone(&f);
            let next_idx = Arc::clone(&next_idx);
            let failed = Arc::clone(&failed);
            let tx = tx.clone();
            let entries = &entries;
            scope.spawn(move || loop {
                if failed.load(Ordering::SeqCst) {
                    return;
                }
                let idx = next_idx.fetch_add(1, Ordering::SeqCst);
                if idx >= entries.len() {
                    return;
                }
                let e = entries[idx];
                let mut comp = vec![0u8; e.comp_len as usize];
                if let Err(err) = f.read_exact_at(&mut comp, base_off + e.comp_offset) {
                    let _ = tx.send((idx, Err(format!("comp read failed: {}", err))));
                    failed.store(true, Ordering::SeqCst);
                    return;
                }
                let mut dec = xz2::read::XzDecoder::new(&comp[..]);
                let mut data = Vec::with_capacity(e.decomp_len as usize);
                if let Err(err) = dec.read_to_end(&mut data) {
                    let _ = tx.send((idx, Err(format!("xz decode failed: {}", err))));
                    failed.store(true, Ordering::SeqCst);
                    return;
                }
                if data.len() as u64 != e.decomp_len {
                    let _ = tx.send((
                        idx,
                        Err(format!(
                            "chunk {} decoded to {} bytes, table says {}",
                            idx,
                            data.len(),
                            e.decomp_len
                        )),
                    ));
                    failed.store(true, Ordering::SeqCst);
                    return;
                }
                if tx.send((idx, Ok(data))).is_err() {
                    return; // writer gone
                }
            });
        }
        drop(tx);

        // ── Writer (main thread) ──
        match out {
            ChunkOut::Pwrite(ref dev_path) => {
                let dev = std::fs::OpenOptions::new()
                    .write(true)
                    .open(dev_path)
                    .map_err(|e| {
                        format!("chunked flash: cannot open device '{}': {}", dev_path, e)
                    })?;
                let mut written: u64 = 0;
                let mut received = 0usize;
                let mut err: Option<String> = None;
                while received < entries.len() {
                    let (idx, res) = match rx.recv() {
                        Ok(v) => v,
                        Err(_) => {
                            err = Some("chunked flash: decode pool exited unexpectedly".into());
                            break;
                        }
                    };
                    received += 1;
                    match res {
                        Ok(data) => {
                            if let Err(e) = dev.write_all_at(&data, decomp_offsets[idx]) {
                                err = Some(format!("chunked flash: pwrite failed: {}", e));
                                break;
                            }
                            written += data.len() as u64;
                        }
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
                drop(rx);
                match err {
                    Some(e) => Err(e),
                    None => Ok(written),
                }
            }
            ChunkOut::Stdout | ChunkOut::File(_) => {
                // Ordered stream: buffer out-of-order completions, flush in
                // order (bounded by workers in flight).
                let mut sink: BufWriter<Box<dyn Write>> = if let ChunkOut::File(ref p) = out {
                    BufWriter::with_capacity(
                        BUF,
                        Box::new(
                            File::create(p)
                                .map_err(|e| format!("chunked flash: cannot create '{}': {}", p, e))?,
                        ),
                    )
                } else {
                    BufWriter::with_capacity(BUF, Box::new(io::stdout()))
                };
                let mut pending: std::collections::BTreeMap<usize, Vec<u8>> =
                    std::collections::BTreeMap::new();
                let mut next_write = 0usize;
                let mut received = 0usize;
                let mut written: u64 = 0;
                let mut err: Option<String> = None;
                while received < entries.len() {
                    let (idx, res) = match rx.recv() {
                        Ok(v) => v,
                        Err(_) => {
                            err = Some("chunked flash: decode pool exited unexpectedly".into());
                            break;
                        }
                    };
                    received += 1;
                    match res {
                        Ok(data) => {
                            pending.insert(idx, data);
                        }
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                    while err.is_none() {
                        let data = match pending.remove(&next_write) {
                            Some(d) => d,
                            None => break,
                        };
                        if let Err(e) = sink.write_all(&data) {
                            err = Some(format!("chunked flash: write failed: {}", e));
                            break;
                        }
                        written += data.len() as u64;
                        next_write += 1;
                    }
                    if err.is_some() {
                        break;
                    }
                }
                // Flush stdout/file BEFORE dropping — a short write at the end
                // of a pipe is a failed flash, not a warning.
                if err.is_none() {
                    if let Err(e) = sink.flush() {
                        err = Some(format!("chunked flash: flush failed: {}", e));
                    }
                }
                drop(rx);
                match err {
                    Some(e) => Err(e),
                    None => Ok(written),
                }
            }
        }
    })?;

    Ok(total_written)
}

/// Self-test: round-trip every algorithm plus a ZIP extraction, with no
/// filesystem dependencies outside the system temp dir.
///
/// The flasher runs `otaku-decomp --selftest` right after extracting the
/// helper from the flashable ZIP: if this passes, decompression on this
/// recovery is proven working BEFORE any partition is touched.
pub fn selftest() -> Result<(), String> {
    let payload: Vec<u8> = b"otaku-decomp selftest payload - OTAku T49 bundled decompressor. "
        .repeat(16);

    for alg in [Alg::Gzip, Alg::Bzip2, Alg::Xz, Alg::Lz4, Alg::Zstd] {
        let compressed = compress_bytes(alg, &payload)?;
        let mut out = Vec::new();
        decompress_stream(alg, &compressed[..], &mut out)
            .map_err(|e| format!("{}: decode error: {}", alg.name(), e))?;
        if out != payload {
            return Err(format!("{}: round-trip mismatch ({} bytes out)", alg.name(), out.len()));
        }
    }

    // ZIP extraction round-trip (covers --unzip-entry).
    let tmp_zip = std::env::temp_dir().join("otaku-decomp-selftest.zip");
    let tmp_out = std::env::temp_dir().join("otaku-decomp-selftest.out");
    {
        let f = File::create(&tmp_zip)
            .map_err(|e| format!("selftest zip create: {}", e))?;
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file("selftest.bin", opts)
            .map_err(|e| format!("selftest zip entry: {}", e))?;
        w.write_all(b"zip-selftest-ok")
            .map_err(|e| format!("selftest zip write: {}", e))?;
        w.finish().map_err(|e| format!("selftest zip finish: {}", e))?;
    }
    let n = unzip_entry(
        tmp_zip.to_str().unwrap_or_default(),
        "selftest.bin",
        tmp_out.to_str().unwrap_or_default(),
    )?;
    let read_back = std::fs::read(&tmp_out)
        .map_err(|e| format!("selftest read back: {}", e))?;
    let _ = std::fs::remove_file(&tmp_zip);
    let _ = std::fs::remove_file(&tmp_out);
    if read_back != b"zip-selftest-ok" || n != read_back.len() as u64 {
        return Err("zip extraction round-trip mismatch".to_string());
    }

    // T51: chunked flash path round-trip — build a mini DDBU v2 bundle
    // (2 independently-compressed chunks + trailer), decode it back via
    // flash_chunked, and prove the compressed-hash gate refuses a bad hash.
    // If this passes, the chunked flasher wiring is proven working on this
    // recovery BEFORE any partition is touched (same guarantee the codec
    // round-trips above give for classic bundles).
    {
        use crate::dd::chunked::{encode_chunk_table, patch_header_v2, ChunkEntry};
        use sha2::{Digest, Sha256};
        use std::io::Seek as _;

        let data = b"otaku-decomp chunked selftest payload. ".repeat(48);
        let half = data.len() / 2;
        let c0 = compress_bytes(Alg::Xz, &data[..half])?;
        let c1 = compress_bytes(Alg::Xz, &data[half..])?;

        let tmp_bundle = std::env::temp_dir().join("otaku-decomp-selftest-chunked.bin");
        let tmp_out = std::env::temp_dir().join("otaku-decomp-selftest-chunked.out");
        let comp_hash: String;
        {
            let mut f = File::create(&tmp_bundle)
                .map_err(|e| format!("selftest chunked bundle create: {}", e))?;
            let wr = |f: &mut File, bytes: &[u8], what: &str| -> Result<(), String> {
                f.write_all(bytes)
                    .map_err(|e| format!("selftest chunked {}: {}", what, e))
            };
            let mut hdr = vec![0u8; 4096];
            hdr[..4].copy_from_slice(b"DDBU");
            hdr[4..6].copy_from_slice(&2u16.to_le_bytes()); // version 2
            hdr[6..8].copy_from_slice(&3u16.to_le_bytes()); // compress id = xz
            hdr[8..10].copy_from_slice(&1u16.to_le_bytes()); // num_parts
            hdr[10..12].copy_from_slice(&4096u16.to_le_bytes()); // header size
            // (v2 fields are patched AFTER the trailer offset is known)
            wr(&mut f, &hdr, "header")?;

            let mut entries: Vec<ChunkEntry> = Vec::new();
            for (i, comp) in [&c0, &c1].iter().enumerate() {
                let comp_offset = f
                    .stream_position()
                    .map_err(|e| format!("selftest chunked pos: {}", e))?;
                wr(&mut f, comp, "chunk")?;
                let pos = f
                    .stream_position()
                    .map_err(|e| format!("selftest chunked pos: {}", e))?;
                let aligned = pos.div_ceil(4096) * 4096;
                if aligned > pos {
                    wr(&mut f, &vec![0u8; (aligned - pos) as usize], "pad")?;
                }
                entries.push(ChunkEntry {
                    comp_offset,
                    comp_len: comp.len() as u64,
                    decomp_len: if i == 0 {
                        half as u64
                    } else {
                        (data.len() - half) as u64
                    },
                });
            }
            let table_offset = f
                .stream_position()
                .map_err(|e| format!("selftest chunked pos: {}", e))?;
            wr(&mut f, &encode_chunk_table(&entries), "trailer")?;
            patch_header_v2(&mut hdr, table_offset, &[2]);
            f.seek(std::io::SeekFrom::Start(0))
                .map_err(|e| format!("selftest chunked seek: {}", e))?;
            wr(&mut f, &hdr, "header rewrite")?;
            f.flush()
                .map_err(|e| format!("selftest chunked flush: {}", e))?;

            let mut hasher = Sha256::new();
            hasher.update(&c0);
            hasher.update(&c1);
            comp_hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        }

        // Good hash → full decode round-trip.
        let n = flash_chunked(
            tmp_bundle.to_str().unwrap_or_default(),
            0,
            0,
            ChunkOut::File(tmp_out.to_str().unwrap_or_default().to_string()),
            Some(&comp_hash),
        )?;
        let read_back = std::fs::read(&tmp_out)
            .map_err(|e| format!("selftest chunked read back: {}", e))?;
        if read_back != data || n != data.len() as u64 {
            return Err("chunked flash round-trip mismatch".to_string());
        }
        // Bad hash → must refuse BEFORE decoding (output file must be
        // untouched: the gate runs ahead of the pool).
        let bad = flash_chunked(
            tmp_bundle.to_str().unwrap_or_default(),
            0,
            0,
            ChunkOut::File(tmp_out.to_str().unwrap_or_default().to_string()),
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        );
        if bad.is_ok() {
            return Err("chunked flash accepted a WRONG compressed hash".to_string());
        }
        let _ = std::fs::remove_file(&tmp_bundle);
        let _ = std::fs::remove_file(&tmp_out);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alg_roundtrip_all_five() {
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        for alg in [Alg::Gzip, Alg::Bzip2, Alg::Xz, Alg::Lz4, Alg::Zstd] {
            let compressed = compress_bytes(alg, &data).expect("compress");
            assert!(
                !compressed.is_empty(),
                "{}: empty compressed output",
                alg.name()
            );
            let mut out = Vec::new();
            decompress_stream(alg, &compressed[..], &mut out).expect("decompress");
            assert_eq!(out, data, "{}: round-trip mismatch", alg.name());
        }
    }

    #[test]
    fn test_alg_mapping() {
        assert_eq!(Alg::from_name("gzip"), Some(Alg::Gzip));
        assert_eq!(Alg::from_name("zstd"), Some(Alg::Zstd));
        assert_eq!(Alg::from_name("brotli"), None);
        assert_eq!(Alg::from_compress_id(0), None, "ALG_NONE is cat, not a codec");
        assert_eq!(Alg::from_compress_id(4), None, "legacy brotli must not map");
        assert_eq!(Alg::from_compress_id(6), Some(Alg::Zstd));
        assert_eq!(Alg::from_name("lz4").map(|a| a.name()), Some("lz4"));
    }

    #[test]
    fn test_decompress_stream_reports_byte_count() {
        let data = b"hello otaku-decomp".repeat(64);
        let compressed = compress_bytes(Alg::Gzip, &data).unwrap();
        let mut out = Vec::new();
        let n = decompress_stream(Alg::Gzip, &compressed[..], &mut out).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(out.len(), data.len());
    }

    #[test]
    fn test_unzip_entry_roundtrip() {
        let dir = std::env::temp_dir();
        let zip_path = dir.join("t49_unzip_test.zip");
        let out_path = dir.join("t49_unzip_test.out");
        {
            let f = File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("otaku.bin", opts).unwrap();
            w.write_all(b"ddbu-fake-payload-bytes").unwrap();
            w.finish().unwrap();
        }
        let n = unzip_entry(
            zip_path.to_str().unwrap(),
            "otaku.bin",
            out_path.to_str().unwrap(),
        )
        .expect("unzip_entry");
        assert_eq!(n, b"ddbu-fake-payload-bytes".len() as u64);
        let content = std::fs::read(&out_path).unwrap();
        assert_eq!(content, b"ddbu-fake-payload-bytes");
        // Missing entry must be a clean error, not a panic.
        let err = unzip_entry(zip_path.to_str().unwrap(), "nope.bin", out_path.to_str().unwrap());
        assert!(err.is_err());
        let _ = std::fs::remove_file(&zip_path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn test_selftest_green() {
        selftest().expect("selftest must pass");
    }

    /// T51: flash_chunked must honor `base_off` (direct-read mode where the
    /// "bundle" is the ZIP file itself: junk bytes precede otaku.bin) and
    /// refuse v1 bundles + out-of-range partition indices.
    #[test]
    fn test_flash_chunked_base_offset_and_gates() {
        use crate::dd::chunked::{encode_chunk_table, patch_header_v2, ChunkEntry};

        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let half = data.len() / 2;
        let c0 = compress_bytes(Alg::Xz, &data[..half]).unwrap();
        let c1 = compress_bytes(Alg::Xz, &data[half..]).unwrap();

        // Build the bundle, then prepend junk to simulate ZIP direct-read.
        let bundle = std::env::temp_dir().join("t51_flash_bundle.bin");
        let shifted = std::env::temp_dir().join("t51_flash_shifted.bin");
        let out_path = std::env::temp_dir().join("t51_flash_out.bin");
        {
            use std::io::{Seek as _, SeekFrom};
            let mut f = File::create(&bundle).unwrap();
            let mut hdr = vec![0u8; 4096];
            hdr[..4].copy_from_slice(b"DDBU");
            hdr[4..6].copy_from_slice(&2u16.to_le_bytes());
            hdr[6..8].copy_from_slice(&3u16.to_le_bytes());
            hdr[8..10].copy_from_slice(&1u16.to_le_bytes());
            hdr[10..12].copy_from_slice(&4096u16.to_le_bytes());
            f.write_all(&hdr).unwrap();
            let mut entries = Vec::new();
            for (i, comp) in [&c0, &c1].iter().enumerate() {
                let off = f.stream_position().unwrap();
                f.write_all(comp).unwrap();
                let pos = f.stream_position().unwrap();
                let aligned = pos.div_ceil(4096) * 4096;
                if aligned > pos {
                    f.write_all(&vec![0u8; (aligned - pos) as usize]).unwrap();
                }
                entries.push(ChunkEntry {
                    comp_offset: off,
                    comp_len: comp.len() as u64,
                    decomp_len: if i == 0 { half as u64 } else { (data.len() - half) as u64 },
                });
            }
            let table_off = f.stream_position().unwrap();
            f.write_all(&encode_chunk_table(&entries)).unwrap();
            patch_header_v2(&mut hdr, table_off, &[2]);
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&hdr).unwrap();
        }
        // Junk prefix = 59 bytes (a realistic ZIP local-header shift).
        let junk = 59usize;
        {
            let orig = std::fs::read(&bundle).unwrap();
            let mut shifted_bytes = vec![0u8; junk];
            shifted_bytes.extend_from_slice(&orig);
            std::fs::write(&shifted, &shifted_bytes).unwrap();
        }

        // base_off path: decode from the shifted file.
        let n = flash_chunked(
            shifted.to_str().unwrap(),
            junk as u64,
            0,
            ChunkOut::File(out_path.to_str().unwrap().to_string()),
            None,
        )
        .expect("base_off decode");
        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&out_path).unwrap(), data);

        // v1 bundle must be refused (honest error, no half-parse).
        {
            let mut orig = std::fs::read(&bundle).unwrap();
            orig[4..6].copy_from_slice(&1u16.to_le_bytes());
            std::fs::write(&shifted, &orig).unwrap();
        }
        assert!(flash_chunked(
            shifted.to_str().unwrap(),
            0,
            0,
            ChunkOut::File(out_path.to_str().unwrap().to_string()),
            None
        )
        .is_err());

        // Out-of-range partition index must be refused.
        assert!(flash_chunked(
            bundle.to_str().unwrap(),
            0,
            5,
            ChunkOut::File(out_path.to_str().unwrap().to_string()),
            None
        )
        .is_err());

        let _ = std::fs::remove_file(&bundle);
        let _ = std::fs::remove_file(&shifted);
        let _ = std::fs::remove_file(&out_path);
    }
}
