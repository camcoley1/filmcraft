//! `ai.interpolate` (M6f): a clip's frames at 2x or 4x the frame rate, from the helper's
//! `interpolate` job (Practical-RIFE 4.25 through ONNX Runtime).
//!
//! `ai.interpolate {clip?, factor=2|4 | targetFps, place=false, wait=false}` takes one video clip
//! (`clip`, else the first selected video clip); `targetFps` must be 2 or 4 times the clip's frame
//! rate. The job's thread decodes the frames the clip plays plus the next one (the last one again at
//! the end of the media) as PNG files; the helper returns `factor` frames per source frame, never
//! blending across a scene cut (it repeats the frames on either side instead). The original clip
//! and its media are never changed. Applied in one undo step ("Interpolate Frames"): a NEW item, the
//! frames as an image sequence at `factor` x the source rate (same pixel aspect), in a bin
//! "Interpolated: <clip>"; frame `k * factor` of it is source frame `first + k`, so its media time is
//! the source's minus the exact time of the first frame. With `place`, a copy of the clip on a new
//! video track above plays it over the same timeline range (same start, duration, speed and
//! direction; source In, effect and mask keyframes and markers moved by that time), so sequence
//! time is unchanged, and the original is turned off in the same step (otherwise the two would be
//! composited together wherever the copy is not opaque or a transition meets the original); undo
//! turns it back on. The result is checked and opened on the job's thread, so applying it only
//! edits the project. Refused before anything runs: what [`super::video`] refuses, frames larger
//! than [`MAX_SIZE`], more than [`MAX_FRAMES`] frames, and a job whose frames would take more than
//! [`super::upscale::MAX_JOB_BYTES`] of disk or more time than a job may run
//! ([`SECONDS_PER_MEGAPIXEL`] per in-between frame).

use std::time::Duration;

use filmcraft_project::{ClipId, TrackId, TrackItem};
use filmcraft_time::FrameRate;
use serde_json::{Value, json};

use super::upscale::{check_cost, context, pick, png_bytes, prepared_or_again};
use super::video::{VideoTarget, decode_frames, import_prepared, prepare_sequence, shift_media_time, video_target};
use crate::commands::{bad, bool_p, u64_p};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clip":id?,"factor":2|4=2,"targetFps":f64? (2 or 4 times the clip's rate, in place of factor),"place":bool=false (a copy of the clip playing it on a new track above),"wait":bool=false}"#;

/// Most source frames interpolated in one job.
pub const MAX_FRAMES: i64 = 3_000;
/// Largest frames interpolated (4K UHD).
pub const MAX_SIZE: (u32, u32) = (3_840, 2_160);
const MAX_FRAME_BYTES: u64 = 256 << 20;
/// Measured on the reference machine (Intel i7, CPU, ONNX Runtime): about 3.5 s per megapixel
/// for each in-between frame.
pub const SECONDS_PER_MEGAPIXEL: f64 = 3.5;
/// Prefix of the helper's result files.
const PREFIX: &str = "mid-";

fn target(s: &Session, c: ClipId) -> std::result::Result<VideoTarget, super::video::Refused> {
    video_target(s, c, MAX_FRAMES)
}

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    super::upscale::enabled_for(s, "interpolate")
}

/// `factor` (2 or 4), or `targetFps` 2 or 4 times `rate`.
fn factor_p(p: &Value, rate: FrameRate, cmd: &str) -> Result<u32> {
    if let Some(fps) = p.get("targetFps").filter(|v| !v.is_null()) {
        let fps = fps.as_f64().filter(|f| f.is_finite() && *f > 0.0).ok_or_else(|| bad(cmd, "`targetFps` is a frame rate"))?;
        let r = rate.num as f64 / rate.den.max(1) as f64;
        return [2u32, 4]
            .into_iter()
            .find(|f| (fps / (r * f64::from(*f)) - 1.0).abs() < 1e-4)
            .ok_or_else(|| bad(cmd, format!("`targetFps` {fps} is not 2 or 4 times the clip's {r:.3} fps")));
    }
    match p.get("factor") {
        None | Some(Value::Null) => Ok(2),
        Some(_) => match u64_p(p, "factor") {
            Some(f @ (2 | 4)) => Ok(f as u32),
            _ => Err(bad(cmd, "`factor` is 2 or 4")),
        },
    }
}

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.interpolate";
    let wait = bool_p(p, "wait").unwrap_or(false);
    let place = bool_p(p, "place").unwrap_or(false);
    let t = pick(s, p, target)?;
    let factor = factor_p(p, t.rate, cmd)?;
    if t.size.0 > MAX_SIZE.0 || t.size.1 > MAX_SIZE.1 {
        return Err(EngineError::Other(format!("{}: frames up to {}x{} are interpolated, not {}x{}", t.name, MAX_SIZE.0, MAX_SIZE.1, t.size.0, t.size.1)));
    }
    // the frames themselves, never a proxy
    let src = s.media.full_res_source(&s.project, t.item, &*s.services).ok_or_else(|| EngineError::Other(format!("{}: the media cannot be read", t.name)))?;
    let frames = u64::try_from(t.frames).unwrap_or(u64::MAX);
    let out_bytes = png_bytes(t.size.0, t.size.1).saturating_mul(frames.saturating_mul(u64::from(factor)));
    let bytes = png_bytes(t.size.0, t.size.1).saturating_mul(frames.saturating_add(1)).saturating_add(out_bytes);
    let mid = t.frames as f64 * f64::from(factor - 1);
    let seconds = f64::from(t.size.0) * f64::from(t.size.1) / 1e6 * SECONDS_PER_MEGAPIXEL * mid;
    let limit = s.ai.limits.timeout;
    check_cost(&t.name, bytes, seconds, limit)?;
    // the frames played and the next one (repeated past the media's end): N + 1 inputs, N x factor out
    let (rate, range, size) = (t.rate, (t.first, t.frames.saturating_add(1), t.total), t.size);
    let spec = super::JobSpec {
        kind: "interpolate",
        label: "Interpolate Frames",
        clips: &[t.clip],
        params: json!({"factor": factor, "needBytes": out_bytes}),
        context: json!({
            "name": t.name, "offset": t.offset().0, "frames": t.frames, "rate": [t.rate.num, t.rate.den],
            "factor": factor, "size": [t.size.0, t.size.1], "par": [t.par.0, t.par.1], "alpha": [t.alpha.0, t.alpha.1], "place": place,
        }),
        // twice the estimate (a busy machine), within the job ceiling
        timeout: Some(Duration::from_secs_f64(seconds * 2.0 + 600.0).min(limit)),
        prepare: Some(Box::new(move |dir: &std::path::Path, stop: &dyn Fn() -> bool, progress: &mut dyn FnMut(f64)| {
            decode_frames(&*src, rate, range, size, dir, "src-", stop, progress)
        })),
        loose: false,
        media: true,
    };
    super::start_job(s, spec, wait)
}

/// The result's frame count and rate: `factor` frames per source frame.
fn output(cx: &super::upscale::Context) -> Result<(i64, FrameRate)> {
    let count = cx.frames.checked_mul(i64::from(cx.factor)).ok_or_else(|| EngineError::Other("too many frames".into()))?;
    let rate = cx
        .rate
        .num
        .checked_mul(i64::from(cx.factor))
        .map(|n| FrameRate::new(n, cx.rate.den))
        .ok_or_else(|| EngineError::Other("the frame rate is out of range".into()))?;
    Ok((count, rate))
}

/// On the job's thread: the result files checked and opened as a sequence.
pub(crate) fn finish(
    files: &[std::path::PathBuf],
    ctx: &Value,
    services: &dyn crate::Services,
    stop: &dyn Fn() -> bool,
) -> std::result::Result<Box<dyn std::any::Any + Send>, String> {
    let cx = context(ctx).map_err(|e| e.to_string())?;
    let (count, rate) = output(&cx).map_err(|e| e.to_string())?;
    let dir = files.first().and_then(|f| f.parent()).ok_or("the helper wrote no frames")?;
    let p = prepare_sequence(files, dir, PREFIX, count, rate, cx.size, MAX_FRAME_BYTES, services, stop)?;
    Ok(Box::new(p))
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let cx = context(&pj.context)?;
    let place = pj.context.get("place").and_then(Value::as_bool).unwrap_or(false);
    let (count, rate) = output(&cx)?;
    let prep = prepared_or_again(s, pj, files, PREFIX, count, rate, cx.size, MAX_FRAME_BYTES)?;
    let seq = pj.fingerprint.seq;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let placed = place && s.state.active_sequence == Some(seq);
    let r = super::apply_with_media(s, pj, "Interpolate Frames", files, move |_| {
        let step: super::Step = Box::new(move |s: &mut Session| {
            let item = import_prepared(s, &prep, &format!("Interpolated: {}", cx.name), cx.par, cx.alpha)?;
            if !placed {
                return Ok(());
            }
            let added = s.execute("sequence.addTracks", json!({"video": 1, "audio": 0}))?;
            let track = added["video"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_u64)
                .map(TrackId)
                .ok_or_else(|| EngineError::Other("could not add a track".into()))?;
            let offset = cx.offset;
            s.edit("Interpolate Frames", move |pr, _| {
                let id = ClipId(pr.alloc_id());
                let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                let (_, original) = q.find_item_mut(clip).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
                let mut copy: TrackItem = original.clone();
                // the copy replaces it in the picture (not composited over it)
                original.enabled = false;
                copy.id = id;
                copy.link = None;
                copy.group = None;
                copy.name = format!("{} ({}x)", copy.name, cx.factor);
                shift_media_time(&mut copy, item, offset);
                q.video_tracks.iter_mut().find(|t| t.id == track).ok_or_else(|| EngineError::Other("the new track is gone".into()))?.items.push(copy);
                Ok(())
            })
        });
        vec![step]
    });
    if r.is_ok() && placed {
        s.toast("Interpolated clip placed above; the original is turned off (undo brings it back)");
    }
    if r.is_ok() && place && !placed {
        s.toast("Interpolated frames imported; open the clip's sequence to place them");
    }
    r
}

#[cfg(test)]
#[path = "ai_interpolate_tests.rs"]
mod tests;
