//! `ai.beatGrid` and `ai.musicFit` against the fake helper (a fixed 120 BPM grid: beats every
//! 0.5 s from 0.25 s of the decoded input, a downbeat every fourth), and `fit_plan` itself.

use filmcraft_project::{ClipId, ItemId, Label, TrackKind};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::json;

use super::super::enhance::tests::with_audio;
use super::super::tests::{Fixture, fixture, snapshot};
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

/// Beats every 0.51 s from 0.25 s of the decoded input, a downbeat every fourth: a bar is 2.04 s.
fn odd_grid(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let len = std::fs::metadata(&req.inputs[0]).map(|m| (m.len().saturating_sub(44)) as f64 / 4.0 / 22_050.0).unwrap_or(10.0);
    let beats: Vec<f64> = (0..).map(|k| 0.25 + 0.51 * k as f64).take_while(|t| *t < len).collect();
    let downbeats: Vec<f64> = beats.iter().copied().step_by(4).collect();
    let v = json!({"tempo": 60.0 / 0.51, "beats": beats, "downbeats": downbeats, "beatsPerBar": 4, "downbeatsEstimated": true});
    vec![("beats.json".into(), serde_json::to_vec(&v).unwrap())]
}

/// The loops below jump back four bars (8.16 s): this tone is half a cycle off after that, so a
/// hard cut flips its phase.
const TONE: f64 = 3570.5 / 8.16;

/// `seconds` of a 48 kHz stereo 16-bit sine at [`TONE`] Hz, half scale.
fn sine_wav(path: &std::path::Path, seconds: u32) {
    let n = 48_000 * seconds as usize;
    let data = (n * 4) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &1u16.to_le_bytes(),
        &2u16.to_le_bytes(),
        &48_000u32.to_le_bytes(),
        &(192_000u32).to_le_bytes(),
        &4u16.to_le_bytes(),
        &16u16.to_le_bytes(),
    ] {
        b.extend_from_slice(v);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..n {
        let v = ((2.0 * std::f64::consts::PI * TONE * i as f64 / 48_000.0).sin() * 16_000.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// The largest sample-to-sample step of the sequence's mix (left) in 10 ms around `t`.
fn largest_step(f: &Fixture, t: Tick) -> f32 {
    let seq = f.s.state.active_sequence.unwrap();
    let q = f.s.project.sequence(seq).unwrap();
    let provider = f.s.media.provider(f.s.project.clone(), f.s.services.clone());
    let at = t.0 / (TICKS_PER_SECOND / 48_000);
    let b = filmcraft_render::audio::mix_sequence(&f.s.project, q, at - 240, 480, &provider);
    b.channels[0].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

/// The loop joins of a music fit are crossfaded: a sine whose bars are not whole cycles plays
/// across every join with no larger step than anywhere else (without the crossfades it jumps).
#[test]
fn music_fit_loop_joins_do_not_click() {
    let mut f = fixture("bt-click", Script { features: vec!["beats".into()], result: ResultMode::Custom(odd_grid), ..Default::default() });
    f.s.ai.media_root = Some(f.cache.join("media"));
    std::fs::create_dir_all(&f.cache).unwrap();
    let wav = f.cache.join("tone.wav");
    sine_wav(&wav, 10);
    let item = ItemId(f.s.execute("file.import", json!({"paths": [wav.to_string_lossy()]})).unwrap()["items"][0].as_u64().unwrap());
    let seq = f.s.state.active_sequence.unwrap();
    let clip =
        f.s.edit("Place", |pr, _| {
            let r = pr.sequence(seq).unwrap().settings.frame_rate;
            let it = pr.make_track_item(item, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick(S), Tick(4 * S)), r).unwrap();
            let id = it.id;
            pr.sequence_mut(seq).unwrap().audio_tracks[0].items.push(it);
            Ok(id)
        })
        .unwrap();
    f.s.state.selection = vec![clip];
    f.s.execute("ai.musicFit", json!({"seconds": 20.0, "wait": true})).unwrap();
    let q = f.s.project.sequence(seq).unwrap().clone();
    let (track, _) = q.find_item(clip).unwrap();
    let t = q.track(track).unwrap();
    let joins: Vec<Tick> = t.items.iter().skip(1).map(|p| p.start).collect();
    assert!(joins.len() >= 2, "looped: {}", t.items.len());
    assert_eq!(t.transitions.len(), joins.len(), "one crossfade per join");
    let frame = q.settings.frame_rate.tick_of(1);
    for x in &t.transitions {
        assert_eq!((x.effect.effect.as_str(), x.duration), ("constant_gain", frame));
    }
    // a steady sine steps by at most 2 pi f / rate of its amplitude
    let normal = largest_step(&f, t.items[0].start + Tick(S));
    for j in &joins {
        let step = largest_step(&f, *j);
        assert!(step <= normal * 1.5, "join at {} s steps by {step}, elsewhere {normal}", j.0 as f64 / S as f64);
    }
    // the same cuts without the crossfades click
    f.s.edit("Hard cuts", |pr, _| {
        pr.sequence_mut(seq).unwrap().audio_tracks[0].transitions.clear();
        Ok(())
    })
    .unwrap();
    let worst = joins.iter().map(|j| largest_step(&f, *j)).fold(0.0, f32::max);
    assert!(worst > normal * 5.0, "hard cuts jump by {worst} (normal {normal})");
}

/// The clip's fade-out moves to the fitted end (End at Cut); the level is continuous across every
/// join (the copies carry the clip's own Volume); markers stay on the first piece.
#[test]
fn music_fit_moves_the_fade_out_and_keeps_the_level_across_joins() {
    let (mut f, clip) = session("bt-fade");
    let seq = f.s.state.active_sequence.unwrap();
    f.s.execute("sequence.applyAudioTransition", json!({"clip": clip.0, "edge": "out", "frames": 12})).unwrap();
    f.s.edit("Level", |pr, _| {
        let it = pr.sequence_mut(seq).unwrap().audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        let src = it.source_in;
        it.effect_mut("volume").unwrap().param_mut("level").unwrap().value = filmcraft_project::ParamValue::Float(-6.0);
        it.markers.push(filmcraft_project::Marker {
            id: filmcraft_project::MarkerId(77),
            start: src + ticks(2.0),
            duration: Tick::ZERO,
            name: "hit".into(),
            comment: String::new(),
            kind: filmcraft_project::MarkerKind::Comment,
            color: Label::Green,
        });
        Ok(())
    })
    .unwrap();
    let fade = f.s.project.sequence(seq).unwrap().audio_tracks[0].transitions[0].duration;
    f.s.execute("ai.musicFit", json!({"seconds": 21.0, "wait": true})).unwrap();
    let q = f.s.project.sequence(seq).unwrap();
    let t = &q.audio_tracks[0];
    let media = t.items.iter().find(|i| i.id == clip).unwrap().item;
    let pieces: Vec<_> = t.items.iter().filter(|i| i.item == media).collect();
    assert!(pieces.len() >= 2);
    let last = pieces.last().unwrap();
    let outs: Vec<_> = t.transitions.iter().filter(|x| x.to.is_none()).collect();
    assert_eq!(outs.len(), 1, "{:?}", t.transitions);
    assert_eq!((outs[0].from, outs[0].start + outs[0].duration, outs[0].duration), (Some(last.id), last.end(), fade), "the fade-out ends the fitted music");
    assert_eq!(outs[0].align, filmcraft_project::TransitionAlign::EndAtCut);
    let level = |p: &filmcraft_project::TrackItem, at: Tick| p.effect("volume").unwrap().f64_at("level", p.source_time_at(at));
    for w in pieces.windows(2) {
        let (a, b) = (w[0], w[1]);
        assert_eq!(a.end(), b.start);
        assert_eq!(level(a, a.end() - Tick(1)), level(b, b.start), "level jumps at the join at {:?}", b.start);
        assert_eq!(level(b, b.start), -6.0);
    }
    for p in &pieces {
        assert_eq!(p.markers.len(), usize::from(p.id == clip), "piece {:?}", p.id);
    }
}

/// Keyframed automation: a fit that loops is refused with the reason (copies cannot carry
/// media-time automation); a trim keeps the keys and the level curve unchanged.
#[test]
fn keyframed_clips_are_trimmed_but_not_looped() {
    let (mut f, clip) = session("bt-keys");
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Automate", |pr, _| {
        let it = pr.sequence_mut(seq).unwrap().audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        let src = it.source_in;
        it.effect_mut("volume").unwrap().param_mut("level").unwrap().keyframes = vec![
            filmcraft_project::Keyframe::new(src + ticks(0.5), filmcraft_project::ParamValue::Float(0.0)),
            filmcraft_project::Keyframe::new(src + ticks(1.5), filmcraft_project::ParamValue::Float(-30.0)),
        ];
        Ok(())
    })
    .unwrap();
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.musicFit", json!({"seconds": 21.0, "wait": true})).unwrap_err().to_string();
    assert!(e.contains("keyframed automation (volume level)"), "{e}");
    assert_eq!(snapshot(&f.s), before);
    let old = f.s.active_sequence().unwrap().find_item(clip).unwrap().1.clone();
    f.s.execute("ai.musicFit", json!({"seconds": 2.0, "wait": true})).unwrap();
    let new = f.s.active_sequence().unwrap().find_item(clip).unwrap().1.clone();
    assert!(new.duration < old.duration);
    assert_eq!(new.effect("volume").unwrap().param("level").unwrap().keyframes.len(), 2);
    for k in 0..20 {
        let at = new.start + Tick(new.duration.0 * k / 20);
        let lv = |p: &filmcraft_project::TrackItem| p.effect("volume").unwrap().f64_at("level", p.source_time_at(at));
        if new.source_time_at(at) == old.source_time_at(at) {
            assert_eq!(lv(&new), lv(&old), "at {at:?}");
        }
    }
}

/// A trim leaves a crossfade from the clip into its neighbour at the old cut: dropped.
#[test]
fn a_trim_drops_the_crossfade_into_the_neighbour() {
    let (mut f, clip) = session("bt-xfade");
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Neighbour", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        let mut n = q.audio_tracks[0].items.iter().find(|i| i.id == clip).unwrap().clone();
        n.id = ClipId(9_999);
        n.start = n.end();
        q.audio_tracks[0].items.push(n);
        Ok(())
    })
    .unwrap();
    f.s.execute("sequence.applyAudioTransition", json!({"clip": 9_999, "edge": "in", "frames": 12})).unwrap();
    assert_eq!(f.s.project.sequence(seq).unwrap().audio_tracks[0].transitions.len(), 1);
    f.s.execute("ai.musicFit", json!({"seconds": 2.0, "wait": true})).unwrap();
    let t = &f.s.project.sequence(seq).unwrap().audio_tracks[0];
    assert!(t.transitions.iter().all(|x| x.from != Some(clip)), "{:?}", t.transitions);
}

/// More than 500 pieces is refused before any edit.
#[test]
fn a_fit_of_more_than_500_pieces_is_refused() {
    let (mut f, _) = session("bt-cap");
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.musicFit", json!({"seconds": 4500.0, "wait": true})).unwrap_err().to_string();
    assert!(e.contains("at most 500"), "{e}");
    assert_eq!(snapshot(&f.s), before);
}

/// The user switches to another sequence while the fit runs: it lands in the clip's sequence, and
/// the user's active sequence is kept.
#[test]
fn a_fit_lands_in_its_sequence_when_another_is_active() {
    let (mut f, clip, _) =
        with_audio("bt-switch", Script { features: vec!["beats".into()], steps: 3, result: ResultMode::Custom(grid), ..Default::default() }, 48_000);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Unlink", |pr, _| {
        for t in pr.sequence_mut(seq).unwrap().all_tracks_mut() {
            for it in &mut t.items {
                it.link = None;
            }
        }
        Ok(())
    })
    .unwrap();
    f.s.state.selection = vec![clip];
    let id = f.s.execute("ai.musicFit", json!({"seconds": 12.0})).unwrap()["job"].as_u64().unwrap();
    let other = filmcraft_project::ItemId(f.s.execute("file.newSequence", json!({"name": "Other"})).unwrap()["sequence"].as_u64().unwrap());
    f.s.state.active_sequence = Some(other);
    super::super::tests::finish(&mut f.s, id);
    assert!(super::super::tests::job_error(&f.s, id).is_none(), "{:?}", super::super::tests::job_error(&f.s, id));
    assert_eq!(f.s.state.active_sequence, Some(other), "the user's sequence stays active");
    let q = f.s.project.sequence(seq).unwrap();
    let media = q.find_item(clip).unwrap().1.item;
    assert!(q.audio_tracks[0].items.iter().filter(|i| i.item == media).count() >= 2, "looped in the clip's sequence");
    assert!(!q.audio_tracks[0].transitions.is_empty(), "with its join crossfades");
}

/// Bars too sparse to land within half a beat: refused with a reason, nothing changes.
#[test]
fn a_fit_far_from_the_target_is_refused() {
    fn sparse(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
        // fast beats at the start, then one bar line 8.5 s later: the only fits are about 8.5 s
        let v = json!({"tempo": 1200.0, "beats": [0.25, 0.3, 0.35, 8.75], "downbeats": [0.25, 8.75], "beatsPerBar": 4, "downbeatsEstimated": true});
        vec![("beats.json".into(), serde_json::to_vec(&v).unwrap())]
    }
    let (mut f, _, _) = with_audio("bt-sparse", Script { features: vec!["beats".into()], result: ResultMode::Custom(sparse), ..Default::default() }, 48_000);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Unlink", |pr, _| {
        for t in pr.sequence_mut(seq).unwrap().all_tracks_mut() {
            for it in &mut t.items {
                it.link = None;
            }
        }
        Ok(())
    })
    .unwrap();
    let clip = f.s.project.sequence(seq).unwrap().audio_tracks[0].items[0].id;
    f.s.state.selection = vec![clip];
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.musicFit", json!({"seconds": 4.0, "wait": true})).unwrap_err().to_string();
    assert!(e.contains("more than half a beat"), "{e}");
    assert_eq!(snapshot(&f.s), before);
}

/// Hostile parameters are refused before any job.
#[test]
fn hostile_beat_parameters_are_refused() {
    let (mut f, _) = session("bt-hostile");
    for p in [json!({"beatsPerBar": 0}), json!({"beatsPerBar": 17}), json!({"beatsPerBar": "x"}), json!({"beatsPerBar": -3})] {
        assert!(f.s.execute("ai.beatGrid", p.clone()).is_err(), "{p}");
    }
    for p in [json!({"seconds": -1.0}), json!({"seconds": 0.0}), json!({"seconds": 1e300}), json!({"seconds": "x"}), json!({"seconds": 8.0, "beatsPerBar": 0})]
    {
        assert!(f.s.execute("ai.musicFit", p.clone()).is_err(), "{p}");
    }
    assert!(f.helper.started().is_empty());
}

/// Helper times past the analysed audio, or a downbeat that is not a beat: refused, nothing changes.
#[test]
fn malformed_beat_results_change_nothing() {
    fn late(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
        let v = json!({"tempo": 120.0, "beats": [0.25, 0.75, 500.0], "downbeats": [0.25], "beatsPerBar": 4, "downbeatsEstimated": true});
        vec![("beats.json".into(), serde_json::to_vec(&v).unwrap())]
    }
    fn stray(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
        let v = json!({"tempo": 120.0, "beats": [0.25, 0.75, 1.25], "downbeats": [0.25, 1.0], "beatsPerBar": 4, "downbeatsEstimated": true});
        vec![("beats.json".into(), serde_json::to_vec(&v).unwrap())]
    }
    for (name, write, why) in [("bt-late", late as filmcraft_sidecar::fake::Writer, "past the"), ("bt-stray", stray, "is not a beat")] {
        let (mut f, clip, _) = with_audio(name, Script { features: vec!["beats".into()], result: ResultMode::Custom(write), ..Default::default() }, 48_000);
        let seq = f.s.state.active_sequence.unwrap();
        f.s.edit("Unlink", |pr, _| {
            for t in pr.sequence_mut(seq).unwrap().all_tracks_mut() {
                for it in &mut t.items {
                    it.link = None;
                }
            }
            Ok(())
        })
        .unwrap();
        f.s.state.selection = vec![clip];
        super::super::ensure_project_id(&mut f.s);
        let before = snapshot(&f.s);
        for (cmd, p) in [("ai.beatGrid", json!({"wait": true})), ("ai.musicFit", json!({"seconds": 20.0, "wait": true}))] {
            let e = f.s.execute(cmd, p).unwrap_err().to_string();
            assert!(e.contains(why), "{name} {cmd}: {e}");
            assert_eq!(snapshot(&f.s), before, "{name} {cmd}");
        }
    }
}

/// Media whose duration is near the end of the tick range, played at its end: the helper's times
/// are added to the offset without overflow, and the markers land where the beats are.
#[test]
fn beats_near_the_end_of_the_tick_range() {
    let (mut f, clip) = session("bt-huge");
    let seq = f.s.state.active_sequence.unwrap();
    let item = f.s.active_sequence().unwrap().find_item(clip).unwrap().1.item;
    let end = i64::MAX - 7 * S;
    f.s.edit("Huge", |pr, _| {
        if let Some(filmcraft_project::ItemKind::Media(m)) = pr.item_mut(item).map(|i| &mut i.kind) {
            m.info.duration = Tick(end);
        }
        let it = pr.sequence_mut(seq).unwrap().audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        it.source_in = Tick(end - 4 * S);
        Ok(())
    })
    .unwrap();
    f.s.execute("ai.beatGrid", json!({"wait": true})).unwrap();
    let q = f.s.active_sequence().unwrap();
    let it = q.find_item(clip).unwrap().1;
    assert!(!q.markers.is_empty());
    for m in &q.markers {
        assert!(m.start >= it.start && m.start < it.end(), "{:?}", m.start);
    }
}
