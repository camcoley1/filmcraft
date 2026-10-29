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
    // range 1.0 to 3.5: starts at 1.0 on speaker 1's camera (the range's first speech is speaker 2 at
    // 1.5, so the first shot is speaker 2's), cuts exactly at the word start
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
