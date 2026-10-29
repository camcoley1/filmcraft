//! `ai.beatGrid` and `ai.musicFit` (M6d): beats and bars of music, from the helper's `beats` job
//! (librosa beat tracking, each beat moved onto its transient).
//!
//! - `ai.beatGrid {clip?, beatsPerBar=4, wait=false}`: sequence markers at the clip's beats inside
//!   it, in one undo step ("Beat Grid"): "Beat" markers, and "Bar n" markers in another colour on
//!   the (estimated) downbeats. Markers already at the same time with the same name are kept.
//! - `ai.musicFit {clip?, seconds, beatsPerBar=4, duck=false, wait=false}`: makes a music clip
//!   last `seconds`, ending on a bar line, as timeline edits only: it is trimmed, or looped by
//!   putting further pieces of the same media after it (each from a downbeat to a downbeat), and
//!   it may start on a later beat of its first bar so the length lands within half a beat of the
//!   target. No new audio file is written. With `duck`, the pieces become Music with ducking
//!   against Dialogue and `essentialSound.generateDucking` writes the ducking keyframes, all in the
//!   same undo step ("Fit Music"). The clip must play forward at 100 %, without a frame hold, and
//!   not be linked to video; the track after it must be free for the fitted length.
//!
//! Beats are found in the clip's used range (`beatGrid`) or from its first source frame to the end
//! of the media (`musicFit`, which may need what follows), decoded mono at 22.05 kHz on that
//! rate's sample grid, so beat times map to media time exactly.

use filmcraft_project::essential::{AudioType, EssentialSound};
use filmcraft_project::{ClipId, Label, Marker, MarkerId, MarkerKind, TrackItem};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use super::audio::{AudioTarget, Refused, audio_target, decode_to_wav, timeline_time};
use crate::commands::{bool_p, clips_p, f64_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

pub(crate) const GRID_PARAMS: &str = r#"{"clip":id?,"beatsPerBar":1..16=4,"wait":bool=false}"#;
pub(crate) const FIT_PARAMS: &str =
    r#"{"clip":id?,"seconds":f (the length wanted),"beatsPerBar":1..16=4,"duck":bool=false (duck under Dialogue),"wait":bool=false}"#;

/// The analysis rate.
pub const RATE: u32 = 22_050;
/// Longest range analysed (20 minutes).
pub const MAX_SECONDS: f64 = 1200.0;
const MAX_RESULT_BYTES: u64 = 16 << 20;
const MAX_BEATS: usize = 20_000;

fn target(s: &Session, c: ClipId, to_end: bool) -> std::result::Result<AudioTarget, Refused> {
    audio_target(s, c, RATE, 0.0, MAX_SECONDS, to_end)
}

fn first_clip(s: &Session, p: &Value, to_end: bool) -> Result<AudioTarget> {
    let mut clips = clips_p(s, p);
    clips.sort_by_key(|c| target(s, *c, to_end).map_or(1, |t| u8::from(!t.audio_track)));
    for c in clips {
        match target(s, c, to_end) {
            Ok(t) => return Ok(t),
            Err(Refused::Hard(e)) => return Err(EngineError::Other(e)),
            Err(Refused::Skip) => {}
        }
    }
    Err(EngineError::Other("select a clip with audio in the timeline".into()))
}

fn has_audio(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    if s.state.selection.iter().any(|c| !matches!(target(s, *c, false), Err(Refused::Skip))) {
        Ok(())
    } else {
        Err("select a clip with audio in the timeline".into())
    }
}

pub(crate) fn grid_enabled(s: &Session) -> std::result::Result<(), String> {
    super::helper_ready(s, "beats")?;
    has_audio(s)
}

pub(crate) fn fit_enabled(s: &Session) -> std::result::Result<(), String> {
    super::helper_ready(s, "beats")?;
    has_audio(s)
}

fn beats_per_bar(p: &Value, cmd: &str) -> Result<u64> {
    let n = u64_p(p, "beatsPerBar").unwrap_or(4);
    if (1..=16).contains(&n) { Ok(n) } else { Err(crate::commands::bad(cmd, "`beatsPerBar` is 1 to 16")) }
}

fn start(s: &mut Session, t: AudioTarget, label: &str, bpb: u64, context: Value, wait: bool) -> Result<Value> {
    let src = s.source(t.item).ok_or_else(|| EngineError::Other(format!("{}: the media cannot be read", t.name)))?;
    let (stream, channels, offset, samples) = (t.stream, t.channels.clone(), t.offset, t.samples);
    let mut context = context;
    context["offset"] = json!(t.offset_ticks().0);
    let spec = super::JobSpec {
        kind: "beats",
        label,
        clips: &[t.clip],
        params: json!({"beatsPerBar": bpb}),
        context,
        timeout: Some(std::time::Duration::from_secs_f64(t.seconds() + 600.0)),
        prepare: Some(Box::new(move |dir: &std::path::Path, stop: &dyn Fn() -> bool, progress: &mut dyn FnMut(f64)| {
            let path = dir.join("source.wav");
            decode_to_wav(&*src, stream, &channels, true, offset, samples, RATE, &path, stop, progress)?;
            Ok(vec![path.to_string_lossy().to_string()])
        })),
        loose: false,
        media: false,
    };
    super::start_job(s, spec, wait)
}

pub(crate) fn run_grid(s: &mut Session, p: &Value) -> Result<Value> {
    let bpb = beats_per_bar(p, "ai.beatGrid")?;
    let t = first_clip(s, p, false)?;
    start(s, t, "Beat Grid", bpb, json!({"mode": "grid"}), bool_p(p, "wait").unwrap_or(false))
}

pub(crate) fn run_fit(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.musicFit";
    let bpb = beats_per_bar(p, cmd)?;
    let seconds = f64_p(p, "seconds").ok_or_else(|| crate::commands::bad(cmd, "need `seconds`"))?;
    if !(seconds.is_finite() && seconds > 0.0 && seconds <= MAX_SECONDS * 4.0) {
        return Err(crate::commands::bad(cmd, "`seconds` must be positive (at most 80 minutes)"));
    }
    let t = first_clip(s, p, true)?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let (_, it) = q.find_item(t.clip).ok_or(EngineError::NoSequence)?;
    if it.speed != 1.0 || it.reverse || it.frame_hold.is_some() {
        return Err(EngineError::Other(format!("{}: music fit needs the clip at 100 % speed, forward, without a frame hold", it.name)));
    }
    if it.link.is_some() {
        return Err(EngineError::Other(format!("{}: unlink the clip from its video first", it.name)));
    }
    let ctx =
        json!({"mode": "fit", "target": (seconds * TICKS_PER_SECOND as f64).round() as i64, "duck": bool_p(p, "duck").unwrap_or(false), "beatsPerBar": bpb});
    start(s, t, "Fit Music", bpb, ctx, bool_p(p, "wait").unwrap_or(false))
}

/// The helper's beats and downbeats, in media time.
fn read_result(pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<(Vec<Tick>, Vec<Tick>)> {
    let f =
        files.iter().find(|f| f.file_name().is_some_and(|n| n == "beats.json")).ok_or_else(|| EngineError::Other("the helper wrote no beats.json".into()))?;
    let mut file = filmcraft_sidecar::open_result(f, &pj.out_dir, MAX_RESULT_BYTES).map_err(|e| EngineError::Other(e.to_string()))?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).map_err(|e| EngineError::Other(format!("beats.json: {e}")))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| EngineError::Other(format!("beats.json: {e}")))?;
    let offset = pj.context.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let times = |k: &str| -> Result<Vec<Tick>> {
        let a = v.get(k).and_then(Value::as_array).ok_or_else(|| EngineError::Other(format!("beats.json: no {k}")))?;
        if a.len() > MAX_BEATS {
            return Err(EngineError::Other(format!("beats.json: more than {MAX_BEATS} {k}")));
        }
        let mut out: Vec<Tick> = Vec::with_capacity(a.len());
        for t in a {
            let s = t
                .as_f64()
                .filter(|s| s.is_finite() && *s >= 0.0 && *s <= MAX_SECONDS * 2.0)
                .ok_or_else(|| EngineError::Other(format!("beats.json: a bad time in {k}")))?;
            out.push(Tick(offset + (s * TICKS_PER_SECOND as f64).round() as i64));
        }
        out.sort();
        out.dedup();
        Ok(out)
    };
    let beats = times("beats")?;
    let downbeats = times("downbeats")?;
    Ok((beats, downbeats))
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let (beats, downbeats) = read_result(pj, files)?;
    match pj.context.get("mode").and_then(Value::as_str) {
        Some("grid") => apply_grid(s, pj, &beats, &downbeats),
        Some("fit") => apply_fit(s, pj, &beats, &downbeats),
        _ => Err(EngineError::Other("unknown beats job".into())),
    }
}

fn apply_grid(s: &mut Session, pj: &super::PendingAi, beats: &[Tick], downbeats: &[Tick]) -> Result<()> {
    let seq = pj.fingerprint.seq;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let it = s.project.sequence(seq).and_then(|q| q.find_item(clip)).map(|(_, it)| it.clone()).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
    let mut marks: Vec<(Tick, String, Label)> = Vec::new();
    let mut bar = 0;
    for b in beats {
        let Some(t) = timeline_time(&it, *b) else { continue };
        if downbeats.binary_search(b).is_ok() {
            bar += 1;
            marks.push((t, format!("Bar {bar}"), Label::Rose));
        } else {
            marks.push((t, "Beat".into(), Label::Green));
        }
    }
    if marks.is_empty() {
        return Err(EngineError::Other("no beats were found in the clip".into()));
    }
    let step: super::Step = Box::new(move |s: &mut Session| {
        s.edit("Beat Grid", move |pr, _| {
            let ids: Vec<u64> = marks.iter().map(|_| pr.alloc_id()).collect();
            let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            for ((t, name, color), id) in marks.into_iter().zip(ids) {
                if q.markers.iter().any(|m| m.start == t && m.name == name) {
                    continue;
                }
                let comment = if color == Label::Rose { "downbeat (estimated)".to_string() } else { String::new() };
                q.markers.push(Marker { id: MarkerId(id), start: t, duration: Tick::ZERO, name, comment, kind: MarkerKind::Comment, color });
            }
            q.markers.sort_by_key(|m| m.start);
            Ok(())
        })
    });
    super::apply_atomic(s, "Beat Grid", vec![step])
}

/// Pieces of media `(from, to)` that play one after another to last about `target`, ending on a
/// downbeat: the first starts at a beat at or after `source_in` (up to the end of the first bar,
/// so the length can land within half a beat), later ones loop whole bars from the first
/// downbeat. `None`
/// when there are not two downbeats to loop between.
pub(crate) fn fit_plan(beats: &[Tick], downbeats: &[Tick], source_in: Tick, target: Tick) -> Option<Vec<(Tick, Tick)>> {
    let downs: Vec<Tick> = downbeats.iter().copied().filter(|d| *d >= source_in).collect();
    let (&d_a, &d_z) = (downs.first()?, downs.last()?);
    // where the first piece may start: any beat from the clip's start up to the end of the first
    // bar (a pickup, or a later beat of the bar), so the length can land within half a beat
    let first_bar_end = downs.get(1).copied().unwrap_or(d_z);
    let starts: Vec<Tick> = beats.iter().copied().filter(|b| *b >= source_in && *b < first_bar_end.max(d_a + Tick(1))).collect();
    let starts = if starts.is_empty() { vec![d_a] } else { starts };
    let body = d_z - d_a;
    let mut best: Option<(i64, Vec<(Tick, Tick)>)> = None;
    for b in starts {
        // trim: end on the downbeat that makes the length closest to the target
        let mut plan: Vec<(Tick, Tick)>;
        let first_max = d_z - b;
        if first_max >= target || body.0 <= 0 {
            let e = *downs.iter().filter(|d| **d > b).min_by_key(|d| ((**d - b) - target).0.abs())?;
            plan = vec![(b, e)];
        } else {
            // loop: the first piece to the last downbeat, whole bodies, then part of a body
            plan = vec![(b, d_z)];
            let mut left = target - first_max;
            let mut guard = 0;
            while left >= body && guard < 10_000 {
                plan.push((d_a, d_z));
                left -= body;
                guard += 1;
            }
            let e = downs.iter().copied().min_by_key(|d| ((*d - d_a) - left).0.abs()).unwrap_or(d_a);
            if e > d_a {
                plan.push((d_a, e));
            }
        }
        let total: i64 = plan.iter().map(|(a, b)| (*b - *a).0).sum();
        let err = (total - target.0).abs();
        if best.as_ref().is_none_or(|(e, _)| err < *e) {
            best = Some((err, plan));
        }
    }
    best.map(|(_, p)| p)
}

fn apply_fit(s: &mut Session, pj: &super::PendingAi, beats: &[Tick], downbeats: &[Tick]) -> Result<()> {
    let seq = pj.fingerprint.seq;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let target = Tick(pj.context.get("target").and_then(Value::as_i64).unwrap_or(0));
    let duck = pj.context.get("duck").and_then(Value::as_bool).unwrap_or(false);
    let q = s.project.sequence(seq).ok_or(EngineError::NoSequence)?;
    let (track, it) = q.find_item(clip).map(|(t, it)| (t, it.clone())).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
    let plan = fit_plan(beats, downbeats, it.source_in, target).ok_or_else(|| EngineError::Other("not enough bars were found to fit the music".into()))?;
    let total: Tick = plan.iter().fold(Tick::ZERO, |acc, (a, b)| acc + (*b - *a));
    // the fitted clip may only cover the clip itself and empty track
    let tr = q.track(track).ok_or(EngineError::NoSequence)?;
    let end = it.start + total;
    if tr.items.iter().any(|o| o.id != clip && o.start < end && o.end() > it.start) {
        return Err(EngineError::Other("the track is not free for the fitted length".into()));
    }
    let step: super::Step = Box::new(move |s: &mut Session| {
        let ids = s.edit("Fit Music", move |pr, _| {
            let new_ids: Vec<u64> = plan.iter().skip(1).map(|_| pr.alloc_id()).collect();
            let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            let tr = q.all_tracks_mut().find(|t| t.id == track).ok_or(EngineError::NoSequence)?;
            let mut at = it.start;
            let mut ids = Vec::new();
            for (k, (a, b)) in plan.iter().enumerate() {
                let piece = if k == 0 {
                    tr.items.iter_mut().find(|o| o.id == clip).ok_or_else(|| EngineError::Other("the clip is gone".into()))?
                } else {
                    let mut c: TrackItem = it.clone();
                    c.id = ClipId(new_ids[k - 1]);
                    c.link = None;
                    c.group = None;
                    tr.items.push(c);
                    tr.items.last_mut().ok_or(EngineError::NoSequence)?
                };
                piece.start = at;
                piece.source_in = *a;
                piece.duration = *b - *a;
                if duck {
                    let mut es = piece.essential.clone().unwrap_or_else(|| EssentialSound::new(AudioType::Music));
                    es.kind = AudioType::Music;
                    es.ducking.enabled = true;
                    if !es.ducking.against.contains(&AudioType::Dialogue) {
                        es.ducking.against.push(AudioType::Dialogue);
                    }
                    piece.essential = Some(es);
                }
                ids.push(piece.id);
                at += *b - *a;
            }
            tr.items.sort_by_key(|o| o.start);
            Ok(ids)
        })?;
        if duck {
            s.execute("essentialSound.generateDucking", json!({"clips": ids.iter().map(|c| c.0).collect::<Vec<_>>()}))?;
        }
        Ok(())
    });
    super::apply_atomic(s, "Fit Music", vec![step])
}

#[cfg(test)]
#[path = "ai_beats_tests.rs"]
mod tests;
