//! Auto-Switch by Speaker (`multicam.autoSwitch`) and Label Speakers (`transcript.diarize`).

use filmcraft_project::ItemId;
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use crate::Session;
use crate::multicam_tests::{a1, mc_session, v1};

fn tk(sec: f64) -> i64 {
    (sec * TICKS_PER_SECOND as f64).round() as i64
}

/// `(text, start, end, speaker)` words as a `transcript.set` transcript.
fn transcript(words: &[(f64, f64, u32)]) -> Value {
    json!({
        "language": "en",
        "speakers": [{"name": "Speaker 1"}, {"name": "Speaker 2"}, {"name": "Speaker 3"}],
        "words": words.iter().enumerate().map(|(i, (a, b, sp))| json!({"text": format!("w{i}"), "start": tk(*a), "end": tk(*b), "speaker": sp})).collect::<Vec<_>>(),
    })
}

fn set_transcript(s: &mut Session, item: ItemId, words: &[(f64, f64, u32)]) {
    s.execute("transcript.set", json!({"item": item.0, "transcript": transcript(words)})).unwrap();
}

/// Frame of a cut list entry.
fn frames(r: &Value) -> Vec<(i64, u64)> {
    r["cuts"].as_array().unwrap().iter().map(|c| (FrameRate::FPS_24.frame_at(Tick(c["time"].as_i64().unwrap())), c["angle"].as_u64().unwrap())).collect()
}

const LEAD: f64 = 0.125;

/// Why a command is disabled ("" when it is enabled).
fn why(s: &Session, id: &str) -> String {
    crate::commands::find(id).and_then(|c| (c.enabled)(s).err()).unwrap_or_default()
}

#[test]
fn two_speakers_alternate_cameras_with_a_lead_and_one_undo_step() {
    let (mut s, items, _, _) = mc_session("camera1");
    // 4 s clip: speaker 1, speaker 2, speaker 1
    set_transcript(&mut s, items[0], &[(0.0, 0.5, 0), (0.6, 1.0, 0), (1.5, 2.0, 1), (2.1, 2.5, 1), (3.0, 3.5, 0)]);
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let r = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    // cuts: start, 1.5 - 0.125 = 1.375 s (frame 33), 3.0 - 0.125 = 2.875 s (frame 69)
    assert_eq!(frames(&r), [(0, 0), (33, 1), (69, 0)], "{r}");
    assert_eq!(v1(&s), [(0, 0), (33, 1), (69, 0)]);
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(*s.project, *before);
    assert_eq!(v1(&s), [(0, 0)]);
    // deterministic: the same run gives the same cuts
    let again = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    assert_eq!(frames(&again), frames(&r));
}

#[test]
fn explicit_speaker_cameras_and_audio_follows_video() {
    let (mut s, items, _, _) = mc_session("switch");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (1.5, 2.5, 1), (3.0, 3.8, 0)]);
    s.execute("multicam.audioFollowsVideo", json!({"enabled": true})).unwrap();
    let r = s.execute("multicam.autoSwitch", json!({"speakers": {"Speaker 1": 3, "Speaker 2": 2}, "minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    assert_eq!(frames(&r), [(0, 2), (33, 1), (69, 2)], "{r}");
    assert_eq!(v1(&s), [(0, 2), (33, 1), (69, 2)]);
    assert_eq!(a1(&s), v1(&s), "the linked audio followed");
    // video only leaves the audio alone
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("multicam.autoSwitch", json!({"speakers": {"Speaker 1": 3, "Speaker 2": 2}, "minShotSeconds": 0.5, "leadSeconds": LEAD, "videoOnly": true}))
        .unwrap();
    assert_ne!(a1(&s), v1(&s));
}

#[test]
fn short_turns_are_merged_and_cuts_never_land_inside_a_word() {
    let (mut s, items, _, _) = mc_session("camera1");
    // speaker 2 interjects 0.3 s inside speaker 1's talk, which has a long word at 2.0 to 3.2
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (1.2, 1.5, 1), (1.7, 2.0, 0), (2.0, 3.2, 0)]);
    let r = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 1.0, "leadSeconds": LEAD})).unwrap();
    assert_eq!(frames(&r), [(0, 0)], "the interjection is merged away: {r}");
    s.execute("edit.undo", json!({})).unwrap();
    // with a lead that would land inside the previous long word, the cut waits for the word to end
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (1.2, 2.45, 0), (2.5, 3.9, 1)]);
    let r = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.5, "leadSeconds": 0.2})).unwrap();
    let words = [(0.0, 1.0), (1.2, 2.45), (2.5, 3.9)];
    for c in r["cuts"].as_array().unwrap().iter().skip(1) {
        let t = c["time"].as_i64().unwrap();
        assert!(!words.iter().any(|(a, b)| tk(*a) < t && t < tk(*b)), "cut {t} inside a word");
    }
    assert!(r["cuts"].as_array().unwrap().len() >= 2);
}

#[test]
fn wide_camera_covers_a_long_silence() {
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 0.8, 0), (3.0, 3.8, 1)]);
    let r = s.execute("multicam.autoSwitch", json!({"wideCamera": 3, "wideAfterSeconds": 1.5, "minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    // speaker 1, wide from 0.8, speaker 2 from 2.875
    assert_eq!(frames(&r), [(0, 0), (19, 2), (69, 1)], "{r}");
}

#[test]
fn lead_time_and_range_are_respected() {
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (1.5, 2.5, 1), (3.0, 3.8, 0)]);
    let r = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.2, "leadSeconds": 0.0, "inSeconds": 1.0, "outSeconds": 3.5})).unwrap();
    // range 1.0 to 3.5: the range's first speech is speaker 2's (at 1.5), so the first shot is
    // speaker 2's camera from 1.0; the cut back to speaker 1 is exactly at the word start (lead 0)
    assert_eq!(frames(&r), [(24, 1), (72, 0)], "{r}");
    assert_eq!(v1(&s)[0], (0, 0), "before the range nothing changed");
}

#[test]
fn disabled_reasons_and_errors() {
    let (mut s, items, _, _) = mc_session("camera1");
    assert!(!s.is_enabled("multicam.autoSwitch"));
    assert!(why(&s, "multicam.autoSwitch").contains("transcript"));
    assert!(s.execute("multicam.autoSwitch", json!({})).is_err());
    // words without speakers
    s.execute(
        "transcript.set",
        json!({"item": items[0].0, "transcript": {"language": "en", "speakers": [], "words": [{"text": "hi", "start": tk(0.5), "end": tk(1.0)}]}}),
    )
    .unwrap();
    assert!(s.is_enabled("multicam.autoSwitch"));
    let e = s.execute("multicam.autoSwitch", json!({})).unwrap_err().to_string();
    assert!(e.contains("speaker labels"), "{e}");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0)]);
    for bad in [
        json!({"wideCamera": 9}),
        json!({"wideAfterSeconds": 1.0}),
        json!({"speakers": {"Speaker 1": 0}}),
        json!({"speakers": "x"}),
        json!({"minShotSeconds": -1.0}),
        json!({"leadSeconds": 1e9}),
    ] {
        assert!(s.execute("multicam.autoSwitch", bad.clone()).is_err(), "{bad}");
    }
    // a speaker with no camera falls back and is reported
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 2)]);
    let r = s.execute("multicam.autoSwitch", json!({"speakers": {"Speaker 1": 1}, "minShotSeconds": 0.1})).unwrap();
    assert_eq!(r["unmappedSpeakers"], json!([]), "speaker 3 maps to camera 3 by index: {r}");
    // no multi-camera clip
    let mut empty = Session::default();
    assert!(!empty.is_enabled("multicam.autoSwitch"));
    assert!(empty.execute("multicam.autoSwitch", json!({})).is_err());
}

#[test]
fn diarize_is_disabled_without_a_transcript_and_errors_cleanly() {
    let mut s = Session::default();
    assert!(!s.is_enabled("transcript.diarize"));
    assert!(s.execute("transcript.diarize", json!({})).is_err());
    let (mut s, items, _, _) = mc_session("camera1");
    assert!(why(&s, "transcript.diarize").contains("transcript"));
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (2.0, 3.0, 0)]);
    assert!(s.is_enabled("transcript.diarize"));
    assert!(s.execute("transcript.diarize", json!({"item": 999_999, "wait": true})).is_err());
    // a transcript on a clip with audio: the labels are replaced in one undo step
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let r = s.execute("transcript.diarize", json!({"item": items[0].0, "wait": true, "maxSpeakers": 4})).unwrap();
    assert!(r["speakers"].as_u64().unwrap_or(0) >= 1, "{r}");
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(*s.project, *before);
}

/// Two synthetic voices spoken one after the other: re-running diarization on a transcript whose
/// words all say "Speaker 1" finds two speakers, as one undo step. Skipped without `say`.
#[test]
fn diarize_finds_two_voices() {
    use std::process::Command;
    if !cfg!(target_os = "macos") || Command::new("say").arg("-v").arg("?").output().is_err() {
        eprintln!("skipped: needs macOS say");
        return;
    }
    let dir = crate::media_test_util::tmp_dir("diarize-voices");
    let say = |voice: &str, text: &str, name: &str| -> Option<Vec<f32>> {
        let aiff = dir.join(format!("{name}.aiff"));
        let wav = dir.join(format!("{name}.wav"));
        let ok = Command::new("say").args(["-v", voice, "-o"]).arg(&aiff).arg(text).status().ok()?.success()
            && Command::new("afconvert").args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"]).arg(&aiff).arg(&wav).status().ok()?.success();
        if !ok {
            return None;
        }
        let b = std::fs::read(&wav).ok()?;
        let pos = b.windows(4).position(|w| w == b"data")?;
        let n = u32::from_le_bytes(b.get(pos + 4..pos + 8)?.try_into().ok()?) as usize;
        let data = b.get(pos + 8..(pos + 8 + n).min(b.len()))?;
        Some(data.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect())
    };
    let lines = [
        ("Samantha", "The quarterly numbers look strong and the team is very happy with the progress."),
        ("Daniel", "I agree completely, although we should still watch the costs carefully this year."),
        ("Samantha", "Then let us schedule the review for next week and invite everybody involved."),
        ("Daniel", "That works for me, I will prepare the slides and send them over on Monday."),
    ];
    let mut audio: Vec<f32> = Vec::new();
    let mut words: Vec<(f64, f64, u32)> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for (k, (voice, text)) in lines.iter().enumerate() {
        let Some(samples) = say(voice, text, &format!("l{k}")) else {
            eprintln!("skipped: say/afconvert unavailable for {voice}");
            return;
        };
        let start = audio.len() as f64 / 16000.0;
        let dur = samples.len() as f64 / 16000.0;
        let ws: Vec<&str> = text.split(' ').collect();
        let each = (dur - 0.2) / ws.len() as f64;
        for (i, w) in ws.iter().enumerate() {
            words.push((start + 0.1 + each * i as f64, start + 0.1 + each * (i as f64 + 0.9), 0));
            texts.push((*w).to_string());
        }
        audio.extend(samples);
        audio.extend(std::iter::repeat_n(0.0f32, 16000));
    }
    let wav = dir.join("talk.wav");
    std::fs::write(&wav, crate::voiceover::write_wav_f32(&audio, 16000)).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [wav.to_string_lossy()]})).unwrap();
    let item = ItemId(r["items"][0].as_u64().unwrap());
    let tr = json!({
        "language": "en", "speakers": [{"name": "Speaker 1"}],
        "words": words.iter().zip(&texts).map(|((a, b, sp), t)| json!({"text": t, "start": tk(*a), "end": tk(*b), "speaker": sp})).collect::<Vec<_>>(),
    });
    s.execute("transcript.set", json!({"item": item.0, "transcript": tr})).unwrap();
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let r = s.execute("transcript.diarize", json!({"item": item.0, "wait": true})).unwrap();
    assert!(r["speakers"].as_u64().unwrap() >= 2, "two voices: {r}");
    assert_eq!(s.history.undo.len(), undo + 1);
    let t = s.project.transcripts.get(&item).unwrap().clone();
    let label = |line: usize| {
        let n: Vec<usize> = lines.iter().map(|l| l.1.split(' ').count()).collect();
        let a: usize = n[..line].iter().sum();
        let mid = a + n[line] / 2;
        t.words[mid].speaker
    };
    assert_eq!(label(0), label(2), "same voice, same label");
    assert_eq!(label(1), label(3));
    assert_ne!(label(0), label(1), "different voices, different labels");
    assert_eq!(t.words.len(), words.len(), "the words are untouched");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(*s.project, *before);
    let _ = std::fs::remove_dir_all(&dir);
}

fn set_clip(s: &mut Session, f: impl FnOnce(&mut filmcraft_project::TrackItem)) {
    let seq = s.state.active_sequence.unwrap();
    let mut p = (*s.project).clone();
    if let Some(it) = p.sequence_mut(seq).and_then(|q| q.video_tracks[0].items.first_mut()) {
        f(it);
    }
    s.project = std::sync::Arc::new(p);
}

#[test]
fn a_rerun_covers_every_piece_and_matches_a_fresh_run() {
    let words = [(0.0, 0.8, 0), (1.0, 1.4, 1), (1.6, 3.9, 0)];
    let (mut fresh, items, _, _) = mc_session("camera1");
    set_transcript(&mut fresh, items[0], &words);
    fresh.execute("multicam.autoSwitch", json!({"minShotSeconds": 2.0, "leadSeconds": LEAD})).unwrap();
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &words);
    let first = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.3, "leadSeconds": LEAD})).unwrap();
    assert!(first["cuts"].as_array().unwrap().len() >= 3, "{first}");
    assert!(v1(&s).len() >= 3);
    // the playhead is on the first piece only; the re-run still takes the whole run
    s.set_playhead(Tick::ZERO);
    let again = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 2.0, "leadSeconds": LEAD})).unwrap();
    assert!(again["pieces"].as_u64().unwrap() >= 3, "{again}");
    assert_eq!(v1(&s), v1(&fresh));
}

#[test]
fn explicit_targets_are_never_replaced_by_another_clip() {
    let (mut s, items, _, _) = mc_session("switch");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (1.5, 2.5, 1)]);
    let before = s.project.clone();
    // a bogus id, and the linked audio clip
    let audio = s.active_sequence().unwrap().audio_tracks[0].items[0].id.0;
    for clip in [999_999, audio] {
        let e = s.execute("multicam.autoSwitch", json!({"clip": clip, "minShotSeconds": 0.2})).unwrap_err().to_string();
        assert!(e.contains("is not an enabled multi-camera video clip"), "{e}");
        let e = s.execute("multicam.autoSwitch", json!({"clips": [clip], "minShotSeconds": 0.2})).unwrap_err().to_string();
        assert!(e.contains("is not an enabled multi-camera video clip"), "{e}");
    }
    assert_eq!(*s.project, *before, "nothing was switched");
    // the video clip by id works
    let video = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    s.execute("multicam.autoSwitch", json!({"clip": video, "minShotSeconds": 0.2, "leadSeconds": LEAD})).unwrap();
}

#[test]
fn speed_reverse_hold_and_locked_tracks_are_refused() {
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (1.5, 2.5, 1)]);
    let before = s.project.clone();
    for f in [
        (|c: &mut filmcraft_project::TrackItem| c.speed = 2.0) as fn(&mut filmcraft_project::TrackItem),
        |c| c.reverse = true,
        |c| c.frame_hold = Some(Tick::ZERO),
    ] {
        set_clip(&mut s, f);
        let e = s.execute("multicam.autoSwitch", json!({})).unwrap_err().to_string();
        assert!(e.contains("speed change, reverse or frame hold"), "{e}");
        s.project = before.clone();
    }
    let seq = s.state.active_sequence.unwrap();
    let mut p = (*s.project).clone();
    p.sequence_mut(seq).unwrap().video_tracks[0].locked = true;
    s.project = std::sync::Arc::new(p);
    let e = s.execute("multicam.autoSwitch", json!({})).unwrap_err().to_string();
    assert!(e.contains("locked"), "{e}");
}

#[test]
fn speakers_without_a_camera_fall_back_and_are_listed() {
    let (mut s, items, _, _) = mc_session("camera1");
    // the fourth speaker (index 3) has no camera 4 of three
    let tr = json!({
        "language": "en",
        "speakers": [{"name": "A"}, {"name": "B"}, {"name": "C"}, {"name": "D"}],
        "words": [
            {"text": "a", "start": tk(0.0), "end": tk(1.0), "speaker": 0},
            {"text": "d", "start": tk(1.5), "end": tk(2.5), "speaker": 3},
        ],
    });
    s.execute("transcript.set", json!({"item": items[0].0, "transcript": tr})).unwrap();
    let r = s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.2, "leadSeconds": LEAD})).unwrap();
    assert_eq!(r["unmappedSpeakers"], json!(["D"]), "{r}");
    assert_eq!(frames(&r), [(0, 0)], "camera 1 for both: {r}");
    s.execute("edit.undo", json!({})).unwrap();
    let r = s.execute("multicam.autoSwitch", json!({"wideCamera": 3, "minShotSeconds": 0.2, "leadSeconds": LEAD})).unwrap();
    assert_eq!(frames(&r), [(0, 0), (33, 2)], "the unmapped speaker is shown on the wide camera: {r}");
}

#[test]
fn one_speaker_after_a_long_gap_and_trailing_silence_go_wide() {
    let (mut s, items, _, _) = mc_session("camera1");
    // 4 s clip: speaker 1 at 0 to 0.5 and again 2.5 to 3.0, then 1 s of nothing
    set_transcript(&mut s, items[0], &[(0.0, 0.5, 0), (2.5, 3.0, 0)]);
    let r = s.execute("multicam.autoSwitch", json!({"wideCamera": 3, "wideAfterSeconds": 0.9, "minShotSeconds": 0.2, "leadSeconds": LEAD})).unwrap();
    // wide from 0.5, back to speaker 1 at 2.375 (frame 57), wide again from 3.0 (frame 72)
    assert_eq!(frames(&r), [(0, 0), (12, 2), (57, 0), (72, 2)], "{r}");
}

#[test]
fn label_speakers_follows_a_selected_multicam_clip_to_its_audio() {
    let (mut s, items, _, _) = mc_session("camera1");
    // two cameras have transcripts; the one with more words is the multi-camera audio's main one
    set_transcript(&mut s, items[0], &[(0.0, 0.5, 0), (0.6, 1.0, 0), (1.5, 2.0, 0), (2.5, 3.0, 0)]);
    set_transcript(&mut s, items[1], &[(0.0, 0.5, 0)]);
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    // nothing selected and two transcripts: the user is told what to do
    s.execute("timeline.select", json!({"clips": []})).unwrap();
    s.execute("project.select", json!({"items": []})).unwrap();
    let e = s.execute("transcript.diarize", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("select the clip or media"), "{e}");
    s.execute("timeline.select", json!({"clips": [clip]})).unwrap();
    let r = s.execute("transcript.diarize", json!({"wait": true})).unwrap();
    assert_eq!(r["item"], items[0].0, "{r}");
}

#[test]
fn a_cancelled_job_changes_nothing_and_a_stale_result_is_dropped() {
    use std::sync::atomic::Ordering;
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (2.0, 3.0, 0)]);
    // cancel flag set before the work starts: it stops at once
    let prog = filmcraft_export::Progress::default();
    prog.cancel.store(true, Ordering::Relaxed);
    let src = s.source(items[0]).unwrap();
    let base = (**s.project.transcripts.get(&items[0]).unwrap()).clone();
    let len = 16_000 * 4;
    let r = crate::transcript_diarize::label(&*src, len, &base, &filmcraft_speech::diarize::Params::default(), &prog);
    assert_eq!(r.unwrap_err(), "stopped");
    // a job started, then the transcript is replaced before it is applied
    let undo = s.history.undo.len();
    let r = s.execute("transcript.diarize", json!({"item": items[0].0})).unwrap();
    let job = r["job"].as_u64().unwrap();
    set_transcript(&mut s, items[0], &[(0.0, 0.5, 1), (1.0, 1.5, 1), (2.0, 2.5, 1)]);
    let edited = s.project.transcripts.get(&items[0]).unwrap().clone();
    for _ in 0..600 {
        if s.jobs.iter().find(|j| j.id == job).is_none_or(|j| j.progress.finished.load(Ordering::Relaxed)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    s.poll_persistence();
    assert!(std::sync::Arc::ptr_eq(s.project.transcripts.get(&items[0]).unwrap(), &edited), "the edited transcript stays");
    assert_eq!(s.history.undo.len(), undo + 1, "only the user's own edit is in the history");
    assert!(s.diarize_jobs.is_empty());
}

#[test]
fn a_job_that_gets_no_thread_is_not_left_pending() {
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 1.0, 0), (2.0, 3.0, 0)]);
    crate::transcript_diarize::fail_spawns(true);
    let e = s.execute("transcript.diarize", json!({"item": items[0].0})).unwrap_err().to_string();
    crate::transcript_diarize::fail_spawns(false);
    assert!(e.contains("can't start"), "{e}");
    assert!(s.diarize_jobs.is_empty() && s.jobs.is_empty());
    // and the next attempt is not refused as "already running"
    s.execute("transcript.diarize", json!({"item": items[0].0, "wait": true})).unwrap();
}

/// The chunked decode of Label Speakers equals one whole decode, also across the 60 s and 120 s
/// chunk boundaries of a 130 s file.
#[test]
fn chunked_decoding_matches_a_whole_decode_across_chunk_boundaries() {
    let dir = crate::media_test_util::tmp_dir("diarize-chunks");
    let rate = 48_000usize;
    let n = rate * 130;
    let mut r = 1u32;
    let samples: Vec<f32> = (0..n)
        .map(|i| {
            r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (i as f32 * 0.05).sin() * 0.3 + ((r >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.1
        })
        .collect();
    let wav = dir.join("long.wav");
    std::fs::write(&wav, crate::voiceover::write_wav_f32(&samples, rate as u32)).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [wav.to_string_lossy()]})).unwrap();
    let item = ItemId(r["items"][0].as_u64().unwrap());
    let whole = crate::transcript::item_audio(&s, item).unwrap();
    let src = s.source(item).unwrap();
    let prog = filmcraft_export::Progress::default();
    let chunked = crate::transcript_diarize::decode_chunked(&*src, whole.len() as i64, &prog).unwrap();
    assert_eq!(chunked.len(), whole.len());
    for centre in [60 * 16_000usize, 120 * 16_000] {
        let (a, b) = (centre - 400, centre + 400);
        let worst = (a..b).map(|i| (chunked[i] - whole[i]).abs()).fold(0f32, f32::max);
        assert!(worst < 1e-3, "around {} s: {worst}", centre / 16_000);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session whose clip has been cut into three pieces by a first run, then `mutate` applied to
/// the pieces; returns how many pieces a run anchored at piece `anchor` takes (or the error).
fn pieces_after(mutate: impl FnOnce(&mut Vec<filmcraft_project::TrackItem>), anchor: usize) -> std::result::Result<u64, String> {
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 0.5, 0), (0.6, 1.0, 0), (1.5, 2.0, 1), (2.1, 2.5, 1), (3.0, 3.5, 0)]);
    s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    let mut p = (*s.project).clone();
    mutate(&mut p.sequence_mut(seq).unwrap().video_tracks[0].items);
    let id = p.sequence(seq).unwrap().video_tracks[0].items[anchor].id.0;
    s.project = std::sync::Arc::new(p);
    s.execute("multicam.autoSwitch", json!({"clip": id, "minShotSeconds": 0.5, "leadSeconds": LEAD}))
        .map(|r| r["pieces"].as_u64().unwrap())
        .map_err(|e| e.to_string())
}

#[test]
fn a_run_stops_at_gaps_source_jumps_disabled_and_changed_pieces() {
    let frame = FrameRate::FPS_24.frame_duration();
    // untouched: all three pieces
    assert_eq!(pieces_after(|_| {}, 0), Ok(3));
    // a gap after piece 0: it is a run of one, and the rest another of two
    assert_eq!(pieces_after(|v| v[0].duration -= frame, 0), Ok(1));
    assert_eq!(pieces_after(|v| v[0].duration -= frame, 1), Ok(2));
    // a jump in the source after piece 1
    assert_eq!(pieces_after(|v| v[2].source_in += frame, 1), Ok(2));
    assert_eq!(pieces_after(|v| v[2].source_in += frame, 2), Ok(1));
    // a disabled piece and a piece with a speed change end the run instead of refusing it
    assert_eq!(pieces_after(|v| v[2].enabled = false, 0), Ok(2));
    assert_eq!(pieces_after(|v| v[2].speed = 2.0, 1), Ok(2));
    // but the targeted piece itself must be plain
    let e = pieces_after(|v| v[1].speed = 2.0, 1).unwrap_err();
    assert!(e.contains("speed change"), "{e}");
}

/// Pieces the run refuses to take stay exactly as they were, before the anchor as well as after it.
fn run_leaves_alone(mutate: impl FnOnce(&mut Vec<filmcraft_project::TrackItem>), anchor: usize, excluded: &[usize], pieces: u64) {
    let (mut s, items, _, _) = mc_session("camera1");
    set_transcript(&mut s, items[0], &[(0.0, 0.5, 0), (0.6, 1.0, 0), (1.5, 2.0, 1), (2.1, 2.5, 1), (3.0, 3.5, 0)]);
    s.execute("multicam.autoSwitch", json!({"minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    let mut p = (*s.project).clone();
    mutate(&mut p.sequence_mut(seq).unwrap().video_tracks[0].items);
    let before = p.sequence(seq).unwrap().video_tracks[0].items.clone();
    let id = before[anchor].id.0;
    s.project = std::sync::Arc::new(p);
    // a different transcript, so the run would change every piece it takes
    set_transcript(&mut s, items[0], &[(0.0, 0.9, 1), (1.5, 2.0, 0), (2.1, 3.9, 1)]);
    let r = s.execute("multicam.autoSwitch", json!({"clip": id, "minShotSeconds": 0.5, "leadSeconds": LEAD})).unwrap();
    assert_eq!(r["pieces"].as_u64(), Some(pieces), "{r}");
    let after = &s.active_sequence().unwrap().video_tracks[0].items;
    for &k in excluded {
        let b = &before[k];
        let a = after.iter().find(|c| c.id == b.id).expect("the piece is still there");
        assert_eq!((a.start, a.duration, a.multicam, a.enabled, a.speed), (b.start, b.duration, b.multicam, b.enabled, b.speed), "piece {k} was changed");
    }
}

#[test]
fn pieces_before_the_anchor_are_checked_like_the_ones_after_it() {
    let frame = FrameRate::FPS_24.frame_duration();
    // a disabled predecessor is not taken into the run (the anchor is piece 1: the run is 1..=2)
    run_leaves_alone(|v| v[0].enabled = false, 1, &[0], 2);
    // a predecessor with a speed change, and one that does not continue the source
    run_leaves_alone(|v| v[0].speed = 2.0, 1, &[0], 2);
    run_leaves_alone(|v| v[1].source_in += frame, 2, &[0, 1], 1);
    // a disabled piece two back stops the run there; the one next to the anchor is taken
    run_leaves_alone(|v| v[0].enabled = false, 2, &[0], 2);
    // the other way round still works: a disabled successor is left alone
    run_leaves_alone(|v| v[2].enabled = false, 0, &[2], 2);
}
