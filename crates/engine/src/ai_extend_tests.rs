//! `ai.generativeExtend` against the scripted fake helper: the frame sent, placement after and
//! before the clip with ripple, one undo step, stale and cancelled jobs, failures, parameters.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use filmcraft_media::DemoScene;
use filmcraft_project::{ClipId, ItemId, SequenceSettings, TrackKind};
use filmcraft_sidecar::Limits;
use filmcraft_sidecar::fake::{FakeHelper, ResultMode, Script};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::json;

use crate::Session;
use crate::media_test_util::{make_movie, tmp_dir};

const RATE: FrameRate = FrameRate::FPS_24;

struct Fx {
    s: Session,
    helper: FakeHelper,
    clip: ClipId,
    after: ClipId,
    dir: PathBuf,
}

fn script(generated: &std::path::Path, steps: u32) -> Script {
    Script {
        features: vec!["generativeExtend".into()],
        steps,
        result: ResultMode::Copy { name: "extended.mp4".into(), from: generated.to_path_buf() },
        ..Default::default()
    }
}

/// A 160x90 24 fps sequence with a 2 s clip of a real movie at 0 s and a second clip behind it
/// at 3 s; the clip is selected; a fake helper returns a real 2 s movie.
fn fixture(name: &str, seq_size: (u32, u32), steps: u32) -> Fx {
    let dir = tmp_dir(name);
    let src = dir.join("src.mp4");
    let generated = dir.join("generated.mp4");
    make_movie(&src, DemoScene::Aurora, 160, 90, 48);
    make_movie(&generated, DemoScene::CityNight, 96, 54, 48);
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [src.to_string_lossy()]})).unwrap();
    let item = ItemId(r["items"][0].as_u64().unwrap());
    let mut p = (*s.project).clone();
    let seq = p.new_sequence("Seq", SequenceSettings { width: seq_size.0, height: seq_size.1, frame_rate: RATE, ..Default::default() }, 2, 2, None);
    let two = RATE.tick_of(48);
    let a = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, two), RATE).unwrap();
    let b = p.make_track_item(item, TrackKind::Video, RATE.tick_of(72), TimeRange::new(Tick::ZERO, two), RATE).unwrap();
    let (clip, after) = (a.id, b.id);
    let q = p.sequence_mut(seq).unwrap();
    q.video_tracks[0].items.push(a);
    q.video_tracks[0].items.push(b);
    s.project = std::sync::Arc::new(p);
    s.state.active_sequence = Some(seq);
    s.state.open_sequences = vec![seq];
    s.state.selection = vec![clip];
    s.ai.cache_root = Some(dir.join("cache"));
    s.ai.limits = Limits { poll_interval: Duration::from_millis(5), max_poll_failures: 3, io_timeout: Duration::from_secs(2), ..Default::default() };
    let helper = FakeHelper::start(script(&generated, steps)).unwrap();
    s.execute("ai.connect", json!({"port": helper.port()})).unwrap();
    Fx { s, helper, clip, after, dir }
}

fn finish(s: &mut Session, id: u64) {
    let t0 = std::time::Instant::now();
    loop {
        crate::ai::poll(s);
        if s.jobs.iter().find(|j| j.id == id).is_none_or(|j| j.progress.finished.load(Ordering::Relaxed)) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "job {id} never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn job_error(s: &Session, id: u64) -> Option<String> {
    s.jobs.iter().find(|j| j.id == id)?.result.lock().unwrap().clone()?.err()
}

fn snapshot(s: &Session) -> serde_json::Value {
    serde_json::to_value(&*s.project).unwrap()
}

fn v1(s: &Session) -> Vec<(ClipId, i64, i64, bool)> {
    s.active_sequence().unwrap().video_tracks[0].items.iter().map(|c| (c.id, RATE.frame_at(c.start), RATE.frame_at(c.duration), c.reverse)).collect()
}

fn reason(s: &Session) -> String {
    (crate::find_command("ai.generativeExtend").unwrap().enabled)(s).unwrap_err()
}

#[test]
fn the_end_of_a_clip_is_extended_with_ripple_in_one_undo_step() {
    let mut f = fixture("ext-end", (160, 90), 2);
    let (before_items, undo) = (v1(&f.s), f.s.history.undo.len());
    let before = snapshot(&f.s);
    let r = f.s.execute("ai.generativeExtend", json!({"seconds": 1, "wait": true, "prompt": "  keep going  "})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    // the helper got a PNG of the last frame at the sequence size, the prompt and the length
    let req = &f.helper.started()[0];
    assert_eq!(req.kind, "generativeExtend");
    assert_eq!(req.params["seconds"], 1.0);
    assert_eq!(req.params["side"], "end");
    assert_eq!(req.params["prompt"], "keep going");
    assert_eq!(req.inputs.len(), 1);
    let png = std::fs::read(&req.inputs[0]).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let (w, h) = (u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap()));
    assert_eq!((w, h), (160, 90));
    // placed: [clip 0..48][new 48..72][second now at 96..144], on the same track
    let now = v1(&f.s);
    assert_eq!(now.len(), 3);
    assert_eq!(now[0], before_items[0]);
    assert_eq!((now[1].1, now[1].2, now[1].3), (48, 24, false));
    assert_eq!((now[2].0, now[2].1), (f.after, 72 + 24));
    let q = f.s.active_sequence().unwrap();
    let new = &q.video_tracks[0].items[1];
    assert!(new.scale_to_frame && new.name.ends_with("(extended)"));
    // the result is in a bin called Generated, and one undo takes it all away
    let bin = f.s.project.root.children.iter().find_map(|e| match e {
        filmcraft_project::BinEntry::Bin(b) if b.name == "Generated" => Some(b.children.len()),
        _ => None,
    });
    assert_eq!(bin, Some(1));
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Generative Extend");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(v1(&f.s), before_items);
    assert!(f.s.project.root.children.iter().all(|e| !matches!(e, filmcraft_project::BinEntry::Bin(b) if b.name == "Generated")));
    // the id given when the job started stays; everything else is as it was
    let mut after = snapshot(&f.s);
    let mut b = before;
    after["id"] = json!(null);
    b["id"] = json!(null);
    assert_eq!(after, b);
}

#[test]
fn the_start_of_a_clip_is_extended_backwards() {
    let mut f = fixture("ext-start", (160, 90), 2);
    f.s.execute("ai.generativeExtend", json!({"seconds": 1.5, "side": "start", "wait": true})).unwrap();
    assert_eq!(f.helper.started()[0].params["side"], "start");
    let now = v1(&f.s);
    // 1.5 s = 36 frames: the new clip sits where the clip was, played backwards, the clip behind it
    assert_eq!(now.len(), 3);
    assert_eq!((now[0].1, now[0].2, now[0].3), (0, 36, true));
    assert_eq!((now[1].0, now[1].1), (f.clip, 36));
    assert_eq!((now[2].0, now[2].1), (f.after, 72 + 36));
}

#[test]
fn the_new_clip_is_never_longer_than_the_video() {
    let mut f = fixture("ext-short", (160, 90), 1);
    // ten seconds asked, the movie has two
    f.s.execute("ai.generativeExtend", json!({"seconds": 10, "wait": true})).unwrap();
    assert_eq!(v1(&f.s)[1].2, 48);
}

#[test]
fn the_frame_covers_the_sequence_size() {
    // a portrait sequence over landscape media: the frame is cropped, not letterboxed
    let mut f = fixture("ext-cover", (90, 160), 1);
    f.s.execute("ai.generativeExtend", json!({"wait": true})).unwrap();
    let png = std::fs::read(&f.helper.started()[0].inputs[0]).unwrap();
    assert_eq!((u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap())), (90, 160));
    assert_eq!(super::frame_size(3840, 2160), (1280, 720));
    assert_eq!(super::frame_size(1080, 1920), (720, 1280));
    assert_eq!(super::frame_size(160, 90), (160, 90));
    assert_eq!(super::frame_size(1, 1), (16, 16));
}

#[test]
fn the_cover_resize_crops_the_middle() {
    // 4x2 source: left half red, right half blue; a 2x2 cover takes the middle columns
    let mut px = Vec::new();
    for _ in 0..2 {
        for x in 0..4 {
            px.extend(if x < 2 { [255u8, 0, 0, 255] } else { [0, 0, 255, 255] });
        }
    }
    let out = super::cover(&px, 4, 2, 2, 2).unwrap();
    assert_eq!(out.len(), 16);
    assert_eq!(&out[0..4], &[255, 0, 0, 255], "left pixel is the red edge");
    assert_eq!(&out[4..8], &[0, 0, 255, 255], "right pixel is the blue edge");
    assert!(super::cover(&px, 0, 2, 2, 2).is_none() && super::cover(&px[..4], 4, 2, 2, 2).is_none());
}

#[test]
fn a_changed_clip_drops_the_result() {
    let mut f = fixture("ext-stale", (160, 90), 3);
    let id = f.s.execute("ai.generativeExtend", json!({})).unwrap()["job"].as_u64().unwrap();
    f.s.edit_sequence("Move", |q, _, _| {
        q.video_tracks[0].items[0].start = RATE.tick_of(24);
        Ok(())
    })
    .unwrap();
    let before = snapshot(&f.s);
    finish(&mut f.s, id);
    let e = job_error(&f.s, id).unwrap_or_default();
    assert!(e.contains("a target clip changed") && e.contains("kept in"), "{e}");
    assert_eq!(snapshot(&f.s), before);
}

#[test]
fn cancelling_changes_nothing_and_tells_the_helper() {
    let mut f = fixture("ext-cancel", (160, 90), 100_000);
    let before = snapshot(&f.s);
    let id = f.s.execute("ai.generativeExtend", json!({})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert_eq!(f.helper.cancelled().len(), 1);
    let mut after = snapshot(&f.s);
    let mut b = before;
    after["id"] = json!(null);
    b["id"] = json!(null);
    assert_eq!(after, b);
}

#[test]
fn a_failing_helper_and_an_unreadable_clip_fail_the_job_cleanly() {
    let mut f = fixture("ext-fail", (160, 90), 1);
    drop(f.helper);
    f.helper = FakeHelper::start(Script {
        features: vec!["generativeExtend".into()],
        result: ResultMode::Fail("fal.ai refused the key (HTTP 401)".into()),
        ..Default::default()
    })
    .unwrap();
    f.s.execute("ai.connect", json!({"port": f.helper.port()})).unwrap();
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.generativeExtend", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("refused the key"), "{e}");
    assert!(f.s.ai.jobs.is_empty());
    assert_eq!(v1(&f.s).len(), 2);
    let _ = before;
    // a clip whose media cannot be read: the job fails in its preparation, the helper is not asked
    let mut g = fixture("ext-offline", (160, 90), 1);
    let item = g.s.active_sequence().unwrap().video_tracks[0].items[0].item;
    let mut p = (*g.s.project).clone();
    if let Some(filmcraft_project::ItemKind::Media(m)) = p.item_mut(item).map(|i| &mut i.kind) {
        m.media = filmcraft_project::MediaRef::File { path: "/nonexistent/gone.mp4".into() };
    }
    g.s.project = std::sync::Arc::new(p);
    g.s.media = Default::default();
    let e = g.s.execute("ai.generativeExtend", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("offline") && e.contains("relink"), "{e}");
    assert!(g.helper.started().is_empty(), "the helper was not contacted: {e}");
}

#[test]
fn parameters_are_checked_and_the_command_says_why_it_is_disabled() {
    let mut f = fixture("ext-params", (160, 90), 1);
    for bad in [json!({"seconds": 0}), json!({"seconds": 11}), json!({"seconds": "abc"}), json!({"side": "middle"}), json!({"prompt": "x".repeat(1001)})] {
        let e = f.s.execute("ai.generativeExtend", bad.clone());
        assert!(e.is_err(), "{bad}");
    }
    assert!(f.helper.started().is_empty());
    f.s.state.selection.clear();
    assert_eq!(reason(&f.s), "select a video clip in the timeline");
    f.s.state.selection = vec![f.clip];
    assert!(f.s.is_enabled("ai.generativeExtend"));
    // a helper without the feature (no fal key): disabled with the usual reason
    let other = FakeHelper::start(Script { features: vec!["echo".into()], ..Default::default() }).unwrap();
    f.s.execute("ai.connect", json!({"port": other.port()})).unwrap();
    assert!(reason(&f.s).contains("does not provide `generativeExtend`"), "{}", reason(&f.s));
    assert_eq!(crate::find_command("ai.generativeExtend").unwrap().menu, ["Clip", "AI"]);
    let _ = f.dir;
}
