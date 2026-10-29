//! M8e: a 10-bit 4:2:2 HEVC (Main 4:2:2 10) MOV imports and renders with hardware decoding
//! switched off (the pure-Rust decoder): the decoded planes equal ffmpeg's decode, and a frame
//! exported from the Program Monitor is within rounding of ffmpeg's RGB conversion of the same
//! picture (bilinear chroma upsampling; ffmpeg-made fixture, skipped without ffmpeg / libx265).

use serde_json::json;

use crate::Session;
use filmcraft_project::ItemId;

const W: usize = 320;
const H: usize = 180;
/// A smooth picture (blurred test pattern): chroma upsampling choices stay within rounding.
const SOURCE: &str = "testsrc2=size=320x180:rate=25:duration=1,gblur=sigma=3";

fn ffmpeg_run(ffmpeg: &std::path::Path, args: &[&str], out: &std::path::Path) -> bool {
    std::process::Command::new(ffmpeg).args(["-y", "-loglevel", "error"]).args(args).arg(out).status().is_ok_and(|s| s.success())
}

#[test]
fn hevc_422_10_imports_and_renders_in_software() {
    let Some(ffmpeg) = filmcraft_testkit::oracle::ffmpeg_or_skip("hevc 4:2:2 import") else { return };
    let dir = filmcraft_testkit::fixtures_dir("engine/hevc422");
    let mov = dir.join("main422_10.mov");
    let encode = [
        "-f",
        "lavfi",
        "-i",
        SOURCE,
        "-c:v",
        "libx265",
        "-pix_fmt",
        "yuv422p10le",
        "-x265-params",
        "log-level=error:bframes=3",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-color_range",
        "tv",
        "-tag:v",
        "hvc1",
    ];
    let Some(mov) = filmcraft_testkit::fixtures::generate(&mov, |tmp| ffmpeg_run(&ffmpeg, &encode, tmp)) else {
        eprintln!("SKIPPED: ffmpeg without libx265");
        return;
    };
    let yuv = filmcraft_testkit::fixtures::generate(&dir.join("main422_10.yuv"), |tmp| {
        ffmpeg_run(&ffmpeg, &["-i", &mov.to_string_lossy(), "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuv422p10le"], tmp)
    })
    .unwrap();
    let rgb = filmcraft_testkit::fixtures::generate(&dir.join("main422_10_frame10_bilinear.rgb"), |tmp| {
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

    let mut s = Session::default();
    s.execute("prefs.set", json!({"key": "playback.hardwareDecoding", "value": "off"})).unwrap();
    assert!(!filmcraft_codecs::hw::hardware_decoding());
    let r = s.execute("file.import", json!({"paths": [mov.to_string_lossy()]})).unwrap();
    assert!(r["errors"].as_array().is_none_or(|e| e.is_empty()), "{r}");
    let item = ItemId(r["items"][0].as_u64().unwrap());
    let v = s.project.item(item).unwrap().as_media().unwrap().info.video.clone().unwrap();
    assert_eq!((v.width as usize, v.height as usize), (W, H));

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
        assert!(got == want, "frame {i}: decoded planes differ from ffmpeg's");
    }

    // a Program Monitor frame through the whole pipeline (sequence, render, PNG export)
    s.execute("file.newSequenceFromClip", json!({"items": [item.0]})).unwrap();
    s.set_playhead(v.frame_rate.tick_of(10));
    let png = std::env::temp_dir().join(format!("fc-hevc422-{}.png", std::process::id()));
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
    assert!(mean < 2.0 && max <= 6, "rendered frame vs ffmpeg: mean abs diff {mean:.3}, max {max}");
}
