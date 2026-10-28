//! `ai.reframe` (M6a): keep the main person in frame for another aspect (9:16 from 16:9…).
//!
//! `ai.reframe {clips?, aspect?, smoothing=0.5, followSpeed=0.5, subjectIndex=0, sampleFps=6,
//! wait=false}` starts one helper job per selected video clip. The helper samples the clip's
//! source range at `sampleFps`, tracks people (YOLOX-s), picks the main subject (largest and most
//! present; `subjectIndex` picks another) and returns a smoothed crop centre per sample for the
//! target aspect (default: the sequence's). The result is applied in one undo step ("Auto
//! Reframe"): Motion Scale fills the frame with the crop, Motion Position follows the crop centre
//! as keyframes (thinned where they add nothing), the Anchor Point returns to the centre and Scale
//! to Frame Size is turned off. A clip that is rotated, scaled non-uniformly or whose pixels have
//! another aspect than the sequence's is refused, since the crop would not land where it was
//! measured.

use filmcraft_geom::Vec2;
use filmcraft_project::{ClipId, Keyframe, MediaRef, Param, ParamValue, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use crate::commands::{bad, bool_p, clips_p, f64_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clips":[id]?,"aspect":"9:16"|[w,h]? (default: the sequence's),"smoothing":0..1=0.5,"followSpeed":0..1=0.5,"subjectIndex":n=0,"sampleFps":1..30=6,"wait":bool=false}"#;

/// Samples per job at most (the helper's limit too).
const MAX_SAMPLES: usize = 20_000;
/// Largest `reframe.json` read.
const MAX_RESULT: u64 = 64 << 20;
/// A position keyframe is dropped when the line through its neighbours passes within this many
/// sequence pixels of it.
const THIN_PX: f64 = 0.25;

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    super::helper_ready(s, "reframe")?;
    has_seq(s)?;
    let q = s.active_sequence().ok_or("no sequence is open")?;
    let video = s.state.selection.iter().any(|c| q.find_item(*c).is_some_and(|(t, _)| q.track(t).is_some_and(|t| t.kind == TrackKind::Video)));
    if video { Ok(()) } else { Err("select a video clip in the timeline".into()) }
}

/// `"9:16"`, `[9, 16]` or nothing (the sequence's own aspect).
fn aspect_p(s: &Session, p: &Value) -> Result<(f64, f64)> {
    let cmd = "ai.reframe";
    let (w, h) = match p.get("aspect") {
        None | Some(Value::Null) => {
            let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
            (q.settings.width as f64, q.settings.height as f64)
        }
        Some(Value::String(a)) => {
            let (w, h) = a.split_once(':').ok_or_else(|| bad(cmd, "`aspect` is \"w:h\" or [w, h]"))?;
            (w.trim().parse().map_err(|_| bad(cmd, "`aspect` width"))?, h.trim().parse().map_err(|_| bad(cmd, "`aspect` height"))?)
        }
        Some(Value::Array(a)) if a.len() == 2 => (a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0)),
        Some(_) => return Err(bad(cmd, "`aspect` is \"w:h\" or [w, h]")),
    };
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 && w < 1e5 && h < 1e5) {
        return Err(bad(cmd, "`aspect` must be two positive numbers"));
    }
    Ok((w, h))
}

/// Why `clip` cannot be reframed, if it cannot.
fn check_clip(s: &Session, c: ClipId) -> std::result::Result<(String, (u32, u32)), String> {
    let q = s.active_sequence().ok_or("no sequence is open")?;
    let (t, it) = q.find_item(c).ok_or("the clip is not in the open sequence")?;
    if q.track(t).is_none_or(|t| t.kind != TrackKind::Video) {
        return Err(format!("{}: not a video clip", it.name));
    }
    let (_, m, _) = s.project.resolve_media(it.item).ok_or_else(|| format!("{}: no media", it.name))?;
    let MediaRef::File { path } = &m.media else { return Err(format!("{}: generated media has nothing to track", it.name)) };
    let v = m.info.video.as_ref().ok_or_else(|| format!("{}: the media has no video", it.name))?;
    let src_par = filmcraft_project::sane_par(m.interpret.par.unwrap_or(v.par));
    let seq_par = filmcraft_project::sane_par(q.settings.par);
    if (filmcraft_project::par_ratio(src_par) - filmcraft_project::par_ratio(seq_par)).abs() > 1e-6 {
        return Err(format!("{}: its pixel aspect differs from the sequence's", it.name));
    }
    if let Some(mo) = it.effect("motion").filter(|e| e.enabled) {
        let rotated = mo.param("rotation").is_some_and(|p| p.is_animated() || p.value.as_f64().is_some_and(|r| r != 0.0));
        let uniform = mo.param("uniform_scale").and_then(|p| p.value.as_bool()).unwrap_or(true);
        if rotated || !uniform {
            return Err(format!("{}: reset its Rotation and Uniform Scale first", it.name));
        }
    }
    Ok((path.clone(), (v.width, v.height)))
}

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.reframe";
    let aspect = aspect_p(s, p)?;
    let unit = |k: &str, d: f64| f64_p(p, k).unwrap_or(d).clamp(0.0, 1.0);
    let (smoothing, follow) = (unit("smoothing", 0.5), unit("followSpeed", 0.5));
    let subject = u64_p(p, "subjectIndex").unwrap_or(0).min(1000);
    let fps = f64_p(p, "sampleFps").unwrap_or(6.0).clamp(1.0, 30.0);
    let detector = match p.get("detector").and_then(Value::as_str) {
        None | Some("yolox") => "yolox",
        Some("brightest") => "brightest",
        Some(d) => return Err(bad(cmd, format!("unknown detector `{d}`"))),
    };
    let wait = bool_p(p, "wait").unwrap_or(false);
    let clips = clips_p(s, p);
    let mut jobs = Vec::new();
    for c in clips {
        let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
        if q.find_item(c).and_then(|(t, _)| q.track(t)).is_none_or(|t| t.kind != TrackKind::Video) {
            continue;
        }
        let (_, size) = check_clip(s, c).map_err(EngineError::Other)?;
        let (_, it) = q.find_item(c).ok_or(EngineError::NoSequence)?;
        // sample times along the clip, in media time; spread out further for very long clips
        let dur = it.duration.0.max(1) as f64;
        let n = ((dur / TICKS_PER_SECOND as f64) * fps).ceil().clamp(1.0, MAX_SAMPLES as f64) as i64;
        let step = (it.duration.0 / n.max(1)).max(1);
        let mts: Vec<i64> = (0..n).map(|k| it.source_time_at(it.start + Tick(step * k)).0.max(0)).collect();
        let times: Vec<f64> = mts.iter().map(|t| *t as f64 / TICKS_PER_SECOND as f64).collect();
        let params = json!({"times": times, "aspect": [aspect.0, aspect.1], "smoothing": smoothing, "followSpeed": follow, "subjectIndex": subject, "detector": detector});
        let context = json!({"mt": mts, "source": [size.0, size.1], "aspect": [aspect.0, aspect.1]});
        let spec = super::JobSpec { kind: "reframe", label: "Auto Reframe", clips: &[c], params, context, timeout: None, prepare: None };
        jobs.push(super::start_job(s, spec, wait)?);
    }
    if jobs.is_empty() {
        return Err(EngineError::Other("select a video clip in the timeline".into()));
    }
    Ok(if jobs.len() == 1 { jobs.remove(0) } else { json!({"jobs": jobs}) })
}

/// Keyframes `(t, value)` without those the straight line between their kept neighbours already
/// gives (within [`THIN_PX`]); the first and last stay.
fn thin(keys: Vec<(Tick, Vec2)>) -> Vec<(Tick, Vec2)> {
    if keys.len() <= 2 {
        return keys;
    }
    let mut out: Vec<(Tick, Vec2)> = Vec::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        let (Some(prev), Some(next)) = (out.last(), keys.get(i + 1)) else {
            out.push(*k);
            continue;
        };
        let span = (next.0.0 - prev.0.0) as f64;
        let f = if span > 0.0 { (k.0.0 - prev.0.0) as f64 / span } else { 0.0 };
        let lerp = Vec2::new(prev.1.x + (next.1.x - prev.1.x) * f, prev.1.y + (next.1.y - prev.1.y) * f);
        if (lerp.x - k.1.x).abs() > THIN_PX || (lerp.y - k.1.y).abs() > THIN_PX {
            out.push(*k);
        }
    }
    out
}

/// Read and check `reframe.json` against what the job asked for.
fn read_result(pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<(f64, f64, f64, f64, Vec<Vec2>)> {
    let file = files
        .iter()
        .find(|f| f.file_name().is_some_and(|n| n == "reframe.json"))
        .ok_or_else(|| EngineError::Other("the helper wrote no reframe.json".into()))?;
    let mut f = filmcraft_sidecar::open_result(file, &pj.out_dir, MAX_RESULT).map_err(|e| EngineError::Other(e.to_string()))?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut f, &mut text).map_err(|e| EngineError::Other(format!("reframe.json: {e}")))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| EngineError::Other(format!("reframe.json: {e}")))?;
    let pair = |k: &str| -> Option<(f64, f64)> {
        let a = v.get(k)?.as_array()?;
        Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
    };
    let bad = |m: &str| EngineError::Other(format!("reframe.json: {m}"));
    let (sw, sh) = pair("source").ok_or_else(|| bad("no source size"))?;
    let (cw, ch) = pair("crop").ok_or_else(|| bad("no crop size"))?;
    let want = pj.context.get("source").and_then(Value::as_array);
    let want = want.and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)));
    if want != Some((sw, sh)) {
        return Err(bad("the helper read another frame size than the media's"));
    }
    if !(cw.is_finite() && ch.is_finite() && cw > 0.0 && ch > 0.0 && cw <= sw + 0.5 && ch <= sh + 0.5) {
        return Err(bad("the crop does not fit the frame"));
    }
    let n = pj.context.get("mt").and_then(Value::as_array).map_or(0, Vec::len);
    let centers = v.get("centers").and_then(Value::as_array).ok_or_else(|| bad("no centres"))?;
    if centers.len() != n || n == 0 {
        return Err(bad("one centre per sample is expected"));
    }
    let mut out = Vec::with_capacity(n);
    for c in centers {
        let a = c.as_array().ok_or_else(|| bad("a centre is not [x, y]"))?;
        let (x, y) = (a.first().and_then(Value::as_f64).unwrap_or(f64::NAN), a.get(1).and_then(Value::as_f64).unwrap_or(f64::NAN));
        if !(x.is_finite() && y.is_finite() && (-1.0..=sw + 1.0).contains(&x) && (-1.0..=sh + 1.0).contains(&y)) {
            return Err(bad("a centre is outside the frame"));
        }
        out.push(Vec2::new(x, y));
    }
    Ok((sw, sh, cw, ch, out))
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let (sw, sh, cw, ch, centers) = read_result(pj, files)?;
    let mts: Vec<Tick> = pj.context.get("mt").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_i64).map(Tick).collect();
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let seq = pj.fingerprint.seq;
    let (qw, qh) = s.project.sequence(seq).map(|q| (q.settings.width as f64, q.settings.height as f64)).ok_or(EngineError::NoSequence)?;
    // the crop fills the frame; Position puts the crop centre at the frame centre
    let k = (qw / cw).max(qh / ch);
    let keys: Vec<(Tick, Vec2)> =
        mts.iter().zip(&centers).map(|(t, c)| (*t, Vec2::new(qw / 2.0 + (sw / 2.0 - c.x) * k, qh / 2.0 + (sh / 2.0 - c.y) * k))).collect();
    let mut keys = thin(keys);
    keys.sort_by_key(|k| k.0);
    keys.dedup_by_key(|k| k.0);
    let step: super::Step = Box::new(move |s: &mut Session| {
        s.edit("Auto Reframe", move |pr, _| {
            let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            let (_, it) = q.find_item_mut(clip).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
            it.scale_to_frame = false;
            let m = it.effects.iter_mut().find(|e| e.effect == "motion").ok_or_else(|| EngineError::Other("the clip has no Motion".into()))?;
            m.enabled = true;
            let mut position = Param::new(ParamValue::Vec2(keys.first().map_or(Vec2::new(qw / 2.0, qh / 2.0), |k| k.1)));
            position.keyframes = keys.iter().map(|(t, v)| Keyframe::new(*t, ParamValue::Vec2(*v))).collect();
            m.params.insert("position".into(), position);
            m.params.insert("scale".into(), Param::new(ParamValue::Float(k * 100.0)));
            m.params.insert("uniform_scale".into(), Param::new(ParamValue::Bool(true)));
            m.params.insert("anchor".into(), Param::new(ParamValue::Vec2(Vec2::new(f64::NAN, f64::NAN))));
            Ok(())
        })
    });
    super::apply_atomic(s, "Auto Reframe", vec![step])
}

#[cfg(test)]
#[path = "ai_reframe_tests.rs"]
mod tests;
