//! `ai.enhanceSpeech` against the fake helper (an identity "enhancement" that hands the decoded
//! input back): only the targeted audio clip is relinked, sample-accurately, in one undo step;
//! the original media and the linked video stay; bad results, stale results and cancel change
//! nothing.

use filmcraft_project::{ClipId, ItemId, TrackKind};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::json;

use super::super::tests::{Fixture, finish, fixture, job_error, reason, snapshot};

/// `seconds` of `rate` Hz stereo 16-bit PCM: silence with a click at 3.0 s.
pub(in crate::ai) fn click_wav(path: &std::path::Path, rate: u32, seconds: u32) {
    let n = (rate * seconds) as usize;
    let data = (n * 4) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &1u16.to_le_bytes(),
        &2u16.to_le_bytes(),
        &rate.to_le_bytes(),
        &(rate * 4).to_le_bytes(),
        &4u16.to_le_bytes(),
        &16u16.to_le_bytes(),
    ] {
        b.extend_from_slice(v);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..n {
        let v: i16 = if i == 3 * rate as usize { 30_000 } else { 0 };
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Each decoded input handed back as its result (an identity "enhancement"), resampled to the
/// rate asked for by nearest sample, so a click keeps its time to within a sample.
fn identity(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    req.inputs
        .iter()
        .enumerate()
        .map(|(k, input)| {
            let out = req.params["outRates"][k].as_u64().unwrap_or(48_000) as u32;
            (format!("enhanced-{k}.wav"), resample(&std::fs::read(input).unwrap_or_default(), out))
        })
        .collect()
}

/// A 32-bit float WAV (FilmCraft's decode) at `out` Hz, nearest-sample.
fn resample(b: &[u8], out: u32) -> Vec<u8> {
    let ch = u16::from_le_bytes([b[22], b[23]]) as usize;
    let rate = u32::from_le_bytes(b[24..28].try_into().unwrap());
    let frames: Vec<&[u8]> = b[44..].chunks_exact(ch * 4).collect();
    let n = (frames.len() as u64 * u64::from(out) + u64::from(rate) / 2) / u64::from(rate);
    let data = (n as usize * ch * 4) as u32;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &3u16.to_le_bytes(),
        &(ch as u16).to_le_bytes(),
        &out.to_le_bytes(),
        &(out * ch as u32 * 4).to_le_bytes(),
        &(ch as u16 * 4).to_le_bytes(),
        &32u16.to_le_bytes(),
    ] {
        w.extend_from_slice(v);
    }
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for j in 0..n {
        let i = ((j * u64::from(rate) + u64::from(out) / 2) / u64::from(out)) as usize;
        w.extend_from_slice(frames.get(i.min(frames.len() - 1)).unwrap());
    }
    w
}

fn short(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x03\x00\x01\x00\x80\xbb\x00\x00\x00\xee\x02\x00\x04\x00\x20\x00data\x00\x00\x00\x00");
    vec![("enhanced-0.wav".into(), b)]
}

/// The decoded input handed back at 48 kHz whatever rate was asked for (a helper that ignores
/// `outRates`).
fn wrong_rate(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    vec![("enhanced-0.wav".into(), std::fs::read(&req.inputs[0]).unwrap_or_default())]
}

/// A session whose sequence holds, besides the fixture's video clip, an audio clip of a 10 s
/// click WAV at `rate` (source In 1 s, 4 s long, linked to the video clip); returns it selected.
pub(in crate::ai) fn with_audio(name: &str, script: Script, rate: u32) -> (Fixture, ClipId, ItemId) {
    let mut f = fixture(name, script);
    f.s.ai.media_root = Some(f.cache.join("media"));
    std::fs::create_dir_all(&f.cache).unwrap();
    let wav = f.cache.join("voice.wav");
    click_wav(&wav, rate, 10);
    let item = ItemId(f.s.execute("file.import", json!({"paths": [wav.to_string_lossy()]})).unwrap()["items"][0].as_u64().unwrap());
    let seq = f.s.state.active_sequence.unwrap();
    let tps = TICKS_PER_SECOND;
    let clip =
        f.s.edit("Place", |pr, _| {
            let r = pr.sequence(seq).unwrap().settings.frame_rate;
            let mut it = pr.make_track_item(item, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick(tps), Tick(4 * tps)), r).unwrap();
            it.link = Some(9);
            let q = pr.sequence_mut(seq).unwrap();
            q.video_tracks[0].items[0].link = Some(9);
            let id = it.id;
            q.audio_tracks[0].items.push(it);
            Ok(id)
        })
        .unwrap();
    f.s.state.selection = vec![f.clip, clip];
    (f, clip, item)
}

fn audio_clip(f: &Fixture, c: ClipId) -> filmcraft_project::TrackItem {
    f.s.active_sequence().unwrap().find_item(c).unwrap().1.clone()
}

#[test]
fn only_the_audio_clip_is_relinked_sample_accurately_in_one_undo_step() {
    let (mut f, clip, item) =
        with_audio("en-relink", Script { features: vec!["enhance".into()], result: ResultMode::Custom(identity), ..Default::default() }, 48_000);
    let before = audio_clip(&f, clip);
    let video_before = f.s.active_sequence().unwrap().find_item(f.clip).unwrap().1.clone();
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.enhanceSpeech", json!({"wait": true, "strength": 0.7})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = f.helper.started()[0].clone();
    assert_eq!(f.helper.started().len(), 1, "the linked video clip gets no job of its own");
    assert_eq!((req.kind.as_str(), req.params["strength"].as_f64(), req.params["outRates"][0].as_u64()), ("enhance", Some(0.7), Some(48_000)));
    assert!(!std::path::Path::new(&req.inputs[0]).exists(), "the decoded input is deleted");
    let after = audio_clip(&f, clip);
    assert_ne!(after.item, item, "relinked to the enhanced file");
    assert_eq!((after.start, after.duration, after.speed, after.reverse), (before.start, before.duration, before.speed, before.reverse));
    // the decoded range starts 1 s before the clip's In (the pad), on the 48 kHz grid
    let offset = before.source_in - after.source_in;
    assert_eq!(offset, Tick(0), "a 1 s In with a 1 s pad starts at the top of the file");
    let new = f.s.project.item(after.item).and_then(|i| i.as_media()).unwrap();
    let filmcraft_project::MediaRef::File { path } = &new.media else { panic!("a file") };
    assert!(path.contains("AI Media") && std::path::Path::new(path).exists(), "{path}");
    // the click is where the source has it (identity enhancement): sample-accurate
    let b = std::fs::read(path).unwrap();
    let samples: Vec<f32> = b[44..].chunks_exact(8).map(|c| f32::from_le_bytes(c[..4].try_into().unwrap()).abs()).collect();
    let i = samples.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
    assert_eq!(Tick(i as i64 * 5_292_000) + offset, Tick(3 * TICKS_PER_SECOND));
    // the original media and the linked video are untouched
    assert!(f.s.project.item(item).is_some());
    assert_eq!(f.s.active_sequence().unwrap().find_item(f.clip).unwrap().1.item, video_before.item);
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Enhance Speech");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(audio_clip(&f, clip).item, item);
}

/// The loudest sample of a mono or stereo float WAV: (index, rate).
fn peak(path: &str) -> (usize, u32) {
    let b = std::fs::read(path).unwrap();
    let ch = u16::from_le_bytes([b[22], b[23]]) as usize;
    let rate = u32::from_le_bytes(b[24..28].try_into().unwrap());
    let i =
        b[44..].chunks_exact(ch * 4).map(|c| f32::from_le_bytes(c[..4].try_into().unwrap()).abs()).enumerate().max_by(|a, b| a.1.total_cmp(&b.1)).unwrap().0;
    (i, rate)
}

/// A later, sped-up In of a 44.1 kHz recording: the result comes back at 44.1 kHz and the click
/// plays at the same timeline time after the relink (within a sample of 44.1 kHz).
#[test]
fn a_later_in_point_shifts_by_the_exact_offset_and_the_source_rate_comes_back() {
    let (mut f, clip, _) =
        with_audio("en-offset", Script { features: vec!["enhance".into()], result: ResultMode::Custom(identity), ..Default::default() }, 44_100);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Later", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        let it = q.audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        it.source_in = Tick(TICKS_PER_SECOND * 5 / 2 + 123);
        it.speed = 2.0;
        it.duration = Tick(TICKS_PER_SECOND);
        Ok(())
    })
    .unwrap();
    let before = audio_clip(&f, clip);
    f.s.execute("ai.enhanceSpeech", json!({"wait": true})).unwrap();
    assert_eq!(f.helper.started()[0].params["outRates"][0], 44_100, "resampled back to the source's rate");
    let after = audio_clip(&f, clip);
    let offset = before.source_in - after.source_in;
    assert_eq!(offset.0 % 5_292_000, 0, "the decoded range starts on a 48 kHz sample");
    assert!(offset > Tick::ZERO);
    let new = f.s.project.item(after.item).and_then(|i| i.as_media()).unwrap();
    let filmcraft_project::MediaRef::File { path } = &new.media else { panic!("a file") };
    let (i, rate) = peak(path);
    assert_eq!(rate, 44_100);
    // the click is at 3.0 s of the source; in the new file at 3.0 s - offset; on the timeline the
    // clip shows it at the same time before and after
    let click_new = Tick(i as i64 * 5_760_000);
    assert!((click_new + offset - Tick(3 * TICKS_PER_SECOND)).0.abs() <= 5_760_000, "click at {click_new:?} + {offset:?}");
    let t = before.start + Tick(((Tick(3 * TICKS_PER_SECOND) - before.source_in).0 as f64 / 2.0) as i64);
    assert!((after.source_time_at(t) - click_new).0.abs() <= 2 * 5_760_000, "the timeline shows the click where it was");
}

/// A helper that ignores the rate asked for is refused (nothing changes).
#[test]
fn a_result_at_another_rate_is_refused() {
    let (mut f, _, _) =
        with_audio("en-rate", Script { features: vec!["enhance".into()], result: ResultMode::Custom(wrong_rate), ..Default::default() }, 44_100);
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.enhanceSpeech", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("48000 Hz, not 44100 Hz"), "{e}");
    assert_eq!(snapshot(&f.s), before);
}

/// The clip's Volume keyframes and markers are in media time: they move with the relink, so the
/// clip sounds the same at every timeline time (source In 3 s: a 2 s offset).
#[test]
fn keyframes_and_markers_follow_the_relink() {
    let (mut f, clip, _) =
        with_audio("en-keys", Script { features: vec!["enhance".into()], result: ResultMode::Custom(identity), ..Default::default() }, 48_000);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Fade", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        let it = q.audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        it.source_in = Tick(3 * TICKS_PER_SECOND);
        let at = |s: f64| Tick((s * TICKS_PER_SECOND as f64) as i64);
        let level = it.effect_mut("volume").unwrap().param_mut("level").unwrap();
        level.keyframes = vec![
            filmcraft_project::Keyframe::new(at(3.5), filmcraft_project::ParamValue::Float(0.0)),
            filmcraft_project::Keyframe::new(at(5.5), filmcraft_project::ParamValue::Float(-30.0)),
        ];
        it.markers.push(filmcraft_project::Marker {
            id: filmcraft_project::MarkerId(77),
            start: at(4.0),
            duration: Tick::ZERO,
            name: "line".into(),
            comment: String::new(),
            kind: filmcraft_project::MarkerKind::Comment,
            color: filmcraft_project::Label::Green,
        });
        Ok(())
    })
    .unwrap();
    let before = audio_clip(&f, clip);
    f.s.execute("ai.enhanceSpeech", json!({"wait": true})).unwrap();
    let after = audio_clip(&f, clip);
    let offset = before.source_in - after.source_in;
    assert_eq!(offset, Tick(2 * TICKS_PER_SECOND));
    for k in 0..40 {
        let t = before.start + Tick(before.duration.0 * k / 40);
        let level = |it: &filmcraft_project::TrackItem| it.effect("volume").unwrap().f64_at("level", it.source_time_at(t));
        assert_eq!(level(&after), level(&before), "at {t:?}");
    }
    assert_eq!(after.markers[0].start + offset, before.markers[0].start);
}

/// Several clips playing different ranges: one job (one input per range), one undo step.
#[test]
fn several_clips_are_enhanced_in_one_job_and_one_undo_step() {
    let (mut f, clip, item) =
        with_audio("en-multi", Script { features: vec!["enhance".into()], result: ResultMode::Custom(identity), ..Default::default() }, 48_000);
    let seq = f.s.state.active_sequence.unwrap();
    let second =
        f.s.edit("Second", |pr, _| {
            let r = pr.sequence(seq).unwrap().settings.frame_rate;
            let it = pr
                .make_track_item(item, TrackKind::Audio, Tick(10 * TICKS_PER_SECOND), TimeRange::new(Tick(6 * TICKS_PER_SECOND), Tick(2 * TICKS_PER_SECOND)), r)
                .unwrap();
            let id = it.id;
            pr.sequence_mut(seq).unwrap().audio_tracks[0].items.push(it);
            Ok(id)
        })
        .unwrap();
    f.s.state.selection = vec![clip, second];
    let undo = f.s.history.undo.len();
    f.s.execute("ai.enhanceSpeech", json!({"wait": true})).unwrap();
    assert_eq!(f.helper.started().len(), 1, "one job");
    assert_eq!(f.helper.started()[0].inputs.len(), 2, "one input per range");
    assert!(audio_clip(&f, clip).item != item && audio_clip(&f, second).item != item);
    assert_ne!(audio_clip(&f, clip).item, audio_clip(&f, second).item);
    assert_eq!(f.s.history.undo.len(), undo + 1, "one undo step");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!((audio_clip(&f, clip).item, audio_clip(&f, second).item), (item, item));
}

/// A remixed clip is refused (its plan plays pieces from anywhere in the media).
#[test]
fn remixed_clips_are_refused() {
    let (mut f, clip, _) = with_audio("en-remix", Script { features: vec!["enhance".into(), "beats".into()], ..Default::default() }, 48_000);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Remix", |pr, _| {
        let it = pr.sequence_mut(seq).unwrap().audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        let e: filmcraft_project::EffectInstance =
            serde_json::from_value(json!({"effect": filmcraft_render::remix::EFFECT, "enabled": true, "params": {}})).unwrap();
        it.effects.push(e);
        Ok(())
    })
    .unwrap();
    f.s.state.selection = vec![clip];
    for (cmd, p) in [("ai.enhanceSpeech", json!({})), ("ai.beatGrid", json!({})), ("ai.musicFit", json!({"seconds": 8.0}))] {
        let e = f.s.execute(cmd, p).unwrap_err().to_string();
        assert!(e.contains("remixed"), "{cmd}: {e}");
    }
    assert!(f.helper.started().is_empty());
}

#[test]
fn bad_stale_and_cancelled_results_change_nothing() {
    let (mut f, _, _) = with_audio("en-bad", Script { features: vec!["enhance".into()], result: ResultMode::Custom(short), ..Default::default() }, 48_000);
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.enhanceSpeech", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("not as long as the decoded source"), "{e}");
    assert_eq!(snapshot(&f.s), before);
    assert!(f.s.execute("ai.enhanceSpeech", json!({"strength": 1.5})).is_err());

    let (mut g, clip, item) =
        with_audio("en-stale", Script { features: vec!["enhance".into()], steps: 3, result: ResultMode::Custom(identity), ..Default::default() }, 48_000);
    let id = g.s.execute("ai.enhanceSpeech", json!({})).unwrap()["job"].as_u64().unwrap();
    g.s.execute("clip.audioGain", json!({"clips": [clip.0], "gainDb": 3.0})).ok();
    let seq = g.s.state.active_sequence.unwrap();
    g.s.edit("Move", |pr, _| {
        pr.sequence_mut(seq).unwrap().audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap().start = Tick(TICKS_PER_SECOND);
        Ok(())
    })
    .unwrap();
    finish(&mut g.s, id);
    assert!(job_error(&g.s, id).is_some_and(|e| e.contains("changed")), "{:?}", job_error(&g.s, id));
    assert_eq!(audio_clip(&g, clip).item, item);

    let (mut h, clip, item) = with_audio(
        "en-cancel",
        Script { features: vec!["enhance".into()], steps: 100_000, result: ResultMode::Custom(identity), ..Default::default() },
        48_000,
    );
    let id = h.s.execute("ai.enhanceSpeech", json!({})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    h.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut h.s, id);
    assert_eq!(job_error(&h.s, id).as_deref(), Some("stopped"));
    assert_eq!(audio_clip(&h, clip).item, item);
}

#[test]
fn enhance_needs_the_feature_and_an_audio_clip() {
    let f = fixture("en-gate", Script { features: vec!["enhance".into()], ..Default::default() });
    // only the fixture's video clip (generated media) is selected
    assert_eq!(reason(&f.s, "ai.enhanceSpeech"), "select an audio clip in the timeline");
    let (g, _, _) = with_audio("en-gate2", Script::default(), 48_000);
    assert!(reason(&g.s, "ai.enhanceSpeech").contains("does not provide `enhance`"));
}
