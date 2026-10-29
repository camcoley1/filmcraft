//! Bit-exactness against ffmpeg's decoder on libx265 / VideoToolbox fixtures (skipped when ffmpeg is
//! absent).

mod common;

macro_rules! fixture_tests {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                match common::check_fixture(stringify!($name)) {
                    Ok(true) => {}
                    Ok(false) => eprintln!("skipped {}", stringify!($name)),
                    Err(e) => panic!("{e}"),
                }
            }
        )*
    };
}

fixture_tests!(
    intra_main,
    intra_main10,
    intra_nosao_nodbk,
    ultrafast,
    veryfast,
    medium,
    slow,
    medium_main10,
    slow_main10,
    p_only,
    bframes,
    no_sao,
    no_deblock,
    deblock_m3_3,
    no_wpp,
    rect_amp,
    tskip,
    weightp,
    weightp_main10,
    crf0,
    crf51,
    qp1_main10,
    lossless,
    cu_lossless,
    slices4,
    scaling_list,
    ctu16,
    ctu32_tu4,
    min_cu16,
    no_signhide,
    constrained_intra,
    no_tmvp,
    no_strong_intra,
    max_merge1,
    open_gop,
    keyint5_idr,
    odd_1918x1078,
    hd_1080p,
    uhd_4k_main10,
    vt_main,
    vt_main10,
    intra_422_10,
    intra_422_12,
    intra_422_nofilt,
    medium_422_8,
    medium_422_10,
    medium_422_12,
    bframes_422_10,
    slices4_422_10,
    rect_amp_422_10,
    weightp_422_10,
    tskip_422_10,
    chroma_qp_422_10,
    scaling_list_422_10,
    lossless_422_10,
    crf0_422_10,
    odd_422_10,
    medium_main12,
);

/// Range-extension conformance bitstreams (ITU-T JCT-VC RExt suite, not in the repository): with
/// `FILMCRAFT_HEVC_REXT_DIR` set to a folder holding them (any layout), every listed 4:2:0 / 4:2:2
/// stream must be present and decode (single- and multi-threaded) to the MD5 of the suite's own
/// reconstruction (`*_yuv.md5` / `md5sum.txt`), and the others (4:4:4, unsupported tools) must be
/// refused with `Error::Unsupported`, never decoded wrongly. Skipped without the variable.
#[test]
fn rext_conformance_streams() {
    let Some(dir) = std::env::var_os("FILMCRAFT_HEVC_REXT_DIR").map(std::path::PathBuf::from) else {
        eprintln!("skipped: FILMCRAFT_HEVC_REXT_DIR not set");
        return;
    };
    // (stream name, Some(MD5 of the reconstructed YUV) = decoded exactly, None = refused)
    let expected: &[(&str, Option<&str>)] = &[
        ("ADJUST_IPRED_ANGLE_A_RExt_Mitsubishi_2", Some("b045776950c2e06d35348bd03268c07f")),
        ("GENERAL_8b_420_RExt_Sony_1", Some("99eb7a5106988759fd0eaecdb66e2cd2")),
        ("GENERAL_10b_420_RExt_Sony_1", Some("d19c7e1764971983f0e827277db2f69a")),
        ("GENERAL_12b_420_RExt_Sony_1", Some("27a2b09b136687f7a391bfdbc50ac964")),
        ("GENERAL_10b_422_RExt_Sony_1", Some("ea5dfb01c95bffb169cd78e549bd39ab")),
        ("GENERAL_12b_422_RExt_Sony_1", Some("d4f51ab5405659ad90e788083bcfc18d")),
        ("IPCM_A_RExt_NEC_2", Some("8d4eb18812338f9f475035b8da27e74a")),
        ("Main_422_10_A_RExt_Sony_2", Some("66530b83be6647f87efbb2b9ae6e1e28")),
        ("Main_422_10_B_RExt_Sony_2", Some("fd163bcbdc24792a23781ee7aace74eb")),
        ("WPP_AND_TILE_10Bit422Test_HIGH_TP_444_10BIT_RExt_Apple_2", None),
        ("PERSIST_RPARAM_A_RExt_Sony_3", None),
        ("TSCTX_8bit_RExt_SHARP_1", None),
        ("IPCM_B_RExt_NEC", None),
    ];
    let mut files = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "bit" || x == "bin") {
                files.push(p);
            }
        }
    }
    for &(name, want) in expected {
        let path = files.iter().find(|p| p.file_stem().is_some_and(|s| s == name)).unwrap_or_else(|| panic!("{name} missing from FILMCRAFT_HEVC_REXT_DIR"));
        let Some(md5) = want else {
            let err = common::decode_file_threads(path, 1).err().map(|(_, e, _)| e);
            assert!(matches!(err, Some(filmcraft_hevc::Error::Unsupported(_))), "{name}: expected an unsupported error, got {err:?}");
            continue;
        };
        for threads in [1, 0] {
            let pics = common::decode_file_threads(path, threads).unwrap_or_else(|(au, e, _)| panic!("{name}: error at {au}: {e}"));
            let bytes: Vec<u8> = pics.iter().flat_map(|p| [&p.y, &p.u, &p.v].into_iter().flat_map(|pl| pl.to_le_bytes())).collect();
            assert_eq!(common::md5_hex(&bytes), md5, "{name} (threads {threads}): reconstruction differs from the conformance MD5");
        }
    }
}

/// The MD5 helper the conformance checks rely on (RFC 1321 test vectors).
#[test]
fn md5_matches_rfc1321() {
    assert_eq!(common::md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(common::md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(common::md5_hex(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"), "57edf4a22be3c955ac49da2e2107b67a");
}

/// Draft mode: sub-layer non-reference pictures skip deblocking and SAO and are flagged; every
/// other picture stays bit-exact, single-threaded and frame-threaded.
#[test]
fn draft_mode_changes_only_flagged_non_reference_pictures() {
    let mut checked = 0;
    for name in ["bframes", "medium", "slow", "weightp_main10"] {
        let f = common::fixture(name);
        let Some((hevc, yuv)) = common::ensure(f) else {
            eprintln!("skipped {name}");
            continue;
        };
        let reference = std::fs::read(&yuv).unwrap();
        let (w, h) = (f.width as usize, f.height as usize);
        let bps = if f.bit_depth > 8 { 2 } else { 1 };
        let fsize = (w * h + 2 * w.div_ceil(2) * h.div_ceil(2)) * bps;
        for threads in [1, 3] {
            let pics = common::decode_file_opts(&hevc, threads, true).unwrap_or_else(|(au, e, _)| panic!("{name}: error at {au}: {e}"));
            common::check_pts(&pics).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(pics.len() * fsize, reference.len(), "{name}: picture count");
            let (mut draft, mut changed) = (0, 0);
            for (i, p) in pics.iter().enumerate() {
                let bytes: Vec<u8> = [&p.y, &p.u, &p.v].iter().flat_map(|pl| pl.to_le_bytes()).collect();
                let same = bytes == reference[i * fsize..(i + 1) * fsize];
                if p.draft {
                    draft += 1;
                    changed += !same as usize;
                } else {
                    assert!(same, "{name} (threads {threads}): picture {i} (not draft) differs from the reference");
                }
            }
            assert!(draft > 0, "{name}: has non-reference pictures");
            assert!(changed > 0, "{name}: skipping the in-loop filters changes some draft picture");
            checked += 1;
        }
    }
    if checked == 0 {
        eprintln!("skipped: no fixtures");
    }
}

/// Print which coding tools each fixture exercises (run with `-- --ignored --nocapture`).
#[test]
#[ignore]
fn coverage_report() {
    for f in common::FIXTURES {
        let Some((hevc, _)) = common::ensure(f) else { continue };
        let data = std::fs::read(hevc).unwrap();
        let mut dec = filmcraft_hevc::Decoder::new();
        for au in common::split_access_units(&data) {
            let _ = dec.decode(au, 0);
        }
        dec.flush();
        println!("{:<20} {:?}", f.name, dec.stats());
    }
}

/// The comparison must report a single flipped sample (guards against a vacuous harness).
#[test]
fn compare_detects_mismatch() {
    let f = common::fixture("intra_main10");
    let Some((hevc, yuv)) = common::ensure(f) else { return };
    let pics = common::decode_file_threads(&hevc, 1).map_err(|e| e.1).unwrap();
    let mut reference = std::fs::read(yuv).unwrap();
    common::compare(&pics, &reference, f.width as usize, f.height as usize, f.bit_depth, f.chroma).unwrap();
    let pos = reference.len() - 7;
    reference[pos] ^= 1;
    let err = common::compare(&pics, &reference, f.width as usize, f.height as usize, f.bit_depth, f.chroma).unwrap_err();
    assert!(err.contains("plane V"), "{err}");
}

/// Pre-generate every fixture and its reference decode (`cargo xtask fixtures`).
#[test]
#[ignore]
fn generate_fixtures() {
    let dir = common::fixtures_dir();
    for f in common::FIXTURES {
        let outs = [dir.join(format!("{}.hevc", f.name)), dir.join(format!("{}.yuv", f.name))];
        filmcraft_testkit::fixtures::generate_and_report(&format!("hevc/{}", f.name), &outs, || common::ensure(f));
    }
}
