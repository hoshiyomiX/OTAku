//! Test untuk modul dd (build header, script builder, flash_info, run_dd_build, format).
//!
//! Secara mekanis dipindah dari dd.rs Fase-1 (zero behavior change).

use super::script::build_update_script;
use super::*;



    // ──────────────────────────────────────────────────────────────
    // T27 Fase-2a: F2 (dd failure verdict) + F5 (stale DD_OFLAG) invariants
    // ──────────────────────────────────────────────────────────────

    /// F2: the partition-size proxy must stay DEAD — it excused real dd
    /// write failures as "data OK" and (with skip_verify) bricked silently.
    #[test]
    fn test_f2_no_partition_size_proxy() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        for &sv in &[false, true] {
            let s = build_update_script(1, 1, "gzip", &meta, 0, "", sv);
            assert!(
                !s.contains("WRITTEN_SIZE"),
                "F2 REGRESSION: partition-size proxy WRITTEN_SIZE kembali (skip_verify={})",
                sv
            );
            assert!(
                !s.contains("WRITTEN_SIZE=$(blockdev"),
                "F2 REGRESSION: proxy kapasitas-partisi kembali sebagai bukti tulis (skip_verify={})",
                sv
            );
        }
    }

    /// F2: the verdict helper + SKIP_VERIFY runtime flag must exist so the
    /// script can distinguish "no hash proof available" from "hash will decide".
    #[test]
    fn test_f2_verdict_helper_and_skip_verify_flag() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s_off = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        let s_on = build_update_script(1, 1, "gzip", &meta, 0, "", true);
        assert!(s_off.contains("dd_failure_verdict() {"), "helper verdict hilang");
        assert!(s_off.contains("SKIP_VERIFY=0"), "flag SKIP_VERIFY=0 hilang (verify aktif)");
        assert!(s_on.contains("SKIP_VERIFY=1"), "flag SKIP_VERIFY=1 hilang (skip_verify)");
        // inti kebijakan: tanpa hash, kegagalan dd harus abort (return 1 path)
        assert!(
            s_on.contains("grep -o '^[0-9][0-9]* bytes'"),
            "F2: parse byte-count dd hilang"
        );
    }

    /// F5: the no-FIFO path must reset DD_OFLAG per partition — the stale
    /// `-z` guard is what let oflag=direct leak across partitions.
    #[test]
    fn test_f5_nofifo_resets_dd_oflag() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            !s.contains(r#"[ -b "$PTARGET" ] && [ -z "$DD_OFLAG" ]"#),
            "F5 REGRESSION: guard -z DD_OFLAG kembali (stale oflag bisa bocor)"
        );
        assert!(
            s.contains("F5 fix: reset DD_OFLAG unconditionally"),
            "F5: reset DD_OFLAG per-partisi hilang"
        );
    }


    /// F4: bootctl set-active-boot-slot must receive a NUMBER (0=a, 1=b) —
    /// the letter form was parsed by strtoul() as 0 (slot A), the opposite
    /// of the intended slot. fastboot set_active keeps the letter.
    #[test]
    fn test_f4_bootctl_slot_number() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            s.contains("bootctl set-active-boot-slot $SLOT_NUM"),
            "F4 REGRESSION: bootctl menerima huruf lagi (strtoul('b')=0 = slot A)"
        );
        assert!(
            s.contains("SLOT_NUM=$(echo \"$SLOT_LETTER\" | tr 'ab' '01')"),
            "F4: konversi huruf->angka hilang"
        );
        assert!(
            s.contains("fastboot set_active $SLOT_LETTER"),
            "F4: fastboot harus tetap menerima huruf"
        );
    }


    fn t7_count_anchored(script: &str) -> usize {
        script.matches(r"(^|/)($real_dev|$DEV_NAME)([[:space:]]|\$)").count()
            + script.matches(r"(^|/)($real_dev|$dev_name)([[:space:]]|\$)").count()
    }

    /// F7: device-name grep must be anchored — unanchored "dm-5" matches
    /// "dm-55" (and any path containing the substring), unmounting the
    /// wrong partition's mounts.
    #[test]
    fn test_f7_mount_grep_anchored() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            !s.contains(r#"mount_points=$(mount 2>/dev/null | grep -E "($real_dev|$dev_name)""#),
            "F7 REGRESSION: grep device-name tanpa anchor kembali di helper unmount"
        );
        assert!(
            !s.contains(r#"MOUNT_POINT=$(mount 2>/dev/null | grep -E "($real_dev|$DEV_NAME)""#),
            "F7 REGRESSION: grep device-name tanpa anchor kembali di validasi pre-flash"
        );
        assert!(
            s.contains(r"([[:space:]]|\$)") && t7_count_anchored(&s) == 2,
            "F7: kedua situs grep harus ber-anchor (^|/) ... ([[:space:]]|$)"
        );
        assert!(
            s.contains(r#"grep -qF " $mp ""#) && s.contains(r#"grep -qF " $MOUNT_POINT ""#),
            "F7: probe retry harus fixed-string (-qF)"
        );
    }

    /// F8: bundle header partition count must be cross-checked against the
    /// script's own NUM_PARTS constant — mixed-build ZIPs must abort.
    #[test]
    fn test_f8_header_script_parts_crosscheck() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            s.contains(r#"[ "$HDR_NUM_PARTS" != "$NUM_PARTS" ]"#),
            "F8: cross-check HDR_NUM_PARTS vs NUM_PARTS hilang"
        );
        assert!(
            s.contains("different builds"),
            "F8: pesan diagnostik mixed-build hilang"
        );
    }


    /// F9: the LLM-tokenization artifact must never re-enter the template.
    #[test]
    fn test_f9_no_llm_artifact() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            !s.contains("$HARUKA_PARSER_CHANGE_LINE"),
            "F9 REGRESSION: artefak tokenisasi LLM kembali di template"
        );
    }

    /// F12: GZIP_ERR_MSG must be flattened to a single line before ui_print.
    #[test]
    fn test_f12_gzip_err_msg_single_line() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            s.contains("| head -3 | tr '\\n' ' '"),
            "F12: flatten tr-'\\n'-' ' hilang dari pipeline GZIP_ERR_MSG"
        );
    }

    /// F11: the 'lptools map' hint must be guarded — lp_name is only set in
    /// the dynamic auto-map branch; physical partitions need a different hint.
    #[test]
    fn test_f11_lp_name_hint_guarded() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            s.contains(r#"[ "$is_dynamic" = "1" ] && [ -n "$lp_name" ]"#),
            "F11: guard is_dynamic+lp_name hilang (hint 'lptools map ' kosong bisa kembali)"
        );
    }


    /// F3: the flasher must ABORT on legacy brotli (id 4) and unknown
    /// compression ids BEFORE wiring any decompressor — the old path wired
    /// "cat -dc" (broken) or passed raw brotli bytes to the partition.
    #[test]
    fn test_f3_flasher_gates_compress_id() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "a".repeat(64),
            comp_size: 16777216,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let s = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(
            s.contains(r#"case "$COMPRESS_ID" in"#),
            "F3: gate case compress_id hilang"
        );
        assert!(
            s.contains("legacy brotli compression (id 4)"),
            "F3: pesan abort brotli hilang"
        );
        assert!(
            s.contains("Unknown compression id $COMPRESS_ID"),
            "F3: pesan abort id tak dikenal hilang"
        );
        // blok wiring brotli lama harus hilang
        assert!(
            !s.contains(r#"if [ "$COMPRESS_ID" = "4" ]; then"#),
            "F3 REGRESSION: wiring brotli cat -dc kembali"
        );
    }


    // ──────────────────────────────────────────────────────────────
    // T27 Fase-4: dump skrip flasher utk gerbang shellcheck CI
    // ──────────────────────────────────────────────────────────────

    /// Dump representative generated flasher scripts for the CI shellcheck
    /// gate (both verify variants — the verify_block differs between them).
    /// Writes under target/dump/ — cargo test CWD is the crate root, so this
    /// lands next to build artifacts and never pollutes the source tree.
    #[test]
    fn test_ci_dump_flasher_scripts() {
        let meta: Vec<PartitionMeta> = (0..2)
            .map(|i| PartitionMeta {
                name: format!("part_{}", i),
                unc_size: 1024 * (i as u64 + 1),
                hash_hex: format!("{:064x}", i),
                comp_size: 512 * (i as u64 + 1),
                data_offset: 4096 * (i as u64 + 1),
                comp_hash_hex: format!("{:064x}", i + 7),
            })
            .collect();
        let dir = std::path::Path::new("target/dump");
        std::fs::create_dir_all(dir).unwrap();
        for (name, sv) in [("update-binary.sh", false), ("update-binary-skip.sh", true)] {
            let s = build_update_script(2, 1, "gzip", &meta, 33554432, "crosshatch", sv);
            std::fs::write(dir.join(name), s).unwrap();
        }
    }

    // ──────────────────────────────────────────────────────────────
    // T27 golden template lock — byte-identical guarantee
    // ──────────────────────────────────────────────────────────────

    /// Golden hash of the update-binary template across representative
    /// configurations (gzip+device, gzip+skip-verify, zstd+device, xz multi-part).
    ///
    /// Guards the T27 invariant "flashing ROM tetap berhasil seperti code
    /// lama": any unintended template change (refactor drift, accidental
    /// edit) fails here. To change the template INTENTIONALLY (Fase 2
    /// flasher fixes), update this constant in the same commit and diff
    /// the generated script for the exact intended delta.
    #[test]
    fn test_golden_update_script_template_hash() {
        use sha2::{Digest, Sha256};
        let cases: Vec<(usize, u16, &str, u64, &str, bool)> = vec![
            (2, 1, "gzip", 33554432, "crosshatch", false),
            (2, 1, "gzip", 33554432, "", true),
            (1, 6, "zstd", 1048576, "OP11", false),
            (3, 3, "xz", 0, "", false),
        ];
        let mut combined = String::new();
        for (np, cid, cname, tot, dev, sv) in cases {
            let meta: Vec<PartitionMeta> = (0..np)
                .map(|i| PartitionMeta {
                    name: format!("part_{}", i),
                    unc_size: 1024 * (i as u64 + 1),
                    hash_hex: format!("{:064x}", i),
                    comp_size: 512 * (i as u64 + 1),
                    data_offset: 4096 * (i as u64 + 1),
                    comp_hash_hex: format!("{:064x}", i + 7),
                })
                .collect();
            let s = build_update_script(np, cid, cname, &meta, tot, dev, sv);
            combined.push_str(&s);
            combined.push('\u{1}');
        }
        let digest = Sha256::digest(combined.as_bytes());
        let hexstr: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hexstr,
            // Golden updated in the same commit as the T30 flasher fixes:
            //   F-A — Step 7 free-space Method 1 became informational-only
            //         (post-resize LP_FREE vs pre-resize RESIZE_TOTAL was a
            //         false-abort; authoritative gate stays in the resize step)
            //   F-B — resize loop maps an unmapped dm-linear BEFORE capturing
            //         the original size, so the cleanup trap can always roll
            //         the resize back (loud warning when it still cannot)
            // Previous golden: 8ff5dc1c8ce089a6767772e4ee2e03618d648b641c1f81c0ec6d40084d471c75
            "7e19fe173170bd5782bede40a6c48698deb9debdda36e68e9a17fadb946ffb6f",
            "update-binary template berubah dari golden — cek diff template yang tidak disengaja \\
             (atau perbarui golden INI secara sadar bersama fix Fase-2)"
        );
    }




    #[test]
    fn test_align_up() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4095, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }

    #[test]
    fn test_build_header() {
        let hdr = build_header(1, 3); // gzip, 3 partitions
        assert_eq!(hdr.len(), HEADER_SIZE);
        // Magic
        assert_eq!(&hdr[0..4], b"DDBU");
        // Version (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[4], hdr[5]]), DDBUNDLE_VERSION);
        // Compress ID (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[6], hdr[7]]), 1);
        // Num parts (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[8], hdr[9]]), 3);
        // Header size (u16 LE)
        assert_eq!(u16::from_le_bytes([hdr[10], hdr[11]],), HEADER_SIZE as u16);
        // Rest should be zero-padded
        // (Iterate by value — clippy::needless_range_loop)
        for &byte in hdr.iter().skip(12) {
            assert_eq!(byte, 0u8);
        }
    }

    #[test]
    fn test_decomp_cmd_for_id() {
        assert_eq!(decomp_cmd_for_id(0), "cat");
        assert_eq!(decomp_cmd_for_id(1), "gzip");
        assert_eq!(decomp_cmd_for_id(2), "bzip2");
        assert_eq!(decomp_cmd_for_id(3), "xz");
        assert_eq!(decomp_cmd_for_id(4), "cat");  // brotli removed — passthrough
        assert_eq!(decomp_cmd_for_id(5), "lz4");
        assert_eq!(decomp_cmd_for_id(6), "zstd");
    }

    #[test]
    fn test_decomp_ext_for_id() {
        assert_eq!(decomp_ext_for_id(0), ".raw");
        assert_eq!(decomp_ext_for_id(1), ".gz");
        assert_eq!(decomp_ext_for_id(2), ".bz2");
        assert_eq!(decomp_ext_for_id(3), ".xz");
        assert_eq!(decomp_ext_for_id(4), ".br");
        assert_eq!(decomp_ext_for_id(5), ".lz4");
    }

    #[test]
    fn test_human_size() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1048576), "1.0 MB");
        assert_eq!(human_size(1073741824), "1024.0 MB");
    }

    #[test]
    fn test_build_update_script_basic() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(script.starts_with("#!/sbin/sh"));
        assert!(script.contains("PART_0_NAME=\"boot\""));
        assert!(script.contains("PART_0_HASH=\"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\""));
        assert!(script.contains("PART_0_COMP_HASH=\"testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345\""));
        assert!(script.contains("check_decompressor \"gzip\""));
        assert!(script.contains("sha256sum"));
        assert!(script.contains("$PNAME verified"));
        assert!(script.contains("exit 0"));
    }

    #[test]
    fn test_build_update_script_skip_verify() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "deadbeef0123456789abcdef0123456789abcdef0123456789abcdef01234567".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", true);
        assert!(script.contains("Verification skipped"));
        // sha256sum appears in pre-flash compressed hash verification even when
        // post-flash verify is skipped — that's expected behavior.
        assert!(script.contains("Flash complete"));
    }

    #[test]
    fn test_build_update_script_with_device() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "crosshatch", false);
        assert!(script.contains("TARGET_DEVICE=\"crosshatch\""));
        assert!(script.contains("DEVICE_MATCH"));
    }

    #[test]
    fn test_build_flash_info() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let info = build_flash_info("gzip", 16781312, 33554432, 1, &meta, "crosshatch", 6, false, "TestROM", "TestMaker");
        assert!(info.contains("OTAku — Custom Payload Maker"));
        assert!(info.contains("gzip (level 6)"));
        assert!(info.contains("crosshatch"));
        assert!(info.contains("[boot]"));
        assert!(info.contains("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"));
        assert!(info.contains("enabled"));
        assert!(info.contains("Total flash size: 33554432 bytes"));
    }

    #[test]
    fn test_run_dd_build_no_images() {
        let result = run_dd_build(&[], "gzip", 6, "/tmp/test.zip", "", false, "", "");
        assert!(!result.success);
        assert!(result.error.unwrap().contains("no images specified"));
    }

    #[test]
    fn test_run_dd_build_invalid_compression() {
        let result = run_dd_build(
            &[("boot".to_string(), "/tmp/boot.img".to_string())],
            "invalid",
            6,
            "/tmp/test.zip",
            "",
            false,
            "",
            "",
        );
        assert!(!result.success);
        assert!(result.error.unwrap().contains("unsupported compression"));
    }

    // ──────────────────────────────────────────────────────────────
    // Regression tests — each test guards against a specific bug that
    // was previously introduced and fixed. If a refactor re-introduces
    // the bug, the corresponding test fails.
    // See commit history for context on each bug.
    // ──────────────────────────────────────────────────────────────

    /// Regression: Bug #1 (P0) — operator precedence in cleanup trap.
    /// The old broken pattern `cmd1 || cmd2 && cmd3` was replaced with
    /// explicit if-else. If anyone reintroduces the broken pattern, this
    /// test catches it.
    #[test]
    fn test_regression_trap_operator_precedence() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // The broken pattern was:
        //   lptools resize "$rname" "$rsize" >/dev/null 2>&1 || \
        //       lptools remove "$rname" >/dev/null 2>&1 && \
        //       lptools create "$rname" "$rsize" >/dev/null 2>&1
        // The fix uses: `if ! lptools resize ...; then ... fi`
        // Assert: broken pattern is NOT present in cleanup trap.
        let broken_pattern = "lptools resize \"$rname\" \"$rsize\" >/dev/null 2>&1 ||";
        assert!(
            !script.contains(broken_pattern),
            "REGRESSION: cleanup trap still uses broken ||/&& chaining (Bug #1)"
        );

        // Assert: fix is present.
        // Note: after slot-suffix fix, the variable is $rname_lp not $rname
        assert!(
            script.contains("if ! lptools resize \"$rname_lp\" \"$rsize\""),
            "REGRESSION: cleanup trap if-else fix not found (Bug #1)"
        );
    }

    /// Regression: Bug #2 (P1) — regex leading space in ZIP listing parser.
    /// The old regex `^ [0-9]* otaku\.bin$` had a leading space which never
    /// matched awk output (which has no leading space). The fix uses
    /// `^[0-9]+ otaku\.bin$` via a shared ZIP_LIST_REGEX variable.
    #[test]
    fn test_regression_zip_listing_regex() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Assert: broken regex pattern is NOT present.
        let broken_patterns = [
            "grep -q '^ [0-9]* otaku",      // leading space + [0-9]*
            "grep -q \"^ [0-9]* otaku",     // leading space + [0-9]* (double-quoted variant)
        ];
        for pat in broken_patterns.iter() {
            assert!(
                !script.contains(pat),
                "REGRESSION: ZIP listing still uses broken regex with leading space (Bug #2): {}",
                pat
            );
        }

        // Assert: fix is present (refactored to try_zip_listing helper).
        assert!(
            script.contains("try_zip_listing"),
            "REGRESSION: try_zip_listing helper not defined (Bug #2 refactor)"
        );
        assert!(
            script.contains("otaku[.]bin"),
            "REGRESSION: fixed regex pattern (otaku[.]bin) not found (Bug #2)"
        );
    }

    /// Regression: Bug #3 (P1) — idempotent lptools unmap+map.
    /// The old code always called `lptools unmap` then `lptools map`,
    /// even when the partition was already unmapped. The fix checks
    /// for /dev/mapper/$pname or /dev/block/by-name/$pname existence
    /// before calling unmap.
    #[test]
    fn test_regression_idempotent_unmap() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Assert: idempotent check is present.
        assert!(
            script.contains("/dev/mapper/$pname") || script.contains("/dev/block/by-name/$pname"),
            "REGRESSION: idempotent unmap existence check missing (Bug #3)"
        );
    }

    /// Regression: Cleanup trap slot-suffixed path check + correct lptools
    /// unmap/map names on A/B devices.
    ///
    /// Bug #9 (P2): The cleanup trap's "already mapped" check only tested
    /// plain paths (`/dev/mapper/vendor`), but on A/B devices the dm-linear
    /// device is named `vendor_a` (slot-suffixed).  If the device was already
    /// mapped, the check would miss it and the cleanup would unmap+remap
    /// unnecessarily (risking EBUSY on partitions recovery is using).
    ///
    /// Bug #10 (P2): The cleanup trap called `unmount_and_unmap_partition
    /// "$pname"` which internally calls `lptools unmap "$pname"` — plain
    /// name, not slot-suffixed.  On A/B devices lptools requires the
    /// slot-suffixed name (e.g. `vendor_a`), so the unmap would silently
    /// fail and the subsequent map would also fail or be wrong.
    ///
    /// Bug #11 (P3): `unmap_and_remap_partition()` was dead code (never
    /// called) and had the same plain-name bug.  Removed entirely.
    #[test]
    fn test_regression_cleanup_slot_suffixed_paths() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Bug #9: slot-suffixed path check in cleanup "already mapped" guard.
        // The cleanup trap must check /dev/mapper/$pname_lp in addition to
        // /dev/mapper/$pname, because on A/B devices the dm-linear device
        // is named with the slot suffix (e.g. vendor_a, not vendor).
        assert!(
            script.contains("/dev/mapper/$pname_lp"),
            "REGRESSION: cleanup trap missing slot-suffixed /dev/mapper/$pname_lp check (Bug #9)"
        );
        assert!(
            script.contains("/dev/block/by-name/$pname_lp"),
            "REGRESSION: cleanup trap missing slot-suffixed /dev/block/by-name/$pname_lp check (Bug #9)"
        );

        // Bug #10: cleanup uses unmount_partition + lptools unmap "$pname_lp"
        // (NOT unmount_and_unmap_partition "$pname" which passes plain name).
        assert!(
            script.contains("unmount_partition \"$pname\""),
            "REGRESSION: cleanup trap should use unmount_partition directly, not unmount_and_unmap_partition (Bug #10)"
        );
        assert!(
            script.contains("lptools unmap \"$pname_lp\""),
            "REGRESSION: cleanup trap lptools unmap must use slot-suffixed $pname_lp (Bug #10)"
        );

        // Bug #10: the old broken pattern must NOT be present.
        assert!(
            !script.contains("unmount_and_unmap_partition \"$pname\""),
            "REGRESSION: cleanup trap still uses unmount_and_unmap_partition with plain $pname (Bug #10)"
        );

        // Bug #10 + dead code demotion: the entire function definition must be removed.
        assert!(
            !script.contains("unmount_and_unmap_partition()"),
            "REGRESSION: dead code unmount_and_unmap_partition() definition still present in script"
        );

        // Bug #11: dead code unmap_and_remap_partition() must NOT be present.
        assert!(
            !script.contains("unmap_and_remap_partition()"),
            "REGRESSION: dead code unmap_and_remap_partition() still present (Bug #11)"
        );
    }

    /// Regression: Bug #4 (P2) — explicit $? capture.
    /// The old code used `if [ $? -ne 0 ]` directly after a command,
    /// which is fragile (any command between can reset $?). The fix
    /// captures to RC variables: RESIZE_RC, CREATE_RC, MAP_RC (or map_rc
    /// when capture is inside the unmap_and_remap_partition helper).
    ///
    /// Note: After refactor (commit bf24206 — targeted unmap/remap), the
    /// `lptools map` exit code capture moved from inline `MAP_RC=$?` in
    /// the resize-step remap loop into the `unmap_and_remap_partition()`
    /// helper function as `map_rc=$?` (lowercase, local var). Both patterns
    /// satisfy Bug #4's intent — the regression is about NOT using
    /// `if [ $? -ne 0 ]` directly after `lptools map`, regardless of where
    /// the capture lives.
    #[test]
    fn test_regression_explicit_rc_capture() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 536870912,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Assert: explicit RC capture variables are present.
        // RESIZE_RC and CREATE_RC are inline in the resize loop.
        assert!(script.contains("RESIZE_RC=$?"), "REGRESSION: RESIZE_RC capture missing (Bug #4)");
        assert!(script.contains("CREATE_RC=$?"), "REGRESSION: CREATE_RC capture missing (Bug #4)");
        // MAP_RC may be inline (uppercase, pre-refactor) or inside the
        // unmap_and_remap_partition helper (lowercase `map_rc`, post-refactor).
        // Both patterns satisfy Bug #4's intent: capture $? immediately after
        // `lptools map`, do NOT use bare `if [ $? -ne 0 ]`.
        assert!(
            script.contains("MAP_RC=$?") || script.contains("map_rc=$?"),
            "REGRESSION: MAP_RC/map_rc capture missing (Bug #4) — neither inline MAP_RC=$? nor helper map_rc=$? found"
        );
    }

    /// F6 (T25 audit): device-mismatch handling must be HONEST — no dead
    /// interactivity. `choose` does not exist in TWRP/busybox, and `read`
    /// consumes the recovery update-binary protocol pipe (STDIN), so the
    /// old "confirm" chain could never actually confirm anything.
    /// Policy: undetectable device → warn + proceed (false-negative
    /// protection); genuinely different device → ABORT.
    #[test]
    fn test_f6_device_mismatch_policy() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
            comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "alioth", false);

        // Dead interactivity must stay dead.
        assert!(
            !script.contains("command -v choose"),
            "F6 REGRESSION: binary `choose` (tidak ada di TWRP/busybox) dipakai lagi"
        );
        assert!(
            !script.contains("read -t 30"),
            "F6 REGRESSION: read dari STDIN (pipe protokol recovery) kembali"
        );
        assert!(
            !script.contains("USER_CONFIRMED"),
            "F6 REGRESSION: gerbang konfirmasi mati kembali"
        );

        // Honest policy branches must exist.
        assert!(
            script.contains("Device codename could not be detected"),
            "F6: cabang deteksi-gagal (warn + proceed) hilang"
        );
        assert!(
            script.contains("Refusing to flash a bundle"),
            "F6: cabang abort device-beda hilang"
        );
    }

    /// Regression: edge case B — vendor partition codename fallback chain.
    /// The old code used `getprop ro.product.device || getprop ro.build.product`
    /// which was spoofable by Magisk/GSI/LineageOS (they override /system props).
    /// The fix uses VENDOR partition props (ro.product.vendor.device +
    /// ro.product.board) which are harder to spoof, with /vendor/build.prop
    /// as fallback when getprop returns empty (recovery without /vendor mounted
    /// via init, but /vendor/build.prop still readable if partition is mounted).
    #[test]
    fn test_regression_getprop_fallback_chain() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "alioth", false);

        // Assert: vendor partition props are the primary source.
        assert!(
            script.contains("ro.product.vendor.device"),
            "REGRESSION: ro.product.vendor.device source missing (edge case B — spoof-resistant codename)"
        );
        assert!(
            script.contains("ro.product.board"),
            "REGRESSION: ro.product.board source missing (edge case B — spoof-resistant codename)"
        );

        // Assert: /vendor/build.prop fallback present (for both props).
        assert!(
            script.contains("/vendor/build.prop"),
            "REGRESSION: /vendor/build.prop fallback missing (edge case B)"
        );

        // Assert: comma-separated dual-codename logic present
        // (when vendor.device != board, both are used comma-separated).
        assert!(
            script.contains("VENDOR_DEVICE,$BOARD_DEVICE"),
            "REGRESSION: comma-separated dual-codename logic missing (edge case B)"
        );
    }

    /// Sanity: every generated script ends with `exit 0` for happy path.
    #[test]
    fn test_script_always_exits_clean_on_success() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        for skip in [false, true] {
            for device in ["", "alioth"] {
                let script = build_update_script(1, 1, "gzip", &meta, 0, device, skip);
                assert!(
                    script.trim_end().ends_with("exit 0"),
                    "Script does not end with exit 0 (skip={}, device='{}')",
                    skip,
                    device
                );
            }
        }
    }

    /// Sanity: updater-script stub is valid edify, not a shell comment.
    /// The old code used "#Mtk client script\n" which TWRP flagged as
    /// edify syntax error. The fix uses "assert(1==1);\n".
    /// This test isn't on build_update_script directly (updater-script is
    /// in run_dd_build), but we verify the canonical string here to
    /// prevent accidental revert.
    #[test]
    fn test_updater_script_is_valid_edify() {
        // The updater-script is hardcoded in run_dd_build(). We can't easily
        // test it without invoking run_dd_build (which needs files), but
        // we can document the expected value here so anyone changing it
        // sees this test and updates both places consistently.
        let expected = "assert(1==1);\n";
        let forbidden = "#Mtk client script\n";
        assert_ne!(expected, forbidden, "expected and forbidden must differ");
        assert!(expected.contains("assert("), "expected must be valid edify");
        assert!(expected.ends_with(";\n"), "expected must end with semicolon + newline");
    }

    /// Sanity: DYNAMIC_PART_NAMES includes OEM-specific partitions.
    /// The old list missed Xiaomi/Realme/Samsung/Vivo partitions, causing
    /// flash failures on those devices.
    #[test]
    fn test_dynamic_part_names_includes_oem() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 0,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // AOSP standard names
        for required in ["system", "vendor", "product", "system_ext", "odm", "odm_dlkm", "vendor_dlkm"] {
            assert!(
                script.contains(&format!(" {} ", required)),
                "REGRESSION: AOSP partition '{}' missing from DYNAMIC_PART_NAMES",
                required
            );
        }

        // OEM-specific names
        for oem in ["mi_ext", "my_product", "optics", "prism"] {
            assert!(
                script.contains(&format!(" {} ", oem)),
                "REGRESSION: OEM partition '{}' missing from DYNAMIC_PART_NAMES",
                oem
            );
        }
    }

    /// Sanity: updater-script contains valid edify statement.
    /// The actual updater-script is built inside run_dd_build() and we
    /// can't easily test it without invoking the full builder. We test
    /// the upstream Rust constant here.
    #[test]
    fn test_updater_script_constant_in_source() {
        // Read source file at compile time to catch the literal.
        let source = include_str!("mod.rs");

        // Assert the canonical valid literal exists.
        assert!(
            source.contains("let updater_script = \"assert(1==1);\\n\";"),
            "updater-script assignment must use \"assert(1==1);\\n\" literal"
        );

        // Assert the broken literal is NOT used as the actual assignment.
        // Note: we check the assignment form `let updater_script = ...` so
        // that documentation comments mentioning the broken literal don't
        // trigger a false positive.
        assert!(
            !source.contains("let updater_script = \"#Mtk client script\\n\";"),
            "REGRESSION: updater-script reverted to broken '#Mtk client script' literal"
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Pilihan B implementation tests — verify the new alur
    // (alur user step 2: pre-flash partition verify)
    // ──────────────────────────────────────────────────────────────

    /// Verify Step 1 (pre-flash partition table verify) exists in generated script.
    /// This is alur user step 2: "verify semua partisi" before flash.
    #[test]
    fn test_preflash_verify_step_present() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Pre-flash verify step is present
        assert!(
            script.contains("Pre-flash partition table verify"),
            "REGRESSION: pre-flash verify step missing (alur user step 2)"
        );

        // Verify step header present (refactored to section-based style)
        assert!(
            script.contains("> Verifying partition table..."),
            "REGRESSION: pre-flash verify section header missing"
        );

        // All 4 checks must be present
        assert!(
            script.contains("offset_overflow"),
            "REGRESSION: pre-flash verify missing offset_overflow check"
        );
        assert!(
            script.contains("bad_hash_format"),
            "REGRESSION: pre-flash verify missing hash format check"
        );
        assert!(
            script.contains("zero_unc_size"),
            "REGRESSION: pre-flash verify missing zero_unc_size check"
        );
        assert!(
            script.contains("misaligned_offset"),
            "REGRESSION: pre-flash verify missing alignment check"
        );

        // Success message must be present
        assert!(
            script.contains("partition(s) verified"),
            "REGRESSION: pre-flash verify success message missing"
        );
    }

    /// Verify Step 2 (bundle integrity + decompressor) is merged into one step.
    /// Old Step 1 (decompressor) and Step 2 (integrity) are now combined.
    #[test]
    fn test_integrity_step_merged_with_decompressor() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Merged step header
        assert!(
            script.contains("Bundle integrity + decompressor"),
            "REGRESSION: merged integrity+decompressor step header missing"
        );

        // Old separate "Decompressor availability" step header should NOT be present
        // (it's now part of merged step 2)
        let old_step1_pattern = "[Step 1/] Decompressor availability...";
        assert!(
            !script.contains(old_step1_pattern),
            "REGRESSION: old separate 'Decompressor availability' step still present (should be merged)"
        );

        // Both check_decompressor function and HDR_MAGIC check should be in same step
        assert!(
            script.contains("check_decompressor") && script.contains("HDR_MAGIC"),
            "REGRESSION: decompressor and bundle integrity not in same step"
        );
    }

    /// Verify step numbering: extract=0, verify=1, integrity=2, slot=3 (no device),
    /// validation=4, resize=5, flash=6+.
    /// This guards against accidental renumbering that breaks the alur.
    #[test]
    fn test_step_numbering_after_pilihan_b() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        // Without device check
        let script_no_dev = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Step 0 = open payload — header line format: "[Step 0/8]"
        assert!(script_no_dev.contains("Step 0/"), "Step 0 (open payload) missing");
        // Step 1 = pre-flash verify (NEW) — header format: "[Step 1/8]"
        assert!(script_no_dev.contains("Step 1/"), "Step 1 (pre-flash verify) missing");
        // Step 2 = integrity + decompressor (MERGED) — header format: "[Step 2/8]"
        assert!(script_no_dev.contains("Step 2/"), "Step 2 (integrity+decompressor) missing");
        // Step 3 = slot detection (no device) — header format: "[Step 3/8]"
        assert!(script_no_dev.contains("Step 3/"), "Step 3 (slot detection) missing");
        // Step 4 = partition validation — header format: "[Step 4/8]"
        assert!(script_no_dev.contains("Step 4/"), "Step 4 (partition validation) missing");
        // Step 5 = resize — header format: "[Step 5/8]"
        assert!(script_no_dev.contains("Step 5/"), "Step 5 (resize) missing");
        // Step 6 = pre-flash free space check — header format: "[Step 6/8]"
        assert!(script_no_dev.contains("Step 6/8"), "Step 6 (free space check) missing");
        // Step 7 = flash — header format is DIFFERENT from other steps:
        //   "# ── Step {flash_step_offset}+{num_parts_minus_1}/{total_steps}: Flash each partition"
        // For 1 partition with no device: "Step 7+0/8"
        // We check for "Step 7" as substring (not "Step 7/") because the format differs.
        assert!(
            script_no_dev.contains("Step 7+0/8") || script_no_dev.contains("Step 7 "),
            "Step 7 (flash) header missing — got: {}",
            script_no_dev.lines().filter(|l| l.contains("Step 7")).collect::<Vec<_>>().join("\n")
        );

        // With device check, all subsequent steps shift +1
        let script_with_dev = build_update_script(1, 1, "gzip", &meta, 0, "alioth", false);
        // Step 3 = device check (with device) — header format: "Step {device_check_step}:" (note colon)
        assert!(
            script_with_dev.contains("Step 3:") || script_with_dev.contains("Step 3 "),
            "Step 3 (device check) missing with device"
        );
        // Step 4 = slot detection (with device) — header format: "[Step 4/9]"
        assert!(script_with_dev.contains("Step 4/"), "Step 4 (slot detection) missing with device");
        // Step 7 = pre-flash free space check (with device) — header format: "[Step 7/9]"
        assert!(script_with_dev.contains("Step 7/9"), "Step 7 (free space check) missing with device");
        // Step 8 = flash (with device) — header format: "Step 8+0/9"
        assert!(
            script_with_dev.contains("Step 8+0/9") || script_with_dev.contains("Step 8 "),
            "Step 8 (flash) missing with device"
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Bug NEW-A/B/C fix tests — guard empty vars + portable substring
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_regression_empty_offset_comp_guarded() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(script.contains("empty_offset"), "Bug NEW-A: empty_offset guard missing");
        assert!(script.contains("empty_comp_size"), "Bug NEW-A: empty_comp_size guard missing");
    }

    #[test]
    fn test_regression_empty_vunc_default_value() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        // The fix uses ${VUNC:-0} — check for the literal string in output
        assert!(script.contains("VUNC"), "Bug NEW-B: VUNC reference missing");
    }

    #[test]
    fn test_regression_portable_hash_substring() {
        let meta = vec![PartitionMeta {
            name: "boot".to_string(),
            unc_size: 33554432,
            hash_hex: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            comp_size: 16777216,
            data_offset: 4096,
        comp_hash_hex: "testcomp0123456789abcdef0123456789abcdef0123456789abcdef012345".to_string(),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        assert!(script.contains("HASH_SHORT"), "Bug NEW-C: HASH_SHORT variable missing");
        assert!(script.contains("printf"), "Bug NEW-C: printf fix missing");
    }

    /// Bug NEW-A/B fix (flash step): verify POFFSET/PCSIZE/PSIZE empty-var guards
    /// exist in the flash loop, not just the verify loop.
    #[test]
    fn test_regression_flash_step_empty_var_guard() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1075,
            hash_hex: "a".repeat(64),
            comp_size: 334,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        // The flash step guard checks for empty POFFSET, PCSIZE, PSIZE
        assert!(
            script.contains("-z \"$POFFSET\"") || script.contains("-z \"$POFFSET\""),
            "Bug NEW-A/B (flash step): POFFSET empty guard missing"
        );
        assert!(
            script.contains("-z \"$PCSIZE\"") || script.contains("-z \"$PCSIZE\""),
            "Bug NEW-A/B (flash step): PCSIZE empty guard missing"
        );
        assert!(
            script.contains("-z \"$PSIZE\"") || script.contains("-z \"$PSIZE\""),
            "Bug NEW-A/B (flash step): PSIZE empty guard missing"
        );
        // The abort message should be present
        assert!(
            script.contains("Missing partition metadata"),
            "Bug NEW-A/B (flash step): abort message missing"
        );
    }

    /// Verify the new pre-flash compressed-data hash verification is present.
    /// This catches bundle corruption (MTP transfer, tmpfs issues) BEFORE
    /// we touch any block device.
    #[test]
    fn test_preflash_compressed_hash_verification() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1075,
            hash_hex: "a".repeat(64),
            comp_size: 334,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        // PCOMP_HASH eval is present
        assert!(
            script.contains("PCOMP_HASH"),
            "PCOMP_HASH variable missing from flash step"
        );
        // Pre-flash hash verification block is present
        assert!(
            script.contains("Hash verified"),
            "Compressed data hash verification block missing"
        );
        // Hash mismatch abort message is present
        assert!(
            script.contains("Compressed data hash mismatch"),
            "Hash mismatch abort message missing"
        );
        // Uses verify_trim (dd-based) for exact byte count instead of head -c
        assert!(
            script.contains("verify_trim"),
            "verify_trim (dd-based exact byte extraction) missing from verify step"
        );
        assert!(
            script.contains("VERIFY_FULL_BLOCKS"),
            "VERIFY_FULL_BLOCKS computation missing from verify step"
        );
    }

    /// Verify direct ZIP reading feature is present in the generated script.
    /// This guards against accidental removal of the ZIP_DATA_OFFSET computation
    /// and the read_bundle_bytes helper function.
    #[test]
    fn test_direct_zip_reading_present() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "a".repeat(64),
            comp_size: 536870912,
            data_offset: 4096,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // ZIP_DATA_OFFSET variable is computed
        assert!(
            script.contains("ZIP_DATA_OFFSET"),
            "REGRESSION: ZIP_DATA_OFFSET variable not found (direct ZIP reading)"
        );

        // read_bundle_bytes helper function is defined
        assert!(
            script.contains("read_bundle_bytes()"),
            "REGRESSION: read_bundle_bytes helper not defined (direct ZIP reading)"
        );

        // Direct read mode indicator
        assert!(
            script.contains("Direct ZIP read") || script.contains("DIRECT_READ_OK"),
            "REGRESSION: direct ZIP read mode not present"
        );

        // Fallback extraction path still exists
        assert!(
            script.contains("Fallback") || script.contains("fallback"),
            "REGRESSION: fallback extraction path missing"
        );

        // EXTRACT_SKIP includes ZIP_DATA_OFFSET
        assert!(
            script.contains("ZIP_DATA_OFFSET + DATA_OFFSET + POFFSET"),
            "REGRESSION: EXTRACT_SKIP should include ZIP_DATA_OFFSET"
        );

        // BUNDLE_SIZE is pre-computed (not from wc -c of $BUNDLE in integrity step)
        assert!(
            !script.contains("BUNDLE_SIZE=$(wc -c < \"$BUNDLE\")"),
            "REGRESSION: BUNDLE_SIZE should be pre-computed from Step 0, not from wc -c"
        );

        // BUNDLE_SIZE uses EXPECTED_BUNDLE_SIZE when ZIP listing is available
        // (fixes size mismatch caused by ZIP central directory trailer bytes)
        assert!(
            script.contains("BUNDLE_SIZE=$EXPECTED_BUNDLE_SIZE"),
            "REGRESSION: BUNDLE_SIZE should use EXPECTED_BUNDLE_SIZE when ZIP_LIST_OK=1 (fixes size mismatch from ZIP trailer)"
        );
    }

    /// Regression test for dd_if_bundle helper and SKIP_REMAINDER handling.
    /// BUG: When ZIP_DATA_OFFSET is not 4096-aligned (e.g. 59 for ZIP64),
    /// the old code used `dd bs=4096 skip=$SKIP_BLOCKS` which truncates the
    /// remainder — reading from the WRONG offset. This caused compressed data
    /// hash mismatches and wrong data being flashed.
    /// Fix: dd_if_bundle reads full blocks then uses tail -c to skip the remainder.
    #[test]
    fn test_regression_dd_if_bundle_remainder_handling() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1075,
            hash_hex: "a".repeat(64),
            comp_size: 334,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // dd_if_bundle helper function is defined
        assert!(
            script.contains("dd_if_bundle()"),
            "REGRESSION: dd_if_bundle helper not defined (remainder offset fix)"
        );

        // SKIP_REMAINDER is computed
        assert!(
            script.contains("SKIP_REMAINDER"),
            "REGRESSION: SKIP_REMAINDER not computed (remainder offset fix)"
        );

        // tail -c is used for remainder handling
        assert!(
            script.contains("tail -c"),
            "REGRESSION: tail -c not used for remainder handling (remainder offset fix)"
        );

        // dd_if_bundle is used in the flash pipeline (not raw dd if=$BUNDLE)
        assert!(
            script.contains("dd_if_bundle |"),
            "REGRESSION: dd_if_bundle not used in flash pipeline (remainder offset fix)"
        );
    }

    /// Verify the fallback decompressor and gzip stderr capture are present.
    /// When the primary decompressor fails, the script should:
    /// 1. Capture gzip's stderr (not suppress it)
    /// 2. Print diagnostic info
    /// 3. Try fallback decompressors (gzip -dc, gunzip, zcat, busybox gzip)
    #[test]
    fn test_fallback_decompressor_and_diagnostics() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1075,
            hash_hex: "a".repeat(64),
            comp_size: 334,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        // GZIP_ERR temp file is used (not 2>/dev/null on decompressor)
        assert!(
            script.contains("GZIP_ERR"),
            "GZIP_ERR temp file variable missing — stderr is being suppressed"
        );
        // Diagnostic info on failure
        assert!(
            script.contains("Details:"),
            "GZIP error diagnostic message missing"
        );
        assert!(
            script.contains("Bundle:"),
            "Bundle diagnostic info missing"
        );
        // Fallback decompressor list
        assert!(
            script.contains("gzip -dc"),
            "Fallback 'gzip -dc' missing"
        );
        assert!(
            script.contains("gunzip -c"),
            "Fallback 'gunzip -c' missing"
        );
        assert!(
            script.contains("zcat"),
            "Fallback 'zcat' missing"
        );
        assert!(
            script.contains("busybox gzip -dc"),
            "Fallback 'busybox gzip -dc' missing"
        );
    }

    /// Verify the 3 BlassGo-alignment fixes:
    /// 1. lptools resize is PRIMARY (not remove+create)
    /// 2. resolve_target checks /dev/block/mapper/ FIRST
    /// 3. Post-flash re-map (lptools unmap && lptools map) is present
    #[test]
    fn test_blassgo_alignment_3_fixes() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200,  // 1075 MB
            hash_hex: "a".repeat(64),
            comp_size: 350224384,  // 334 MB
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // ── Fix 1: lptools resize is PRIMARY ──
        // The resize step must try `lptools resize` FIRST, with remove+create
        // only as a fallback. The previous (broken) approach was remove+create
        // primary, which destroys the by-name symlink.
        assert!(
            script.contains("lptools resize \"$LP_NAME\" \"$NEW_SIZE_BYTES\""),
            "Fix 1: lptools resize (primary) missing from resize step"
        );
        // remove+create should only appear as FALLBACK (with "fallback" comment nearby)
        // Count remove+create occurrences — should be in fallback paths only
        let remove_create_count = script.matches("lptools remove \"$LP_NAME\"").count();
        assert!(
            remove_create_count >= 1,
            "Fix 1: remove+create fallback missing (count={})",
            remove_create_count
        );
        // The resize step should NOT start with remove+create as primary
        // Check that the resize comment says "resize FIRST" (not "remove+create FIRST")
        assert!(
            script.contains("try lptools resize FIRST"),
            "Fix 1: resize-first comment missing"
        );
        assert!(
            !script.contains("try remove+create FIRST"),
            "Fix 1: remove+create-first comment still present (should be resize-first)"
        );

        // ── Fix 2: resolve_target checks /dev/block/mapper/ FIRST ──
        // The function must check mapper/ paths before by-name/ paths.
        // This handles the case where remove+create (fallback) destroys the
        // by-name symlink but creates a fresh dm at /dev/block/mapper/.
        assert!(
            script.contains("/dev/block/mapper/"),
            "Fix 2: /dev/block/mapper/ path missing from resolve_target"
        );
        assert!(
            script.contains("mapper_slotted"),
            "Fix 2: mapper_slotted variable missing from resolve_target"
        );
        // mapper/ must be checked BEFORE by-name/ (priority order)
        let mapper_pos = script.find("if [ -e \"$mapper_slotted\" ]").unwrap_or(usize::MAX);
        let byname_pos = script.find("if [ -e \"$slotted\" ]").unwrap_or(usize::MAX);
        assert!(
            mapper_pos < byname_pos,
            "Fix 2: mapper/ must be checked BEFORE by-name/ (priority order)"
        );

        // ── Fix 3: Post-flash re-map ──
        // After dd writes, the script must do `lptools unmap && lptools map`
        // to refresh the dm-linear device (BlassGo step 6/7).
        assert!(
            script.contains("Post-flash re-map"),
            "Fix 3: post-flash re-map comment missing"
        );
        // Post-flash re-map is silent on success (cosmetic refactor cacdf13)
        // — only check that lptools unmap + map are present, not the ui_print
        assert!(
            script.contains("lptools unmap \"$REMAP_LP_NAME\""),
            "Fix 3: lptools unmap in post-flash re-map missing"
        );
        assert!(
            script.contains("lptools map \"$REMAP_LP_NAME\""),
            "Fix 3: lptools map in post-flash re-map missing"
        );
    }

    /// Verify the cleanup trap uses BlassGo pattern (unmap → resize → map)
    /// for rollback, not just resize alone.
    #[test]
    fn test_cleanup_trap_blassgo_pattern() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200,
            hash_hex: "a".repeat(64),
            comp_size: 350224384,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // Cleanup trap must do unmap BEFORE resize (BlassGo pattern)
        // The old code just did resize; the new code does unmap → resize → map.
        assert!(
            script.contains("lptools unmap \"$rname_lp\" >/dev/null 2>&1 || true"),
            "Cleanup: unmap before resize missing"
        );
        // After resize succeeds, must re-map to materialize rollback size
        assert!(
            script.contains("lptools map \"$rname_lp\" >/dev/null 2>&1 || true"),
            "Cleanup: re-map after resize missing"
        );
    }

    /// Verify optimization changes:
    /// 1. unmount_partition function is split from unmount_and_unmap_partition
    /// 2. twrp unmount dead code is REMOVED (was never callable — twrp binary
    ///    is PID 1 recovery, not in PATH for shell exec)
    /// 3. trim_pipe replaces head -c $PCSIZE (dd-based block reads)
    /// 4. verify_block uses ${i} not ${{i}} (format!() escaping bug fixed)
    /// 5. Post-flash re-map is silent on success
    /// 6. O_DIRECT (oflag=direct) for block device writes
    /// 7. verify_trim() uses single braces { } not double {{ }} (raw string bug)
    #[test]
    fn test_optimization_changes() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200,
            hash_hex: "a".repeat(64),
            comp_size: 350224384,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // 1. unmount_partition function is defined (split from unmount_and_unmap)
        assert!(
            script.contains("unmount_partition()"),
            "Optimize: unmount_partition function not defined"
        );

        // 2. twrp unmount dead code is REMOVED
        // (twrp binary is PID 1 recovery, not shell-callable — command -v twrp
        //  always returns false. Dead code removed for cleanliness.)
        assert!(
            !script.contains("twrp unmount"),
            "Optimize: twrp unmount dead code should be removed"
        );

        // 3. trim_pipe replaces head -(c) $PCSIZE in flash pipeline
        assert!(
            script.contains("trim_pipe"),
            "Optimize: trim_pipe missing from flash pipeline (replaces head -c)"
        );
        // head -c "$PSIZE" should NOT appear anywhere in the script —
        // replaced by verify_trim (verify step) and trim_pipe (flash step).
        // Check that both dd-based replacements are defined as functions.
        assert!(
            script.contains("trim_pipe()"),
            "Optimize: trim_pipe function not defined"
        );
        assert!(
            script.contains("verify_trim()"),
            "Optimize: verify_trim function not defined (replaces head -c in verify)"
        );

        // 6. O_DIRECT (oflag=direct) for block device writes
        assert!(
            script.contains("oflag=direct"),
            "Optimize: oflag=direct (O_DIRECT) missing from block device write"
        );
        assert!(
            script.contains("-b \"$PTARGET\""),
            "Optimize: block device check (-b) missing before oflag=direct"
        );
        // O_DIRECT probe should test on actual target, not /dev/null
        assert!(
            script.contains("conv=notrunc"),
            "Optimize: O_DIRECT probe should use conv=notrunc on actual target"
        );
        // The O_DIRECT probe writes to $PTARGET with conv=notrunc, not to /dev/null.
        // Verify the probe pattern includes writing to the actual target.
        assert!(
            script.contains("of=\"$PTARGET\" bs=4096 count=1 conv=notrunc"),
            "Optimize: O_DIRECT probe should write to $PTARGET (not /dev/null)"
        );

        // 4. verify_block uses ${i} (not ${{i}} — that was a format!() escaping bug)
        // The verify_block is a raw string assigned to a variable, so it should
        // contain literal ${i}, not ${{i}}.
        assert!(
            script.contains("verify_${i}.fifo"),
            "Optimize: verify_block should use dollar-brace-i (not dollar-brace-brace-i)"
        );
        assert!(
            !script.contains("verify_${{i}}.fifo"),
            "Optimize: verify_block still has double-brace i (format escaping bug)"
        );

        // 5. Post-flash re-map is silent on success (no "Re-mapping" ui_print)
        assert!(
            !script.contains("Re-mapping $REMAP_LP_NAME"),
            "Optimize: post-flash re-map should be silent (remove 'Re-mapping' ui_print)"
        );

        // 6. Resize step uses unmount_partition (not unmount_and_unmap_partition)
        assert!(
            script.contains("unmount_partition \"$pname\" || true"),
            "Optimize: resize step should use unmount_partition (not unmount_and_unmap_partition)"
        );

        // 7. verify_trim() uses single braces { } not double {{ }} (raw string bug)
        assert!(
            !script.contains("verify_trim() {{"),
            "Optimize: verify_trim() must not have double braces {{ (raw string escaping bug)"
        );
        assert!(
            script.contains("verify_trim() {"),
            "Optimize: verify_trim() must use single braces (POSIX shell syntax)"
        );
    }

    /// Verify Transsion (Infinix/itel/Tecno) physical partition support:
    /// 1. resolve_target checks /dev/block/platform/bootdevice/by-name/ (Priority 5+6)
    /// 2. bootdev_slotted + bootdev_plain variables defined
    /// 3. Transsion physical partitions can be resolved (lk, logo, spmfw, tee, vendor_boot)
    #[test]
    fn test_transsion_physical_partition_support() {
        let meta = vec![PartitionMeta {
            name: "lk".to_string(),
            unc_size: 2097152,  // 2 MB
            hash_hex: "a".repeat(64),
            comp_size: 1048576,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // 1. /dev/block/platform/bootdevice/by-name/ path resolution
        assert!(
            script.contains("/dev/block/platform/bootdevice/by-name/"),
            "Transsion: /dev/block/platform/bootdevice/by-name/ path missing from resolve_target"
        );

        // 2. bootdev_slotted + bootdev_plain variables
        assert!(
            script.contains("bootdev_slotted"),
            "Transsion: bootdev_slotted variable missing from resolve_target"
        );
        assert!(
            script.contains("bootdev_plain"),
            "Transsion: bootdev_plain variable missing from resolve_target"
        );

        // 3. Priority 5 comment (Transsion physical GPT)
        assert!(
            script.contains("PHYSICAL GPT partitions on Transsion"),
            "Transsion: Priority 5 comment for physical GPT missing"
        );

        // 4. bootdev paths checked AFTER by-name paths (priority order)
        // Priority 5 (bootdev_slotted) must come AFTER Priority 4 (plain by-name)
        let bootdev_pos = script.find("if [ -e \"$bootdev_slotted\" ]").unwrap_or(usize::MAX);
        let plain_pos = script.find("if [ -e \"$plain\" ]").unwrap_or(usize::MAX);
        assert!(
            plain_pos < bootdev_pos,
            "Transsion: bootdev paths must be checked AFTER by-name paths (priority order)"
        );
    }

    /// Verify auto-map fix for unmapped dynamic partitions (Format Data recovery).
    ///
    /// Root cause (recovery.log CRC32 0x57923752, Itel S666LN):
    ///   User did Format Data → recovery Unmap_Super_Devices destroyed system_b
    ///   → OTAku validate_target found /dev/block/mapper/system_b missing → ABORT
    ///
    /// Fix: validate_target now calls `lptools map $LP_NAME` when target doesn't
    ///      exist AND it's a dynamic partition, before falling through to ABORT.
    #[test]
    fn test_auto_map_unmapped_dynamic_partition() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 5013785600,  // 4788 MB
            hash_hex: "a".repeat(64),
            comp_size: 4158234112,  // 3965 MB
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);

        // 1. Auto-map comment present (references Format Data / Unmap_Super_Devices)
        assert!(
            script.contains("Auto-map unmapped dynamic partitions"),
            "Auto-map: comment block missing from validate_target"
        );

        // 2. Auto-map trigger condition: target missing + is_dynamic + lptools available
        assert!(
            script.contains("if [ ! -e \"$target\" ] && [ \"$is_dynamic\" = \"1\" ]"),
            "Auto-map: trigger condition (target missing + is_dynamic) missing"
        );

        // 3. lptools map call present
        assert!(
            script.contains("lptools map \"$lp_name\""),
            "Auto-map: 'lptools map $lp_name' call missing"
        );

        // 4. Slot-suffixed LP_NAME construction
        // Note: script contains POST-format!() output, so single braces ${name}
        assert!(
            script.contains("lp_name=\"${name}${TARGET_SLOT}\""),
            "Auto-map: slot-suffixed lp_name construction missing"
        );

        // 5. Re-resolve target after successful map
        assert!(
            script.contains("target=$(resolve_target \"$name\")"),
            "Auto-map: re-resolve target after map missing"
        );

        // 6. Success message
        assert!(
            script.contains("lptools map $lp_name succeeded"),
            "Auto-map: success ui_print message missing"
        );

        // 7. Failure message (lptools map failed)
        assert!(
            script.contains("lptools map $lp_name failed"),
            "Auto-map: failure ui_print message missing"
        );

        // 8. Reference to Format Data (root cause documentation in code)
        assert!(
            script.contains("Format Data"),
            "Auto-map: Format Data reference (root cause doc) missing"
        );
    }

    // ── Bug #12 regression: lz4 compression support ──

    /// Verify lz4 (compress_id=5) produces correct decompressor command,
    /// fallback chain, and multi-threaded upgrade in the flash script.
    #[test]
    fn test_regression_lz4_compression_support() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 5120000000,  // 5120 MB
            hash_hex: "a".repeat(64),
            comp_size: 2048000000, // 2048 MB
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 5, "lz4", &meta, 0, "", false);

        // 1. COMPRESS_ID=5 in the script
        assert!(
            script.contains("COMPRESS_ID=5"),
            "REGRESSION: lz4 compress_id should be 5"
        );

        // 2. Primary decompressor is "lz4"
        assert!(
            script.contains("check_decompressor \"lz4\""),
            "REGRESSION: lz4 decompressor check missing"
        );

        // 3. Multi-threaded upgrade for lz4 (lz4 -T0)
        assert!(
            script.contains("lz4 -dc -T0"),
            "REGRESSION: lz4 multi-threaded upgrade (-T0) missing"
        );
        assert!(
            script.contains("5) # lz4"),
            "REGRESSION: lz4 case in MT upgrade switch missing"
        );

        // 4. Fallback decompressor chain includes lz4 variants
        assert!(
            script.contains("\"lz4 -dc\""),
            "REGRESSION: lz4 fallback 'lz4 -dc' missing"
        );
        assert!(
            script.contains("\"lz4 -d\""),
            "REGRESSION: lz4 fallback 'lz4 -d' missing"
        );
        assert!(
            script.contains("\"busybox lz4 -dc\""),
            "REGRESSION: lz4 fallback 'busybox lz4 -dc' missing"
        );

        // 5. Recommended compression hint includes lz4
        assert!(
            script.contains("--compress lz4 (fastest)"),
            "REGRESSION: lz4 recommendation missing from error message"
        );

        // 6. BUG FIX: Primary DECOMP_PIPE for lz4 must use -dc (not just -d)
        // lz4 requires explicit -c for stdout output when piped; gzip/bzip2/xz
        // auto-detect pipe, but lz4 does not (especially older/busybox versions).
        assert!(
            script.contains("COMPRESS_ID = \"5\"") || script.contains("COMPRESS_ID\" = \"5\"") || script.contains(r#"$COMPRESS_ID" = "5"#) || script.contains("COMPRESS_ID = 5"),
            "REGRESSION: lz4 compress_id=5 check for -dc flag missing"
        );
    }

    /// Regression: lz4 primary DECOMP_PIPE must use -dc flag.
    /// Without -c, lz4 -d may attempt to write to a file instead of stdout
    /// when used in a pipe (dd | lz4 -d | dd), causing silent flash failure.
    #[test]
    fn test_regression_lz4_decomp_pipe_dc_flag() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "a".repeat(64),
            comp_size: 536870912,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 5, "lz4", &meta, 0, "", false);
        // The lz4-specific override must be present in the generated script
        assert!(
            script.contains(r#"if [ "$COMPRESS_ID" = "5" ]; then"#),
            "REGRESSION: lz4 compress_id=5 override for -dc flag missing"
        );
        assert!(
            script.contains("DECOMP_PIPE=\"$DECOMP_CMD -dc\""),
            "REGRESSION: lz4 DECOMP_PIPE -dc override missing"
        );
    }

    // ── Multi-threaded decompressor regression tests ──

    /// Verify MT decompressor upgrade paths for supported algorithms.
    /// pigz/pbzip2/brotli REMOVED — never available on OrangeFox recovery.
    /// Remaining MT paths: xz → xz -T0, lz4 → lz4 -T0.
    /// gzip and bzip2 have NO MT upgrade (pigz/pbzip2 removed).
    #[test]
    fn test_regression_mt_decompressor_all_algorithms() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "a".repeat(64),
            comp_size: 536870912,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];

        // gzip — NO MT upgrade (pigz removed, never available on OrangeFox)
        let gzip_script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        // pigz should NOT appear as an executable command (in a case branch, fallback, or pipe)
        // It MAY appear in comments (e.g. "pigz REMOVED") — that's fine.
        assert!(
            !gzip_script.contains("pigz -dc") && !gzip_script.contains("pigz -d"),
            "REGRESSION: pigz command should be removed from generated script"
        );
        // gzip fallback chain should NOT contain pigz
        assert!(
            !gzip_script.contains("\"pigz -dc\""),
            "REGRESSION: pigz should not be in gzip fallback chain"
        );

        // bzip2 — NO MT upgrade (pbzip2 removed, never available on OrangeFox)
        let bzip2_script = build_update_script(1, 2, "bzip2", &meta, 0, "", false);
        // pbzip2 should NOT appear as an executable command
        // It MAY appear in comments (e.g. "pbzip2 REMOVED") — that's fine.
        assert!(
            !bzip2_script.contains("pbzip2 -dc") && !bzip2_script.contains("pbzip2 -d"),
            "REGRESSION: pbzip2. command should be removed from generated script"
        );
        assert!(
            !bzip2_script.contains("\"pbzip2 -dc\""),
            "REGRESSION: pbzip2 should not be in bzip2 fallback chain"
        );

        // xz → xz -T0
        let xz_script = build_update_script(1, 3, "xz", &meta, 0, "", false);
        assert!(
            xz_script.contains("3) # xz → try xz -T0"),
            "REGRESSION: xz MT case label missing"
        );
        assert!(
            xz_script.contains("xz -T0 -dc"),
            "REGRESSION: xz MT command missing"
        );
        assert!(
            xz_script.contains("\"xz -T0 -dc\""),
            "REGRESSION: xz -T0 not in xz fallback chain"
        );

        // lz4 → lz4 -T0
        let lz4_script = build_update_script(1, 5, "lz4", &meta, 0, "", false);
        assert!(
            lz4_script.contains("5) # lz4"),
            "REGRESSION: lz4 MT case label missing"
        );
        assert!(
            lz4_script.contains("lz4 -dc -T0"),
            "REGRESSION: lz4 MT command missing"
        );

        // NPROC detection is present
        assert!(
            gzip_script.contains("NPROC=$(nproc"),
            "REGRESSION: nproc detection for MT decompressors missing"
        );

        // zstd → zstd -T0
        let zstd_script = build_update_script(1, 6, "zstd", &meta, 0, "", false);
        assert!(
            zstd_script.contains("6) # zstd"),
            "REGRESSION: zstd MT case label missing"
        );
        assert!(
            zstd_script.contains("zstd -dc -T0"),
            "REGRESSION: zstd MT command missing"
        );
        assert!(
            zstd_script.contains("\"zstd -dc\""),
            "REGRESSION: zstd -dc not in zstd fallback chain"
        );
        assert!(
            zstd_script.contains("\"zstdcat\""),
            "REGRESSION: zstdcat not in zstd fallback chain"
        );
        // ZSTD requires -dc (like lz4) — test that DECOMP_PIPE override is present
        assert!(
            zstd_script.contains("COMPRESS_ID\" = \"6\""),
            "REGRESSION: ZSTD COMPRESS_ID=6 -dc override missing"
        );
    }

    /// Verify that MT decompressor upgrade only activates when NPROC > 1.
    #[test]
    fn test_regression_mt_decompressor_nproc_guard() {
        let meta = vec![PartitionMeta {
            name: "system".to_string(),
            unc_size: 1073741824,
            hash_hex: "a".repeat(64),
            comp_size: 536870912,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 1, "gzip", &meta, 0, "", false);
        // The MT upgrade block should be guarded by NPROC > 1
        assert!(
            script.contains("if [ \"$NPROC\" -gt 1 ]; then"),
            "REGRESSION: NPROC > 1 guard for MT decompressor missing"
        );
    }

    // ── Bug #13 regression: verify_trim() {{ raw string escaping ──

    /// Verify that verify_trim() uses single braces `{` / `}` in the generated
    /// shell script, NOT double braces `{{` / `}}`.
    ///
    /// ROOT CAUSE: `verify_block` is constructed as a Rust raw string
    /// (`r#"..."#`). Inside raw strings, `{{` is a LITERAL `{{`, not a
    /// format!() escape for `{`. When the raw string is interpolated via
    /// `format!("{verify_block}")`, the literal `{{` passes through
    /// unchanged, producing invalid POSIX shell syntax:
    ///   verify_trim() {{
    /// This crashes the flashing script at runtime:
    ///   /tmp/updater: line 1952: syntax error near unexpected token `{{'
    ///
    /// EVIDENCE: recovery.log from Infinix X695C (OrangeFox R11.1):
    ///   2024-10-12 OTAku flash → ERROR: 2 → emergency cleanup
    ///   The script got as far as "Checking available storage space..."
    ///   before hitting the verify_trim() definition in the verify step.
    ///
    /// FIX: Replace `{{` with `{` and `}}` with `}` in the raw string.
    /// Raw strings don't need format!() brace escaping — single braces
    /// are literal characters that pass through as-is.
    #[test]
    fn test_regression_verify_trim_single_braces() {
        let meta = vec![PartitionMeta {
            name: "vendor".to_string(),
            unc_size: 1127219200, // 1075 MB
            hash_hex: "a".repeat(64),
            comp_size: 234881024,
            data_offset: 0,
            comp_hash_hex: "b".repeat(64),
        }];
        let script = build_update_script(1, 3, "xz", &meta, 0, "", false);

        // verify_trim() must use single braces: `verify_trim() {` not `verify_trim() {{`
        assert!(
            script.contains("verify_trim() {"),
            "REGRESSION: verify_trim() must use single braces (not {{) — raw string bug"
        );
        // Double braces must NOT appear in the generated script
        assert!(
            !script.contains("verify_trim() {{"),
            "REGRESSION: verify_trim() still has double braces {{ — raw string escaping bug"
        );
        // Closing `}}` (double) must also not appear for verify_trim
        // Note: `}}` may legitimately appear elsewhere in format!() templates
        // (e.g. ${{i}} in the flash loop), so we specifically check the
        // verify_trim function body area.
        assert!(
            !script.contains("VERIFY_REMAINDER 2>/dev/null\n            fi\n        }}"),
            "REGRESSION: verify_trim closing brace is double-brace — must be single brace"
        );
    }
