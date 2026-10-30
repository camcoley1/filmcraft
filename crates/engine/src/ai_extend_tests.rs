//! `ai.generativeExtend` against the scripted fake helper: the frame sent, placement after and
//! before the clip with ripple (other tracks, linked audio, a bed crossing the cut, other frame
//! rates, transitions), one undo step, the result kept out of the cache, the job's own sequence,
//! the confirmation, resume, locked tracks, stale and cancelled jobs, failures, parameters.

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

fn secs(s: f64) -> Tick {
    Tick::from_seconds_f64(s)
}

struct Fx {
    s: Session,
    helper: FakeHelper,
    seq: ItemId,
    rate: FrameRate,
    /// V1: `clip` at 1..3 s, `after` at 4..6 s; A1: their linked sound; A2: a bed at 0..8 s.
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

/// A sequence of `seq_size` at `rate` over a 24 fps movie (the source rate differs from the
/// sequence's unless `rate` is 24): see [`Fx`]; the clip is selected; a fake helper returns a real
/// 2 s movie.
fn fixture(name: &str, seq_size: (u32, u32), rate: FrameRate, steps: u32) -> Fx {
    let dir = tmp_dir(name);
    let src = dir.join("src.mp4");
    let generated = dir.join("generated.mp4");
    make_movie(&src, DemoScene::Aurora, 160, 90, 192);
    make_movie(&generated, DemoScene::CityNight, 96, 54, 48);
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [src.to_string_lossy()]})).unwrap();
    let item = ItemId(r["items"][0].as_u64().unwrap());
    let mut p = (*s.project).clone();
    let seq = p.new_sequence("Seq", SequenceSettings { width: seq_size.0, height: seq_size.1, frame_rate: rate, ..Default::default() }, 2, 2, None);
    let put = |p: &mut filmcraft_project::Project, kind, start: f64, src_in: f64, len: f64, link| {
        let mut c = p.make_track_item(item, kind, secs(start), TimeRange::new(secs(src_in), secs(len)), rate).unwrap();
        c.link = link;
        c
    };
    let a = put(&mut p, TrackKind::Video, 1.0, 0.5, 2.0, Some(1));
    let b = put(&mut p, TrackKind::Video, 4.0, 0.0, 2.0, Some(2));
    let aa = put(&mut p, TrackKind::Audio, 1.0, 0.5, 2.0, Some(1));
    let ba = put(&mut p, TrackKind::Audio, 4.0, 0.0, 2.0, Some(2));
    let bed = put(&mut p, TrackKind::Audio, 0.0, 0.0, 8.0, None);
    let (clip, after) = (a.id, b.id);
    let q = p.sequence_mut(seq).unwrap();
    q.video_tracks[0].items.extend([a, b]);
    q.audio_tracks[0].items.extend([aa, ba]);
    q.audio_tracks[1].items.push(bed);
    s.project = std::sync::Arc::new(p);
    s.state.active_sequence = Some(seq);
    s.state.open_sequences = vec![seq];
    s.state.selection = vec![clip];
    s.ai.cache_root = Some(dir.join("cache"));
    s.ai.media_root = Some(dir.join("media"));
    s.ai.limits = Limits { poll_interval: Duration::from_millis(5), max_poll_failures: 3, io_timeout: Duration::from_secs(2), ..Default::default() };
    let helper = FakeHelper::start(script(&generated, steps)).unwrap();
    s.execute("ai.connect", json!({"port": helper.port()})).unwrap();
    Fx { s, helper, seq, rate, clip, after, dir }
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

/// Start a job, read the PNG the helper got while it runs (the job deletes it once the helper is
/// done), and run it to the end.
fn run_reading_frame(f: &mut Fx, p: serde_json::Value) -> ((u32, u32), u64) {
    let id = f.s.execute("ai.generativeExtend", p).unwrap()["job"].as_u64().unwrap();
    let t0 = std::time::Instant::now();
    while f.helper.started().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(20), "the helper was never asked");
        std::thread::sleep(Duration::from_millis(2));
    }
    let png = std::fs::read(&f.helper.started()[0].inputs[0]).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let size = (u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap()));
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id), None);
    (size, id)
}

fn job_error(s: &Session, id: u64) -> Option<String> {
    s.jobs.iter().find(|j| j.id == id)?.result.lock().unwrap().clone()?.err()
}

fn snapshot(s: &Session) -> serde_json::Value {
    let mut v = serde_json::to_value(&*s.project).unwrap();
    // the id given when the job started stays after an undo
    v["id"] = json!(null);
    v
}

/// (id, start, duration, source in, reverse, link) of every item on a track, in frames of the
/// sequence (source in: ticks).
type Row = (ClipId, i64, i64, i64, bool, Option<u64>);
fn rows(s: &Session, seq: ItemId, kind: TrackKind, track: usize) -> Vec<Row> {
    let q = s.project.sequence(seq).unwrap();
    let rate = q.settings.frame_rate;
    let t = match kind {
        TrackKind::Video => &q.video_tracks[track],
        _ => &q.audio_tracks[track],
    };
    t.items.iter().map(|c| (c.id, rate.frame_at(c.start), rate.frame_at(c.duration), c.source_in.0, c.reverse, c.link)).collect()
}

fn reason(s: &Session) -> String {
    (crate::find_command("ai.generativeExtend").unwrap().enabled)(s).unwrap_err()
}

#[test]
fn the_end_of_a_clip_is_extended_with_ripple_in_one_undo_step() {
    let mut f = fixture("ext-end", (160, 90), FrameRate::FPS_24, 40);
    let (v1, a1, a2) = (rows(&f.s, f.seq, TrackKind::Video, 0), rows(&f.s, f.seq, TrackKind::Audio, 0), rows(&f.s, f.seq, TrackKind::Audio, 1));
    let (undo, before) = (f.s.history.undo.len(), snapshot(&f.s));
    let (size, _) = run_reading_frame(&mut f, json!({"seconds": 1, "prompt": "  keep going  ", "confirm": true}));
    // the helper got a PNG of the last frame at the sequence size, the prompt and the length
    let req = &f.helper.started()[0];
    assert_eq!(req.kind, "generativeExtend");
    assert_eq!((req.params["seconds"].as_f64(), req.params["side"].as_str(), req.params["prompt"].as_str()), (Some(1.0), Some("end"), Some("keep going")));
    assert_eq!(req.inputs.len(), 1);
    assert_eq!(size, (160, 90));
    // V1: [clip 24..72][new 72..96][after 96+24..], the new clip from the video's start, forwards
    let now = rows(&f.s, f.seq, TrackKind::Video, 0);
    assert_eq!(now.len(), 3);
    assert_eq!(now[0], v1[0]);
    assert_eq!((now[1].1, now[1].2, now[1].3, now[1].4, now[1].5), (72, 24, 0, false, None));
    assert_eq!(now[2], (f.after, 96 + 24, 48, v1[1].3, false, Some(2)));
    // A1: the clip's sound stays, the next clip's sound moves with its picture (still linked)
    let a1_now = rows(&f.s, f.seq, TrackKind::Audio, 0);
    assert_eq!(a1_now[0], a1[0]);
    assert_eq!(a1_now[1], (a1[1].0, a1[1].1 + 24, a1[1].2, a1[1].3, false, Some(2)));
    // A2 (sync-locked): the bed is split at the cut and its second part continues the source
    let a2_now = rows(&f.s, f.seq, TrackKind::Audio, 1);
    assert_eq!(a2_now.len(), 2);
    assert_eq!((a2_now[0].1, a2_now[0].2), (0, 72));
    assert_eq!((a2_now[1].1, a2_now[1].2, a2_now[1].3), (96, 192 - 72, secs(3.0).0));
    let q = f.s.project.sequence(f.seq).unwrap();
    let new = &q.video_tracks[0].items[1];
    assert_eq!(new.name, "src.mp4 (extended)");
    // Fill Frame: a 96x54 video in a 160x90 frame is scaled to cover it, not fitted
    let scale = new.effect("motion").unwrap().f64_at("scale", Tick::ZERO);
    assert!(!new.scale_to_frame && (scale - 100.0 * 160.0 / 96.0).abs() < 0.01, "{scale}");
    // the result is in a bin called Generated, its file in the project's AI media (not the cache)
    let bin = f.s.project.root.children.iter().find_map(|e| match e {
        filmcraft_project::BinEntry::Bin(b) if b.name == "Generated" => Some(b.children.len()),
        _ => None,
    });
    assert_eq!(bin, Some(1));
    let path = match f.s.project.resolve_media(new.item).map(|(_, m, _)| m.media.clone()) {
        Some(filmcraft_project::MediaRef::File { path }) => PathBuf::from(path),
        other => panic!("{other:?}"),
    };
    assert!(path.starts_with(f.dir.join("media")) && path.exists(), "{}", path.display());
    // pruning the whole AI cache leaves it alone
    crate::ai::prune_cache(&f.dir.join("cache").join("AI"), 0, &[], Duration::ZERO);
    assert!(path.exists());
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Generative Extend");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(rows(&f.s, f.seq, TrackKind::Video, 0), v1);
    assert_eq!(rows(&f.s, f.seq, TrackKind::Audio, 0), a1);
    assert_eq!(rows(&f.s, f.seq, TrackKind::Audio, 1), a2);
    assert_eq!(snapshot(&f.s), before);
}

#[test]
fn the_start_of_a_clip_is_extended_backwards_at_another_frame_rate() {
    // a 25 fps sequence over 24 fps media
    let mut f = fixture("ext-start", (160, 90), FrameRate::new(25, 1), 1);
    let (v1, a1, a2) = (rows(&f.s, f.seq, TrackKind::Video, 0), rows(&f.s, f.seq, TrackKind::Audio, 0), rows(&f.s, f.seq, TrackKind::Audio, 1));
    let before = snapshot(&f.s);
    f.s.execute("ai.generativeExtend", json!({"seconds": 1.5, "side": "start", "wait": true, "confirm": true})).unwrap();
    assert_eq!(f.helper.started()[0].params["side"], "start");
    // 1.5 s = 37.5 frames at 25 fps: 37. The new clip sits where the clip was, played backwards
    // from source 37 frames down to 0, the clip and everything behind it 37 frames later
    let now = rows(&f.s, f.seq, TrackKind::Video, 0);
    assert_eq!(now.len(), 3);
    assert_eq!((now[0].1, now[0].2, now[0].3, now[0].4), (25, 37, 0, true));
    assert_eq!(now[1], (f.clip, 25 + 37, 50, v1[0].3, false, Some(1)));
    assert_eq!(now[2], (f.after, 100 + 37, 50, v1[1].3, false, Some(2)));
    let a1_now = rows(&f.s, f.seq, TrackKind::Audio, 0);
    for (n, b) in a1_now.iter().zip(&a1) {
        assert_eq!((n.0, n.1, n.2, n.3, n.5), (b.0, b.1 + 37, b.2, b.3, b.5), "linked sound stays in sync");
    }
    let a2_now = rows(&f.s, f.seq, TrackKind::Audio, 1);
    assert_eq!((a2_now[0].1, a2_now[0].2), (0, 25));
    assert_eq!((a2_now[1].1, a2_now[1].2, a2_now[1].3), (25 + 37, 200 - 25, secs(1.0).0));
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!((rows(&f.s, f.seq, TrackKind::Video, 0), rows(&f.s, f.seq, TrackKind::Audio, 0), rows(&f.s, f.seq, TrackKind::Audio, 1)), (v1, a1, a2));
    assert_eq!(snapshot(&f.s), before);
}

#[test]
fn the_frame_sent_is_the_clips_last_or_first_source_frame() {
    // 30 fps sequence, 24 fps media, the clip shows source 0.5 s to 2.5 s at 1..3 s
    let f = fixture("ext-time", (160, 90), FrameRate::new(30, 1), 1);
    let q = f.s.project.sequence(f.seq).unwrap();
    let (_, it) = q.find_item(f.clip).unwrap();
    let fd = f.rate.frame_duration();
    assert_eq!(it.source_time_at(super::frame_time(it, fd, true)), secs(0.5) + secs(2.0) - fd);
    assert_eq!(it.source_time_at(super::frame_time(it, fd, false)), secs(0.5));
}

#[test]
fn the_new_clip_is_never_longer_than_the_video_and_snaps_to_frames() {
    let mut f = fixture("ext-short", (160, 90), FrameRate::new(30, 1), 1);
    // ten seconds asked, the movie has two: 60 frames at 30 fps
    f.s.execute("ai.generativeExtend", json!({"seconds": 10, "wait": true, "confirm": true})).unwrap();
    let now = rows(&f.s, f.seq, TrackKind::Video, 0);
    assert_eq!((now[1].1, now[1].2), (90, 60));
    assert_eq!((now[2].0, now[2].1), (f.after, 120 + 60));
}

#[test]
fn the_frame_covers_the_sequence_size() {
    // a portrait sequence over landscape media: the frame is cropped, not letterboxed
    let mut f = fixture("ext-cover", (90, 160), FrameRate::FPS_24, 40);
    let (size, _) = run_reading_frame(&mut f, json!({"confirm": true}));
    assert_eq!(size, (90, 160));
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
fn the_result_lands_in_the_jobs_sequence_whichever_is_open() {
    let mut f = fixture("ext-other-seq", (160, 90), FrameRate::FPS_24, 20);
    let id = f.s.execute("ai.generativeExtend", json!({"confirm": true, "seconds": 1})).unwrap()["job"].as_u64().unwrap();
    // another sequence is opened while the job runs
    let mut p = (*f.s.project).clone();
    let other = p.new_sequence("Other", SequenceSettings { width: 160, height: 90, frame_rate: FrameRate::FPS_24, ..Default::default() }, 1, 1, None);
    f.s.project = std::sync::Arc::new(p);
    f.s.state.active_sequence = Some(other);
    let other_before = serde_json::to_value(f.s.project.sequence(other).unwrap()).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id), None);
    assert_eq!(rows(&f.s, f.seq, TrackKind::Video, 0).len(), 3);
    assert_eq!(serde_json::to_value(f.s.project.sequence(other).unwrap()).unwrap(), other_before);
    assert_eq!(f.s.state.active_sequence, Some(other));
}

#[test]
fn a_transition_on_the_extended_edge_moves_to_the_new_clips_edge() {
    for at_end in [true, false] {
        let mut f = fixture(if at_end { "ext-tr-end" } else { "ext-tr-start" }, (160, 90), FrameRate::FPS_24, 1);
        // make the two clips adjacent (the second right after the first, at 3 s) with a dissolve
        let (a, b) = (f.clip, f.after);
        f.s.edit_sequence("Move", |q, _, _| {
            q.video_tracks[0].items[1].start = secs(3.0);
            q.audio_tracks[0].items[1].start = secs(3.0);
            Ok(())
        })
        .unwrap();
        f.s.execute("sequence.applyVideoTransition", json!({"clip": a, "edge": "out", "frames": 12})).unwrap();
        let tr = f.s.project.sequence(f.seq).unwrap().video_tracks[0].transitions[0].clone();
        assert_eq!((tr.from, tr.to), (Some(a), Some(b)));
        let target = if at_end { a } else { b };
        f.s.state.selection = vec![target];
        f.s.execute("ai.generativeExtend", json!({"seconds": 1, "side": if at_end { "end" } else { "start" }, "wait": true, "confirm": true})).unwrap();
        let q = f.s.project.sequence(f.seq).unwrap();
        let t = &q.video_tracks[0];
        let new = t.items.iter().find(|i| i.name.ends_with("(extended)")).unwrap().id;
        assert_eq!(t.transitions.len(), 1);
        let now = &t.transitions[0];
        if at_end {
            // a | new | b: the dissolve now runs from the new clip into b, on that cut
            assert_eq!((now.from, now.to, now.start), (Some(new), Some(b), tr.start + secs(1.0)));
            assert_eq!(t.item(b).unwrap().start, secs(4.0));
        } else {
            // a | new | b: the dissolve runs from a into the new clip, where it was
            assert_eq!((now.from, now.to, now.start), (Some(a), Some(new), tr.start));
            assert_eq!(t.item(new).unwrap().start, secs(3.0));
        }
        let cut = if at_end { t.item(b).unwrap().start } else { t.item(new).unwrap().start };
        assert!(now.start < cut && now.end() > cut, "the dissolve straddles its cut");
    }
}

#[test]
fn a_new_request_needs_a_confirmation_and_a_locked_track_is_refused_first() {
    let mut f = fixture("ext-confirm", (160, 90), FrameRate::FPS_24, 1);
    for p in [json!({"wait": true}), json!({"wait": true, "confirm": false}), json!({"confirm": "yes"})] {
        let e = f.s.execute("ai.generativeExtend", p).unwrap_err().to_string();
        assert!(e.contains("sends the clip's frame") && e.contains("billed") && e.contains("confirm: true"), "{e}");
    }
    let mut p = (*f.s.project).clone();
    p.sequence_mut(f.seq).unwrap().video_tracks[0].locked = true;
    f.s.project = std::sync::Arc::new(p);
    let e = f.s.execute("ai.generativeExtend", json!({"wait": true, "confirm": true})).unwrap_err().to_string();
    assert!(e.contains("locked"), "{e}");
    assert!(f.helper.started().is_empty(), "nothing was sent");
    assert!(f.s.ai.jobs.is_empty() && f.s.jobs.is_empty());
}

#[test]
fn resume_fetches_an_earlier_request_without_a_frame() {
    let mut f = fixture("ext-resume", (160, 90), FrameRate::FPS_24, 1);
    for bad in [json!({"resume": "../x"}), json!({"resume": 5}), json!({"resume": ""}), json!({"resume": "a".repeat(81)})] {
        assert!(f.s.execute("ai.generativeExtend", bad.clone()).is_err(), "{bad}");
    }
    assert!(f.helper.started().is_empty());
    let r = f.s.execute("ai.generativeExtend", json!({"resume": "4f1c2b8e-0d7a", "seconds": 1, "wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = &f.helper.started()[0];
    assert_eq!(req.params["resume"], "4f1c2b8e-0d7a");
    assert!(req.inputs.iter().all(|i| !i.ends_with(".png")), "no frame was made: {:?}", req.inputs);
    assert_eq!(rows(&f.s, f.seq, TrackKind::Video, 0).len(), 3);
}

#[test]
fn a_changed_clip_drops_the_result() {
    let mut f = fixture("ext-stale", (160, 90), FrameRate::FPS_24, 3);
    let id = f.s.execute("ai.generativeExtend", json!({"confirm": true})).unwrap()["job"].as_u64().unwrap();
    f.s.edit_sequence("Move", |q, _, _| {
        q.video_tracks[0].items[0].start = secs(0.5);
        Ok(())
    })
    .unwrap();
    let before = snapshot(&f.s);
    finish(&mut f.s, id);
    let e = job_error(&f.s, id).unwrap_or_default();
    assert!(e.contains("a target clip changed") && e.contains("kept in"), "{e}");
    assert_eq!(snapshot(&f.s), before);
    // the paid result went back to the cache folder named in the message, not lost
    assert!(std::fs::read_dir(f.dir.join("cache")).is_ok());
}

#[test]
fn cancelling_changes_nothing_and_tells_the_helper() {
    let mut f = fixture("ext-cancel", (160, 90), FrameRate::FPS_24, 100_000);
    let before = snapshot(&f.s);
    let id = f.s.execute("ai.generativeExtend", json!({"confirm": true})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert_eq!(f.helper.cancelled().len(), 1);
    assert_eq!(snapshot(&f.s), before);
}

#[test]
fn a_failing_helper_and_an_unreadable_clip_fail_the_job_cleanly() {
    let mut f = fixture("ext-fail", (160, 90), FrameRate::FPS_24, 1);
    drop(f.helper);
    f.helper = FakeHelper::start(Script {
        features: vec!["generativeExtend".into()],
        result: ResultMode::Fail("fal.ai refused the key; check FAL_KEY and the account (HTTP 401)".into()),
        ..Default::default()
    })
    .unwrap();
    f.s.execute("ai.connect", json!({"port": f.helper.port()})).unwrap();
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.generativeExtend", json!({"wait": true, "confirm": true})).unwrap_err().to_string();
    assert!(e.contains("refused the key"), "{e}");
    assert!(f.s.ai.jobs.is_empty());
    assert_eq!(snapshot(&f.s), before);
    // a clip whose media cannot be read: refused before the helper is asked
    let mut g = fixture("ext-offline", (160, 90), FrameRate::FPS_24, 1);
    let item = g.s.project.sequence(g.seq).unwrap().video_tracks[0].items[0].item;
    let mut p = (*g.s.project).clone();
    if let Some(filmcraft_project::ItemKind::Media(m)) = p.item_mut(item).map(|i| &mut i.kind) {
        m.media = filmcraft_project::MediaRef::File { path: "/nonexistent/gone.mp4".into() };
    }
    g.s.project = std::sync::Arc::new(p);
    g.s.media = Default::default();
    let e = g.s.execute("ai.generativeExtend", json!({"wait": true, "confirm": true})).unwrap_err().to_string();
    assert!(e.contains("offline") && e.contains("relink"), "{e}");
    assert!(g.helper.started().is_empty(), "the helper was not contacted: {e}");
}

#[test]
fn parameters_are_checked_and_the_command_says_why_it_is_disabled() {
    let mut f = fixture("ext-params", (160, 90), FrameRate::FPS_24, 1);
    for bad in [json!({"seconds": 0}), json!({"seconds": 11}), json!({"seconds": "abc"}), json!({"side": "middle"}), json!({"prompt": "x".repeat(1001)})] {
        let mut p = bad.clone();
        p["confirm"] = json!(true);
        assert!(f.s.execute("ai.generativeExtend", p).is_err(), "{bad}");
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
    let spec = crate::find_command("ai.generativeExtend").unwrap();
    assert_eq!(spec.menu, ["Clip", "AI"]);
    assert!(spec.params.contains("\"confirm\"") && spec.params.contains("\"resume\""));
}

#[test]
fn the_quote_and_dont_ask_again_for_this_project() {
    // LTX makes 8k + 1 frames at 24 fps: 3 s asks for 73 frames, 3.04 s, about 0.12 USD
    let q = super::quote(None, 3.0);
    assert_eq!((q.model.as_str(), q.name.as_str(), q.rate), (super::DEFAULT_MODEL, "LTX-Video 13B distilled", Some(0.04)));
    assert!((q.billed_seconds - 73.0 / 24.0).abs() < 1e-9 && (q.usd.unwrap() - 0.04 * 73.0 / 24.0).abs() < 1e-9);
    let w = super::quote(Some("alibaba/wan-3.0-prime/image-to-video"), 2.2);
    assert_eq!((w.name.as_str(), w.rate, w.billed_seconds, w.usd), ("Wan 3.0 Prime", None, 3.0, None));
    assert_eq!(super::quote(Some("other/model"), f64::NAN).name, "other/model");
    let mut f = fixture("ext-dont-ask", (160, 90), FrameRate::FPS_24, 1);
    let e = f.s.execute("ai.generativeExtend", json!({})).unwrap_err().to_string();
    assert!(e.contains("LTX-Video 13B distilled") && e.contains("about 0.12 USD") && e.contains(super::RATES_READ), "{e}");
    // `dontAskAgain` without `confirm` changes nothing
    assert!(f.s.execute("ai.generativeExtend", json!({"dontAskAgain": true})).is_err());
    assert!(!f.s.project.settings.generative_extend_no_confirm);
    f.s.execute("ai.generativeExtend", json!({"confirm": true, "dontAskAgain": true, "wait": true})).unwrap();
    assert!(f.s.project.settings.generative_extend_no_confirm);
    // the next request in this project needs no `confirm`; the setting is saved with the project
    f.s.state.selection = vec![f.clip];
    f.s.execute("ai.generativeExtend", json!({"wait": true})).unwrap();
    assert_eq!(f.helper.started().len(), 2);
    let v = serde_json::to_value(&f.s.project.settings).unwrap();
    assert_eq!(v["generative_extend_no_confirm"], true);
    let fresh = serde_json::to_value(filmcraft_project::ProjectSettings::default()).unwrap();
    assert!(fresh.get("generative_extend_no_confirm").is_none(), "not written while off");
}
