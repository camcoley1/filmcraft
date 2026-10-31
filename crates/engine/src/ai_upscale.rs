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
//! multiplied) by the factor, so the picture stays where it was, only sharper. With Scale to Frame
//! Size the fit follows the new size, so only the Anchor Point is multiplied; a clip without a
//! Motion effect gets one at 100/factor %. The result is checked and opened on the job's thread,
//! so applying it only edits the project.
//!
//! Refused before anything runs: stills, image sequences, a frame hold, interpreted frame rates
//! or colour spaces, HDR or linear footage (see [`super::video`]); masks and any enabled effect
//! whose look depends on the source's pixel size (blurs, distortions, points, Lumetri Sharpen /
//! Denoise / Blur: [`super::video::pixel_space_effect`]), since those would change on a larger
//! source; a turned-off Motion without Scale to Frame Size (its 100 % would show the result
//! larger); more than [`MAX_FRAMES`] frames; results larger than 8K; a job whose frames would
//! take more than [`MAX_JOB_BYTES`] of disk, or more time than the job limit allows on this kind
//! of computer ([`SECONDS_PER_OUT_MEGAPIXEL`]). The helper checks the free space itself before it
//! writes.

use std::time::Duration;

use filmcraft_geom::Vec2;
use filmcraft_project::{ClipId, ParamValue, TrackItem};
use filmcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::video::{
    PreparedSequence, Refused, VideoTarget, decode_frames, import_prepared, pixel_space_effect, prepare_sequence, prepared, shift_media_time, video_target,
};
use crate::commands::{bad, bool_p, clips_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clip":id?,"factor":2|4=2,"wait":bool=false}"#;

/// Most frames upscaled in one job.
pub const MAX_FRAMES: i64 = 3_000;
/// Largest result (8K UHD).
pub const MAX_OUT: (u32, u32) = (7_680, 4_320);
const MAX_FRAME_BYTES: u64 = 512 << 20;
/// Most disk one job's frames may take (the decoded inputs plus the results).
pub const MAX_JOB_BYTES: u64 = 48 << 30;
/// Measured on the reference machine (Intel i7, CPU): about 1.1 s per megapixel of output.
pub const SECONDS_PER_OUT_MEGAPIXEL: f64 = 1.1;
/// Prefix of the helper's result files.
const PREFIX: &str = "up-src-";

/// Bytes a PNG frame of `w` x `h` is allowed for in estimates (RGBA, before compression: an
/// upper bound).
pub(crate) fn png_bytes(w: u32, h: u32) -> u64 {
    u64::from(w).saturating_mul(u64::from(h)).saturating_mul(4)
}

/// "about 3 h 20 min", for estimates.
pub(crate) fn duration_text(seconds: f64) -> String {
    let m = (seconds / 60.0).ceil().max(1.0) as u64;
    if m < 60 { format!("about {m} min") } else { format!("about {} h {} min", m / 60, m % 60) }
}

/// Refuse a job whose frames would need more than [`MAX_JOB_BYTES`] or more than `limit` of
/// time (`seconds`, an estimate for this kind of computer).
pub(crate) fn check_cost(name: &str, bytes: u64, seconds: f64, limit: Duration) -> Result<()> {
    if bytes > MAX_JOB_BYTES {
        return Err(EngineError::Other(format!(
            "{name}: the frames would take about {} GB of disk (at most {} GB per job); process a shorter range",
            bytes.div_ceil(1 << 30),
            MAX_JOB_BYTES >> 30
        )));
    }
    if seconds > limit.as_secs_f64() {
        return Err(EngineError::Other(format!(
            "{name}: this would take {} on this kind of computer, more than a job may run ({}); process a shorter range",
            duration_text(seconds),
            duration_text(limit.as_secs_f64())
        )));
    }
    Ok(())
}

fn target(s: &Session, c: ClipId) -> std::result::Result<VideoTarget, Refused> {
    let t = video_target(s, c, MAX_FRAMES)?;
    let Some((_, it)) = s.active_sequence().and_then(|q| q.find_item(c)) else { return Err(Refused::Skip) };
    let hard = |why: String| Refused::Hard(format!("{}: {why}", t.name));
    if it.effects.iter().any(|e| !e.masks.is_empty()) {
        return Err(hard("the clip has masks (their points are in source pixels); remove them first".into()));
    }
    if let Some(e) = pixel_space_effect(it) {
        return Err(hard(format!("the effect {e} is measured in source pixels and would change on the larger source; remove or turn it off first")));
    }
    if !it.scale_to_frame && it.effect("motion").is_some_and(|m| !m.enabled) {
        return Err(hard("Motion is turned off, so the larger result would show larger; turn Motion on first".into()));
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
    // the frames themselves, never a proxy
    let src = s.media.full_res_source(&s.project, t.item, &*s.services).ok_or_else(|| EngineError::Other(format!("{}: the media cannot be read", t.name)))?;
    let frames = u64::try_from(t.frames).unwrap_or(u64::MAX);
    let out_bytes = png_bytes(out.0, out.1).saturating_mul(frames);
    let bytes = png_bytes(t.size.0, t.size.1).saturating_mul(frames).saturating_add(out_bytes);
    let seconds = f64::from(out.0) * f64::from(out.1) / 1e6 * SECONDS_PER_OUT_MEGAPIXEL * t.frames as f64;
    let limit = s.ai.limits.timeout;
    check_cost(&t.name, bytes, seconds, limit)?;
    let (rate, range, size) = (t.rate, (t.first, t.frames, t.total), t.size);
    let spec = super::JobSpec {
        kind: "upscale",
        label: "Upscale",
        clips: &[t.clip],
        params: json!({"factor": factor, "backend": "lanczos", "needBytes": out_bytes}),
        context: json!({
            "name": t.name, "offset": t.offset().0, "frames": t.frames, "rate": [t.rate.num, t.rate.den],
            "factor": factor, "size": [t.size.0, t.size.1], "par": [t.par.0, t.par.1], "alpha": [t.alpha.0, t.alpha.1],
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

/// The job's context: (name, offset, frames, rate, factor, size, par, alpha).
pub(crate) struct Context {
    pub name: String,
    pub offset: Tick,
    pub frames: i64,
    pub rate: FrameRate,
    pub factor: u32,
    pub size: (u32, u32),
    pub par: (u32, u32),
    /// The source's Interpret Footage alpha: (ignore, invert).
    pub alpha: (bool, bool),
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
        alpha: c
            .get("alpha")
            .and_then(Value::as_array)
            .map(|a| (a.first().and_then(Value::as_bool).unwrap_or(false), a.get(1).and_then(Value::as_bool).unwrap_or(false)))
            .unwrap_or((false, false)),
    })
}

/// The prepared result of a job, or (the project was saved elsewhere while it ran, so its media
/// folder moved) the result prepared again here.
pub(crate) fn prepared_or_again(
    s: &Session,
    pj: &super::PendingAi,
    files: &[std::path::PathBuf],
    prefix: &str,
    count: i64,
    rate: FrameRate,
    size: (u32, u32),
    max_bytes: u64,
) -> Result<std::sync::Arc<PreparedSequence>> {
    match prepared(pj) {
        Ok(p) if std::path::Path::new(&p.path).starts_with(pj.result_dir()) => {
            Ok(std::sync::Arc::new(PreparedSequence { path: p.path.clone(), name: p.name.clone(), src: p.src.clone(), identity: p.identity }))
        }
        _ => prepare_sequence(files, pj.result_dir(), prefix, count, rate, size, max_bytes, &*s.services, &|| false)
            .map(std::sync::Arc::new)
            .map_err(EngineError::Other),
    }
}

/// On the job's thread: the result files checked and opened as a sequence ([`super::Finisher`]).
pub(crate) fn finish(
    files: &[std::path::PathBuf],
    ctx: &Value,
    services: &dyn crate::Services,
    stop: &dyn Fn() -> bool,
) -> std::result::Result<Box<dyn std::any::Any + Send>, String> {
    let cx = context(ctx).map_err(|e| e.to_string())?;
    let dir = files.first().and_then(|f| f.parent()).ok_or("the helper wrote no frames")?;
    let size = (cx.size.0.saturating_mul(cx.factor), cx.size.1.saturating_mul(cx.factor));
    let p = prepare_sequence(files, dir, PREFIX, cx.frames, cx.rate, size, MAX_FRAME_BYTES, services, stop)?;
    Ok(Box::new(p))
}

/// Motion that shows a picture `factor` times larger exactly as before: Scale (and its width and
/// height) divided, keyframes too, unless Scale to Frame Size fits the picture anyway; a set
/// Anchor Point multiplied in every case (it is in source pixels). A clip without a Motion effect
/// gets one (otherwise it would show at 100 %).
fn rescale_motion(it: &mut TrackItem, factor: f64) {
    let fit = it.scale_to_frame;
    if !fit
        && it.effect("motion").is_none()
        && let Some(m) = filmcraft_project::effect::intrinsic_video().into_iter().find(|e| e.effect == "motion")
    {
        it.effects.insert(0, m);
    }
    let Some(m) = it.effect_mut("motion") else { return };
    if !fit {
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
    let size = (cx.size.0.saturating_mul(cx.factor), cx.size.1.saturating_mul(cx.factor));
    let prep = prepared_or_again(s, pj, files, PREFIX, cx.frames, cx.rate, size, MAX_FRAME_BYTES)?;
    let seq = pj.fingerprint.seq;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    super::apply_with_media(s, pj, "Upscale", files, move |_| {
        let step: super::Step = Box::new(move |s: &mut Session| {
            let item = import_prepared(s, &prep, &format!("Upscaled: {}", cx.name), cx.par, cx.alpha)?;
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
