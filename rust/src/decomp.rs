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

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

/// Version of the bundled helper (surfaced by `otaku-decomp --version`).
pub const DECOMP_VERSION: &str = "1.0.0";

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
        assert_eq!(n, 21);
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
}
