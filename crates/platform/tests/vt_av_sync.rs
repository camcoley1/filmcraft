//! Audio / video sync of VideoToolbox exports (macOS) with and without B-frames: a one-frame white
//! flash on black and a one-frame tone burst start at the same sequence frame. After both tracks'
//! edit lists (the video's B-frame delay, AAC's priming) are applied, as ffmpeg (an external oracle
//! only) applies them, the flash and the burst start together within one frame, every video frame
//! is there (the last one at its time), and the audio lasts as long as the video up to the
//! container's rounding. H.264 and H.265, 30000/1001 and 25 fps, AAC in MP4 and PCM in QuickTime.
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

/// The audio as ffmpeg decodes it (edit list / priming applied), mono f32 at [`SR`].
fn audio_samples(ffmpeg: &Path, path: &str) -> Vec<f32> {
    let sr = SR.to_string();
    let out = Command::new(ffmpeg).args(["-v", "error", "-i", path, "-map", "0:a:0", "-ac", "1", "-ar", &sr, "-f", "f32le", "-"]).output().unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
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

                    // audio: the burst's onset, and the length
                    let a = audio_samples(&ffmpeg, &path);
                    let onset = a.iter().position(|v| v.abs() > 0.05).unwrap_or_else(|| panic!("{name}: no tone burst"));
                    let t_click = onset as f64 / f64::from(SR);
                    let dur_a = a.len() as f64 / f64::from(SR);
                    let dur_v = frames as f64 * frame_s;
                    // AAC ends on a whole 1024-sample frame unless the edit list trims it; PCM is exact
                    let slack = if audio == AudioCodec::Aac { 1024.0 / f64::from(SR) + 1e-3 } else { 1e-3 };
                    eprintln!(
                        "{name}: flash frame {flash} at {t_flash:.4} s, click at {t_click:.4} s (Δ {:+.2} ms), audio {dur_a:.4} s / video {dur_v:.4} s",
                        (t_click - t_flash) * 1000.0
                    );
                    assert_eq!(flash as i64, MARK, "{name}: the flash is on frame {flash}");
                    assert!((t_click - t_flash).abs() <= frame_s, "{name}: click {t_click} vs flash {t_flash}");
                    assert!((dur_a - dur_v).abs() <= slack, "{name}: audio {dur_a} s, video {dur_v} s");
                    ran += 1;
                }
            }
        }
    }
    eprintln!("{ran} exports checked");
}
