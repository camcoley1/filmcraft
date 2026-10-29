//! M8e: 10-bit 4:2:2 HEVC (Main 4:2:2 10) and H.264 (High 4:2:2) MOVs import and render with
//! hardware decoding switched off (the pure-Rust decoders): the decoded planes equal ffmpeg's
//! decode, and a frame exported from the Program Monitor is within rounding of ffmpeg's RGB
//! conversion of the same picture (bilinear chroma upsampling; ffmpeg-made fixtures, skipped
//! without ffmpeg / the encoder).

use serde_json::json;

use crate::Session;
use filmcraft_project::ItemId;

const W: usize = 320;
const H: usize = 180;
/// A smooth picture (blurred test pattern): chroma upsampling choices stay within rounding.
const SOURCE: &str = "testsrc2=size=320x180:rate=25:duration=1,gblur=sigma=3";

/// The tests switch the process-wide hardware decoding setting off and back on: one at a time.
static HW_SWITCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ffmpeg_run(ffmpeg: &std::path::Path, args: &[&str], out: &std::path::Path) -> bool {
    std::process::Command::new(ffmpeg).args(["-y", "-loglevel", "error"]).args(args).arg(out).status().is_ok_and(|s| s.success())
}

#[test]
fn hevc_422_10_imports_and_renders_in_software() {
    check_422_10("main422_10", &["-c:v", "libx265", "-x265-params", "log-level=error:bframes=3"], "HEVC");
}

#[test]
fn h264_high422_10_imports_and_renders_in_software() {
    check_422_10("high422_10", &["-c:v", "libx264", "-profile:v", "high422", "-bf", "3"], "H.264");
}

/// Encode the 4:2:2 10-bit fixture `name` with the encoder arguments `codec`, import it with
/// hardware decoding off, and compare decoded planes and a rendered frame with ffmpeg.
fn check_422_10(name: &str, codec: &[&str], codec_name: &str) {
    let Some(ffmpeg) = filmcraft_testkit::oracle::ffmpeg_or_skip("4:2:2 import") else { return };
    let dir = filmcraft_testkit::fixtures_dir("engine/422");
    let mov = dir.join(format!("{name}.mov"));
    let mut encode = vec!["-f", "lavfi", "-i", SOURCE];
    encode.extend_from_slice(codec);
    encode.extend_from_slice(&["-pix_fmt", "yuv422p10le", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]);
    if codec_name == "HEVC" {
        encode.extend_from_slice(&["-tag:v", "hvc1"]);
    }
    let Some(mov) = filmcraft_testkit::fixtures::generate(&mov, |tmp| ffmpeg_run(&ffmpeg, &encode, tmp)) else {
        eprintln!("SKIPPED ({name}): ffmpeg without the encoder");
        return;
    };
    let yuv = filmcraft_testkit::fixtures::generate(&dir.join(format!("{name}.yuv")), |tmp| {
        ffmpeg_run(&ffmpeg, &["-i", &mov.to_string_lossy(), "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuv422p10le"], tmp)
    })
    .unwrap();
    let rgb = filmcraft_testkit::fixtures::generate(&dir.join(format!("{name}_frame10_bilinear.rgb")), |tmp| {
        ffmpeg_run(
            &ffmpeg,
            &[
                "-i",
                &mov.to_string_lossy(),
                "-vf",
                "select=eq(n\\,10),scale=in_color_matrix=bt709:in_range=tv:out_range=full:flags=bilinear+accurate_rnd,format=rgb24",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
            ],
            tmp,
        )
    })
    .unwrap();
    let _switch = HW_SWITCH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut s = Session::default();
    s.execute("prefs.set", json!({"key": "playback.hardwareDecoding", "value": "off"})).unwrap();
    assert!(!filmcraft_codecs::hw::hardware_decoding());
    let r = s.execute("file.import", json!({"paths": [mov.to_string_lossy()]})).unwrap();
    assert!(r["errors"].as_array().is_none_or(|e| e.is_empty()), "{r}");
    let item = ItemId(r["items"][0].as_u64().unwrap());
    let v = s.project.item(item).unwrap().as_media().unwrap().info.video.clone().unwrap();
    assert_eq!((v.width as usize, v.height as usize), (W, H), "{name}");
    assert!(v.codec.starts_with(codec_name), "{name}: codec {}", v.codec);

    // decoded planes: bit-exact with ffmpeg
    let reference = std::fs::read(&yuv).unwrap();
    let fsize = (W * H + 2 * (W / 2) * H) * 2;
    assert_eq!(reference.len(), 25 * fsize);
    let src = s.source(item).unwrap();
    for i in [0usize, 7, 10, 24] {
        let f = src.video_frame(filmcraft_media::FrameRequest::full(v.frame_rate.tick_of(i as i64))).unwrap();
        let filmcraft_frame::PixelData::Yuv16 { planes, chroma, bits, .. } = &f.data else { panic!("frame {i}: not 16-bit YUV") };
        assert_eq!((*chroma, *bits), (filmcraft_frame::Chroma::C422, 10), "frame {i}");
        let want: Vec<u16> = reference[i * fsize..(i + 1) * fsize].chunks(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        let got: Vec<u16> = planes.iter().flat_map(|p| p.iter().copied()).collect();
        assert!(got == want, "{name} frame {i}: decoded planes differ from ffmpeg's");
    }

    // a Program Monitor frame through the whole pipeline (sequence, render, PNG export)
    s.execute("file.newSequenceFromClip", json!({"items": [item.0]})).unwrap();
    s.set_playhead(v.frame_rate.tick_of(10));
    let png = std::env::temp_dir().join(format!("fc-{name}-{}.png", std::process::id()));
    s.execute("file.exportFrame", json!({"target": "program", "path": png.to_string_lossy()})).unwrap();
    let image = image::load_from_memory(&std::fs::read(&png).unwrap()).unwrap().to_rgb8();
    let _ = std::fs::remove_file(&png);
    s.execute("prefs.set", json!({"key": "playback.hardwareDecoding", "value": "auto"})).unwrap();
    assert_eq!(image.dimensions(), (W as u32, H as u32));
    let want = std::fs::read(&rgb).unwrap();
    assert_eq!(want.len(), W * H * 3);
    let (mut sum, mut max) = (0u64, 0u8);
    for (a, b) in image.as_raw().iter().zip(&want) {
        let d = a.abs_diff(*b);
        sum += d as u64;
        max = max.max(d);
    }
    let mean = sum as f64 / want.len() as f64;
    // (measured: mean 1.44, max 4: the compositor's float pipeline rounds differently from swscale)
    assert!(mean < 2.0 && max <= 6, "{name}: rendered frame vs ffmpeg: mean abs diff {mean:.3}, max {max}");
}
