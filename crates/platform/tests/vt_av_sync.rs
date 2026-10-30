//! Audio / video sync of VideoToolbox exports (macOS) with and without B-frames: a one-frame white
//! flash on black and a one-frame tone burst start at the same sequence frame. After both tracks'
//! edit lists (the video's B-frame delay, AAC's priming) are applied, as ffmpeg (an external oracle
//! only) applies them, the flash and the burst start together within one frame, every video frame
//! is there (the last one at its time), and the audio lasts as long as the video up to the
//! container's rounding. H.264 and H.265, 30000/1001 and 25 fps, AAC in MP4 and PCM in QuickTime.
//!
//! The bounds are sample-level: the flash and the burst start within 1 ms of each other (the
//! burst's time taken from the decoded audio's own first timestamp, after the edit list), and the
//! AAC track's priming edit (1024 samples = 21.33 ms at 48 kHz, more than the bound) is checked to
//! be what moves the burst into place: decoding with the edit lists ignored puts it exactly the
//! priming late. B-frame exports are checked to really be reordered.
//!
//! Its own test binary: the encoder counters are process-wide. Skips (SKIPPED) without a
//! VideoToolbox encoder (`FILMCRAFT_REQUIRE_VT=1` makes that a failure) or without ffmpeg / ffprobe.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use filmcraft_export::{AudioCodec, BitrateMode, ExportSettings, Format, HardwareEncoding, Multiplexer, Progress, export, hw_encode_stats};
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{Generator, MediaSource};
use filmcraft_platform::hardware_encode::videotoolbox_encoder_factory;
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_render::SourceMap;
use filmcraft_time::{FrameRate, Tick, TimeRange};

const W: u32 = 640;
const H: u32 = 360;
/// The frame the flash and the tone burst start on.
const MARK: i64 = 17;
const SR: u32 = 48_000;

fn skip(why: &str) {
    assert!(std::env::var("FILMCRAFT_REQUIRE_VT").as_deref() != Ok("1"), "FILMCRAFT_REQUIRE_VT=1 but {why}");
    eprintln!("SKIPPED: {why}");
}

fn add(p: &mut Project, m: &mut SourceMap, g: Generator, rate: FrameRate, frames: i64) -> filmcraft_project::ItemId {
    let src = GeneratorSource::new(g, W, H, rate, rate.tick_of(frames));
    let info = src.info().clone();
    let id = p.add_item(
        &info.name.clone(),
        Label::Iris,
        ItemKind::Media(MediaClip {
            media: MediaRef::Generator(src.generator.clone()),
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
    m.0.insert(id, Arc::new(src));
    id
}

/// `frames` of black, with a one-frame white flash (V2) and a one-frame 1 kHz tone burst (A1) at
/// frame [`MARK`].
fn project(rate: FrameRate, frames: i64) -> (Arc<Project>, filmcraft_project::ItemId, SourceMap) {
    let mut p = Project::new("sync");
    let mut m = SourceMap::default();
    let black = add(&mut p, &mut m, Generator::BlackVideo, rate, frames);
    let white = add(&mut p, &mut m, Generator::ColorMatte { color: [1.0, 1.0, 1.0, 1.0] }, rate, frames);
    let tone = add(&mut p, &mut m, Generator::Tone { hz: 1000.0, db: -6.0 }, rate, frames);
    let seq = p.new_sequence("s", SequenceSettings { width: W, height: H, frame_rate: rate, sample_rate: SR, ..Default::default() }, 2, 1, None);
    let at = rate.tick_of(MARK);
    let one = TimeRange::new(Tick::ZERO, rate.tick_of(1));
    let v1 = p.make_track_item(black, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, rate.tick_of(frames)), rate).unwrap();
    let v2 = p.make_track_item(white, TrackKind::Video, at, one, rate).unwrap();
    let a1 = p.make_track_item(tone, TrackKind::Audio, at, one, rate).unwrap();
    let s = p.sequence_mut(seq).unwrap();
    s.video_tracks[0].items.push(v1);
    s.video_tracks[1].items.push(v2);
    s.audio_tracks[0].items.push(a1);
    (Arc::new(p), seq, m)
}

/// Presentation times (s) of the video frames, after the edit list, in presentation order.
fn video_times(ffprobe: &Path, path: &str) -> Vec<f64> {
    let out =
        Command::new(ffprobe).args(["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=pts_time", "-of", "csv=p=0", path]).output().unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    let mut t: Vec<f64> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
    t.sort_by(f64::total_cmp);
    t
}

/// Mean brightness (0..255) of every decoded video frame, in presentation order.
fn video_brightness(ffmpeg: &Path, path: &str) -> Vec<f64> {
    let out = Command::new(ffmpeg).args(["-v", "error", "-i", path, "-map", "0:v:0", "-vf", "scale=8:8,format=gray", "-f", "rawvideo", "-"]).output().unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks_exact(64).map(|f| f.iter().map(|b| f64::from(*b)).sum::<f64>() / 64.0).collect()
}

/// The audio as ffmpeg decodes it, mono f32 at [`SR`]: edit list / priming applied, or with
/// `ignore_edits` as the samples are stored.
fn audio_samples(ffmpeg: &Path, path: &str, ignore_edits: bool) -> Vec<f32> {
    let sr = SR.to_string();
    let edits = if ignore_edits { "1" } else { "0" };
    let out = Command::new(ffmpeg)
        .args(["-v", "error", "-ignore_editlist", edits, "-i", path, "-map", "0:a:0", "-ac", "1", "-ar", &sr, "-f", "f32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// Presentation time (s) of the first decoded audio frame, after the edit list: the origin of the
/// decoded samples.
fn audio_origin(ffprobe: &Path, path: &str) -> f64 {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "a:0", "-show_frames", "-show_entries", "frame=pts_time", "-of", "csv=p=0", path])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.trim().trim_end_matches(',').parse().ok()).expect("an audio frame time")
}

/// The first sample of the tone burst (index into decoded samples).
fn onset(a: &[f32]) -> Option<usize> {
    a.iter().position(|v| v.abs() > 0.05)
}

/// The MP4 / QuickTime tracks as written: the video's B-frame timing (edit list start, and how many
/// times a sample in decoding order is presented before the one decoded just before it: real
/// reordering) and the audio's edit list start (AAC priming), in their own timescales.
fn track_timing(path: &str) -> (i64, usize, i64, u32) {
    let bytes = std::fs::read(path).unwrap();
    let f = filmcraft_isobmff::open(&bytes[..]).unwrap();
    let edit = |t: &filmcraft_isobmff::Track| t.edits.first().map_or(0, |e| e.media_time);
    let v = &f.tracks[f.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap()];
    let a = &f.tracks[f.track_of_kind(filmcraft_isobmff::TrackKind::Audio).unwrap()];
    let start = edit(v);
    let reordered = v.samples.windows(2).filter(|w| w[1].pts < w[0].pts).count();
    (start, reordered, edit(a), a.timescale)
}

#[test]
fn flash_and_click_stay_in_sync() {
    filmcraft_platform::register();
    let (Some(ffprobe), Some(ffmpeg)) = (filmcraft_testkit::ffprobe_or_skip("vt a/v sync"), filmcraft_testkit::ffmpeg_or_skip("vt a/v sync")) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("fc-vt-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ran = 0;
    for format in [Format::H264, Format::Hevc] {
        for rate in [FrameRate::FPS_29_97, FrameRate::FPS_25] {
            let frames = (2.0 * rate.as_f64()).round() as i64;
            let (p, seq, m) = project(rate, frames);
            let frame_s = rate.den as f64 / rate.num as f64;
            for (audio, mux, ext) in [(AudioCodec::Aac, Multiplexer::Mp4, "mp4"), (AudioCodec::Pcm, Multiplexer::Mov, "mov")] {
                for b_frames in [false, true] {
                    let name = format!(
                        "{}-{}fps-{audio:?}-bf{}.{ext}",
                        format.label().replace([' ', '(', ')', '.'], ""),
                        rate.num / rate.den.max(1),
                        u8::from(b_frames)
                    );
                    let path = dir.join(&name).to_string_lossy().to_string();
                    let mut s = ExportSettings {
                        format,
                        path: path.clone(),
                        include_audio: true,
                        bitrate_kbps: 4_000,
                        bitrate_mode: BitrateMode::Vbr1Pass,
                        hardware_encoding: HardwareEncoding::Auto,
                        hardware_b_frames: b_frames,
                        multiplexer: mux,
                        ..Default::default()
                    };
                    s.audio.codec = audio;
                    if videotoolbox_encoder_factory(format, W, H, rate, &s).is_none_or(|r| r.is_err()) {
                        skip(&format!("no VideoToolbox {} encoder", format.label()));
                        continue;
                    }
                    let before = hw_encode_stats().sessions;
                    let report = export(&p, seq, &s, &m, &Progress::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
                    assert_eq!(report.frames, frames as u64, "{name}");
                    assert!(hw_encode_stats().sessions > before, "{name}: the export went through VideoToolbox");

                    // video: every frame, the last at its time, the flash at MARK
                    let times = video_times(&ffprobe, &path);
                    let light = video_brightness(&ffmpeg, &path);
                    assert_eq!((times.len(), light.len()), (frames as usize, frames as usize), "{name}: frames after the edit list");
                    assert!(times[0].abs() < 1e-3, "{name}: first frame at {}", times[0]);
                    let last = (frames - 1) as f64 * frame_s;
                    assert!((times[frames as usize - 1] - last).abs() < 1e-3, "{name}: last frame at {} (want {last})", times[frames as usize - 1]);
                    let flash = light.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap();
                    assert!(light[flash] > 200.0 && light.iter().filter(|v| **v > 128.0).count() == 1, "{name}: one white frame, {light:?}");
                    let t_flash = times[flash];

                    // the video really is reordered with B-frames (an edit list start and composition
                    // offsets that differ from it), and in decoding order without
                    let (v_start, reordered, a_start, a_scale) = track_timing(&path);
                    if b_frames {
                        assert!(v_start > 0, "{name}: no B-frame delay (edit start {v_start})");
                        assert!(reordered > 0, "{name}: no presentation-time inversion in decoding order");
                    } else {
                        assert_eq!(v_start, 0, "{name}: an edit start without B-frames");
                        assert_eq!(reordered, 0, "{name}: presentation times out of order without B-frames");
                    }

                    // audio: the burst's onset after the edit list, from the decoded audio's own origin
                    let a = audio_samples(&ffmpeg, &path, false);
                    let origin = audio_origin(&ffprobe, &path);
                    let at = onset(&a).unwrap_or_else(|| panic!("{name}: no tone burst"));
                    let t_click = origin + at as f64 / f64::from(SR);
                    let dur_a = a.len() as f64 / f64::from(SR);
                    // the priming: AAC's edit list skips its 1024 priming samples, and that is what puts
                    // the burst in place (stored samples: exactly the priming late); PCM has none
                    let priming = if audio == AudioCodec::Aac { 1024 } else { 0 };
                    assert_eq!((a_start, a_scale), (priming, SR), "{name}: audio edit list start");
                    let raw = audio_samples(&ffmpeg, &path, true);
                    let raw_at = onset(&raw).unwrap_or_else(|| panic!("{name}: no tone burst in the stored samples"));
                    let shift = (raw_at as f64 - at as f64) / f64::from(SR);
                    let want_shift = f64::from(priming as u32) / f64::from(SR);
                    assert!(
                        (shift - want_shift).abs() <= 1e-3,
                        "{name}: the stored burst is {:.2} ms late, want the priming {:.2} ms",
                        shift * 1e3,
                        want_shift * 1e3
                    );
                    let dur_v = frames as f64 * frame_s;
                    // AAC ends on a whole 1024-sample frame unless the edit list trims it; PCM is exact
                    let slack = if audio == AudioCodec::Aac { 1024.0 / f64::from(SR) + 1e-3 } else { 1e-3 };
                    eprintln!(
                        "{name}: flash frame {flash} at {t_flash:.5} s, click at {t_click:.5} s (Δ {:+.3} ms; audio origin {origin}, priming {priming}, stored burst {:+.2} ms later), video edit {v_start} / {reordered} reordered, audio {dur_a:.4} s / video {dur_v:.4} s",
                        (t_click - t_flash) * 1000.0,
                        shift * 1e3
                    );
                    assert_eq!(flash as i64, MARK, "{name}: the flash is on frame {flash}");
                    // sample level: well under the 21.33 ms a missing priming correction would leave
                    assert!((t_click - t_flash).abs() <= 1e-3, "{name}: click {t_click} vs flash {t_flash}");
                    assert!((dur_a - dur_v).abs() <= slack, "{name}: audio {dur_a} s, video {dur_v} s");
                    ran += 1;
                }
            }
        }
    }
    eprintln!("{ran} exports checked");
}
