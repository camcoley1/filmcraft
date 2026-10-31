//! `ai.upscale` against the fake helper (a nearest-neighbour "upscale" of the decoded frames):
//! only the frames the clip plays are decoded, the clip is relinked to the image sequence with its
//! timing and picture unchanged, in one undo step; bad, stale and cancelled results change nothing;
//! the command is disabled with a reason.

use filmcraft_project::{ClipId, ItemId, TrackKind};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::json;

use super::super::tests::{Fixture, finish, fixture, job_error, reason, snapshot};

/// `frames` frames of 64x36 4:2:0 video at 24 fps: grey, with a bright 8x8 square at x = 4 * k
/// in frame k (so each frame says which it is).
pub(in crate::ai) fn y4m(path: &std::path::Path, frames: usize) {
    let (w, h) = (64usize, 36usize);
    let mut b = format!("YUV4MPEG2 W{w} H{h} F24:1 Ip A1:1 C420jpeg\n").into_bytes();
    for k in 0..frames {
        b.extend_from_slice(b"FRAME\n");
        for y in 0..h {
            for x in 0..w {
                let on = (4 * k..4 * k + 8).contains(&x) && (14..22).contains(&y);
                b.push(if on { 235 } else { 60 });
            }
        }
        b.extend(std::iter::repeat_n(128u8, (w / 2) * (h / 2) * 2));
    }
    std::fs::write(path, b).unwrap();
}

/// The x of the bright square in an RGBA PNG (its left edge), and the PNG's size.
pub(in crate::ai) fn square_x(png: &[u8]) -> (u32, (u32, u32)) {
    let img = image::load_from_memory(png).unwrap().to_rgba8();
    let y = img.height() / 2;
    let x = (0..img.width()).find(|x| img.get_pixel(*x, y)[1] > 150).unwrap_or(u32::MAX);
    (x, img.dimensions())
}

/// Each input frame enlarged by `factor` (nearest neighbour), named `up-<input>`.
fn nearest(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let f = req.params["factor"].as_u64().unwrap_or(2) as u32;
    req.inputs
        .iter()
        .map(|p| {
            let img = image::open(p).unwrap().to_rgba8();
            let up = image::imageops::resize(&img, img.width() * f, img.height() * f, image::imageops::FilterType::Nearest);
            let mut out = Vec::new();
            up.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
            let name = std::path::Path::new(p).file_name().unwrap().to_string_lossy();
            (format!("up-{name}"), out)
        })
        .collect()
}

/// One frame short.
fn short(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let mut v = nearest(req);
    v.pop();
    v
}

/// The fixture plus a video clip of a 10-frame clip `walk.y4m` at timeline 3 s playing source
/// frames 2..8 (6 frames), selected.
pub(in crate::ai) fn with_video(name: &str, script: Script) -> (Fixture, ClipId, ItemId) {
    let mut f = fixture(name, script);
    f.s.ai.media_root = Some(f.cache.join("media"));
    std::fs::create_dir_all(&f.cache).unwrap();
    let path = f.cache.join("walk.y4m");
    y4m(&path, 10);
    let item = ItemId(f.s.execute("file.import", json!({"paths": [path.to_string_lossy()]})).unwrap()["items"][0].as_u64().unwrap());
    let seq = f.s.state.active_sequence.unwrap();
    let r = FrameRate::FPS_24;
    let clip =
        f.s.edit("Place", |pr, _| {
            let it =
                pr.make_track_item(item, TrackKind::Video, Tick(3 * filmcraft_time::TICKS_PER_SECOND), TimeRange::new(r.tick_of(2), r.tick_of(6)), r).unwrap();
            let id = it.id;
            pr.sequence_mut(seq).unwrap().video_tracks[0].items.push(it);
            Ok(id)
        })
        .unwrap();
    f.s.state.selection = vec![clip];
    (f, clip, item)
}

fn clip_of(f: &Fixture, c: ClipId) -> filmcraft_project::TrackItem {
    f.s.active_sequence().unwrap().find_item(c).unwrap().1.clone()
}

/// A clip as JSON (its "auto" points are NaN, which is never equal to itself).
fn same(a: &filmcraft_project::TrackItem, b: &filmcraft_project::TrackItem) -> bool {
    serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
}

fn helper(name: &str, write: filmcraft_sidecar::fake::Writer) -> (Fixture, ClipId, ItemId) {
    with_video(name, Script { features: vec!["upscale".into()], result: ResultMode::Custom(write), ..Default::default() })
}

#[test]
fn the_clip_is_relinked_to_its_upscaled_frames_in_one_undo_step() {
    let (mut f, clip, item) = helper("up-relink", nearest);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Motion", |pr, _| {
        let it = pr.sequence_mut(seq).unwrap().video_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        let m = it.effect_mut("motion").unwrap();
        m.param_mut("scale").unwrap().value = filmcraft_project::ParamValue::Float(150.0);
        Ok(())
    })
    .unwrap();
    let before = clip_of(&f, clip);
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.upscale", json!({"factor": 4, "wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = f.helper.started()[0].clone();
    // only the 6 frames the clip plays, decoded by FilmCraft (frame 2 first)
    assert_eq!((req.kind.as_str(), req.inputs.len(), req.params["factor"].as_u64(), req.params["backend"].as_str()), ("upscale", 6, Some(4), Some("lanczos")));
    let after = clip_of(&f, clip);
    assert_ne!(after.item, item);
    assert_eq!((after.start, after.duration, after.speed, after.reverse), (before.start, before.duration, before.speed, before.reverse));
    let r24 = FrameRate::FPS_24;
    assert_eq!(after.source_in, before.source_in - r24.tick_of(2), "media time 0 is source frame 2");
    // the picture keeps its size: Scale divided by the factor
    assert_eq!(after.effect("motion").unwrap().f64_at("scale", Tick::ZERO), 37.5);
    let m = f.s.project.item(after.item).and_then(|i| i.as_media()).unwrap();
    let v = m.info.video.as_ref().unwrap();
    assert_eq!((v.width, v.height, v.frame_rate.sane()), (256, 144, r24));
    assert_eq!(m.info.duration, r24.tick_of(6));
    let filmcraft_project::MediaRef::File { path } = &m.media else { panic!("a file") };
    assert!(path.contains("AI Media"), "{path}");
    // frame k of the result is source frame 2 + k: its square sits at 4 * (2 + k) * 4
    let dir = std::path::Path::new(path).parent().unwrap();
    for k in 0..6u32 {
        let (x, size) = square_x(&std::fs::read(dir.join(format!("up-src-{k:06}.png"))).unwrap());
        assert_eq!((x, size), (16 * (2 + k), (256, 144)), "frame {k}");
    }
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Upscale");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert!(same(&clip_of(&f, clip), &before));
}

#[test]
fn bad_stale_and_cancelled_results_change_nothing() {
    let (mut f, _, _) = helper("up-short", short);
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.upscale", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("wrote no up-src-000005.png"), "{e}");
    assert_eq!(snapshot(&f.s), before);
    for p in [json!({"factor": 3}), json!({"factor": "x"}), json!({"factor": 8})] {
        assert!(f.s.execute("ai.upscale", p.clone()).is_err(), "{p}");
    }

    let (mut g, clip, item) =
        with_video("up-stale", Script { features: vec!["upscale".into()], steps: 3, result: ResultMode::Custom(nearest), ..Default::default() });
    let id = g.s.execute("ai.upscale", json!({})).unwrap()["job"].as_u64().unwrap();
    let seq = g.s.state.active_sequence.unwrap();
    g.s.edit("Move", |pr, _| {
        pr.sequence_mut(seq).unwrap().video_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap().start = Tick(9 * filmcraft_time::TICKS_PER_SECOND);
        Ok(())
    })
    .unwrap();
    finish(&mut g.s, id);
    assert!(job_error(&g.s, id).is_some_and(|e| e.contains("changed")), "{:?}", job_error(&g.s, id));
    assert_eq!(clip_of(&g, clip).item, item);

    let (mut h, clip, item) =
        with_video("up-cancel", Script { features: vec!["upscale".into()], steps: 100_000, result: ResultMode::Custom(nearest), ..Default::default() });
    let id = h.s.execute("ai.upscale", json!({})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    h.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut h.s, id);
    assert_eq!(job_error(&h.s, id).as_deref(), Some("stopped"));
    assert_eq!(clip_of(&h, clip).item, item);
}

#[test]
fn upscale_refuses_masks_and_says_why_it_is_disabled() {
    let (mut f, clip, _) = helper("up-mask", nearest);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Mask", |pr, _| {
        let it = pr.sequence_mut(seq).unwrap().video_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        let m = filmcraft_project::mask::Mask::new(
            "Mask (1)",
            filmcraft_project::mask::MaskPath::ellipse(filmcraft_geom::Vec2::new(32.0, 18.0), filmcraft_geom::Vec2::new(8.0, 8.0)),
        );
        it.effect_mut("opacity").unwrap().masks.push(m);
        Ok(())
    })
    .unwrap();
    let e = f.s.execute("ai.upscale", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("masks"), "{e}");
    assert!(f.helper.started().is_empty());
    // gating: a helper without the feature, and the fixture's generated clip only
    let (g, _, _) = with_video("up-gate", Script::default());
    assert!(reason(&g.s, "ai.upscale").contains("does not provide `upscale`"));
    let mut h = fixture("up-gate2", Script { features: vec!["upscale".into()], ..Default::default() });
    h.s.state.selection = vec![];
    assert_eq!(reason(&h.s, "ai.upscale"), "select a video clip in the timeline");
}

fn edit_clip(f: &mut Fixture, clip: ClipId, edit: impl FnOnce(&mut filmcraft_project::TrackItem)) {
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Edit", |pr, _| {
        edit(pr.sequence_mut(seq).unwrap().video_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap());
        Ok(())
    })
    .unwrap();
}

fn edit_media(f: &mut Fixture, item: ItemId, edit: impl FnOnce(&mut filmcraft_project::MediaClip)) {
    f.s.edit("Edit", |pr, _| {
        if let Some(filmcraft_project::ItemKind::Media(m)) = pr.item_mut(item).map(|i| &mut i.kind) {
            edit(m);
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn the_anchor_follows_with_scale_to_frame_size_a_missing_motion_is_added_and_a_turned_off_one_refused() {
    use filmcraft_project::ParamValue;
    // Scale to Frame Size: the fit follows the new size, so Scale stays; the anchor (source
    // pixels) is multiplied, its keyframes too; the alpha interpretation is carried over
    let (mut f, clip, item) = helper("up-fit", nearest);
    edit_media(&mut f, item, |m| m.interpret.ignore_alpha = true);
    edit_clip(&mut f, clip, |it| {
        it.scale_to_frame = true;
        let m = it.effect_mut("motion").unwrap();
        m.param_mut("scale").unwrap().value = ParamValue::Float(80.0);
        let a = m.param_mut("anchor").unwrap();
        a.value = ParamValue::Vec2(filmcraft_geom::Vec2::new(10.0, 5.0));
        a.keyframes.push(filmcraft_project::keyframe::Keyframe::new(FrameRate::FPS_24.tick_of(3), ParamValue::Vec2(filmcraft_geom::Vec2::new(12.0, 6.0))));
    });
    f.s.execute("ai.upscale", json!({"factor": 4, "wait": true})).unwrap();
    let after = clip_of(&f, clip);
    assert!(after.scale_to_frame);
    assert_eq!(after.effect("motion").unwrap().f64_at("scale", Tick::ZERO), 80.0, "Scale untouched under the fit");
    let a = after.effect("motion").unwrap().param("anchor").unwrap();
    assert_eq!(a.value, ParamValue::Vec2(filmcraft_geom::Vec2::new(40.0, 20.0)));
    let k = &a.keyframes[0];
    assert_eq!(k.value, ParamValue::Vec2(filmcraft_geom::Vec2::new(48.0, 24.0)));
    let m = f.s.project.item(after.item).and_then(|i| i.as_media()).unwrap();
    assert!(m.interpret.ignore_alpha && !m.interpret.invert_alpha, "Ignore Alpha carried to the result");

    // a clip without a Motion effect gets one at 100 / factor %
    let (mut g, clip, _) = helper("up-nomotion", nearest);
    edit_clip(&mut g, clip, |it| it.effects.retain(|e| e.effect != "motion"));
    g.s.execute("ai.upscale", json!({"factor": 2, "wait": true})).unwrap();
    let after = clip_of(&g, clip);
    let m = after.effect("motion").expect("Motion added");
    assert!(m.enabled && m.f64_at("scale", Tick::ZERO) == 50.0);

    // Motion turned off: its 100 % would show the result larger; refused before anything runs
    let (mut h, clip, item) = helper("up-motionoff", nearest);
    edit_clip(&mut h, clip, |it| it.effect_mut("motion").unwrap().enabled = false);
    let e = h.s.execute("ai.upscale", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("Motion is turned off"), "{e}");
    assert!(h.helper.started().is_empty() && clip_of(&h, clip).item == item);
}

#[test]
fn effects_in_source_pixels_and_unsafe_sources_are_refused_before_anything_runs() {
    let (mut f, clip, item) = helper("up-fx", nearest);
    let refused = |f: &mut Fixture, want: &str| {
        let e = f.s.execute("ai.upscale", json!({"wait": true})).unwrap_err().to_string();
        assert!(e.contains(want), "{want}: {e}");
        assert!(f.helper.started().is_empty());
    };
    let add = |f: &mut Fixture, id: &str, set: &[(&str, f64)]| {
        let mut e = filmcraft_project::find_effect(id).unwrap().instance();
        for (k, v) in set {
            e.param_mut(k).unwrap().value = filmcraft_project::ParamValue::Float(*v);
        }
        edit_clip(f, clip, |it| it.effects.push(e));
    };
    add(&mut f, "gaussian_blur", &[("blurriness", 8.0)]);
    refused(&mut f, "Gaussian Blur");
    // turned off, it is ignored; Lumetri with Sharpen is pixel-space
    edit_clip(&mut f, clip, |it| it.effects.iter_mut().filter(|e| e.effect == "gaussian_blur").for_each(|e| e.enabled = false));
    add(&mut f, "lumetri", &[("sharpen", 20.0)]);
    refused(&mut f, "Sharpen");
    // colour work only (Lumetri without Sharpen, Brightness & Contrast): accepted
    edit_clip(&mut f, clip, |it| {
        for e in it.effects.iter_mut().filter(|e| e.effect == "lumetri") {
            e.param_mut("sharpen").unwrap().value = filmcraft_project::ParamValue::Float(0.0);
        }
    });
    add(&mut f, "brightness_contrast", &[("brightness", 10.0)]);
    // a colour space set in Interpret Footage, an image-sequence source: refused
    edit_media(&mut f, item, |m| m.interpret.color_space = Some(filmcraft_color::ColorSpace::Rec2020));
    refused(&mut f, "Interpret Footage sets a colour space");
    edit_media(&mut f, item, |m| {
        m.interpret.color_space = None;
        m.info.kind = filmcraft_media::MediaKind::ImageSequence;
    });
    refused(&mut f, "image-sequence sources");
    edit_media(&mut f, item, |m| m.info.kind = filmcraft_media::MediaKind::Movie);
    // with proxies on, the full-resolution frames are decoded, not the (smaller) proxy's
    let proxy = f.cache.join("proxy.y4m");
    std::fs::write(&proxy, b"YUV4MPEG2 W32 H18 F24:1 Ip A1:1 C420jpeg\n").unwrap();
    edit_media(&mut f, item, |m| m.proxy = Some(filmcraft_project::MediaRef::File { path: proxy.to_string_lossy().to_string() }));
    f.s.media.set_use_proxies(true);
    let r = f.s.execute("ai.upscale", json!({"wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let png = std::fs::read(&f.helper.started()[0].inputs[0]).ok();
    assert!(png.is_none(), "inputs are removed once the helper is done");
    assert_eq!(clip_of(&f, clip).effects.iter().filter(|e| e.effect == "brightness_contrast").count(), 1);
}

#[test]
fn too_long_a_job_is_refused_and_a_failed_job_leaves_no_frames() {
    let (mut f, _, _) = helper("up-cost", nearest);
    f.s.ai.limits.timeout = std::time::Duration::from_millis(100);
    let e = f.s.execute("ai.upscale", json!({"factor": 4})).unwrap_err().to_string();
    assert!(e.contains("more than a job may run"), "{e}");
    assert!(f.helper.started().is_empty());
    let e = super::check_cost("Clip", super::MAX_JOB_BYTES + 1, 1.0, std::time::Duration::from_secs(10)).unwrap_err().to_string();
    assert!(e.contains("GB of disk"), "{e}");
    assert_eq!(super::duration_text(3.0 * 3600.0 + 1200.0), "about 3 h 20 min");

    // a result the job's thread rejects (one frame short): no frame stays in the cache or in the
    // project's AI media folder
    let (mut g, clip, item) = helper("up-leftovers", short);
    let r = g.s.execute("ai.upscale", json!({})).unwrap();
    let (id, dir) = (r["job"].as_u64().unwrap(), std::path::PathBuf::from(r["outDir"].as_str().unwrap()));
    finish(&mut g.s, id);
    assert!(job_error(&g.s, id).is_some_and(|e| e.contains("up-src-000005.png")), "{:?}", job_error(&g.s, id));
    let pngs = std::fs::read_dir(&dir).map(|d| d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "png")).count()).unwrap_or(0);
    assert_eq!(pngs, 0, "no frames left in {}", dir.display());
    let media = g.cache.join("media");
    let left = walk_pngs(&media);
    assert_eq!(left, 0, "no frames left in the AI media folder");
    assert_eq!(clip_of(&g, clip).item, item);
}

fn walk_pngs(dir: &std::path::Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten().map(|e| if e.path().is_dir() { walk_pngs(&e.path()) } else { usize::from(e.path().extension().is_some_and(|x| x == "png")) }).sum()
}

#[test]
fn applying_reads_no_files() {
    // the job's thread opens the result; once it is ready the frames can vanish from disk and the
    // apply (on the UI thread) still works, because it only edits the project
    let (mut f, clip, item) = helper("up-noio", nearest);
    let id = f.s.execute("ai.upscale", json!({})).unwrap()["job"].as_u64().unwrap();
    let t0 = std::time::Instant::now();
    while f.s.ai.jobs.iter().all(|j| j.outcome.lock().unwrap().is_none()) {
        assert!(t0.elapsed() < std::time::Duration::from_secs(30), "never ready");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let dir = f.s.ai.jobs[0].media.clone().unwrap();
    let moved = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect::<Vec<_>>();
    let parked = f.cache.join("parked");
    std::fs::create_dir_all(&parked).unwrap();
    for p in &moved {
        std::fs::rename(p, parked.join(p.file_name().unwrap())).unwrap();
    }
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id), None);
    assert_ne!(clip_of(&f, clip).item, item);
}
