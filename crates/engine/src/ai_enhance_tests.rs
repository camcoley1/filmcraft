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

/// The decoded input handed back as the result.
fn identity(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    vec![("enhanced.wav".into(), std::fs::read(&req.inputs[0]).unwrap_or_default())]
}

fn short(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x03\x00\x01\x00\x80\xbb\x00\x00\x00\xee\x02\x00\x04\x00\x20\x00data\x00\x00\x00\x00");
    vec![("enhanced.wav".into(), b)]
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
    assert_eq!((req.kind.as_str(), req.params["strength"].as_f64(), req.params["outRate"].as_u64()), ("enhance", Some(0.7), Some(48_000)));
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

#[test]
fn a_later_in_point_shifts_by_the_exact_offset_and_the_source_rate_comes_back() {
    let (mut f, clip, _) =
        with_audio("en-offset", Script { features: vec!["enhance".into()], result: ResultMode::Custom(identity), ..Default::default() }, 44_100);
    let seq = f.s.state.active_sequence.unwrap();
    f.s.edit("Later", |pr, _| {
        let q = pr.sequence_mut(seq).unwrap();
        let it = q.audio_tracks[0].items.iter_mut().find(|i| i.id == clip).unwrap();
        it.source_in = Tick(2_500_000_000_000 / 1000 * 1000 + 123);
        it.speed = 2.0;
        it.duration = Tick(TICKS_PER_SECOND);
        Ok(())
    })
    .unwrap();
    let before = audio_clip(&f, clip);
    f.s.execute("ai.enhanceSpeech", json!({"wait": true})).unwrap();
    assert_eq!(f.helper.started()[0].params["outRate"], 44_100, "resampled back to the source's rate");
    let after = audio_clip(&f, clip);
    let offset = before.source_in - after.source_in;
    assert_eq!(offset.0 % 5_292_000, 0, "the decoded range starts on a 48 kHz sample");
    assert!(offset > Tick::ZERO);
    for k in 0..20 {
        let t = before.start + Tick(before.duration.0 * k / 20);
        assert_eq!(after.source_time_at(t) + offset, before.source_time_at(t), "at {t:?}");
    }
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
