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
    assert!(same(&clip_of(&f, clip), &before), "the original clip is untouched");
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
