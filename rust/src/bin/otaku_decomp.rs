//! otaku-decomp — OTAku's bundled universal decompressor CLI (T49).
//!
//! Shipped inside the flashable ZIP next to `otaku.bin` and executed by
//! the update-binary script inside recovery. Replaces every recovery-
//! provided decompressor binary (gzip/bzip2/xz/lz4/zstd) and the
//! recovery-provided unzip for pulling `otaku.bin` out of the ZIP.
//!
//! Usage:
//!   otaku-decomp -a <alg>                     stdin -> stdout decompression
//!                                             (alg: gzip|bzip2|xz|lz4|zstd|none)
//!   otaku-decomp --unzip-entry <zip> <entry> <outfile>
//!                                             extract one ZIP entry (CRC-checked)
//!   otaku-decomp --flash-chunked <bundle> <part_index> [flags]   (T51)
//!                                             parallel decode of a DDBU v2
//!                                             chunked-xz partition; flags:
//!                                               --base N          bundle byte offset
//!                                               (ZIP direct-read mode; default 0)
//!                                               --comp-hash HEX   verify compressed
//!                                               data BEFORE writing (recommended)
//!                                               --pwrite DEV      write chunks directly
//!                                               to the block device at their offsets
//!                                               --out FILE        ordered stream to a
//!                                               file (selftest/debug; default stdout)
//!   otaku-decomp --selftest                   round-trip all codecs + zip extract
//!                                             + chunked flash path
//!   otaku-decomp --version                    print version
//!
//! Exit codes:
//!   0  success
//!   1  usage error
//!   2  decompression / I/O error
//!   3  ZIP error (entry not found, CRC mismatch)
//!   4  self-test failure
//!
//! The binary is a thin wrapper: all logic lives in `otaku_native::decomp`
//! so it is unit-tested with the library (`cargo test --lib`).

use std::io::{self, BufReader, Write};
use std::process::ExitCode;

use otaku_native::decomp::{self, Alg};

const USAGE: &str = "usage: otaku-decomp -a <gzip|bzip2|xz|lz4|zstd|none>\n\
                      \x20      otaku-decomp --unzip-entry <zip> <entry> <outfile>\n\
                      \x20      otaku-decomp --flash-chunked <bundle> <part> [--base N] \
[--comp-hash HEX] [--pwrite DEV | --out FILE]\n\
                      \x20      otaku-decomp --selftest | --version";

fn eprint_err(prefix: &str, err: &str) {
    let _ = writeln!(io::stderr(), "otaku-decomp: {}: {}", prefix, err);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("otaku-decomp {} (OTAku bundled decompressor)", decomp::DECOMP_VERSION);
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--selftest") {
        return match decomp::selftest() {
            Ok(()) => {
                println!("selftest OK: gzip bzip2 xz lz4 zstd + zip-entry + chunked-flash");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprint_err("selftest FAILED", &e);
                ExitCode::from(4)
            }
        };
    }

    if args.iter().any(|a| a == "--unzip-entry") {
        if args.len() != 4 {
            eprintln!("otaku-decomp: --unzip-entry needs exactly 3 arguments");
            eprintln!("{}", USAGE);
            return ExitCode::from(1);
        }
        // args = ["--unzip-entry", zip, entry, out]
        let zip = args[1].as_str();
        let entry = args[2].as_str();
        let out = args[3].as_str();
        return match decomp::unzip_entry(zip, entry, out) {
            Ok(n) => {
                eprintln!("otaku-decomp: extracted {} ({} bytes)", entry, n);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprint_err("zip error", &e);
                ExitCode::from(3)
            }
        };
    }

    if args.iter().any(|a| a == "--flash-chunked") {
        // args = ["--flash-chunked", <bundle>, <part_index>, (flags...)]
        if args.len() < 3 {
            eprintln!("otaku-decomp: --flash-chunked needs <bundle> <part_index> [flags]");
            eprintln!("{}", USAGE);
            return ExitCode::from(1);
        }
        let bundle = args[1].as_str();
        let part_idx: usize = match args[2].parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!(
                    "otaku-decomp: --flash-chunked partition index must be a number (got '{}')",
                    args[2]
                );
                eprintln!("{}", USAGE);
                return ExitCode::from(1);
            }
        };
        let mut base_off: u64 = 0;
        let mut comp_hash: Option<String> = None;
        let mut out = decomp::ChunkOut::Stdout;
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--base" => match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) => {
                        base_off = v;
                        i += 2;
                    }
                    None => {
                        eprintln!("otaku-decomp: --base needs a numeric value");
                        eprintln!("{}", USAGE);
                        return ExitCode::from(1);
                    }
                },
                "--comp-hash" => match args.get(i + 1) {
                    Some(v) => {
                        // T53-F12: v2 bundles ALWAYS bake a comp hash — an
                        // empty value means a torn or hand-edited flasher
                        // line. Refuse it: silently treating it as "no hash"
                        // would disable the pre-write corruption gate.
                        if v.is_empty() {
                            eprintln!(
                                "otaku-decomp: --comp-hash needs a hex value \
                                 (empty is invalid for v2 chunked bundles)"
                            );
                            eprintln!("{}", USAGE);
                            return ExitCode::from(1);
                        }
                        comp_hash = Some(v.clone());
                        i += 2;
                    }
                    None => {
                        eprintln!("otaku-decomp: --comp-hash needs a hex value");
                        eprintln!("{}", USAGE);
                        return ExitCode::from(1);
                    }
                },
                "--pwrite" => match args.get(i + 1) {
                    Some(v) => {
                        out = decomp::ChunkOut::Pwrite(v.clone());
                        i += 2;
                    }
                    None => {
                        eprintln!("otaku-decomp: --pwrite needs a block device path");
                        eprintln!("{}", USAGE);
                        return ExitCode::from(1);
                    }
                },
                "--out" => match args.get(i + 1) {
                    Some(v) => {
                        out = decomp::ChunkOut::File(v.clone());
                        i += 2;
                    }
                    None => {
                        eprintln!("otaku-decomp: --out needs a file path");
                        eprintln!("{}", USAGE);
                        return ExitCode::from(1);
                    }
                },
                other => {
                    eprintln!("otaku-decomp: unknown --flash-chunked flag '{}'", other);
                    eprintln!("{}", USAGE);
                    return ExitCode::from(1);
                }
            }
        }
        return match decomp::flash_chunked(bundle, base_off, part_idx, out, comp_hash.as_deref())
        {
            Ok(n) => {
                eprintln!("otaku-decomp: chunked decode wrote {} bytes", n);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprint_err("chunked flash error", &e);
                ExitCode::from(2)
            }
        };
    }

    // Decompression mode: -a <alg> (also accepts --alg <alg>).
    let alg_name = pick_alg(&args);
    match alg_name {
        None => {
            eprintln!("otaku-decomp: missing or unknown algorithm");
            eprintln!("{}", USAGE);
            ExitCode::from(1)
        }
        Some("none") => {
            // Passthrough (COMPRESS_ID 0 parity — the script uses plain cat,
            // but keep this for manual debugging).
            match copy_passthrough() {
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprint_err("copy error", &e);
                    ExitCode::from(2)
                }
            }
        }
        Some(name) => match Alg::from_name(name) {
            // T53-F11: pick_alg performs NO validation despite the old
            // expect message — an unrecognized -a value (typo, wrong case,
            // unsupported codec) used to panic here: exit code 101 and a
            // backtrace instead of the documented usage error.
            None => {
                eprintln!("otaku-decomp: unknown algorithm '{}'", name);
                eprintln!("{}", USAGE);
                ExitCode::from(1)
            }
            Some(alg) => match run_decompress(alg) {
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprint_err(&format!("{} error", name), &e);
                    ExitCode::from(2)
                }
            },
        },
    }
}

/// Extract the algorithm name from `-a <alg>` / `--alg <alg>` / `--alg=<alg>`.
fn pick_alg(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-a" || a == "--alg" {
            return args.get(i + 1).map(|s| s.as_str());
        }
        if let Some(rest) = a.strip_prefix("--alg=") {
            return Some(rest);
        }
        i += 1;
    }
    None
}

fn run_decompress(alg: Alg) -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    // Lock once; BufReader gives the decoder large sequential reads.
    let reader = stdin.lock();
    let writer = stdout.lock();
    decomp::decompress_stream(alg, reader, writer).map(|_| ()).map_err(|e| e.to_string())
}

fn copy_passthrough() -> Result<(), String> {
    let mut reader = BufReader::new(io::stdin().lock());
    let mut writer = io::stdout().lock();
    io::copy(&mut reader, &mut writer).map(|_| ()).map_err(|e| e.to_string())
}
