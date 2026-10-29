//! Clip ▸ Multi-Camera ▸ Auto-Switch by Speaker (`multicam.autoSwitch`).
//!
//! `multicam.autoSwitch {clip?, speakers?, wideCamera?, wideAfterSeconds?, minShotSeconds=2,
//! leadSeconds=0.1, in?, out?, videoOnly?}` cuts a multi-camera clip to whoever is talking. The
//! words come from the transcripts of the audio in the clip's multi-camera source sequence
//! ([`filmcraft_edit::transcript::sequence_words_nested`]: audio that shows a nested sequence is
//! followed, through its offsets), moved onto the outer timeline; each word's speaker label picks a
//! camera:
//!
//! - `speakers` maps a speaker name to a camera (1-based, in shown order, as `multicam.cutToCamera`
//!   takes it); a speaker not listed gets camera `n + 1` for speaker index `n`;
//! - a speaker with no camera of that number gets `wideCamera`, else camera 1 (reported as
//!   `unmappedSpeakers`).
//!
//! The clip is the one named by `clip`/`clips` (which must be an enabled multi-camera video clip:
//! nothing else is switched instead), else the selected multi-camera clip, else the one at the
//! playhead. A clip that was cut before (by an earlier run, `multicam.cutToCamera` or a recording)
//! is several pieces: the whole run of contiguous pieces of the same source is switched, so a
//! re-run is complete, and `in` / `out` (ticks, `inSeconds`, …) limit it.
//!
//! The cuts ([`filmcraft_edit::multicam_auto::auto_switch`]) are applied through the same path as a
//! recorded pass, as one undo step, to the clip's video and, with Audio Follows Video, its linked
//! multi-camera audio. Clips with a speed change, reverse or frame hold are refused.

use filmcraft_edit as edit;
use filmcraft_edit::multicam_auto::{AutoParams, SpeechWord, auto_switch};
use filmcraft_edit::transcript::sequence_words_nested;
use filmcraft_project::{TrackId, TrackItem, TrackKind};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, clip_p, clips_p, f64_p, has_seq, time_p};
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

/// The multi-camera clip to switch and the contiguous pieces of its source around it.
struct Target {
    track: TrackId,
    clip: TrackItem,
    /// The pieces (in time order) of the same source that follow each other without a gap and show
    /// a continuous stretch of it, `clip` among them.
    run: Vec<TrackItem>,
}

/// `clip`/`clips` named in the parameters (even if empty): an explicit target.
fn explicit(p: &Value) -> bool {
    p.get("clips").is_some_and(|v| !v.is_null()) || p.get("clip").is_some_and(|v| !v.is_null())
}

fn target(s: &Session, p: &Value) -> Result<Target> {
    let cmd = "multicam.autoSwitch";
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let usable = |i: &TrackItem| i.multicam.is_some_and(|m| m.enabled) && s.project.sequence(i.item).is_some();
    let found = |c| q.find_item(c).and_then(|(tid, it)| (q.track(tid)?.kind == TrackKind::Video && usable(it)).then(|| (tid, it.clone())));
    let anchor = if explicit(p) {
        // what was named, or an error: never another clip
        let named: Vec<_> = match p.get("clips").and_then(Value::as_array) {
            Some(a) => a.iter().filter_map(Value::as_u64).map(filmcraft_project::ClipId).collect(),
            None => clip_p(p, "clip").into_iter().collect(),
        };
        let first = named.first().copied();
        named.into_iter().find_map(found).ok_or_else(|| match first {
            Some(c) => bad(cmd, format!("clip {} is not an enabled multi-camera video clip", c.0)),
            None => bad(cmd, "`clip` / `clips` name no clip"),
        })?
    } else {
        clips_p(s, p)
            .into_iter()
            .find_map(found)
            .or_else(|| multicam_clip_at(s, s.playhead()))
            .ok_or_else(|| EngineError::Other("no multi-camera clip selected or at the playhead".into()))?
    };
    let (track, clip) = anchor;
    let items = &q.track(track).ok_or(EngineError::NoSequence)?.items;
    let at = items.iter().position(|i| i.id == clip.id).ok_or(EngineError::NoSequence)?;
    let follows = |a: &TrackItem, b: &TrackItem| a.item == b.item && usable(b) && a.end() == b.start && a.source_out() == b.source_in;
    let (mut lo, mut hi) = (at, at);
    while lo > 0 && follows(&items[lo - 1], &items[lo]) {
        lo -= 1;
    }
    while hi + 1 < items.len() && follows(&items[hi], &items[hi + 1]) {
        hi += 1;
    }
    let run: Vec<TrackItem> = items[lo..=hi].to_vec();
    if let Some(bad_piece) = run.iter().find(|i| (i.speed - 1.0).abs() > 1e-9 || i.reverse || i.frame_hold.is_some()) {
        return Err(bad(cmd, format!("clip {} has a speed change, reverse or frame hold: reset it first", bad_piece.id.0)));
    }
    Ok(Target { track, clip, run })
}

fn can_auto_switch(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    let t = target(s, &Value::Null).map_err(|e| e.to_string())?;
    let src = s.project.sequence(t.clip.item).ok_or("the clip has no multi-camera source")?;
    if sequence_words_nested(src, &s.project.transcripts, &|id| s.project.sequence(id)).is_empty() {
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
    let Target { track, clip, run } = target(s, p)?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    if q.track(track).is_none_or(|t| t.locked) {
        return Err(bad(cmd, "the clip's video track is locked"));
    }
    let src = s.project.sequence(clip.item).ok_or_else(|| bad(cmd, "the clip has no multi-camera source"))?;
    let shown = src.cameras().shown_angles();
    if shown.is_empty() {
        return Err(bad(cmd, "the multi-camera source has no camera shown"));
    }
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
    let mut explicit_map: std::collections::BTreeMap<String, u32> = Default::default();
    match p.get("speakers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (name, n) in m {
                explicit_map.insert(name.clone(), camera(n, &format!("the camera of {name:?}"))?);
            }
        }
        Some(_) => return Err(bad(cmd, "`speakers` must map speaker names to camera numbers")),
    }
    // range on the outer timeline: the whole run, limited by in / out
    let first = run.first().ok_or_else(|| bad(cmd, "no clip"))?;
    let last = run.last().ok_or_else(|| bad(cmd, "no clip"))?;
    let (mut lo, mut hi) = (first.start, last.end());
    if let Some(t) = time_p(s, p, "in") {
        lo = lo.max(t);
    }
    if let Some(t) = time_p(s, p, "out") {
        hi = hi.min(t);
    }
    if hi <= lo {
        return Err(bad(cmd, "the range is empty"));
    }
    // the speech on the outer timeline: the run shows one continuous stretch of the source
    let (s_in, s_out) = (first.source_in, last.source_out());
    let mut unmapped = std::collections::BTreeSet::new();
    let mut words = Vec::new();
    let mut labelled = 0usize;
    let mut labels_anywhere = false;
    let mut tracks_words = 0usize;
    for w in sequence_words_nested(src, &s.project.transcripts, &|id| s.project.sequence(id)) {
        tracks_words += 1;
        labels_anywhere |= w.speaker.is_some();
        if w.end <= s_in || w.start >= s_out {
            continue;
        }
        let at = |t: Tick| first.start + (t.max(s_in).min(s_out) - s_in);
        let (a, b) = (at(w.start).max(lo), at(w.end).min(hi));
        if b <= lo || a >= hi {
            continue;
        }
        let angle = w.speaker.as_ref().map(|name| {
            labelled += 1;
            let index = s.project.transcripts.get(&w.item).and_then(|t| t.words.get(w.index)).and_then(|x| x.speaker);
            explicit_map.get(name).copied().or_else(|| index.and_then(|i| shown.get(i as usize)).map(|a| *a as u32)).unwrap_or_else(|| {
                unmapped.insert(name.clone());
                wide.or_else(|| shown.first().map(|a| *a as u32)).unwrap_or(0)
            })
        });
        words.push(SpeechWord { start: a, end: b.max(a), angle });
    }
    if tracks_words == 0 || words.is_empty() {
        return Err(EngineError::Other("the clip has no transcribed speech in the range (Transcribe first)".into()));
    }
    if labelled == 0 {
        return Err(EngineError::Other(if labels_anywhere {
            "the words in the range have no speaker label (other parts of the transcript do)".into()
        } else {
            "the transcript has no speaker labels: run Label Speakers (transcript.diarize) first".into()
        }));
    }
    words.sort_by_key(|w| (w.start, w.end));
    let cuts = auto_switch(&words, TimeRange::from_bounds(lo, hi), &params);
    if cuts.is_empty() {
        return Err(EngineError::Other("no cuts: the range has no labelled speech".into()));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut tracks = vec![track];
    if s.state.multicam_audio_follows_video && !bool_p(p, "videoOnly").unwrap_or(false) {
        let links: Vec<u64> = run.iter().filter_map(|i| i.link).collect();
        let partners: Vec<_> = q
            .audio_tracks
            .iter()
            .filter(|t| t.items.iter().any(|i| i.multicam.is_some() && i.link.is_some_and(|l| links.contains(&l))))
            .map(|t| t.id)
            .collect();
        tracks.extend(partners);
    }
    let source = clip.item;
    let applied = cuts.clone();
    s.edit_sequence("Auto-Switch by Speaker", move |seq, ctx, _| Ok(edit::multicam::record(seq, &tracks, source, &applied, hi, ctx)))?;
    let list: Vec<Value> = cuts
        .iter()
        .map(|c| json!({"time": c.time.0, "seconds": c.time.seconds(), "angle": c.angle, "camera": shown.iter().position(|a| *a as u32 == c.angle).map(|i| i + 1)}))
        .collect();
    Ok(json!({"clip": clip.id.0, "pieces": run.len(), "cuts": list, "words": words.len(), "unmappedSpeakers": unmapped}))
}
