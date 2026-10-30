//! `ai.enhanceSpeech` (M6c): clean speech of noise and room with DeepFilterNet 3.
//!
//! `ai.enhanceSpeech {clips?, strength=1, wait=false}` starts one helper job for the selected
//! audio clips, with one input per distinct range (clips that play the same media range share
//! one). The job's thread decodes each range a clip plays plus [`PAD_S`] on each side (its stream
//! and channels) at 48 kHz, the model's rate, on the 48 kHz sample grid; the helper enhances each
//! in 60 s chunks with 2 s crossfades, mixes it with the original by `strength`, and resamples it
//! back to the source's rate ([`OUT_RATES`]). A decode failure fails the job (only a stream that
//! ends just before the media is padded with silence). Remixed clips are refused.
//!
//! The result is applied in one undo step ("Enhance Speech") for the whole selection: each
//! `enhanced-<k>.wav`, which the job's thread has moved into the project's AI media folder
//! ([`super::media_dir`]), is imported into a bin "Enhanced: <clip>", and only the targeted audio
//! clips are relinked to it: same start, duration, speed and direction, source In shifted by the
//! exact offset of the decoded range, and so are the clip's effect keyframes and markers (both in
//! media time); stream 0 and the file's own channels. The original media and any linked video stay
//! as they are. A result whose length or rate differs from what was asked fails the apply and
//! nothing changes.

use std::time::Duration;

use filmcraft_project::{BinId, ClipId, ItemId, TrackItem, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use super::audio::{AudioTarget, Refused, audio_target, decode_to_wav, targets};
use crate::commands::{bool_p, clips_p, f64_p, has_seq};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clips":[id]?,"strength":0..1=1 (mix of the enhanced and the original sound),"wait":bool=false}"#;

/// The model's rate.
pub const RATE: u32 = 48_000;
/// Source decoded on each side of what the clip plays (the model's context at the edges).
pub const PAD_S: f64 = 1.0;
/// Longest range enhanced (20 minutes).
pub const MAX_SECONDS: f64 = 1200.0;
const MAX_RESULT_BYTES: u64 = 2 << 30;
const LENGTH_SLACK_S: f64 = 0.05;
/// Result rates the helper makes (each divides [`TICKS_PER_SECOND`], so the result's samples fall
/// on exact ticks); any other source rate gets 48 kHz back.
pub const OUT_RATES: [u32; 12] = [8_000, 11_025, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

fn target(s: &Session, c: filmcraft_project::ClipId) -> std::result::Result<AudioTarget, Refused> {
    let t = audio_target(s, c, RATE, PAD_S, MAX_SECONDS, false)?;
    // only audio clips are relinked; a video clip's sound is changed through its audio clip
    if t.audio_track { Ok(t) } else { Err(Refused::Skip) }
}

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    super::helper_ready(s, "enhance")?;
    has_seq(s)?;
    if s.state.selection.iter().any(|c| !matches!(target(s, *c), Err(Refused::Skip))) { Ok(()) } else { Err("select an audio clip in the timeline".into()) }
}

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let strength = f64_p(p, "strength").unwrap_or(1.0);
    if !(0.0..=1.0).contains(&strength) {
        return Err(crate::commands::bad("ai.enhanceSpeech", "`strength` is 0 to 1"));
    }
    let wait = bool_p(p, "wait").unwrap_or(false);
    let groups = targets(s, clips_p(s, p), target)?;
    if groups.is_empty() {
        return Err(EngineError::Other("select an audio clip in the timeline".into()));
    }
    // one job for the whole selection (one input per distinct range), applied in one undo step
    let (mut inputs, mut parts, mut clips, mut rates) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut seconds = 0.0;
    for (t, group) in groups {
        // back to the source's own rate when the helper can make it
        let out_rate = if OUT_RATES.contains(&t.source_rate) { t.source_rate } else { RATE };
        let src = s.source(t.item).ok_or_else(|| EngineError::Other(format!("{}: the media cannot be read", t.name)))?;
        inputs.push((src, t.stream, t.channels.clone(), (t.offset, t.samples, t.end)));
        parts.push(json!({"name": t.name, "offset": t.offset_ticks().0, "seconds": t.seconds(), "rate": out_rate, "clips": group.iter().map(|c| c.0).collect::<Vec<_>>()}));
        rates.push(out_rate);
        seconds += t.seconds();
        clips.extend(group);
    }
    // CPU enhancement runs near real time: allow 4x the ranges plus 10 minutes
    let timeout = Duration::from_secs_f64(seconds * 4.0 + 600.0);
    let n = inputs.len();
    let spec = super::JobSpec {
        kind: "enhance",
        label: "Enhance Speech",
        clips: &clips,
        params: json!({"strength": strength, "outRates": rates}),
        context: json!({"parts": parts}),
        timeout: Some(timeout),
        prepare: Some(Box::new(move |dir: &std::path::Path, stop: &dyn Fn() -> bool, progress: &mut dyn FnMut(f64)| {
            let mut paths = Vec::with_capacity(n);
            for (k, (src, stream, channels, range)) in inputs.into_iter().enumerate() {
                let path = dir.join(format!("source-{k}.wav"));
                decode_to_wav(&*src, stream, &channels, false, range, RATE, &path, stop, &mut |f| progress((k as f64 + f) / n as f64))?;
                paths.push(path.to_string_lossy().to_string());
            }
            Ok(paths)
        })),
        loose: false,
        media: true,
    };
    super::start_job(s, spec, wait)
}

/// One decoded range of a job ([`run`]'s context).
struct Part {
    name: String,
    offset: Tick,
    seconds: f64,
    rate: u32,
    clips: Vec<ClipId>,
}

fn parts(context: &Value) -> Result<Vec<Part>> {
    let list = context.get("parts").and_then(Value::as_array).ok_or_else(|| EngineError::Other("the job lists no inputs".into()))?;
    list.iter()
        .map(|p| {
            Ok(Part {
                name: p.get("name").and_then(Value::as_str).unwrap_or("Clip").to_string(),
                offset: Tick(p.get("offset").and_then(Value::as_i64).unwrap_or(0)),
                seconds: p.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
                rate: p.get("rate").and_then(Value::as_u64).and_then(|r| u32::try_from(r).ok()).unwrap_or(RATE),
                clips: p.get("clips").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).map(ClipId).collect(),
            })
        })
        .collect()
}

/// Relink clip `it` to `item`, which starts `offset` into the clip's source: the same source In
/// moment, and everything else stored in media time (effect keyframes, clip markers) moved with it.
fn relink(it: &mut TrackItem, item: ItemId, offset: Tick) {
    it.item = item;
    it.source_in -= offset;
    it.audio_stream = 0;
    it.source_channels.clear();
    for e in &mut it.effects {
        for p in e.params.values_mut() {
            for k in &mut p.keyframes {
                k.time -= offset;
            }
        }
    }
    for m in &mut it.markers {
        m.start -= offset;
    }
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let parts = parts(&pj.context)?;
    let mut picked = Vec::with_capacity(parts.len());
    for k in 0..parts.len() {
        let name = format!("enhanced-{k}.wav");
        let f = files
            .iter()
            .find(|f| f.file_name().is_some_and(|n| n.to_string_lossy() == name))
            .ok_or_else(|| EngineError::Other(format!("the helper wrote no {name}")))?;
        filmcraft_sidecar::open_result(f, pj.result_dir(), MAX_RESULT_BYTES).map_err(|e| EngineError::Other(e.to_string()))?;
        picked.push(f.clone());
    }
    let seq = pj.fingerprint.seq;
    super::apply_with_media(s, pj, "Enhance Speech", &picked, move |moved| {
        parts
            .into_iter()
            .zip(moved)
            .map(|(part, path)| {
                let step: super::Step = Box::new(move |s: &mut Session| {
                    let bin: BinId = s.edit("Enhance Speech", |pr, _| Ok(pr.add_bin(&format!("Enhanced: {}", part.name), None)))?;
                    let r = s.execute("file.import", json!({"paths": [path.to_string_lossy()], "bin": bin.0}))?;
                    let item = r.get("items").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_u64).map(ItemId);
                    let errors = r.get("errors").and_then(Value::as_array).map_or(0, Vec::len);
                    let Some(item) = item.filter(|_| errors == 0) else {
                        return Err(EngineError::Other(format!("importing the enhanced audio failed: {}", r.get("errors").cloned().unwrap_or_default())));
                    };
                    let info = s
                        .project
                        .item(item)
                        .and_then(|i| i.as_media())
                        .map(|m| (m.info.duration.0 as f64 / TICKS_PER_SECOND as f64, m.info.audio().map(|a| a.sample_rate)));
                    let Some((d, rate)) = info else { return Err(EngineError::Other("the enhanced audio is not media".into())) };
                    if (d - part.seconds).abs() > LENGTH_SLACK_S {
                        return Err(EngineError::Other("the enhanced audio is not as long as the decoded source".into()));
                    }
                    if rate != Some(part.rate) {
                        return Err(EngineError::Other(format!("the enhanced audio is at {} Hz, not {} Hz", rate.unwrap_or(0), part.rate)));
                    }
                    s.edit("Enhance Speech", move |pr, _| {
                        let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                        for c in &part.clips {
                            if !q.find_item(*c).and_then(|(t, _)| q.track(t)).is_some_and(|t| t.kind == TrackKind::Audio) {
                                return Err(EngineError::Other("only audio clips are relinked".into()));
                            }
                            let Some((_, it)) = q.find_item_mut(*c) else { return Err(EngineError::Other("a target clip is gone".into())) };
                            relink(it, item, part.offset);
                        }
                        Ok(())
                    })
                });
                step
            })
            .collect()
    })
}

#[cfg(test)]
#[path = "ai_enhance_tests.rs"]
pub(in crate::ai) mod tests;
