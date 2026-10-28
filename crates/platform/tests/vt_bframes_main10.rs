//! VideoToolbox B-frames and HEVC Main 10 (macOS) through the real export pipeline.
//!
//! - B-frames (`ExportSettings::hardware_b_frames`), H.264 and H.265: the MP4 has monotonic decode
//!   times, non-negative composition offsets (`ctts`) and an edit list that starts the presentation
//!   at the first picture (time 0); every picture is presented once, in order; our importer decodes
//!   the same pictures as the export without B-frames; ffprobe (an external oracle only) reports
//!   `has_b_frames` > 0, monotonic packet decode times, a first presentation time of 0 and the
//!   right duration, and ffmpeg decodes it without errors.
//! - Main 10: an HDR (Rec. 2100 PQ / HLG) sequence exports as `hvc1` Main 10, yuv420p10le, BT.2020
//!   with the right transfer, limited range, HDR10 static metadata for PQ; our importer reads it
//!   back as 10-bit PQ / HLG close to a ProRes HDR export of the same sequence, and ffmpeg decodes
//!   the same 10-bit pictures as our decoder. SDR stays Main 8-bit BT.709.
//!
//! Its own test binary: the encoder registry and the counters are process-wide. Tests skip
//! (SKIPPED) where VideoToolbox has no encoder for what they need, and the ffprobe / ffmpeg checks
//! where those tools are missing.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use filmcraft_color::{ColorPipeline, WorkingSpace};
use filmcraft_export::{BitrateMode, ColorSignal, EncoderFrame, ExportSettings, Format, HardwareEncoding, Progress, export, hdr_available, hw_encode_stats};
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{DemoScene, FrameRequest, Generator, MediaSource};
use filmcraft_platform::hardware_encode::{config_for, hevc_available, hevc_main10_available, videotoolbox_encoder_factory};
use filmcraft_platform::videotoolbox_encode::{VtColor, VtProfile};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_render::SourceMap;
use filmcraft_time::{FrameRate, Tick, TimeRange};

/// The encoder counters are process-wide: one test at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const W: u32 = 640;
const H: u32 = 360;
const FRAMES: i64 = 45;
const RATE: FrameRate = FrameRate::FPS_29_97;

fn project(working: WorkingSpace) -> (Arc<Project>, filmcraft_project::ItemId, SourceMap) {
    let mut p = Project::new("x");
    let g = GeneratorSource::new(Generator::Demo(DemoScene::OceanSunset), W, H, RATE, RATE.tick_of(FRAMES));
    let info = g.info().clone();
    let clip = p.add_item(
        &info.name.clone(),
        Label::Iris,
        ItemKind::Media(MediaClip {
            media: MediaRef::Generator(g.generator.clone()),
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        }),
        None,
    );
    let color = ColorPipeline { working, ..ColorPipeline::REC709 };
    let seq = p.new_sequence("s", SequenceSettings { width: W, height: H, frame_rate: RATE, color, ..Default::default() }, 1, 1, None);
    let v = p.make_track_item(clip, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, RATE.tick_of(FRAMES)), RATE).unwrap();
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(v);
    let mut m = SourceMap::default();
    m.0.insert(clip, Arc::new(g));
    (Arc::new(p), seq, m)
}

fn tmp(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("fc-vt-m8a-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name).to_string_lossy().to_string()
}

fn settings(format: Format, path: String, b_frames: bool) -> ExportSettings {
    ExportSettings {
        format,
        path,
        include_audio: false,
        bitrate_kbps: 8_000,
        bitrate_mode: BitrateMode::Vbr1Pass,
        keyframe_distance: Some(30),
        hardware_encoding: HardwareEncoding::Auto,
        hardware_b_frames: b_frames,
        ..Default::default()
    }
}

/// Whether VideoToolbox has a hardware encoder for these settings (else the test skips).
fn hardware_takes(s: &ExportSettings) -> bool {
    let probe = ExportSettings { signal: if s.sdr { ColorSignal::default() } else { s.signal }, ..s.clone() };
    match videotoolbox_encoder_factory(s.format, W, H, RATE, &probe) {
        Some(Ok(_)) => true,
        Some(Err(e)) => panic!("factory error: {e}"),
        None => {
            eprintln!("SKIPPED: no VideoToolbox hardware {} encoder (b_frames {})", s.format.label(), s.hardware_b_frames);
            false
        }
    }
}

fn delta(before: filmcraft_export::HwEncodeStats) -> (u64, u64, u64) {
    let after = hw_encode_stats();
    (after.frames - before.frames, after.sessions - before.sessions, after.declined - before.declined)
}

fn decode(path: &str) -> Vec<Arc<filmcraft_frame::VideoFrame>> {
    let bytes: Arc<[u8]> = std::fs::read(path).unwrap().into();
    let src = filmcraft_codecs::open_bytes(path, bytes).unwrap();
    let rate = src.info().frame_rate();
    let n = rate.frame_at(src.info().duration);
    (0..n).map(|i| src.video_frame(FrameRequest::full(rate.tick_of(i))).unwrap()).collect()
}

fn luma8(f: &filmcraft_frame::VideoFrame) -> Arc<Vec<u8>> {
    match &f.data {
        filmcraft_frame::PixelData::Yuv8 { planes, .. } => planes[0].clone(),
        _ => panic!("8-bit planes expected, got {}", f.format_label()),
    }
}

fn yuv16(f: &filmcraft_frame::VideoFrame) -> ([Arc<Vec<u16>>; 3], u32) {
    match &f.data {
        filmcraft_frame::PixelData::Yuv16 { planes, bits, .. } => (planes.clone(), *bits),
        _ => panic!("16-bit planes expected, got {}", f.format_label()),
    }
}

fn psnr8(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

fn psnr10(a: &[u16], b: &[u16]) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (1023.0f64 * 1023.0 / mse).log10() }
}

/// The first video stream's fields as ffprobe reports them.
fn probe_stream(ffprobe: &Path, path: &str) -> HashMap<String, String> {
    let entries = "stream=codec_name,profile,codec_tag_string,pix_fmt,has_b_frames,color_space,color_transfer,color_primaries,color_range,width,height,nb_frames,start_time,duration";
    let out = Command::new(ffprobe).args(["-v", "error", "-select_streams", "v:0", "-show_entries", entries, "-of", "default=nw=1", path]).output().unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// (pts, dts, key) of every video packet, in file (decoding) order, in the stream's time base.
fn probe_packets(ffprobe: &Path, path: &str) -> Vec<(i64, i64, bool)> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts,dts,flags", "-of", "csv=p=0", path])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (f[0].parse().unwrap(), f[1].parse().unwrap(), f[2].starts_with('K'))
        })
        .collect()
}

fn first_frame_side_data(ffprobe: &Path, path: &str) -> String {
    let args = [
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-read_intervals",
        "%+#1",
        "-show_frames",
        "-show_entries",
        "frame=side_data_list",
        "-of",
        "default=nw=1",
        path,
    ];
    String::from_utf8_lossy(&Command::new(ffprobe).args(args).output().unwrap().stdout).to_string()
}

fn assert_ffmpeg_decodes(ffmpeg: &Path, path: &str) {
    let out = Command::new(ffmpeg).args(["-v", "error", "-xerror", "-i", path, "-f", "null", "-"]).output().unwrap();
    assert!(out.status.success() && out.stderr.is_empty(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
}

/// The first picture as ffmpeg decodes it to yuv420p10le: (Y, Cb, Cr) code values.
fn ffmpeg_first_picture(ffmpeg: &Path, path: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let (w, h) = (W as usize, H as usize);
    let out = Command::new(ffmpeg).args(["-v", "error", "-i", path, "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p10le", "-"]).output().unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    let samples: Vec<u16> = out.stdout.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
    assert_eq!(samples.len(), w * h * 3 / 2, "one yuv420p10le picture");
    let (y, c) = samples.split_at(w * h);
    let (u, v) = c.split_at(c.len() / 2);
    (y.to_vec(), u.to_vec(), v.to_vec())
}

/// The MP4's video track timing, checked against what a reordered stream must look like: decode
/// times one frame apart from 0, presentation never before decoding, an edit list starting at the
/// first picture and every picture presented once at `k × 1001`. Returns the edit's media time.
fn check_track_timing(name: &str, bytes: &[u8], b_frames: bool) -> i64 {
    let file = filmcraft_isobmff::open(bytes).unwrap();
    let vt = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).expect("a video track");
    let t = &file.tracks[vt];
    assert_eq!(t.timescale, 30000, "{name}");
    assert_eq!(t.samples.len(), FRAMES as usize, "{name}");
    let dur = 1001i64;
    for (n, s) in t.samples.iter().enumerate() {
        assert_eq!(s.dts, n as i64 * dur, "{name}: sample {n} decode time");
        assert!(s.pts >= s.dts, "{name}: sample {n} presented at {} before it is decoded at {}", s.pts, s.dts);
    }
    assert!(t.samples[0].is_sync, "{name}: the first sample is a sync sample");
    let start = match t.edits.as_slice() {
        [] => 0,
        [e] => {
            assert_eq!(e.media_rate, 0x10000, "{name}");
            e.media_time
        }
        more => panic!("{name}: {} edits", more.len()),
    };
    let mut presented: Vec<i64> = t.samples.iter().map(|s| s.pts - start).collect();
    presented.sort_unstable();
    assert_eq!(presented, (0..FRAMES).map(|k| k * dur).collect::<Vec<_>>(), "{name}: every picture once, the first at time 0");
    // pictures whose composition offset is not the stream's delay are reordered
    let reordered = t.samples.iter().filter(|s| s.pts - s.dts != start).count();
    if b_frames {
        assert!(start > 0 && start % dur == 0, "{name}: edit list media time {start}");
        assert!(reordered > 0, "{name}: no picture was reordered");
        // a sync sample is presented at its decode position: nothing before it in decoding order is shown after it
        for (n, s) in t.samples.iter().enumerate().filter(|(_, s)| s.is_sync) {
            assert!(t.samples[..n].iter().all(|p| p.pts < s.pts), "{name}: sync sample {n} is not a clean entry point");
        }
    } else {
        assert_eq!((start, t.edits.len()), (0, 0), "{name}: no edit list without B-frames");
        assert!(t.samples.iter().all(|s| s.pts == s.dts), "{name}: no composition offsets without B-frames");
    }
    start
}

#[test]
fn b_frames_export_with_ctts_and_an_edit_list() {
    let _one = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    filmcraft_platform::register();
    let ffprobe = filmcraft_testkit::ffprobe_or_skip("vt b-frames");
    let ffmpeg = filmcraft_testkit::ffmpeg_or_skip("vt b-frames");
    let (p, seq, m) = project(ColorPipeline::REC709.working);
    for format in [Format::H264, Format::Hevc] {
        if !hardware_takes(&settings(format, String::new(), true)) {
            continue;
        }
        let name = format.label().to_string();
        let ext = if format == Format::Hevc { "hevc" } else { "h264" };
        let mut decoded = Vec::new();
        for b_frames in [false, true] {
            let path = tmp(&format!("{ext}-bf{}.mp4", u8::from(b_frames)));
            let before = hw_encode_stats();
            let report = export(&p, seq, &settings(format, path.clone(), b_frames), &m, &Progress::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(report.frames, FRAMES as u64, "{name}");
            assert_eq!(delta(before), (FRAMES as u64, 1, 0), "{name} b_frames {b_frames}: every picture through one hardware session");
            let bytes = std::fs::read(&path).unwrap();
            let start = check_track_timing(&format!("{name} b_frames {b_frames}"), &bytes, b_frames);
            eprintln!("{name} b_frames {b_frames}: {} bytes, edit list media time {start}", report.bytes);

            let pictures = decode(&path);
            assert_eq!(pictures.len(), FRAMES as usize, "{name}: pictures decoded by our importer");
            decoded.push(pictures);

            let Some(ffprobe) = &ffprobe else { continue };
            let f = probe_stream(ffprobe, &path);
            let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("?");
            eprintln!("{name} b_frames {b_frames}: ffprobe {f:?}");
            let has_b: u32 = get("has_b_frames").parse().unwrap_or(0);
            assert_eq!(has_b > 0, b_frames, "{name}: has_b_frames {has_b}");
            assert_eq!((get("pix_fmt"), get("nb_frames")), ("yuv420p", "45"), "{name}");
            assert_eq!(get("start_time").parse::<f64>().unwrap(), 0.0, "{name}: first presentation time");
            let duration: f64 = get("duration").parse().unwrap();
            assert!((duration - FRAMES as f64 * 1001.0 / 30000.0).abs() < 1e-3, "{name}: duration {duration}");
            let packets = probe_packets(ffprobe, &path);
            assert_eq!(packets.len(), FRAMES as usize, "{name}");
            assert!(packets.windows(2).all(|w| w[0].1 < w[1].1), "{name}: packet decode times not monotonic: {packets:?}");
            assert!(packets.iter().all(|(pts, dts, _)| pts >= dts), "{name}: a packet presented before its decode time");
            let mut pts: Vec<i64> = packets.iter().map(|p| p.0).collect();
            let in_file_order = pts.windows(2).all(|w| w[0] < w[1]);
            assert_eq!(!in_file_order, b_frames, "{name}: packets in presentation order iff there are no B-frames");
            pts.sort_unstable();
            assert_eq!(pts.first(), Some(&0), "{name}: first presentation time");
            assert!(pts.windows(2).all(|w| w[1] - w[0] == pts[1] - pts[0]), "{name}: presentation times one frame apart");
            if b_frames {
                eprintln!("{name}: ffprobe packets (pts,dts,key) {:?}", &packets[..12]);
            }
            if let Some(ffmpeg) = &ffmpeg {
                assert_ffmpeg_decodes(ffmpeg, &path);
            }
        }
        // same pictures in the same order with and without B-frames (a moving scene: a picture out of
        // order would be far off)
        let (off, on) = (&decoded[0], &decoded[1]);
        let worst = off.iter().zip(on).map(|(a, b)| psnr8(&luma8(a), &luma8(b))).fold(99.0f64, f64::min);
        let shifted = psnr8(&luma8(&off[10]), &luma8(&on[11]));
        eprintln!("{name}: B-frames against none, worst luma PSNR {worst:.1} dB (neighbouring pictures {shifted:.1} dB)");
        assert!(worst > 32.0 && worst > shifted, "{name}: {worst:.1} dB, neighbouring pictures {shifted:.1} dB");
    }
}

#[test]
fn hdr_sequences_export_as_main_10() {
    let _one = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    filmcraft_platform::register();
    assert_eq!(hdr_available(Format::Hevc), hevc_main10_available(), "HDR H.265 is available iff VideoToolbox can encode Main 10");
    if !hevc_main10_available() {
        eprintln!("SKIPPED: no VideoToolbox HEVC Main 10 here (hevc: {})", hevc_available());
        return;
    }
    let ffprobe = filmcraft_testkit::ffprobe_or_skip("vt main 10");
    let ffmpeg = filmcraft_testkit::ffmpeg_or_skip("vt main 10");
    for (ws, ext, ff_transfer, pq, b_frames) in [
        (WorkingSpace::Rec2100Pq, "mp4", "smpte2084", true, false),
        (WorkingSpace::Rec2100Hlg, "mp4", "arib-std-b67", false, false),
        (WorkingSpace::Rec2100Pq, "mov", "smpte2084", true, true),
    ] {
        let name = format!("{}-bf{}.{ext}", ws.id(), u8::from(b_frames));
        let (p, seq, m) = project(ws);

        // the ProRes HDR export of the same sequence is the reference for the pixels (10-bit BT.2020, same matrix and range)
        let ref_path = tmp(&format!("ref-{}.mov", ws.id()));
        export(
            &p,
            seq,
            &ExportSettings { format: Format::ProRes, path: ref_path.clone(), include_audio: false, ..Default::default() },
            &m,
            &Progress::default(),
        )
        .unwrap();
        let reference = decode(&ref_path);

        let path = tmp(&name);
        let before = hw_encode_stats();
        let report = export(&p, seq, &settings(Format::Hevc, path.clone(), b_frames), &m, &Progress::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(report.frames, FRAMES as u64, "{name}");
        assert_eq!(delta(before), (FRAMES as u64, 1, 0), "{name}: every picture through one hardware session, nothing declined");
        let bytes: Arc<[u8]> = std::fs::read(&path).unwrap().into();
        check_track_timing(&name, &bytes, b_frames);

        // our importer: PQ / HLG, BT.2020, 10-bit planes
        let src = filmcraft_codecs::open_bytes(&path, bytes.clone()).unwrap();
        let v = src.info().video.clone().unwrap();
        let want_transfer = if pq { filmcraft_color::Transfer::Pq } else { filmcraft_color::Transfer::Hlg };
        assert_eq!(
            (v.color.transfer, v.color.primaries, v.color.matrix),
            (want_transfer, filmcraft_color::Primaries::Bt2020, filmcraft_color::Matrix::Bt2020Ncl),
            "{name}"
        );
        if pq {
            let hdr = v.hdr.unwrap_or_else(|| panic!("{name}: no HDR metadata"));
            assert_eq!((hdr.mastering_max_nits, hdr.peak_nits()), (Some(1000.0), Some(1000.0)), "{name}");
        }
        let file = filmcraft_isobmff::open(&bytes[..]).unwrap();
        let vt = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).expect("a video track");
        let ve = file.tracks[vt].entries[0].video.clone().expect("a video sample entry");
        assert!(matches!(ve.color, Some(filmcraft_isobmff::ColorInfo::Nclx { primaries: 9, matrix: 9, full_range: false, .. })), "{name}: {:?}", ve.color);
        assert_eq!(ve.mastering_display.is_some() && ve.content_light == Some((0, 0)), pq, "{name}: mdcv / clli");

        let pictures = decode(&path);
        assert_eq!(pictures.len(), FRAMES as usize, "{name}");
        let (mut worst, mut best_levels, mut peak, mut ref_peak) = (99.0f64, usize::MAX, 0u16, 0u16);
        for (a, b) in pictures.iter().zip(&reference) {
            let (pa, bits) = yuv16(a);
            let (pb, _) = yuv16(b);
            assert_eq!(bits, 10, "{name}: decoded bit depth");
            assert!(pa[0].iter().all(|c| *c < 1024));
            worst = worst.min(psnr10(&pa[0], &pb[0]));
            peak = peak.max(pa[0].iter().copied().max().unwrap_or(0));
            ref_peak = ref_peak.max(pb[0].iter().copied().max().unwrap_or(0));
            let mut levels: Vec<u16> = pa[0].to_vec();
            levels.sort_unstable();
            levels.dedup();
            best_levels = best_levels.min(levels.len());
        }
        eprintln!(
            "{name}: {} bytes, worst luma PSNR against the ProRes HDR export {worst:.1} dB, {best_levels} distinct luma levels per picture (min), peak luma code {peak} (ProRes {ref_peak})",
            report.bytes
        );
        assert!(worst > 35.0, "{name}: {worst:.1} dB against the ProRes HDR export");
        assert!(best_levels > 256, "{name}: {best_levels} distinct luma levels: not 10-bit");
        assert!(peak.abs_diff(ref_peak) < 24, "{name}: peak {peak} against {ref_peak}");

        let (Some(ffprobe), Some(ffmpeg)) = (&ffprobe, &ffmpeg) else { continue };
        let f = probe_stream(ffprobe, &path);
        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("?");
        eprintln!("{name}: ffprobe {f:?}");
        assert_eq!((get("codec_name"), get("profile"), get("codec_tag_string"), get("pix_fmt")), ("hevc", "Main 10", "hvc1", "yuv420p10le"), "{name}: {f:?}");
        assert_eq!(
            (get("color_primaries"), get("color_transfer"), get("color_space"), get("color_range")),
            ("bt2020", ff_transfer, "bt2020nc", "tv"),
            "{name}: {f:?}"
        );
        assert_eq!(get("has_b_frames").parse::<u32>().unwrap_or(0) > 0, b_frames, "{name}: {f:?}");
        assert_eq!(get("start_time").parse::<f64>().unwrap(), 0.0, "{name}");
        let side = first_frame_side_data(ffprobe, &path);
        if pq {
            assert!(side.contains("Mastering display metadata") && side.contains("max_luminance=10000000/10000"), "{name}: {side}");
            assert!(side.contains("Content light level metadata"), "{name}: {side}");
        } else {
            assert!(!side.contains("Mastering display") && !side.contains("Content light level"), "{name}: {side}");
        }
        assert_ffmpeg_decodes(ffmpeg, &path);
        // ffmpeg's yuv420p10le picture is our decoder's picture
        let (fy, fu, fv) = ffmpeg_first_picture(ffmpeg, &path);
        let (ours, _) = yuv16(&pictures[0]);
        let diff = |a: &[u16], b: &[u16]| a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        assert_eq!(
            (diff(&fy, &ours[0]), diff(&fu, &ours[1]), diff(&fv, &ours[2])),
            (0, 0, 0),
            "{name}: our HEVC decoder and ffmpeg disagree on the 10-bit pictures"
        );
    }
}

#[test]
fn sdr_stays_main_8_bit() {
    let _one = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    filmcraft_platform::register();
    if !hevc_available() {
        eprintln!("SKIPPED: no VideoToolbox HEVC here");
        return;
    }
    let ffprobe = filmcraft_testkit::ffprobe_or_skip("vt main 10");
    // an HDR sequence with SDR asked for, and a plain SDR sequence: Main, 8-bit, BT.709, no HDR boxes
    for (what, ws, sdr) in [("PQ sequence, settings.sdr", WorkingSpace::Rec2100Pq, true), ("SDR sequence", ColorPipeline::REC709.working, false)] {
        let (p, seq, m) = project(ws);
        let path = tmp(&format!("sdr-{}.mp4", what.replace([' ', ','], "-")));
        let s = ExportSettings { sdr, ..settings(Format::Hevc, path.clone(), false) };
        export(&p, seq, &s, &m, &Progress::default()).unwrap_or_else(|e| panic!("{what}: {e}"));
        let pictures = decode(&path);
        assert!(matches!(&pictures[0].data, filmcraft_frame::PixelData::Yuv8 { .. }), "{what}: 8-bit planes");
        assert_eq!(pictures[0].color.transfer, filmcraft_color::Transfer::Bt709, "{what}");
        let bytes = std::fs::read(&path).unwrap();
        let file = filmcraft_isobmff::open(&bytes[..]).unwrap();
        let vt = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap();
        let ve = file.tracks[vt].entries[0].video.clone().unwrap();
        assert!(ve.mastering_display.is_none() && ve.content_light.is_none(), "{what}: no HDR boxes");
        if let Some(ffprobe) = &ffprobe {
            let f = probe_stream(ffprobe, &path);
            let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("?");
            assert_eq!((get("profile"), get("pix_fmt"), get("color_transfer")), ("Main", "yuv420p", "bt709"), "{what}: {f:?}");
        }
    }
}

#[test]
fn configuration_follows_the_signal_and_the_b_frame_setting() {
    let base = settings(Format::Hevc, String::new(), false);
    let c = config_for(W, H, RATE, &base).unwrap();
    assert_eq!((c.profile, &c.color, c.b_frames), (VtProfile::HevcMain, &VtColor::Bt709, false));
    let c = config_for(W, H, RATE, &ExportSettings { signal: ColorSignal::PQ, hardware_b_frames: true, ..base.clone() }).unwrap();
    assert_eq!((c.profile, c.b_frames), (VtProfile::HevcMain10, true));
    let VtColor::Bt2020Pq { mastering_display, content_light } = &c.color else { panic!("{:?}", c.color) };
    let md = ColorSignal::PQ.static_metadata().unwrap().0.to_bytes();
    assert_eq!((mastering_display, content_light.as_slice()), (&md, &[0u8, 0, 0, 0][..]), "the bytes of the mdcv / clli boxes");
    let c = config_for(W, H, RATE, &ExportSettings { signal: ColorSignal::HLG, ..base.clone() }).unwrap();
    assert_eq!((c.profile, &c.color), (VtProfile::HevcMain10, &VtColor::Bt2020Hlg));
    // H.264 here is 8-bit: HDR stays with the built-in encoder; B-frames follow the setting
    let h264 = settings(Format::H264, String::new(), true);
    assert!(config_for(W, H, RATE, &ExportSettings { signal: ColorSignal::PQ, ..h264.clone() }).is_err());
    assert!(config_for(W, H, RATE, &h264).unwrap().b_frames);
    // B-frames are off unless asked for
    assert!(!ExportSettings::default().hardware_b_frames);
}

fn frame<'a>(rgba: &'a [u8], hdr: Option<&'a [f32]>, index: u64) -> EncoderFrame<'a> {
    EncoderFrame { width: W, height: H, rgba, hdr, index }
}

#[test]
fn main_10_rejects_mismatched_pictures_and_clamps_hostile_floats() {
    let _one = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    filmcraft_platform::register();
    if !hevc_main10_available() {
        eprintln!("SKIPPED: no VideoToolbox HEVC Main 10 here");
        return;
    }
    let rgba = vec![128u8; (W * H * 4) as usize];
    let base = settings(Format::Hevc, tmp("x.mp4"), true);
    let Some(Ok(mut ten)) = videotoolbox_encoder_factory(Format::Hevc, W, H, RATE, &ExportSettings { signal: ColorSignal::PQ, ..base.clone() }) else {
        panic!("a Main 10 encoder")
    };
    let Some(Ok(mut eight)) = videotoolbox_encoder_factory(Format::Hevc, W, H, RATE, &base) else { panic!("a Main encoder") };
    let good = vec![0.5f32; (W * H * 3) as usize];
    // the HDR encoder wants the HDR picture, the SDR encoder the RGBA one
    assert!(ten.encode(&frame(&rgba, None, 0)).is_err());
    assert!(eight.encode(&frame(&rgba, Some(&good), 0)).is_err());
    // wrong-length floats (short, long, empty) and a wrong picture size: errors, nothing submitted
    for bad in [&good[..good.len() - 1], &[][..], &vec![0.5f32; good.len() + 3][..]] {
        assert!(ten.encode(&frame(&rgba, Some(bad), 0)).is_err(), "{} floats", bad.len());
    }
    assert!(ten.encode(&EncoderFrame { width: W + 2, height: H, rgba: &rgba, hdr: Some(&good), index: 0 }).is_err());
    // NaN, infinities and out-of-range values clamp: the pictures encode
    let mut hostile = vec![0.25f32; good.len()];
    for (i, x) in hostile.iter_mut().enumerate() {
        *x = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 7.0, 0.5][i % 6];
    }
    let mut packets = Vec::new();
    for i in 0..8 {
        packets.extend(ten.encode(&frame(&rgba, Some(&hostile), i)).unwrap());
    }
    packets.extend(ten.flush().unwrap());
    assert_eq!(packets.len(), 8);
    assert!(packets.iter().all(|p| p.composition_offset >= 0 && p.duration == 1001));
    // the sample entry of the HDR encoder carries colr + mdcv + clli, the SDR one nothing HDR
    let e = ten.sample_entry();
    let v = e.video.as_ref().unwrap();
    assert!(v.color.is_some() && v.mastering_display.is_some() && v.content_light == Some((0, 0)));
    assert!(eight.sample_entry().video.as_ref().unwrap().color.is_none());
    // the edit list starts at the first picture, one reorder depth into the media
    assert!(ten.media_start().is_some_and(|s| s > 0 && s % 1001 == 0), "{:?}", ten.media_start());
    assert_eq!(eight.media_start(), None, "nothing encoded yet");
}
