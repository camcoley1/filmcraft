//! Bit-exactness against ffmpeg's decoder on libx264-generated fixtures (skipped when ffmpeg is absent).

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
    intra_cavlc,
    intra_cavlc_noise,
    intra_cavlc_8x8,
    intra_cavlc_cqm,
    p_cavlc_nodeblock,
    baseline_qcif,
    baseline_cif_noise,
    cavlc_b,
    cavlc_b_temporal,
    cavlc_weightp,
    slices4_cavlc,
    qp1_cavlc,
    main_cabac_b,
    high_720p,
    high_1080p,
    crop_1918x1078,
    bpyramid,
    weightp2,
    weightb,
    direct_temporal,
    direct_spatial,
    ref4,
    no_deblock,
    deblock_m2_2,
    cqm_jvt,
    slices4,
    keyint10,
    open_gop,
    constrained_intra,
    no8x8dct,
    qp50,
    qp1,
    vt_high,
    vt_main,
    vt_baseline,
    vt_high_1080p,
    high10_intra,
    high10_intra_cavlc,
    high10_medium,
    high10_cavlc_b,
    high10_bpyramid,
    high10_weightp,
    high10_temporal,
    high10_slices_dbk,
    high10_no8x8_cqm,
    high10_qp1,
    high10_qp51,
    high10_odd,
    high422_8_intra,
    high422_8_medium,
    high422_8_cavlc_b,
    high422_10_intra,
    high422_10_intra_cavlc,
    high422_10_medium,
    high422_10_bpyramid,
    high422_10_weightp,
    high422_10_temporal,
    high422_10_slices_dbk,
    high422_10_chroma_qp6,
    high422_10_no8x8_cqm,
    high422_10_qp1,
    high422_10_qp51,
    high422_10_odd,
);

/// JVT professional / FRExt conformance bitstreams (High 10 Intra, High 4:2:2 and High 4:2:2 Intra;
/// ITU-T, not in the repository). With `FILMCRAFT_H264_JVT_DIR` set to a folder holding them (any
/// layout), every listed stream must be present and match its pinned SHA-256; the progressive ones
/// this decoder supports decode to their pinned reconstruction MD5 (single- and frame-threaded), and
/// the interlaced ones are refused with `Error::Unsupported`. Skipped without the variable.
///
/// Oracle: unlike the HEVC suite, the JVT suites ship no reconstruction checksums. Six streams
/// (Hi422FR10-15) ship the reconstructed YUV itself; their MD5s are of that file. The others are
/// the MD5s of `ffmpeg -fps_mode passthrough -f rawvideo -pix_fmt <fmt>` output from ffmpeg 8.1,
/// which also reproduces the six shipped reconstructions bit for bit. No ffmpeg is needed to run
/// the test.
#[test]
fn jvt_high_bit_depth_conformance_streams() {
    let Some(dir) = std::env::var_os("FILMCRAFT_H264_JVT_DIR").map(std::path::PathBuf::from) else {
        eprintln!("skipped: FILMCRAFT_H264_JVT_DIR not set");
        return;
    };
    // (stream name, bitstream SHA-256, Some((width, height, pix_fmt, reconstruction MD5, shipped
    // reconstruction)) = bit-exact, None = refused)
    type Want = Option<(usize, usize, &'static str, &'static str, bool)>;
    let expected: &[(&str, &str, Want)] = &[
        (
            "PPH10I1_Panasonic_A",
            "7c2f72b1091095840aed11dd659530531ad4d17ac59386145878fa51de6667f5",
            Some((1280, 720, "yuv420p10le", "074c5b47578cb72bef0db3930f00303f", false)),
        ),
        (
            "PPH10I2_Panasonic_A",
            "80a02559ed20f0e3cc54d84faf7d8f6d79530c10067e7e4bbc663ef6590838c9",
            Some((1280, 720, "yuv420p10le", "8e82308fe0f3b9a8fdacf8af038ca135", false)),
        ),
        (
            "PPH10I3_Panasonic_A",
            "66c5b426e66b97bb9951a590fc52435d5da03e73673a25bcfbdfddca72a9a117",
            Some((1280, 720, "yuv420p10le", "9fc6af89080ffbf9a5cd181696dbbdd2", false)),
        ),
        ("PPH10I4_Panasonic_A", "fc9b9512027d6c6ae2034e583f12f109823fc1bab4cf6fa9fa8720975b4db37e", None),
        (
            "Hi422FR1_SONY_A",
            "d0fcd8a6699d85f78eb80d8c2b2113ce6eb03347a0dc8ed19aa51b308986a589",
            Some((352, 288, "yuv422p", "e9828a1e3fdc1e3c1760e736ced80211", false)),
        ),
        (
            "Hi422FR2_SONY_A",
            "22af25dca2d8d217b4f2d5c7f2f6aab77053e0fbe758b4f8594a6ba74f45a0eb",
            Some((352, 288, "yuv422p", "8da5d0e6233dfe33b251d32b68a775de", false)),
        ),
        (
            "Hi422FR3_SONY_A",
            "548616c086229b93c43c1c1d1f399f1e135afe9a4df28a8a556748c5483c23e3",
            Some((352, 288, "yuv422p", "58516c013ae18d56abac50e80b9c7d39", false)),
        ),
        (
            "Hi422FR4_SONY_A",
            "be1be3e010901f6042115cd2edc0bf103e85d723c9706f20b28bdbeee7a27793",
            Some((352, 288, "yuv422p", "7faa9c7c38da168dff60eaf9528c9544", false)),
        ),
        (
            "Hi422FR6_SONY_A",
            "2ba847a7f9756eebd6a1204f9eecbb85628ebc6b424d5af2493bccb4fd0e2e61",
            Some((352, 288, "yuv422p10le", "9681ee446d3f9247553ad2b7ed34f562", false)),
        ),
        (
            "Hi422FR7_SONY_A",
            "019973c06bf42a3bf21dfaa6b830f455c51ba5b184d261cced7102e70a2cfa83",
            Some((352, 288, "yuv422p10le", "6c044824f67d9b06b26fa1e2bb9c7946", false)),
        ),
        (
            "Hi422FR8_SONY_A",
            "4c953e739d22f6927bb26d8db4a34644165394827b05a30cf2f9a4d2dff50333",
            Some((352, 288, "yuv422p10le", "facb28dec0ad67a1786a8e5d6735c77f", false)),
        ),
        (
            "Hi422FR9_SONY_A",
            "77d2f849ada618eafd78168cf35004b9d2b46c555017899084b7343a224adb0e",
            Some((352, 288, "yuv422p10le", "80039bfe8c1d434d27743a3538bfb209", false)),
        ),
        (
            "Hi422FR10_SONY_B",
            "8aed60728f2b8be7dd4886a94e73702532941557d712e04680ce8c258be7ccec",
            Some((352, 288, "yuv422p", "b32b7847f791ce22bad747cff08ce398", true)),
        ),
        (
            "Hi422FR11_SONY_B",
            "beafe4fd8806521be39da872d9e3a601bb59c948b9d17da7954f0b19a8603e02",
            Some((352, 288, "yuv422p", "f50fb1a31adf9985b864b959d890922b", true)),
        ),
        (
            "Hi422FR12_SONY_B",
            "762ddab658745333d300d6be4352ae7dda4e4ab14287d5ef3ccf706bbb477632",
            Some((352, 288, "yuv422p", "513afd34c94411f91dfecf2dd1880323", true)),
        ),
        (
            "Hi422FR13_SONY_B",
            "aa55f7064732a83c86fe228ba3cf8ab065b3129cf0f107c9fba469e57dd59881",
            Some((352, 288, "yuv422p10le", "412bad3d2624a83428571d1c7a6d6f92", true)),
        ),
        (
            "Hi422FR14_SONY_B",
            "f760dd936f262789639e4d539fabc10279ac8feef386a118c3131fb48401f758",
            Some((352, 288, "yuv422p10le", "43913b65400b8af68f65aa2520666030", true)),
        ),
        (
            "Hi422FR15_SONY_B",
            "2bd398bf290469405b3d7dd6cea756680ab76537bbaadaf781a99a6647cbb831",
            Some((352, 288, "yuv422p10le", "234509d38715eeed66b55fa8f03074f8", true)),
        ),
        ("Hi422FREXT16_SONY_A", "dac74fe4298833ace84e644c3eaa79ca938b0562b0e8e8dc262c5c0979656219", None),
        (
            "PPH422I1_Panasonic_A",
            "c6b53da3735d92a98bbf4f363e5af00e821bc8f8debe1ec9999910c7511b964c",
            Some((1280, 720, "yuv422p10le", "1ac8535dd650c138d590951f7914b27d", false)),
        ),
        (
            "PPH422I2_Panasonic_A",
            "faca58ec834c82c63ddcf406374c7e76e112b3c5cc71adf0f016b1327f60bfbf",
            Some((1280, 720, "yuv422p10le", "fd1d90779487e37e117e6b1470de0883", false)),
        ),
        (
            "PPH422I3_Panasonic_A",
            "2fc5a02e724c6b726b489e45f08e17a0fb68f04fb0221bd30028776594e1677c",
            Some((1280, 720, "yuv422p10le", "19006fed447cd2017e028beb91ebf0e2", false)),
        ),
        ("PPH422I4_Panasonic_A", "af882d0e6e8ad83d6e7f414b0378867125732df987fb0f364167cd998d6802bf", None),
    ];
    let mut files = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p);
            }
        }
    }
    let find = |stem: &str, exts: &[&str]| {
        files.iter().find(|p| p.file_stem().is_some_and(|s| s == stem) && p.extension().is_some_and(|x| exts.iter().any(|e| x == *e)))
    };
    let missing: Vec<&str> = expected.iter().filter(|(name, _, _)| find(name, &["264", "jsv"]).is_none()).map(|(name, _, _)| *name).collect();
    assert!(missing.is_empty(), "JVT conformance streams missing from FILMCRAFT_H264_JVT_DIR: {missing:?}");
    for &(name, sha256, want) in expected {
        let Some(path) = find(name, &["264", "jsv"]) else { continue };
        let data = std::fs::read(path).unwrap();
        assert_eq!(common::sha256_hex(&data), sha256, "{name}: not the expected bitstream");
        let Some((w, h, pix_fmt, md5, shipped)) = want else {
            let err = common::decode_file_threads(path, 1).err().map(|(_, e, _)| e);
            assert!(matches!(err, Some(filmcraft_h264::Error::Unsupported(_))), "{name}: expected an unsupported error, got {err:?}");
            continue;
        };
        if shipped {
            let yuv = find(name, &["yuv"]).unwrap_or_else(|| panic!("{name}: shipped reconstruction missing"));
            assert_eq!(common::md5_hex(&std::fs::read(yuv).unwrap()), md5, "{name}: shipped reconstruction differs");
        }
        let (bps, _) = common::pix_fmt_layout(pix_fmt);
        for threads in [1, 0] {
            let pics = common::decode_file_threads(path, threads).unwrap_or_else(|(au, e, _)| panic!("{name}: error at {au}: {e}"));
            assert!(pics.iter().all(|p| (p.width as usize, p.height as usize) == (w, h)), "{name}: picture size");
            assert_eq!(common::md5_hex(&common::raw_yuv(&pics, bps)), md5, "{name} (threads {threads}): reconstruction MD5");
        }
    }
}

#[test]
fn hash_helpers_match_reference_vectors() {
    assert_eq!(common::md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(common::md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(common::sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(common::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    assert_eq!(common::sha256_hex(long), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
}

/// Draft mode (reduced-resolution playback) skips deblocking of non-reference pictures only:
/// every other picture stays bit-exact with ffmpeg, single- and frame-threaded, the skipped ones
/// are flagged, and they are the only ones that differ.
#[test]
fn draft_mode_changes_only_flagged_non_reference_pictures() {
    let mut checked = 0;
    for name in ["main_cabac_b", "bpyramid", "cavlc_b", "direct_temporal", "weightb"] {
        let f = common::fixture(name);
        let Some((h264, yuv)) = common::ensure(f) else {
            eprintln!("skipped {name}");
            continue;
        };
        let reference = std::fs::read(&yuv).unwrap();
        let (w, h) = (f.width as usize, f.height as usize);
        let fsize = w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
        for threads in [1, 0] {
            let pics = common::decode_file_opts(&h264, threads, true).unwrap_or_else(|(au, e, _)| panic!("{name}: error at {au}: {e}"));
            common::check_pts(&pics).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(pics.len() * fsize, reference.len(), "{name}: picture count");
            let (mut draft, mut changed) = (0, 0);
            for (i, p) in pics.iter().enumerate() {
                let r = &reference[i * fsize..(i + 1) * fsize];
                let same = p.y.as_u8() == Some(&r[..w * h]) && [p.u.to_le_bytes(), p.v.to_le_bytes()].concat() == r[w * h..];
                if p.draft {
                    draft += 1;
                    changed += !same as usize;
                } else {
                    assert!(same, "{name} (threads {threads}): picture {i} (not draft) differs from the reference");
                }
            }
            assert!(draft > 0, "{name}: has non-reference pictures");
            if name != "no_deblock" {
                assert!(changed > 0, "{name}: skipping deblocking changes some draft picture");
            }
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
        let Some((h264, _)) = common::ensure(f) else { return };
        let data = std::fs::read(h264).unwrap();
        let mut dec = filmcraft_h264::Decoder::new();
        for au in common::split_access_units(&data) {
            dec.decode(au, 0).unwrap();
        }
        dec.flush();
        let s = dec.stats();
        println!(
            "{:<20} pics {:>3} cavlc/cabac {:>3}/{:<3} I/P/B {:>3}/{:>3}/{:>3} i4 {:>6} i8 {:>6} i16 {:>6} pcm {:>4} pskip {:>6} bskip {:>6} bdirect {:>5} inter {:>6} t8 {:>6} mmco {:>3} lt {} gaps {} wp {} tdirect {}",
            f.name,
            s.pictures,
            s.slices_cavlc,
            s.slices_cabac,
            s.slices_i,
            s.slices_p,
            s.slices_b,
            s.mb_i4x4,
            s.mb_i8x8,
            s.mb_i16x16,
            s.mb_pcm,
            s.mb_p_skip,
            s.mb_b_skip,
            s.mb_b_direct16x16,
            s.mb_inter,
            s.mb_inter_8x8_transform,
            s.mmco_ops,
            s.long_term_marks,
            s.frame_num_gaps,
            s.weighted_slices,
            s.temporal_direct_slices
        );
    }
}

/// Pre-generate every fixture and its reference decode (`cargo xtask fixtures`).
#[test]
#[ignore]
fn generate_fixtures() {
    let dir = common::fixtures_dir();
    for f in common::FIXTURES {
        let outs = [dir.join(format!("{}.h264", f.name)), dir.join(format!("{}.yuv", f.name))];
        filmcraft_testkit::fixtures::generate_and_report(&format!("h264/{}", f.name), &outs, || common::ensure(f));
    }
}

/// Concurrent consumers must publish one complete fixture without replacing an open file.
#[test]
fn concurrent_fixture_publication_is_safe() {
    if filmcraft_testkit::ffmpeg_or_skip("concurrent_fixture_publication").is_none() {
        return;
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let name = Box::leak(format!("concurrent_fixture_{}_{stamp}", std::process::id()).into_boxed_str());
    let fixture = common::Fixture {
        name,
        source: "testsrc2",
        width: 64,
        height: 48,
        frames: 2,
        filter: "",
        pix_fmt: "yuv420p",
        args: &["-preset", "ultrafast", "-x264-params", "keyint=1"],
    };
    let open_published = |path: &std::path::Path| {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Like the external decoder, allow reads but deny replacement while open.
            options.share_mode(1);
        }
        options.open(path).unwrap()
    };
    let start = std::sync::Barrier::new(4);
    let results = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    let paths = common::ensure(&fixture).expect("software fixture must generate");
                    // Keep both published files open until every producer has finished.
                    let files = (open_published(&paths.0), open_published(&paths.1));
                    (paths, files)
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().expect("fixture producer panicked")).collect::<Vec<_>>()
    });
    let paths = results[0].0.clone();
    for (got, _) in &results {
        assert_eq!(got, &paths);
    }
    assert!(!std::fs::read(&paths.0).unwrap().is_empty());
    assert_eq!(std::fs::metadata(&paths.1).unwrap().len(), 2 * (64 * 48 + 2 * 32 * 24));
    drop(results);
    std::fs::remove_file(paths.0).unwrap();
    std::fs::remove_file(paths.1).unwrap();
}
