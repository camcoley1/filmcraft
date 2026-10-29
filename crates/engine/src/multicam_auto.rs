//! Clip ▸ Multi-Camera ▸ Auto-Switch by Speaker (`multicam.autoSwitch`).
//!
//! `multicam.autoSwitch {clip?, speakers?, wideCamera?, wideAfterSeconds?, minShotSeconds=2,
//! leadSeconds=0.1, in?, out?, videoOnly?}` cuts a multi-camera clip to whoever is talking. The
//! words come from the transcripts of the audio in the clip's multi-camera source sequence
//! ([`filmcraft_edit::transcript::sequence_words`]), moved onto the outer timeline; each word's
//! speaker label picks a camera:
//!
//! - `speakers` maps a speaker name to a camera (1-based, in shown order, as `multicam.cutToCamera`
//!   takes it); a speaker not listed gets camera `n + 1` for speaker index `n`;
//! - a speaker with no camera of that number gets `wideCamera`, else camera 1 (reported as
//!   `unmappedSpeakers`).
//!
//! The cuts ([`filmcraft_edit::multicam_auto::auto_switch`]) are applied through the same path as a
//! recorded pass, as one undo step, to the clip's video and, with Audio Follows Video, its linked
//! multi-camera audio. The clip is the one named by `clip`, else the selected multi-camera clip,
//! else the one at the playhead. `in` / `out` (ticks, `inSeconds`, …) limit the range.

use filmcraft_edit as edit;
use filmcraft_edit::multicam_auto::{AutoParams, SpeechWord, auto_switch};
use filmcraft_edit::transcript::sequence_words;
use filmcraft_project::{TrackId, TrackItem};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, clips_p, f64_p, has_seq, time_p};
use crate::multicam::multicam_clip_at;
use crate::{EngineError, Result, Session};

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "multicam.autoSwitch",
        label: "Auto-Switch by Speaker",
        menu: &["Clip", "Multi-Camera"],
        shortcut: None,
        params: r#"{"clip":id?,"speakers":{"Speaker 1":camera,..}?,"wideCamera":camera?,"wideAfterSeconds":f?,"minShotSeconds":f=2,"leadSeconds":f=0.1,"in":ticks?,"out":ticks?,"videoOnly":bool?}"#,
        enabled: can_auto_switch,
        run: auto_switch_cmd,
        journal: true,
    }]
}

/// The multi-camera clip to switch: `clip`, the selection, or the clip at the playhead.
fn target_clip(s: &Session, p: &Value) -> Option<(TrackId, TrackItem)> {
    let q = s.active_sequence()?;
    let ok = |i: &TrackItem| i.multicam.is_some_and(|m| m.enabled) && s.project.sequence(i.item).is_some();
    let named = clips_p(s, p).into_iter().find_map(|c| {
        let (tid, it) = q.find_item(c)?;
        (q.track(tid)?.kind == filmcraft_project::TrackKind::Video && ok(it)).then(|| (tid, it.clone()))
    });
    named.or_else(|| multicam_clip_at(s, s.playhead()))
}

fn can_auto_switch(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    let (_, clip) = target_clip(s, &Value::Null).ok_or("select a multi-camera clip (or park the playhead on one)")?;
    let src = s.project.sequence(clip.item).ok_or("the clip has no multi-camera source")?;
    if sequence_words(src, &s.project.transcripts).is_empty() {
        return Err("the multi-camera audio has no transcript (Transcribe first)".into());
    }
    Ok(())
}

fn seconds_p(p: &Value, key: &str, default: f64, cmd: &str) -> Result<Tick> {
    let v = f64_p(p, key).unwrap_or(default);
    if !v.is_finite() || !(0.0..=3600.0).contains(&v) {
        return Err(bad(cmd, format!("`{key}` must be between 0 and 3600 seconds")));
    }
    Ok(Tick::from_seconds_f64(v))
}

fn auto_switch_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "multicam.autoSwitch";
    if s.mcrec.active() {
        return Err(bad(cmd, "stop the multi-camera recording first"));
    }
    let (track, clip) = target_clip(s, p).ok_or_else(|| EngineError::Other("no multi-camera clip selected or at the playhead".into()))?;
    if (clip.speed - 1.0).abs() > 1e-9 || clip.reverse || clip.frame_hold.is_some() {
        return Err(bad(cmd, "the clip has a speed change, reverse or frame hold: reset it first"));
    }
    let src = s.project.sequence(clip.item).ok_or_else(|| bad(cmd, "the clip has no multi-camera source"))?;
    let shown = src.cameras().shown_angles();
    let camera = |n: &Value, what: &str| -> Result<u32> {
        let n = n.as_u64().filter(|n| *n >= 1).ok_or_else(|| bad(cmd, format!("{what} must be a camera number (1 or more)")))? as usize;
        shown.get(n - 1).map(|a| *a as u32).ok_or_else(|| bad(cmd, format!("there is no camera {n} ({} shown)", shown.len())))
    };
    let wide = p.get("wideCamera").filter(|v| !v.is_null()).map(|v| camera(v, "`wideCamera`")).transpose()?;
    let wide_after = match f64_p(p, "wideAfterSeconds") {
        Some(_) if wide.is_none() => return Err(bad(cmd, "`wideAfterSeconds` needs `wideCamera`")),
        Some(_) => Some(seconds_p(p, "wideAfterSeconds", 0.0, cmd)?),
        None => None,
    };
    let params = AutoParams { wide, min_shot: seconds_p(p, "minShotSeconds", 2.0, cmd)?, wide_after, lead: seconds_p(p, "leadSeconds", 0.1, cmd)? };
    let mut explicit: std::collections::BTreeMap<String, u32> = Default::default();
    match p.get("speakers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (name, n) in m {
                explicit.insert(name.clone(), camera(n, &format!("the camera of {name:?}"))?);
            }
        }
        Some(_) => return Err(bad(cmd, "`speakers` must map speaker names to camera numbers")),
    }
    // range on the outer timeline
    let mut lo = clip.start;
    let mut hi = clip.end();
    if let Some(t) = time_p(s, p, "in") {
        lo = lo.max(t);
    }
    if let Some(t) = time_p(s, p, "out") {
        hi = hi.min(t);
    }
    if hi <= lo {
        return Err(bad(cmd, "the range is empty"));
    }
    // the clip's speech on the outer timeline
    let (s_in, s_out) = (clip.source_in, clip.source_out());
    let mut unmapped = std::collections::BTreeSet::new();
    let mut words = Vec::new();
    let mut labelled = 0usize;
    for w in sequence_words(src, &s.project.transcripts) {
        if w.end <= s_in || w.start >= s_out {
            continue;
        }
        let at = |t: Tick| clip.start + (t.max(s_in).min(s_out) - s_in);
        let (a, b) = (at(w.start).max(lo), at(w.end).min(hi));
        if b <= lo || a >= hi {
            continue;
        }
        let angle = w.speaker.as_ref().map(|name| {
            labelled += 1;
            let index = s.project.transcripts.get(&w.item).and_then(|t| t.words.get(w.index)).and_then(|x| x.speaker);
            explicit.get(name).copied().or_else(|| index.and_then(|i| shown.get(i as usize)).map(|a| *a as u32)).unwrap_or_else(|| {
                unmapped.insert(name.clone());
                wide.or_else(|| shown.first().map(|a| *a as u32)).unwrap_or(0)
            })
        });
        words.push(SpeechWord { start: a, end: b.max(a), angle });
    }
    if words.is_empty() {
        return Err(EngineError::Other("the clip has no transcribed speech in the range".into()));
    }
    if labelled == 0 {
        return Err(EngineError::Other("the transcript has no speaker labels: run Label Speakers (transcript.diarize) first".into()));
    }
    words.sort_by_key(|w| (w.start, w.end));
    let cuts = auto_switch(&words, TimeRange::from_bounds(lo, hi), &params);
    if cuts.is_empty() {
        return Err(EngineError::Other("no cuts: the range has no labelled speech".into()));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut tracks = vec![track];
    if s.state.multicam_audio_follows_video
        && !bool_p(p, "videoOnly").unwrap_or(false)
        && let Some(l) = clip.link
    {
        let partners: Vec<_> = q.audio_tracks.iter().filter(|t| t.items.iter().any(|i| i.link == Some(l) && i.multicam.is_some())).map(|t| t.id).collect();
        tracks.extend(partners);
    }
    let source = clip.item;
    let applied = cuts.clone();
    s.edit_sequence("Auto-Switch by Speaker", move |seq, ctx, _| Ok(edit::multicam::record(seq, &tracks, source, &applied, hi, ctx)))?;
    let list: Vec<Value> = cuts
        .iter()
        .map(|c| json!({"time": c.time.0, "seconds": c.time.seconds(), "angle": c.angle, "camera": shown.iter().position(|a| *a as u32 == c.angle).map(|i| i + 1)}))
        .collect();
    Ok(json!({"clip": clip.id.0, "cuts": list, "words": words.len(), "unmappedSpeakers": unmapped}))
}
