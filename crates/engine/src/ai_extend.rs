//! Clip ▸ AI ▸ Generative Extend (`ai.generativeExtend`, M8f).
//!
//! `ai.generativeExtend {clip?, seconds=3 (1 to 10), side="end"|"start", prompt?, model?,
//! resolution?, confirm, resume?, wait=false}` continues a clip by a few seconds with a hosted
//! image-to-video model (fal.ai, called by the `craft-ai` helper, which needs a key). This is a
//! paid service and sends the frame off this computer, so a new request runs only with
//! `confirm: true`; without it the command fails with [`DISCLOSURE`] (ADR 0002 item 13). On the
//! job's thread FilmCraft takes the clip's last source frame (first with `side: "start"`; the raw
//! source frame, before the clip's Motion and effects), scales it to cover the sequence's frame
//! size (long side at most 1280 pixels) and writes it as a PNG; the helper sends it with the
//! prompt and downloads the video it gets back. The helper submits once per job and never again;
//! a failure after fal accepted the request names its id, and `resume: "<id>"` fetches that
//! request's result for the same clip without a new charge (no frame is made, no `confirm`
//! needed).
//!
//! When the result is applied (the usual fingerprint rules of ADR 0002: the clip must not have
//! changed) the video, already moved by the job's thread from the cache into the project's AI
//! media folder (`JobSpec::media`), is imported into a bin named "Generated" and placed as an
//! insert edit in the job's sequence (whichever sequence is open), in one undo step: the new clip
//! goes straight after the clip (straight before it, playing backwards, with `side: "start"`) and
//! everything behind it on the track and on sync-locked tracks moves later. On a sync-locked track
//! an item crossing that point (a music bed, an L-cut) is split there and a gap of the new clip's
//! length opens in it, as for any insert; the clip's own linked audio is not extended. A
//! transition on the extended edge moves to the new clip's outer edge. The new clip is named
//! "<clip> (extended)", fills the frame (Fill Frame), keeps the clip's effects except Motion and
//! animated ones, and plays at 100 % at the video's own frame rate whatever the clip's speed. Its
//! length is the seconds asked for, snapped down to whole sequence frames, or the video's own if
//! that is shorter. A locked clip track is refused before anything is sent.

use std::path::{Path, PathBuf};

use filmcraft_edit as edit;
use filmcraft_media::FrameRequest;
use filmcraft_project::{BinEntry, BinId, ClipId, ItemId, TrackItem, TrackKind, TransitionId};
use filmcraft_sidecar::JobResult;
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

use crate::ai::{JobSpec, PendingAi, Step, helper_ready, start_job};
use crate::commands::{CommandSpec, bad, bool_p, clip_p, has_seq, str_p};
use crate::{EngineError, Result, Session};

/// Longest side of the frame sent to the model.
const MAX_FRAME_SIDE: u32 = 1280;
/// Longest prompt.
const MAX_PROMPT: usize = 1000;
/// The bin the results go in.
const BIN: &str = "Generated";
/// Largest generated video accepted (the helper's own cap).
const MAX_VIDEO_BYTES: u64 = 300 << 20;
/// Why a new request needs `confirm: true`.
pub(crate) const DISCLOSURE: &str = "Generative Extend sends the clip's frame (and the prompt) to fal.ai and is billed to your fal.ai \
account at the model's price; run it again with `confirm: true` to go ahead (`resume: <request id>` fetches an earlier request's \
result without a new charge)";

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "ai.generativeExtend",
        label: "Generative Extend…",
        menu: &["Clip", "AI"],
        shortcut: None,
        params: r#"{"clip":id?,"seconds":1..10=3,"side":"end"|"start"="end","prompt":str?,"model":str?,"resolution":"480p"|"720p"?,"confirm":bool=false (true: send the frame to fal.ai, billed),"resume":str? (a fal request id: its result, no new charge),"wait":bool=false}"#,
        enabled,
        run,
        journal: true,
    }]
}

/// The video clip to extend: `clip`, else the first selected video clip.
fn target(s: &Session, p: &Value) -> Option<ClipId> {
    let q = s.active_sequence()?;
    let video = |c: ClipId| q.find_item(c).filter(|(t, _)| q.track(*t).is_some_and(|t| t.kind == TrackKind::Video)).map(|_| c);
    match clip_p(p, "clip") {
        Some(c) => video(c),
        None => s.state.selection.iter().find_map(|c| video(*c)),
    }
}

fn enabled(s: &Session) -> std::result::Result<(), String> {
    helper_ready(s, "generativeExtend")?;
    has_seq(s)?;
    if target(s, &Value::Null).is_none() {
        return Err("select a video clip in the timeline".into());
    }
    Ok(())
}

/// The frame size sent to the model: the sequence's, scaled so the long side is at most
/// [`MAX_FRAME_SIDE`], both sides even.
fn frame_size(w: u32, h: u32) -> (u32, u32) {
    let k = (f64::from(MAX_FRAME_SIDE) / f64::from(w.max(h).max(1))).min(1.0);
    let even = |v: u32| (((f64::from(v) * k).round() as u32).max(16) + 1) & !1;
    (even(w), even(h))
}

/// `rgba` (straight RGBA8, `w`x`h`) scaled to cover `tw`x`th` and cropped to it by the middle
/// (bilinear), opaque.
fn cover(rgba: &[u8], w: u32, h: u32, tw: u32, th: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || tw == 0 || th == 0 || rgba.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    // source pixels per output pixel, and the crop offset in source pixels
    let k = (f64::from(w) / f64::from(tw)).min(f64::from(h) / f64::from(th));
    let (ox, oy) = ((f64::from(w) - k * f64::from(tw)) / 2.0, (f64::from(h) - k * f64::from(th)) / 2.0);
    let at = |x: u32, y: u32, c: usize| f64::from(rgba[((y.min(h - 1) as usize) * w as usize + x.min(w - 1) as usize) * 4 + c]);
    let mut out = Vec::with_capacity(tw as usize * th as usize * 4);
    for y in 0..th {
        let sy = (oy + (f64::from(y) + 0.5) * k - 0.5).max(0.0);
        let (y0, fy) = (sy.floor() as u32, sy.fract());
        for x in 0..tw {
            let sx = (ox + (f64::from(x) + 0.5) * k - 0.5).max(0.0);
            let (x0, fx) = (sx.floor() as u32, sx.fract());
            for c in 0..3 {
                let top = at(x0, y0, c) * (1.0 - fx) + at(x0 + 1, y0, c) * fx;
                let bottom = at(x0, y0 + 1, c) * (1.0 - fx) + at(x0 + 1, y0 + 1, c) * fx;
                out.push((top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8);
            }
            out.push(255);
        }
    }
    Some(out)
}

/// Write the frame covering `size` as a PNG.
fn write_png(path: &Path, rgba: &[u8], w: u32, h: u32, size: (u32, u32)) -> std::result::Result<(), String> {
    let pixels = cover(rgba, w, h, size.0, size.1).ok_or("the frame has an unexpected size")?;
    let png = filmcraft_export::encode_png(pixels, size.0, size.1).map_err(|e| format!("can't encode the frame: {e}"))?;
    std::fs::write(path, png).map_err(|e| format!("can't write the frame: {e}"))
}

/// The timeline time whose frame is sent: the clip's last frame (`end`) or its first.
fn frame_time(it: &TrackItem, fd: Tick, at_end: bool) -> Tick {
    if at_end { (it.end() - fd).max(it.start) } else { it.start }
}

/// A fal request id as the helper accepts it.
fn request_id(v: &str) -> bool {
    (1..=80).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.generativeExtend";
    let clip = target(s, p).ok_or_else(|| bad(cmd, "select a video clip in the timeline (or pass `clip`)"))?;
    let seconds = match p.get("seconds").filter(|v| !v.is_null()) {
        None => 3.0,
        Some(v) => v.as_f64().ok_or_else(|| bad(cmd, "`seconds` must be a number between 1 and 10"))?,
    };
    if !seconds.is_finite() || !(1.0..=10.0).contains(&seconds) {
        return Err(bad(cmd, "`seconds` must be between 1 and 10"));
    }
    let side = match str_p(p, "side") {
        None | Some("end") => "end",
        Some("start") => "start",
        Some(_) => return Err(bad(cmd, "`side` must be \"end\" or \"start\"")),
    };
    let resume = match p.get("resume").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_str().filter(|r| request_id(r)).ok_or_else(|| bad(cmd, "`resume` must be a fal request id (letters, digits, - and _)"))?),
    };
    // a new request is paid and sends the frame away: never without an explicit yes
    if resume.is_none() && bool_p(p, "confirm") != Some(true) {
        return Err(bad(cmd, DISCLOSURE));
    }
    let mut params = json!({"seconds": seconds, "side": side});
    if let Some(prompt) = str_p(p, "prompt").map(str::trim).filter(|t| !t.is_empty()) {
        if prompt.chars().count() > MAX_PROMPT {
            return Err(bad(cmd, format!("`prompt` is longer than {MAX_PROMPT} characters")));
        }
        params["prompt"] = json!(prompt);
    }
    for key in ["model", "resolution"] {
        if let Some(v) = str_p(p, key).filter(|v| !v.is_empty() && v.len() <= 120) {
            params[key] = json!(v);
        }
    }
    if let Some(r) = resume {
        params["resume"] = json!(r);
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let (track, it) = q.find_item(clip).ok_or_else(|| bad(cmd, "no such clip"))?;
    // the result could not be placed: refuse before paying
    if q.track(track).is_some_and(|t| t.locked) {
        return Err(bad(cmd, "the clip's track is locked; unlock it first"));
    }
    let at_end = side == "end";
    let media_time = it.source_time_at(frame_time(it, q.settings.frame_rate.frame_duration(), at_end));
    let size = frame_size(q.settings.width, q.settings.height);
    let name = it.name.clone();
    let prepare = match resume {
        // the request exists already: no frame is made or sent
        Some(_) => None,
        None => {
            let src = s.source(it.item).filter(|src| src.info().has_video()).ok_or_else(|| bad(cmd, "the clip has no picture to take a frame from"))?;
            // a stand-in picture for missing media must never be sent to a paid model
            if let Some(root) = s.project.resolve_media(it.item).map(|(root, _, _)| root)
                && let Some(why) = s.media.offline_status(root)
            {
                return Err(bad(cmd, format!("the clip's media is offline ({}); relink it first", why.error)));
            }
            let prepare: crate::ai::Prepare = Box::new(move |dir: &Path, _stop: &dyn Fn() -> bool, _step: &mut dyn FnMut(f64)| {
                let info = src.info().video.clone().ok_or("the clip has no picture")?;
                let scale = (f64::from(size.0) / f64::from(info.width.max(1))).max(f64::from(size.1) / f64::from(info.height.max(1))).min(1.0) as f32;
                let frame = src.video_frame(FrameRequest { time: media_time, scale }).map_err(|e| format!("can't read the clip's frame: {e}"))?;
                let (w, h) = (frame.width, frame.height);
                let path = dir.join("frame.png");
                write_png(&path, &frame.to_rgba8(), w, h, size)?;
                Ok(vec![path.to_string_lossy().to_string()])
            });
            Some(prepare)
        }
    };
    let label = format!("Generative Extend: {name}");
    let spec = JobSpec {
        kind: "generativeExtend",
        label: &label,
        clips: &[clip],
        params,
        // what the apply step uses: what was asked, never the helper's echo
        context: json!({"seconds": seconds, "atEnd": at_end, "name": name}),
        timeout: None,
        prepare,
        loose: false,
        media: true,
    };
    start_job(s, spec, bool_p(p, "wait").unwrap_or(false))
}

/// The "Generated" bin, made if it is not there.
fn generated_bin(s: &mut Session) -> Result<BinId> {
    let existing = s.project.root.children.iter().find_map(|e| match e {
        BinEntry::Bin(b) if b.name == BIN => Some(b.id),
        _ => None,
    });
    if let Some(id) = existing {
        return Ok(id);
    }
    s.edit("Generative Extend", |pr, _| Ok(pr.add_bin(BIN, None)))
}

/// What [`place`] needs.
struct Placement {
    seq: ItemId,
    clip: ClipId,
    path: PathBuf,
    seconds: f64,
    at_end: bool,
    name: String,
    /// The video's length by the helper's account (used when the import reports none).
    reported: Option<f64>,
}

/// Import the generated video (moved by the job's thread out of the cache into the project's AI
/// media folder) and place it next to the clip (see the module docs).
pub(crate) fn apply(s: &mut Session, pj: &PendingAi, result: &JobResult, files: &[PathBuf]) -> Result<()> {
    let video = files
        .iter()
        .find(|f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v")))
        .ok_or_else(|| EngineError::Other("the helper's result has no video".into()))?
        .clone();
    // checked again on open (a file swapped after the job's check is refused)
    filmcraft_sidecar::open_result(&video, pj.result_dir(), MAX_VIDEO_BYTES).map_err(|e| EngineError::Other(e.to_string()))?;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let ctx = &pj.context;
    let seconds = ctx.get("seconds").and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(3.0).clamp(1.0, 10.0);
    let at_end = ctx.get("atEnd").and_then(Value::as_bool).unwrap_or(true);
    let name = ctx.get("name").and_then(Value::as_str).unwrap_or("Clip").to_string();
    let reported = result.meta.get("videoDuration").and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0);
    let (seq, video_name) = (pj.fingerprint.seq, video.file_name().map(|n| n.to_owned()));
    crate::ai::apply_with_media(s, pj, "Generative Extend", files, move |moved| {
        let path = moved.into_iter().find(|f| f.file_name().map(|n| n.to_owned()) == video_name).unwrap_or(video);
        let step: Step = Box::new(move |s: &mut Session| place(s, Placement { seq, clip, path, seconds, at_end, name, reported }));
        vec![step]
    })
}

fn place(s: &mut Session, pl: Placement) -> Result<()> {
    // the job's sequence, whichever is open now
    let active = s.state.active_sequence.replace(pl.seq);
    let r = place_in(s, &pl);
    s.state.active_sequence = active;
    r
}

fn place_in(s: &mut Session, pl: &Placement) -> Result<()> {
    let bin = generated_bin(s)?;
    let item = crate::commands::import_streamed(s, &pl.path.to_string_lossy(), Some(bin))?;
    let q = s.project.sequence(pl.seq).ok_or(EngineError::NoSequence)?;
    let (track, it) = q.find_item(pl.clip).ok_or_else(|| EngineError::Other("the clip was deleted".into()))?;
    let it = it.clone();
    let rate = q.settings.frame_rate;
    let fd = rate.frame_duration();
    let probed = s.project.item(item).and_then(|i| i.as_media()).map(|m| m.info.duration).filter(|d| *d > Tick::ZERO);
    let media = probed.or_else(|| pl.reported.map(Tick::from_seconds_f64)).unwrap_or(Tick::ZERO);
    let want = Tick::from_seconds_f64(pl.seconds).min(media);
    let dur = Tick(want.0 / fd.0.max(1) * fd.0);
    if dur < fd {
        return Err(EngineError::Other("the generated video is shorter than one frame".into()));
    }
    let at = if pl.at_end { it.end() } else { it.start };
    let mut made = None;
    let (clip_name, item_name) = (format!("{} (extended)", pl.name), format!("{} (extended)", pl.name));
    s.edit("Generative Extend", |pr, _| {
        if let Some(i) = pr.item_mut(item) {
            i.name = item_name;
        }
        made = pr.make_track_item(item, TrackKind::Video, at, TimeRange::new(Tick::ZERO, dur), rate);
        Ok(())
    })?;
    let mut ti = made.ok_or_else(|| EngineError::Other("the generated video cannot be placed".into()))?;
    ti.reverse = !pl.at_end;
    ti.name = clip_name;
    // the clip's look carries over (colour, Lumetri, opacity...); its Motion does not (the
    // video is framed by Fill Frame below), nor effects animated over the clip's own time
    let fresh = std::mem::take(&mut ti.effects);
    ti.effects = it
        .effects
        .iter()
        .filter_map(|e| if e.effect == "motion" || e.is_animated() { fresh.iter().find(|f| f.effect == e.effect).cloned() } else { Some(e.clone()) })
        .collect();
    for f in fresh {
        if !ti.effects.iter().any(|e| e.effect == f.effect) {
            ti.effects.push(f);
        }
    }
    let new_id = ti.id;
    let (clip, at_end) = (pl.clip, pl.at_end);
    s.edit_sequence("Generative Extend", move |seq, ctx, _| {
        // transitions on the extended edge move to the new clip's outer edge
        let moved: Vec<(TransitionId, Tick)> = seq
            .track(track)
            .map(|t| {
                t.transitions
                    .iter()
                    .filter(|tr| if at_end { tr.from == Some(clip) } else { tr.to == Some(clip) })
                    .map(|tr| (tr.id, if at_end { tr.start + dur } else { tr.start }))
                    .collect()
            })
            .unwrap_or_default();
        edit::insert(seq, vec![(track, ti)], ctx)?;
        if let Some(t) = seq.track_mut(track) {
            for tr in t.transitions.iter_mut() {
                if let Some((_, start)) = moved.iter().find(|(id, _)| *id == tr.id) {
                    tr.start = *start;
                    if at_end {
                        tr.from = Some(new_id);
                    } else {
                        tr.to = Some(new_id);
                    }
                }
            }
        }
        Ok(())
    })?;
    // covers the frame like the source did (the model's aspect is close to, not exactly, the
    // sequence's)
    if crate::clip_ops::fit_fill(s, &json!({"clips": [new_id.0]}), true).is_err() {
        // no frame size to fill from: fitted instead
        s.edit_sequence("Generative Extend", move |seq, _, _| {
            if let Some((_, it)) = seq.find_item_mut(new_id) {
                it.scale_to_frame = true;
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "ai_extend_tests.rs"]
mod tests;
