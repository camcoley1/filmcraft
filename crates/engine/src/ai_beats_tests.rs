//! `ai.beatGrid` and `ai.musicFit` against the fake helper (a fixed 120 BPM grid: beats every
//! 0.5 s from 0.25 s of the decoded input, a downbeat every fourth), and `fit_plan` itself.

use filmcraft_project::{ClipId, Label};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::json;

use super::super::enhance::tests::with_audio;
use super::super::tests::{Fixture, snapshot};
use super::fit_plan;

const S: i64 = TICKS_PER_SECOND;

/// 120 BPM over the decoded input: beats at 0.25 + 0.5 k s, downbeats every 2 s.
fn grid(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let len = std::fs::metadata(&req.inputs[0]).map(|m| (m.len().saturating_sub(44)) as f64 / 4.0 / 22_050.0).unwrap_or(10.0);
    let beats: Vec<f64> = (0..).map(|k| 0.25 + 0.5 * k as f64).take_while(|t| *t < len).collect();
    let downbeats: Vec<f64> = beats.iter().copied().step_by(4).collect();
    let v = json!({"tempo": 120.0, "beats": beats, "downbeats": downbeats, "beatsPerBar": 4, "downbeatsEstimated": true});
    vec![("beats.json".into(), serde_json::to_vec(&v).unwrap())]
}

fn session(name: &str) -> (Fixture, ClipId) {
    let (mut f, clip, _) = with_audio(name, Script { features: vec!["beats".into()], result: ResultMode::Custom(grid), ..Default::default() }, 48_000);
    // the music clip alone, unlinked
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Unlink", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        for t in q.all_tracks_mut() {
            for it in &mut t.items {
                it.link = None;
            }
        }
        Ok(())
    })
    .unwrap();
    f.s.state.selection = vec![clip];
    (f, clip)
}

fn ticks(s: f64) -> Tick {
    Tick((s * S as f64).round() as i64)
}

#[test]
fn beat_markers_land_on_the_beats_in_one_undo_step() {
    let (mut f, clip) = session("bt-grid");
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.beatGrid", json!({"wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let q = f.s.active_sequence().unwrap();
    let (_, it) = q.find_item(clip).unwrap();
    // the clip plays source 1..5 s from timeline 0; beats of the source at 1.25, 1.75, … (the
    // decoded range starts at the clip's In, so the grid's 0.25 s is source 1.25 s)
    let marks: Vec<_> = q.markers.iter().collect();
    assert_eq!(marks.len(), 8, "{marks:?}");
    for (k, m) in marks.iter().enumerate() {
        let want = ticks(0.25 + 0.5 * k as f64);
        assert_eq!(m.start, want, "marker {k}");
        assert_eq!(it.source_time_at(m.start), ticks(1.25 + 0.5 * k as f64));
        let bar = k % 4 == 0;
        assert_eq!(m.color, if bar { Label::Rose } else { Label::Green });
        assert_eq!(m.name.starts_with("Bar"), bar, "{}", m.name);
    }
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Beat Grid");
}

#[test]
fn fit_plans_end_on_a_bar_within_half_a_beat() {
    let beats: Vec<Tick> = (0..60).map(|k| ticks(0.25 + 0.5 * k as f64)).collect();
    let downs: Vec<Tick> = beats.iter().copied().step_by(4).collect();
    // trims: any target up to the media, from any In
    for (source_in, target) in [(0.0, 10.3), (0.0, 7.9), (1.0, 13.0), (3.3, 4.6)] {
        let plan = fit_plan(&beats, &downs, ticks(source_in), ticks(target)).unwrap();
        assert_eq!(plan.len(), 1);
        let (a, b) = plan[0];
        assert!(a >= ticks(source_in) && beats.contains(&a) && downs.contains(&b), "{plan:?}");
        assert!(((b - a).0 - ticks(target).0).abs() <= ticks(0.25).0, "{source_in} {target}: {plan:?}");
    }
    // loops: longer than the media; consecutive whole bars, the last piece ending on a downbeat
    let short_beats: Vec<Tick> = beats.iter().copied().filter(|b| *b < ticks(8.0)).collect();
    let short_downs: Vec<Tick> = short_beats.iter().copied().step_by(4).collect();
    for target in [21.0, 30.4, 9.1] {
        let plan = fit_plan(&short_beats, &short_downs, Tick::ZERO, ticks(target)).unwrap();
        let total: i64 = plan.iter().map(|(a, b)| (*b - *a).0).sum();
        assert!((total - ticks(target).0).abs() <= ticks(0.25).0, "{target}: {plan:?}");
        for (k, (a, b)) in plan.iter().enumerate() {
            assert!(short_downs.contains(b), "{target}: piece {k} ends off a bar");
            if k > 0 {
                assert!(short_downs.contains(a));
            }
        }
    }
    assert!(fit_plan(&beats, &[], Tick::ZERO, ticks(5.0)).is_none());
}

#[test]
fn music_fit_loops_on_bars_and_ducks_in_one_undo_step() {
    let (mut f, clip) = session("bt-fit");
    let seq = f.s.state.active_sequence.unwrap();
    // nothing after the clip on its track
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.musicFit", json!({"seconds": 21.0, "duck": true, "wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let q = f.s.project.sequence(seq).unwrap().clone();
    let (track, first) = q.find_item(clip).map(|(t, i)| (t, i.clone())).unwrap();
    let pieces: Vec<_> = q.track(track).unwrap().items.iter().filter(|i| i.item == first.item).cloned().collect();
    assert!(pieces.len() >= 2, "looped: {}", pieces.len());
    let total: i64 = pieces.iter().map(|p| p.duration.0).sum();
    assert!((total - ticks(21.0).0).abs() <= ticks(0.5).0, "within a beat: {} s", total as f64 / S as f64);
    let mut at = first.start;
    for p in &pieces {
        assert_eq!(p.start, at, "pieces follow each other");
        at = p.end();
        assert!(p.essential.as_ref().is_some_and(|e| e.ducking.enabled), "ducking on");
        // every piece ends on a downbeat of the source: 1.25 + 2 k s
        let end = (p.source_in + p.duration).0 as f64 / S as f64;
        assert!(((end - 1.25) / 2.0 - ((end - 1.25) / 2.0).round()).abs() < 1e-6, "ends at {end} s");
    }
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Fit Music");
    f.s.execute("edit.undo", json!({})).unwrap();
    let q = f.s.project.sequence(seq).unwrap();
    assert_eq!(q.track(track).unwrap().items.iter().filter(|i| i.item == first.item).count(), 1);
}

#[test]
fn music_fit_refuses_what_it_cannot_fit() {
    let (mut f, clip) = session("bt-refuse");
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Fast", |pr, _| {
        pr.sequence_mut(seq).unwrap().audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap().speed = 2.0;
        Ok(())
    })
    .unwrap();
    let e = f.s.execute("ai.musicFit", json!({"seconds": 8.0})).unwrap_err().to_string();
    assert!(e.contains("100 % speed"), "{e}");
    assert!(f.s.execute("ai.musicFit", json!({})).is_err(), "seconds is needed");

    // another clip right after it: the track is not free
    let (mut g, clip) = session("bt-busy");
    let seq = g.s.state.active_sequence.unwrap();
    g.s.edit("Neighbour", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        let mut n = q.audio_tracks[0].items.iter().find(|i| i.id == clip).unwrap().clone();
        n.id = ClipId(9_999);
        n.start = n.end();
        q.audio_tracks[0].items.push(n);
        Ok(())
    })
    .unwrap();
    super::super::ensure_project_id(&mut g.s);
    let before = snapshot(&g.s);
    let e = g.s.execute("ai.musicFit", json!({"seconds": 12.0, "wait": true})).unwrap_err().to_string();
    assert!(e.contains("not free"), "{e}");
    assert_eq!(snapshot(&g.s), before);
}
