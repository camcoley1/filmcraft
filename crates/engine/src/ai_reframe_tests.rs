//! `ai.reframe` against the fake helper: Motion keyframes from the helper's crop path, one undo
//! step, a still subject, a hostile result, refused clips and a stale result.

use filmcraft_geom::Vec2;
use filmcraft_project::{Param, ParamValue};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::json;

use super::super::tests::{Fixture, finish, fixture, job_error, reason, snapshot};

/// The fixture's media is 64 x 36; a 9:16 crop of it is 20.25 x 36.
const CW: f64 = 36.0 * 9.0 / 16.0;

fn times(req: &JobRequest) -> Vec<f64> {
    req.params["times"].as_array().unwrap().iter().map(|t| t.as_f64().unwrap()).collect()
}

fn result(centers: Vec<[f64; 2]>) -> Vec<(String, Vec<u8>)> {
    let v = json!({"source": [64, 36], "crop": [CW, 36.0], "centers": centers, "subject": {"index": 0, "tracks": 1, "coverage": 1.0}});
    vec![("reframe.json".into(), serde_json::to_vec(&v).unwrap())]
}

/// The subject moves right at 10 px/s from x = 12.
fn moving(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    result(times(req).iter().map(|t| [(12.0 + 10.0 * t).clamp(CW / 2.0, 64.0 - CW / 2.0), 18.0]).collect())
}

fn still(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    result(times(req).iter().map(|_| [30.0, 18.0]).collect())
}

fn short(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    result(times(req).iter().skip(1).map(|_| [30.0, 18.0]).collect())
}

/// A 9:16 sequence (36 x 64) over the fixture's 64 x 36 clip, with a helper that offers reframe.
fn vertical(name: &str, write: filmcraft_sidecar::fake::Writer) -> Fixture {
    let mut f = fixture(name, Script { features: vec!["echo".into(), "reframe".into()], result: ResultMode::Custom(write), ..Default::default() });
    let seq = f.s.state.active_sequence.unwrap();
    let q = std::sync::Arc::make_mut(&mut f.s.project).sequence_mut(seq).unwrap();
    q.settings.width = 36;
    q.settings.height = 64;
    f
}

fn motion(f: &Fixture) -> filmcraft_project::EffectInstance {
    let q = f.s.active_sequence().unwrap();
    q.find_item(f.clip).unwrap().1.effect("motion").unwrap().clone()
}

#[test]
fn the_crop_follows_the_subject_in_one_undo_step() {
    let mut f = vertical("rf-move", moving);
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.reframe", json!({"wait": true, "detector": "brightest"})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = f.helper.started()[0].clone();
    assert_eq!(req.kind, "reframe");
    assert_eq!(req.inputs, ["/media/interview.mov"]);
    // 2 s at 6 samples a second, the sequence's aspect, the parameters
    assert_eq!(times(&req).len(), 12);
    assert_eq!(req.params["aspect"], json!([36.0, 64.0]));
    assert_eq!(req.params["detector"], "brightest");
    let m = motion(&f);
    let k = 36.0 / CW;
    assert!((m.f64_at("scale", Tick::ZERO) - k * 100.0).abs() < 1e-9);
    // every sample lands within a fraction of a pixel of the crop path
    for t in times(&req) {
        let cx = (12.0 + 10.0 * t).clamp(CW / 2.0, 64.0 - CW / 2.0);
        let want = Vec2::new(18.0 + (32.0 - cx) * k, 32.0);
        let got = m.vec2_at("position", Tick((t * TICKS_PER_SECOND as f64).round() as i64));
        assert!((got.x - want.x).abs() < 0.3 && (got.y - want.y).abs() < 1e-6, "{t}: {got:?} vs {want:?}");
    }
    let keys = m.param("position").unwrap().keyframes.len();
    assert!((2..12).contains(&keys), "straight runs are thinned: {keys} keyframes");
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Auto Reframe");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert!(motion(&f).param("position").is_none_or(|p| p.keyframes.is_empty()));
}

#[test]
fn a_still_subject_gives_a_still_frame() {
    let mut f = vertical("rf-still", still);
    f.s.execute("ai.reframe", json!({"wait": true})).unwrap();
    let p: Param = motion(&f).param("position").unwrap().clone();
    assert!(p.keyframes.len() <= 2, "{} keyframes", p.keyframes.len());
    let first = p.keyframes[0].value.clone();
    assert!(p.keyframes.iter().all(|k| k.value == first));
    let ParamValue::Vec2(v) = first else { panic!("not a point") };
    assert!((v.x - (18.0 + 2.0 * 36.0 / CW)).abs() < 1e-9);
}

#[test]
fn a_bad_result_changes_nothing() {
    let mut f = vertical("rf-bad", short);
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.reframe", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("one centre per sample"), "{e}");
    assert_eq!(snapshot(&f.s), before);
}

#[test]
fn clips_it_cannot_reframe_are_refused() {
    let mut f = vertical("rf-refuse", still);
    let c = f.clip;
    f.s.execute("effects.setParam", json!({"clip": c.0, "effect": "motion", "param": "rotation", "value": 15.0})).unwrap();
    let e = f.s.execute("ai.reframe", json!({})).unwrap_err().to_string();
    assert!(e.contains("Rotation"), "{e}");
    // the crop fills the sequence frame: only the sequence's aspect makes sense
    for (aspect, ok) in [
        (json!("9:16"), true),
        (json!([36, 64]), true),
        (json!([4, 5]), false),
        (json!("16:9"), false),
        (json!("nine"), false),
        (json!([0, 1]), false),
        (json!(3), false),
    ] {
        let mut g = vertical("rf-aspect", still);
        let r = g.s.execute("ai.reframe", json!({"aspect": aspect, "wait": true}));
        assert_eq!(r.is_ok(), ok, "{aspect}: {r:?}");
    }
    // a rotation is refused even with Motion switched off (the apply switches it on)
    let mut h = vertical("rf-off", still);
    let c = h.clip;
    h.s.execute("effects.setParam", json!({"clip": c.0, "effect": "motion", "param": "rotation", "value": 15.0})).unwrap();
    let seq = h.s.state.active_sequence.unwrap();
    h.s.edit("Motion off", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        q.video_tracks[0].items[0].effect_mut("motion").unwrap().enabled = false;
        Ok(())
    })
    .unwrap();
    let e = h.s.execute("ai.reframe", json!({})).unwrap_err().to_string();
    assert!(e.contains("Rotation"), "{e}");
}

#[test]
fn reframe_needs_the_feature_and_a_video_clip() {
    let f = fixture("rf-gate", Script::default());
    assert!(reason(&f.s, "ai.reframe").contains("does not provide `reframe`"));
    let mut g = vertical("rf-gate2", still);
    assert!(g.s.is_enabled("ai.reframe"));
    g.s.state.selection.clear();
    assert_eq!(reason(&g.s, "ai.reframe"), "select a video clip in the timeline");
}

#[test]
fn a_motion_edit_during_the_job_drops_the_result() {
    let mut f = vertical("rf-stale", moving);
    let id = f.s.execute("ai.reframe", json!({})).unwrap()["job"].as_u64().unwrap();
    let c = f.clip;
    f.s.execute("effects.setParam", json!({"clip": c.0, "effect": "motion", "param": "scale", "value": 120.0})).unwrap();
    finish(&mut f.s, id);
    assert!(job_error(&f.s, id).is_some_and(|e| e.contains("a target clip changed")), "{:?}", job_error(&f.s, id));
    assert!(motion(&f).param("position").is_none_or(|p| p.keyframes.is_empty()));
}

#[test]
fn cancel_mid_reframe_changes_nothing() {
    let mut f = fixture("rf-cancel", Script { features: vec!["reframe".into()], steps: 100_000, result: ResultMode::Custom(still), ..Default::default() });
    let id = f.s.execute("ai.reframe", json!({})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert!(motion(&f).param("position").is_none_or(|p| p.keyframes.is_empty()));
}

/// A path that curves: the thinned keyframes stay within THIN_PX of every sample.
fn parabola(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    result(times(req).iter().map(|t| [(10.125 + 6.0 * t * t).clamp(CW / 2.0, 64.0 - CW / 2.0), 18.0]).collect())
}

fn wrong_aspect(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let v = json!({"source": [64, 36], "crop": [30.0, 36.0], "centers": times(req).iter().map(|_| [32.0, 18.0]).collect::<Vec<_>>()});
    vec![("reframe.json".into(), serde_json::to_vec(&v).unwrap())]
}

/// Forward, reversed, sped-up and trimmed clips: at every sample the crop centre the helper
/// reported lands exactly on the frame centre after Motion (checked through the renderer's own
/// Motion matrix), and between samples the thinned path stays within THIN_PX.
#[test]
fn the_crop_centre_lands_on_the_frame_centre_for_any_clip_timing() {
    for (name, write, source_in, speed, reverse, fps) in [
        ("fwd", moving as filmcraft_sidecar::fake::Writer, 0.0, 1.0, false, 6.0),
        ("rev", moving, 0.0, 1.0, true, 6.0),
        ("fast", moving, 0.0, 2.0, false, 6.0),
        ("trim", moving, 0.5, 1.0, false, 6.0),
        ("curve", parabola, 0.0, 1.0, false, 30.0),
    ] {
        let mut f = vertical(&format!("rf-time-{name}"), write);
        let seq = f.s.state.active_sequence.unwrap();
        let tps = TICKS_PER_SECOND as f64;
        f.s.edit("Timing", |pr, _| {
            let it = &mut pr.sequence_mut(seq).unwrap().video_tracks[0].items[0];
            it.source_in = Tick((source_in * tps) as i64);
            it.speed = speed;
            it.reverse = reverse;
            it.duration = Tick(((2.0 - source_in) / speed * tps) as i64);
            Ok(())
        })
        .unwrap();
        f.s.execute("ai.reframe", json!({"wait": true, "sampleFps": fps})).unwrap();
        let req = f.helper.started()[0].clone();
        let q = f.s.active_sequence().unwrap();
        let (_, it) = q.find_item(f.clip).unwrap();
        let centre = |t: f64| -> f64 {
            if name == "curve" { (10.125 + 6.0 * t * t).clamp(CW / 2.0, 64.0 - CW / 2.0) } else { (12.0 + 10.0 * t).clamp(CW / 2.0, 64.0 - CW / 2.0) }
        };
        let ts = times(&req);
        if reverse {
            assert!(ts.windows(2).all(|w| w[0] > w[1]), "{name}: a reversed clip samples media time backwards");
        }
        for t in &ts {
            let mt = Tick((t * tps).round() as i64);
            let m = filmcraft_render::motion_matrix(q, it, (64, 36), None, mt);
            let p = m.apply(Vec2::new(centre(*t), 18.0));
            assert!((p.x - 18.0).abs() < 0.26 && (p.y - 32.0).abs() < 1e-6, "{name} at {t}: crop centre lands at {p:?}");
        }
    }
}

#[test]
fn a_crop_of_another_aspect_is_refused() {
    let mut f = vertical("rf-crop-aspect", wrong_aspect);
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.reframe", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("aspect"), "{e}");
    assert_eq!(snapshot(&f.s), before);
}
