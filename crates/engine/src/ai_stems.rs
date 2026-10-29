//! `ai.stems` (M6b): split a clip's audio into vocals, drums, bass and other.
//!
//! `ai.stems {clips?, place=false, wait=false}` starts one helper job per selected clip whose media
//! has audio. The job's thread decodes the clip's WHOLE source (at most [`MAX_SECONDS`]) to a
//! 44.1 kHz float WAV in the job folder (FilmCraft's own decoders: the helper needs none), and the
//! helper splits that, so every stem is time-aligned with the source: same length, 44.1 kHz (the
//! model's rate), the source's channels (mono stays mono, more than two become the first two). The result is applied in one
//! undo step ("Extract Stems"): the four WAV files are imported into a new bin "Stems: <clip>"
//! and, with `place`, put on four new audio tracks under the clip with its exact placement
//! (start, source In, duration, speed, direction). A stem whose length differs from the source's
//! fails the apply and nothing is added.

use std::time::Duration;

use filmcraft_project::{BinId, ClipId, ItemId, MediaRef, TrackId, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{bool_p, clips_p, has_seq};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clips":[id]?,"place":bool=false (also put the stems on new audio tracks under the clip),"bandwidth":"16k"|"11k"="16k" (how high the model separates; above it the stems are silent),"wait":bool=false}"#;

/// The stems, in track order.
pub const STEMS: [&str; 4] = ["vocals", "drums", "bass", "other"];
/// Longest source separated (20 minutes).
pub const MAX_SECONDS: f64 = 1200.0;
/// Largest stem file read (a 20-minute 48 kHz stereo float WAV is about 460 MB).
const MAX_STEM_BYTES: u64 = 2 << 30;
/// A stem may differ from the source's length by this much (encoder padding).
const LENGTH_SLACK_S: f64 = 0.05;

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    super::helper_ready(s, "stems")?;
    has_seq(s)?;
    if s.state.selection.iter().any(|c| media_of(s, *c).is_ok()) { Ok(()) } else { Err("select a clip with audio in the timeline".into()) }
}

/// The clip's source file and its duration in seconds, or why it cannot be split.
fn media_of(s: &Session, c: ClipId) -> std::result::Result<(String, f64), String> {
    let q = s.active_sequence().ok_or("no sequence is open")?;
    let (_, it) = q.find_item(c).ok_or("the clip is not in the open sequence")?;
    let (_, m, _) = s.project.resolve_media(it.item).ok_or_else(|| format!("{}: no media", it.name))?;
    let MediaRef::File { path } = &m.media else { return Err(format!("{}: generated media has no audio to split", it.name)) };
    if m.info.audio().is_none() {
        return Err(format!("{}: the media has no audio", it.name));
    }
    Ok((path.clone(), m.info.duration.0 as f64 / TICKS_PER_SECOND as f64))
}

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let place = bool_p(p, "place").unwrap_or(false);
    let bandwidth = match p.get("bandwidth").and_then(Value::as_str) {
        None => "16k",
        Some(b @ ("16k" | "11k")) => b,
        Some(b) => return Err(crate::commands::bad("ai.stems", format!("`bandwidth` is \"16k\" or \"11k\", not `{b}`"))),
    };
    let wait = bool_p(p, "wait").unwrap_or(false);
    let mut jobs = Vec::new();
    for c in clips_p(s, p) {
        let Ok((_, seconds)) = media_of(s, c) else { continue };
        if seconds > MAX_SECONDS {
            return Err(EngineError::Other(format!(
                "the source is {:.0} min long; stems are made for sources up to {:.0} min",
                seconds / 60.0,
                MAX_SECONDS / 60.0
            )));
        }
        let name = s.active_sequence().and_then(|q| q.find_item(c)).map(|(_, it)| it.name.clone()).unwrap_or_default();
        // CPU separation runs at a few times real time: allow 6x the source plus 10 minutes
        let timeout = Duration::from_secs_f64(seconds * 6.0 + 600.0);
        let item = s.active_sequence().and_then(|q| q.find_item(c)).map(|(_, it)| it.item).ok_or(EngineError::NoSequence)?;
        let src = s.source(item).ok_or_else(|| EngineError::Other(format!("{name}: the media cannot be read")))?;
        let spec = super::JobSpec {
            kind: "stems",
            label: "Extract Stems",
            clips: &[c],
            params: json!({"stems": STEMS, "maxSeconds": MAX_SECONDS, "bandwidth": bandwidth}),
            context: json!({"name": name, "place": place, "seconds": seconds}),
            timeout: Some(timeout),
            prepare: Some(Box::new(move |dir: &std::path::Path, cancelled: &dyn Fn() -> bool| {
                let path = dir.join("source.wav");
                decode_to_wav(&*src, seconds, &path, cancelled)?;
                Ok(vec![path.to_string_lossy().to_string()])
            })),
        };
        jobs.push(super::start_job(s, spec, wait)?);
    }
    if jobs.is_empty() {
        return Err(EngineError::Other("select a clip with audio in the timeline".into()));
    }
    Ok(if jobs.len() == 1 { jobs.remove(0) } else { json!({"jobs": jobs}) })
}

/// The model's sample rate.
pub const RATE: u32 = 44_100;

/// Write `seconds` of `src`'s audio, resampled to [`RATE`], as a 32-bit float WAV (streamed, ten
/// seconds at a time; stops when `cancelled`).
fn decode_to_wav(
    src: &dyn filmcraft_media::MediaSource,
    seconds: f64,
    path: &std::path::Path,
    cancelled: &dyn Fn() -> bool,
) -> std::result::Result<(), String> {
    use std::io::Write;
    let channels = src.info().audio().map_or(2, |a| a.channels.clamp(1, 2) as usize);
    let total = (seconds.clamp(0.0, MAX_SECONDS) * RATE as f64).round() as u64;
    let bytes = total.saturating_mul(channels as u64 * 4);
    let size = u32::try_from(bytes).map_err(|_| "the source is too long for a WAV file".to_string())?;
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut w = std::io::BufWriter::new(file);
    let ch = channels as u16;
    let mut head = Vec::with_capacity(44);
    head.extend_from_slice(b"RIFF");
    head.extend_from_slice(&size.saturating_add(36).to_le_bytes());
    head.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &3u16.to_le_bytes(),
        &ch.to_le_bytes(),
        &RATE.to_le_bytes(),
        &(RATE * u32::from(ch) * 4).to_le_bytes(),
        &(ch * 4).to_le_bytes(),
        &32u16.to_le_bytes(),
    ] {
        head.extend_from_slice(v);
    }
    head.extend_from_slice(b"data");
    head.extend_from_slice(&size.to_le_bytes());
    w.write_all(&head).map_err(|e| e.to_string())?;
    let step = RATE as u64 * 10;
    let mut start = 0u64;
    while start < total {
        if cancelled() {
            return Err("stopped".into());
        }
        let n = step.min(total - start) as usize;
        let buf = src.audio(start as i64, n, RATE).map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(n * channels * 4);
        for i in 0..n {
            for c in 0..channels {
                let v = buf.channels.get(c).or(buf.channels.first()).and_then(|ch| ch.get(i)).copied().unwrap_or(0.0);
                out.extend_from_slice(&(if v.is_finite() { v } else { 0.0 }).to_le_bytes());
            }
        }
        w.write_all(&out).map_err(|e| e.to_string())?;
        start += n as u64;
    }
    w.flush().map_err(|e| e.to_string())
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let mut paths = Vec::with_capacity(STEMS.len());
    for stem in STEMS {
        let f = files
            .iter()
            .find(|f| f.file_name().is_some_and(|n| n.to_string_lossy() == format!("{stem}.wav")))
            .ok_or_else(|| EngineError::Other(format!("the helper wrote no {stem}.wav")))?;
        // checked again on open (a file swapped after the job's check is refused)
        filmcraft_sidecar::open_result(f, &pj.out_dir, MAX_STEM_BYTES).map_err(|e| EngineError::Other(e.to_string()))?;
        paths.push(f.to_string_lossy().to_string());
    }
    let name = pj.context.get("name").and_then(Value::as_str).unwrap_or("Clip").to_string();
    let place = pj.context.get("place").and_then(Value::as_bool).unwrap_or(false);
    let seconds = pj.context.get("seconds").and_then(Value::as_f64).unwrap_or(0.0);
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let seq = pj.fingerprint.seq;
    if place && s.state.active_sequence != Some(seq) {
        return Err(EngineError::Other("open the clip's sequence to place its stems".into()));
    }
    let bin_name = format!("Stems: {name}");
    let import: super::Step = Box::new(move |s: &mut Session| {
        let bin: BinId = s.edit("Extract Stems", |pr, _| Ok(pr.add_bin(&bin_name, None)))?;
        let r = s.execute("file.import", json!({"paths": paths, "bin": bin.0}))?;
        let errors = r.get("errors").and_then(Value::as_array).map_or(0, Vec::len);
        let items: Vec<ItemId> = r.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).map(ItemId).collect();
        if errors > 0 || items.len() != STEMS.len() {
            return Err(EngineError::Other(format!("importing the stems failed: {}", r.get("errors").cloned().unwrap_or_default())));
        }
        for (stem, item) in STEMS.iter().zip(&items) {
            let d = s.project.item(*item).and_then(|i| i.as_media()).map(|m| m.info.duration.0 as f64 / TICKS_PER_SECOND as f64);
            if d.is_none_or(|d| (d - seconds).abs() > LENGTH_SLACK_S) {
                return Err(EngineError::Other(format!("the {stem} stem is not as long as the source")));
            }
        }
        if !place {
            return Ok(());
        }
        let added = s.execute("sequence.addTracks", json!({"video": 0, "audio": STEMS.len()}))?;
        let tracks: Vec<TrackId> = added["audio"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(TrackId).collect();
        if tracks.len() != STEMS.len() {
            return Err(EngineError::Other("could not add the stem tracks".into()));
        }
        s.edit("Extract Stems", move |pr, _| {
            let rate = pr.sequence(seq).map(|q| q.settings.frame_rate).ok_or(EngineError::NoSequence)?;
            let it = pr.sequence(seq).and_then(|q| q.find_item(clip)).map(|(_, it)| it.clone()).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
            for ((stem, item), track) in STEMS.iter().zip(&items).zip(&tracks) {
                let range = TimeRange::new(it.source_in, it.duration.max(Tick(1)));
                let mut ti = pr
                    .make_track_item(*item, TrackKind::Audio, it.start, range, rate)
                    .ok_or_else(|| EngineError::Other(format!("cannot place the {stem} stem")))?;
                (ti.start, ti.duration, ti.source_in, ti.speed, ti.reverse) = (it.start, it.duration, it.source_in, it.speed, it.reverse);
                ti.name = format!("{name} ({stem})");
                let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                q.audio_tracks.iter_mut().find(|t| t.id == *track).ok_or_else(|| EngineError::Other("a stem track is gone".into()))?.items.push(ti);
            }
            Ok(())
        })
    });
    super::apply_atomic(s, "Extract Stems", vec![import])
}

#[cfg(test)]
#[path = "ai_stems_tests.rs"]
mod tests;
