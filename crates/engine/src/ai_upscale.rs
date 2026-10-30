//! `ai.upscale` (M6f): a clip's frames enlarged 2x or 4x by the helper's `upscale` job.
//!
//! **Not AI-enhanced.** No super-resolution model with permissively licensed weights and training
//! data was found (craft-suite/sidecar/LICENSES.md), so the helper's only backend for now is a
//! careful classical scaler (`lanczos`: gamma-correct Lanczos-3 on premultiplied alpha, an
//! edge-aware unsharp mask, every pixel clamped to its input neighbourhood's range); the command
//! is labelled "High-Quality Upscale (not AI-enhanced)". The helper's `backend` parameter leaves
//! room for a learned model later without changes here.
//!
//! `ai.upscale {clip?, factor=2|4, wait=false}` takes one video clip (`clip`, else the first
//! selected video clip). The job's thread decodes the frames the clip plays (its source range on
//! the media's frame grid) with FilmCraft's decoder as PNG files; the helper writes one enlarged
//! PNG per frame; the job's thread moves them into the project's AI media folder. Applied in one
//! undo step ("Upscale"): the frames are imported as an image sequence at the source's frame rate
//! and pixel aspect into a bin "Upscaled: <clip>", and the clip is relinked to it: same start,
//! duration, speed and direction, source In shifted by the exact media time of the first frame
//! (and so are its effect keyframes and markers), and Motion Scale divided (Anchor Point
//! multiplied) by the factor, so the picture stays where it was, only sharper. A clip with Scale
//! to Frame Size keeps its Motion values (the fit follows the new size). Refused: stills, a frame
//! hold, interpreted frame rates, masks (their points are in source pixels), more than
//! [`MAX_FRAMES`] frames, and results larger than 8K.

use std::time::Duration;

use filmcraft_geom::Vec2;
use filmcraft_project::{BinId, ClipId, ItemId, ParamValue, TrackItem};
use filmcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::video::{Refused, VideoTarget, decode_frames, result_frames, shift_media_time, video_target};
use crate::commands::{bad, bool_p, clips_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clip":id?,"factor":2|4=2,"wait":bool=false}"#;

/// Most frames upscaled in one job.
pub const MAX_FRAMES: i64 = 3_000;
/// Largest result (8K UHD).
pub const MAX_OUT: (u32, u32) = (7_680, 4_320);
const MAX_FRAME_BYTES: u64 = 512 << 20;
/// Allowance per frame in the job's time limit (CPU, up to 8K out).
const SECONDS_PER_FRAME: f64 = 60.0;

fn target(s: &Session, c: ClipId) -> std::result::Result<VideoTarget, Refused> {
    let t = video_target(s, c, MAX_FRAMES)?;
    let masked = s.active_sequence().and_then(|q| q.find_item(c)).is_some_and(|(_, it)| it.effects.iter().any(|e| !e.masks.is_empty()));
    if masked {
        return Err(Refused::Hard(format!("{}: the clip has masks (their points are in source pixels); remove them first", t.name)));
    }
    Ok(t)
}

/// The clip to upscale: `clip`, else the first selected video clip.
pub(crate) fn pick(s: &Session, p: &Value, f: impl Fn(&Session, ClipId) -> std::result::Result<VideoTarget, Refused>) -> Result<VideoTarget> {
    let clips = match p.get("clip").and_then(Value::as_u64) {
        Some(c) => vec![ClipId(c)],
        None => clips_p(s, p),
    };
    for c in clips {
        match f(s, c) {
            Ok(t) => return Ok(t),
            Err(Refused::Hard(e)) => return Err(EngineError::Other(e)),
            Err(Refused::Skip) => {}
        }
    }
    Err(EngineError::Other("select a video clip in the timeline".into()))
}

pub(crate) fn enabled_for(s: &Session, feature: &str) -> std::result::Result<(), String> {
    super::helper_ready(s, feature)?;
    has_seq(s)?;
    if s.state.selection.iter().any(|c| !matches!(video_target(s, *c, i64::MAX), Err(Refused::Skip))) {
        Ok(())
    } else {
        Err("select a video clip in the timeline".into())
    }
}

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    enabled_for(s, "upscale")
}

/// `factor`: 2 or 4 (default 2).
fn factor_p(p: &Value, cmd: &str) -> Result<u32> {
    match p.get("factor") {
        None | Some(Value::Null) => Ok(2),
        Some(_) => match u64_p(p, "factor") {
            Some(f @ (2 | 4)) => Ok(f as u32),
            _ => Err(bad(cmd, "`factor` is 2 or 4")),
        },
    }
}

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.upscale";
    let factor = factor_p(p, cmd)?;
    let wait = bool_p(p, "wait").unwrap_or(false);
    let t = pick(s, p, target)?;
    let out = (t.size.0.saturating_mul(factor), t.size.1.saturating_mul(factor));
    if out.0 > MAX_OUT.0 || out.1 > MAX_OUT.1 {
        return Err(EngineError::Other(format!(
            "{}: {}x{} upscaled {factor}x would be {}x{}; at most {}x{}",
            t.name, t.size.0, t.size.1, out.0, out.1, MAX_OUT.0, MAX_OUT.1
        )));
    }
    let src = s.source(t.item).ok_or_else(|| EngineError::Other(format!("{}: the media cannot be read", t.name)))?;
    let (rate, range, size) = (t.rate, (t.first, t.frames, t.total), t.size);
    let spec = super::JobSpec {
        kind: "upscale",
        label: "Upscale",
        clips: &[t.clip],
        params: json!({"factor": factor, "backend": "lanczos"}),
        context: json!({
            "name": t.name, "offset": t.offset().0, "frames": t.frames, "rate": [t.rate.num, t.rate.den],
            "factor": factor, "size": [t.size.0, t.size.1], "par": [t.par.0, t.par.1],
        }),
        timeout: Some(Duration::from_secs_f64(t.frames as f64 * SECONDS_PER_FRAME + 600.0)),
        prepare: Some(Box::new(move |dir: &std::path::Path, stop: &dyn Fn() -> bool, progress: &mut dyn FnMut(f64)| {
            decode_frames(&*src, rate, range, size, dir, "src-", stop, progress)
        })),
        loose: false,
        media: true,
    };
    super::start_job(s, spec, wait)
}

/// The job's context: (name, offset, frames, rate, factor, size, par).
pub(crate) struct Context {
    pub name: String,
    pub offset: Tick,
    pub frames: i64,
    pub rate: FrameRate,
    pub factor: u32,
    pub size: (u32, u32),
    pub par: (u32, u32),
}

pub(crate) fn context(c: &Value) -> Result<Context> {
    let pair = |k: &str| -> Option<(i64, i64)> {
        let a = c.get(k)?.as_array()?;
        Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?))
    };
    let bad_ctx = || EngineError::Other("the job's context is damaged".into());
    let (num, den) = pair("rate").ok_or_else(bad_ctx)?;
    let (w, h) = pair("size").ok_or_else(bad_ctx)?;
    let (pn, pd) = pair("par").unwrap_or((1, 1));
    let u = |v: i64| u32::try_from(v).map_err(|_| bad_ctx());
    Ok(Context {
        name: c.get("name").and_then(Value::as_str).unwrap_or("Clip").to_string(),
        offset: Tick(c.get("offset").and_then(Value::as_i64).unwrap_or(0)),
        frames: c.get("frames").and_then(Value::as_i64).filter(|n| *n > 0).ok_or_else(bad_ctx)?,
        rate: FrameRate::new(num, den).sane(),
        factor: c.get("factor").and_then(Value::as_u64).and_then(|f| u32::try_from(f).ok()).filter(|f| matches!(f, 1 | 2 | 4)).ok_or_else(bad_ctx)?,
        size: (u(w)?, u(h)?),
        par: (u(pn)?, u(pd)?),
    })
}

/// Import the PNG sequence `frames` (already in the project's AI media folder) at `rate` with
/// pixel aspect `par` into a new bin `bin_name`, checking it has `count` frames of `size`.
pub(crate) fn import_frames(
    s: &mut Session,
    frames: &[std::path::PathBuf],
    bin_name: &str,
    rate: FrameRate,
    par: (u32, u32),
    count: i64,
    size: (u32, u32),
) -> Result<ItemId> {
    let first = frames.first().ok_or_else(|| EngineError::Other("the helper wrote no frames".into()))?;
    let bin: BinId = s.edit("Import", |pr, _| Ok(pr.add_bin(bin_name, None)))?;
    let (item, n, missing) = crate::commands::import_image_sequence_at(s, &first.to_string_lossy(), Some(bin), rate)?;
    if i64::try_from(n).ok() != Some(count) || !missing.is_empty() {
        return Err(EngineError::Other(format!("the result has {n} frames ({} missing), not {count}", missing.len())));
    }
    let got = s.project.item(item).and_then(|i| i.as_media()).and_then(|m| m.info.video.as_ref()).map(|v| (v.width, v.height));
    if got != Some(size) {
        return Err(EngineError::Other(format!("the result is {:?}, not {}x{}", got, size.0, size.1)));
    }
    s.edit("Import", move |pr, _| {
        if let Some(filmcraft_project::ItemKind::Media(m)) = pr.item_mut(item).map(|i| &mut i.kind) {
            m.interpret.par = Some(par);
        }
        Ok(())
    })?;
    Ok(item)
}

/// Motion that shows a picture `factor` times larger exactly as before: Scale (and its width and
/// height) divided, a set Anchor Point multiplied, keyframes too. Nothing to do with Scale to Frame
/// Size, whose fit follows the new size.
fn rescale_motion(it: &mut TrackItem, factor: f64) {
    if it.scale_to_frame {
        return;
    }
    let Some(m) = it.effect_mut("motion") else { return };
    for key in ["scale", "scale_width", "scale_height"] {
        if let Some(p) = m.param_mut(key) {
            if let ParamValue::Float(v) = &mut p.value {
                *v /= factor;
            }
            for k in &mut p.keyframes {
                if let ParamValue::Float(v) = &mut k.value {
                    *v /= factor;
                }
            }
        }
    }
    if let Some(p) = m.param_mut("anchor") {
        let grow = |v: &mut ParamValue| {
            if let ParamValue::Vec2(a) = v
                && !a.x.is_nan()
            {
                *a = Vec2::new(a.x * factor, a.y * factor);
            }
        };
        grow(&mut p.value);
        for k in &mut p.keyframes {
            grow(&mut k.value);
        }
    }
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let cx = context(&pj.context)?;
    let frames = result_frames(files, pj.result_dir(), "up-src-", cx.frames, MAX_FRAME_BYTES)?;
    let seq = pj.fingerprint.seq;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    super::apply_with_media(s, pj, "Upscale", &frames, move |moved| {
        let step: super::Step = Box::new(move |s: &mut Session| {
            let size = (cx.size.0.saturating_mul(cx.factor), cx.size.1.saturating_mul(cx.factor));
            let item = import_frames(s, &moved, &format!("Upscaled: {}", cx.name), cx.rate, cx.par, cx.frames, size)?;
            let (offset, factor) = (cx.offset, f64::from(cx.factor));
            s.edit("Upscale", move |pr, _| {
                let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                let (_, it) = q.find_item_mut(clip).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
                shift_media_time(it, item, offset);
                rescale_motion(it, factor);
                Ok(())
            })
        });
        vec![step]
    })
}

#[cfg(test)]
#[path = "ai_upscale_tests.rs"]
pub(in crate::ai) mod tests;
