//! `ai.interpolate` against the fake helper (each source frame repeated `factor` times): a NEW item
//! at `factor` x the rate, frame `k * factor` = source frame `first + k`, the original untouched;
//! with `place` a copy on a new track plays it over the same timeline range; one undo step.

use filmcraft_project::ClipId;
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{FrameRate, Tick};
use serde_json::json;

use super::super::tests::{Fixture, fixture, reason, snapshot};
use super::super::upscale::tests::{square_x, with_video};

/// N + 1 inputs: N x factor outputs, input k repeated factor times.
fn repeat(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let f = req.params["factor"].as_u64().unwrap_or(2) as usize;
    let mut out = Vec::new();
    for p in req.inputs.iter().take(req.inputs.len().saturating_sub(1)) {
        let b = std::fs::read(p).unwrap();
        for _ in 0..f {
            out.push((format!("mid-{:06}.png", out.len()), b.clone()));
        }
    }
    out
}

fn helper(name: &str) -> (Fixture, ClipId, filmcraft_project::ItemId) {
    with_video(name, Script { features: vec!["interpolate".into()], result: ResultMode::Custom(repeat), ..Default::default() })
}

fn clip_of(f: &Fixture, c: ClipId) -> filmcraft_project::TrackItem {
    f.s.active_sequence().unwrap().find_item(c).unwrap().1.clone()
}

/// A clip as JSON (its "auto" points are NaN, which is never equal to itself).
fn same(a: &filmcraft_project::TrackItem, b: &filmcraft_project::TrackItem) -> bool {
    serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
}

#[test]
fn a_new_item_at_twice_the_rate_is_placed_over_the_same_range() {
    let (mut f, clip, item) = helper("in-place");
    let before = clip_of(&f, clip);
    let tracks = f.s.active_sequence().unwrap().video_tracks.len();
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.interpolate", json!({"targetFps": 48.0, "place": true, "wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = f.helper.started()[0].clone();
    // the 6 frames played plus the next one
    assert_eq!((req.inputs.len(), req.params["factor"].as_u64()), (7, Some(2)));
    // the original is only turned off (the copy replaces it in the picture, not composited over it)
    let mut off = before.clone();
    off.enabled = false;
    assert!(same(&clip_of(&f, clip), &off), "the original is turned off and otherwise untouched");
    let q = f.s.active_sequence().unwrap().clone();
    assert_eq!(q.video_tracks.len(), tracks + 1);
    let copy = q.video_tracks.last().unwrap().items[0].clone();
    assert_ne!(copy.item, item);
    let r24 = FrameRate::FPS_24;
    let m = f.s.project.item(copy.item).and_then(|i| i.as_media()).unwrap();
    let v = m.info.video.as_ref().unwrap();
    assert_eq!((v.frame_rate.sane(), m.info.duration), (FrameRate::new(48, 1), r24.tick_of(6)), "12 frames at 48 fps: the same 6 frames' time");
    assert_eq!((copy.start, copy.duration, copy.speed, copy.reverse), (before.start, before.duration, before.speed, before.reverse));
    // the same picture of time: at every timeline point the copy shows the source time minus the
    // time of frame 2
    for k in 0..24 {
        let t = before.start + Tick(before.duration.0 * k / 24);
        assert_eq!(copy.source_time_at(t) + r24.tick_of(2), before.source_time_at(t), "at {t:?}");
    }
    // frame 2k of the new item is source frame 2 + k
    let filmcraft_project::MediaRef::File { path } = &m.media else { panic!("a file") };
    let dir = std::path::Path::new(path).parent().unwrap();
    for k in 0..6u32 {
        let (x, _) = square_x(&std::fs::read(dir.join(format!("mid-{:06}.png", 2 * k))).unwrap());
        assert_eq!(x, 4 * (2 + k), "frame {}", 2 * k);
    }
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Interpolate Frames");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(f.s.active_sequence().unwrap().video_tracks.len(), tracks);
    assert!(same(&clip_of(&f, clip), &before), "undo turns the original back on");
}

#[test]
fn the_placed_copy_keeps_animated_masks_on_their_frames() {
    use filmcraft_project::mask::{Mask, MaskPath};
    let (mut f, clip, _) = helper("in-mask");
    let seq = f.s.state.active_sequence.unwrap();
    let r24 = FrameRate::FPS_24;
    // a mask whose feather is keyframed at source frame 4 (media time)
    f.s.edit("Mask", |pr, _| {
        let it = pr.sequence_mut(seq).unwrap().video_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        let mut m = Mask::new("Mask (1)", MaskPath::ellipse(filmcraft_geom::Vec2::new(32.0, 18.0), filmcraft_geom::Vec2::new(8.0, 8.0)));
        m.feather.keyframes.push(filmcraft_project::keyframe::Keyframe::new(r24.tick_of(4), filmcraft_project::ParamValue::Float(20.0)));
        it.effect_mut("opacity").unwrap().masks.push(m);
        Ok(())
    })
    .unwrap();
    f.s.execute("ai.interpolate", json!({"place": true, "wait": true})).unwrap();
    let q = f.s.active_sequence().unwrap();
    let copy = q.video_tracks.last().unwrap().items[0].clone();
    let k = &copy.effect("opacity").unwrap().masks[0].feather.keyframes[0];
    // the new media starts at source frame 2: the keyframe moves by that time, so it lands on the
    // same picture at the same timeline time
    assert_eq!(k.time, r24.tick_of(4) - r24.tick_of(2));
    let original = clip_of(&f, clip);
    assert_eq!(original.effect("opacity").unwrap().masks[0].feather.keyframes[0].time, r24.tick_of(4), "the original keeps its own");
}

#[test]
fn without_place_only_the_item_is_added_and_bad_rates_are_refused() {
    let (mut f, clip, _) = helper("in-bin");
    let before = snapshot(&f.s);
    for p in [json!({"targetFps": 30.0}), json!({"factor": 3}), json!({"targetFps": "x"})] {
        assert!(f.s.execute("ai.interpolate", p.clone()).is_err(), "{p}");
    }
    assert_eq!(snapshot(&f.s), before);
    let q0 = f.s.active_sequence().unwrap().clone();
    f.s.execute("ai.interpolate", json!({"factor": 4, "wait": true})).unwrap();
    assert_eq!(f.helper.started()[0].params["factor"], 4);
    let q = f.s.active_sequence().unwrap();
    assert_eq!(q.video_tracks.len(), q0.video_tracks.len(), "not placed");
    assert!(same(&clip_of(&f, clip), q0.find_item(clip).unwrap().1));
    let made =
        f.s.project.items.values().filter_map(|i| i.as_media()).find(|m| m.info.video.as_ref().is_some_and(|v| v.frame_rate.sane() == FrameRate::new(96, 1)));
    assert!(made.is_some_and(|m| m.info.duration == FrameRate::FPS_24.tick_of(6)), "24 frames at 96 fps");
}

#[test]
fn the_model_state_and_the_selection_gate_the_command() {
    let states =
        json!({"interpolate": {"state": "needs model", "reason": "the interpolation model is not installed: run tools/convert_rife.sh <archive.zip>"}});
    let (f, _, _) = with_video("in-gate", Script { features: vec![], states, ..Default::default() });
    let why = reason(&f.s, "ai.interpolate");
    assert!(why.starts_with("interpolate needs its model") && why.contains("convert_rife.sh"), "{why}");
    let mut g = fixture("in-gate2", Script { features: vec!["interpolate".into()], ..Default::default() });
    g.s.state.selection = vec![];
    assert_eq!(reason(&g.s, "ai.interpolate"), "select a video clip in the timeline");
}

/// Like [`repeat`], but output frame 1 is a truncated PNG (frame 0 is fine).
fn corrupt_second(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let mut out = repeat(req);
    if let Some((_, b)) = out.get_mut(1) {
        b.truncate(b.len() / 2);
    }
    out
}

#[test]
fn placement_is_refused_in_a_transition_and_the_copy_goes_directly_above_its_track() {
    // a clip in a transition: placing a copy on another track would lose it
    let (mut f, clip, _) = helper("in-tr");
    f.s.execute("sequence.applyVideoTransition", json!({"clip": clip.0, "edge": "out", "frames": 6})).unwrap();
    let e = f.s.execute("ai.interpolate", json!({"place": true, "wait": true})).unwrap_err().to_string();
    assert!(e.contains("transition"), "{e}");
    assert!(f.helper.started().is_empty());
    // without `place` it is fine (only the new item)
    f.s.execute("ai.interpolate", json!({"wait": true})).unwrap();

    // a "title" on the track above: after placing, it is still above the copy
    let (mut g, clip, item) = helper("in-order");
    let seq = g.s.state.active_sequence.unwrap();
    if g.s.active_sequence().unwrap().video_tracks.len() < 2 {
        g.s.execute("sequence.addTracks", json!({"video": 1})).unwrap();
    }
    let title =
        g.s.edit("Title", |pr, _| {
            let r = FrameRate::FPS_24;
            let it = pr
                .make_track_item(
                    item,
                    filmcraft_project::TrackKind::Video,
                    Tick(3 * filmcraft_time::TICKS_PER_SECOND),
                    filmcraft_time::TimeRange::new(Tick::ZERO, r.tick_of(6)),
                    r,
                )
                .unwrap();
            let id = it.id;
            pr.sequence_mut(seq).unwrap().video_tracks[1].items.push(it);
            Ok(id)
        })
        .unwrap();
    g.s.state.selection = vec![clip];
    g.s.execute("ai.interpolate", json!({"place": true, "wait": true})).unwrap();
    let q = g.s.active_sequence().unwrap();
    let index_of = |c: ClipId| q.video_tracks.iter().position(|t| t.items.iter().any(|i| i.id == c)).unwrap();
    let copy = q.video_tracks.iter().flat_map(|t| &t.items).find(|i| i.name.ends_with("(2x)")).unwrap().id;
    assert_eq!(index_of(clip), 0);
    assert_eq!(index_of(copy), 1, "directly above the clip's own track");
    assert_eq!(index_of(title), 2, "what was above the clip stays above the copy");
}

#[test]
fn every_result_frame_is_decoded_before_anything_is_applied() {
    let (mut f, clip, item) =
        with_video("in-corrupt", Script { features: vec!["interpolate".into()], result: ResultMode::Custom(corrupt_second), ..Default::default() });
    // the job gives the project its id when it starts; everything else must stay as it is
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.interpolate", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("mid-000001.png does not decode"), "{e}");
    assert_eq!(snapshot(&f.s), before, "nothing applied");
    assert_eq!(clip_of(&f, clip).item, item);
}
