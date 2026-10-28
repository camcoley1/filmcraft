//! CapCut desktop drafts (`draft_content.json`, and `draft_info.json` of newer versions), import only.
//!
//! Source: the community draft schema (pyJianYingDraft and the CapCut draft notes). No CapCut code
//! or binary was consulted. The file is plain JSON; recent CapCut builds encrypt it, which is
//! reported as a parse error that says so.
//!
//! Read: the canvas size and frame rate, video and audio tracks with their segments (timeline and
//! source ranges, speed, volume, visibility), the media they use (path, size; a missing file is
//! offline and goes through the normal relink flow) and text tracks, which become one caption
//! track each. Every time is in microseconds, converted to [`Tick`] exactly (`us * 254016`).
//!
//! Reported, not imported (a warning with a count): effects, filters, transitions, stickers,
//! animations, keyframes, text styling, and position / scale / rotation of segments.
//!
//! Hostile input: the file is capped at [`MAX_BYTES`], tracks, segments and materials at fixed
//! counts, ranges that are negative or overflow are skipped with a warning, unknown fields are
//! ignored and a wrong type is a parse error.

use std::collections::HashMap;

use filmcraft_media::MediaKind;
use filmcraft_project::{Caption, CaptionFormat, CaptionTrack, ClipId, ItemId, TrackId, TrackKind, validate_frame_size};
use filmcraft_time::{FrameRate, Tick};
use serde::Deserialize;
use serde_json::Value;

use crate::common::{Builder, MediaSpec, empty_sequence, file_name, gain_to_db, rate_from_f64, resolve_path, settings_for};
use crate::{Error, Format, ImportOptions, Imported, Report, Result};

/// Largest draft read (64 MiB; 16 MiB on the web).
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
#[cfg(target_arch = "wasm32")]
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Most tracks read.
const MAX_TRACKS: usize = 256;
/// Most extra tracks a segment overlapping its own track may move down to.
const MAX_EXTRA_TRACKS: usize = 8;
/// Most segments read in all tracks.
const MAX_SEGMENTS: usize = 200_000;
/// Most materials read per kind.
const MAX_MATERIALS: usize = 200_000;
/// Ticks per microsecond (254 016 000 000 / 1 000 000).
const TICKS_PER_US: i64 = 254_016;
/// Draft file names inside a draft folder, in the order tried.
pub const DRAFT_FILES: [&str; 2] = ["draft_content.json", "draft_info.json"];

/// Whether `bytes` look like a CapCut draft: a JSON object with `canvas_config`, `tracks` and
/// `materials` at the top level (decided from the head of the file without parsing all of it).
pub fn sniff(bytes: &[u8]) -> bool {
    let b = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if b.iter().find(|c| !c.is_ascii_whitespace()) != Some(&b'{') || b.len() > MAX_BYTES {
        return false;
    }
    #[derive(Deserialize)]
    struct Shape {
        canvas_config: Value,
        tracks: Value,
        materials: Value,
    }
    serde_json::from_slice::<Shape>(b).is_ok_and(|s| s.canvas_config.is_object() && s.tracks.is_array() && s.materials.is_object())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Draft {
    canvas_config: Canvas,
    fps: Option<f64>,
    tracks: Vec<TrackDoc>,
    materials: Materials,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Canvas {
    width: Option<i64>,
    height: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TrackDoc {
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    segments: Vec<Segment>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Segment {
    material_id: String,
    target_timerange: Range,
    source_timerange: Option<Range>,
    speed: Option<f64>,
    volume: Option<f64>,
    visible: Option<bool>,
    clip: Option<Value>,
    common_keyframes: Vec<Value>,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default)]
struct Range {
    start: Option<i64>,
    duration: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Materials {
    videos: Vec<MediaDoc>,
    audios: Vec<MediaDoc>,
    texts: Vec<TextDoc>,
    /// Other material arrays by name, only counted (everything CapCut adds that is not read).
    #[serde(flatten)]
    other: HashMap<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MediaDoc {
    id: String,
    path: Option<String>,
    material_name: Option<String>,
    name: Option<String>,
    duration: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TextDoc {
    id: String,
    content: Option<Value>,
}

/// Material arrays that are reported when non-empty, with the words used in the report.
const UNSUPPORTED: [(&str, &str); 9] = [
    ("effects", "effects"),
    ("video_effects", "video effects"),
    ("audio_effects", "audio effects"),
    ("filters", "filters"),
    ("transitions", "transitions"),
    ("stickers", "stickers"),
    ("material_animations", "animations"),
    ("beats", "beat markers"),
    ("speeds", "speed curves (the constant speed of each segment was kept)"),
];

fn perr(message: impl Into<String>) -> Error {
    Error::parse(Format::CapCut, message)
}

/// Microseconds to ticks, `None` when they overflow the supported time range.
fn ticks(us: i64) -> Option<Tick> {
    let t = us.checked_mul(TICKS_PER_US).map(Tick)?;
    (t >= Tick::MIN && t <= Tick::MAX).then_some(t)
}

pub(crate) fn import(bytes: &[u8], opts: &ImportOptions) -> Result<(Imported, Report)> {
    if bytes.len() > MAX_BYTES {
        return Err(perr(format!("the draft is larger than {} MiB", MAX_BYTES / (1024 * 1024))));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let draft: Draft = serde_json::from_slice(bytes).map_err(|e| {
        let hint = if bytes.first().is_some_and(|c| *c != b'{') {
            " (newer CapCut versions encrypt their drafts; export the project as FCPXML or XML from CapCut instead)"
        } else {
            ""
        };
        perr(format!("{e}{hint}"))
    })?;
    let mut report = Report::default();
    if draft.tracks.is_empty() {
        return Err(Error::Empty);
    }
    let base = opts.base_dir.as_deref();
    let mut b = Builder::new(opts.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("CapCut Draft"));

    let rate = match draft.fps {
        Some(f) if f.is_finite() && f > 0.0 && f <= 1000.0 => rate_from_f64(f),
        _ => {
            report.warn("the draft has no usable frame rate; 30 fps was used");
            FrameRate::FPS_30
        }
    };
    let by_id = |list: &[MediaDoc]| -> HashMap<String, usize> { list.iter().take(MAX_MATERIALS).enumerate().map(|(i, m)| (m.id.clone(), i)).collect() };
    let videos = by_id(&draft.materials.videos);
    let audios = by_id(&draft.materials.audios);
    let texts: HashMap<&str, &TextDoc> = draft.materials.texts.iter().take(MAX_MATERIALS).map(|t| (t.id.as_str(), t)).collect();
    if draft.materials.videos.len().max(draft.materials.audios.len()).max(draft.materials.texts.len()) > MAX_MATERIALS {
        report.warn(format!("only the first {MAX_MATERIALS} materials of each kind were read"));
    }

    // canvas: the draft's own, else the first video material's
    let canvas = match (draft.canvas_config.width, draft.canvas_config.height) {
        (Some(w), Some(h)) => u32::try_from(w).ok().zip(u32::try_from(h).ok()),
        _ => None,
    }
    .filter(|(w, h)| validate_frame_size(*w, *h).is_ok())
    .or_else(|| {
        let v = draft.materials.videos.iter().find_map(|m| u32::try_from(m.width?).ok().zip(u32::try_from(m.height?).ok()))?;
        validate_frame_size(v.0, v.1).ok().map(|()| v)
    });
    let (width, height) = canvas.unwrap_or_else(|| {
        report.warn("the draft has no usable canvas size; 1920x1080 was used");
        (1920, 1080)
    });
    let mut seq = empty_sequence(settings_for(rate, width, height, false));
    let seq_id = b.reserve_sequence(opts.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("CapCut Draft"), seq.settings.clone(), None);

    if draft.tracks.len() > MAX_TRACKS {
        report.warn(format!("only the first {MAX_TRACKS} tracks were read"));
    }
    let mut segments_read = 0usize;
    let (mut vi, mut ai) = (0usize, 0usize);
    let mut media_items: HashMap<(bool, usize), ItemId> = HashMap::new();
    let (mut overlapping, mut bad_ranges, mut missing, mut transformed, mut keyframed, mut reverse_speed, mut caption_overlaps) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut has_text_style = false;
    let mut unsupported_tracks: HashMap<String, u32> = HashMap::new();

    for t in draft.tracks.iter().take(MAX_TRACKS) {
        let kind = match t.kind.as_str() {
            "video" => Some(TrackKind::Video),
            "audio" => Some(TrackKind::Audio),
            "text" => None,
            other => {
                *unsupported_tracks.entry(other.to_string()).or_default() += 1;
                continue;
            }
        };
        let is_text = t.kind == "text";
        let mut captions: Vec<Caption> = Vec::new();
        let preferred = match kind {
            Some(TrackKind::Video) => {
                vi += 1;
                vi - 1
            }
            Some(TrackKind::Audio) => {
                ai += 1;
                ai - 1
            }
            None => 0,
        };
        for s in &t.segments {
            segments_read += 1;
            if segments_read > MAX_SEGMENTS {
                report.warn(format!("only the first {MAX_SEGMENTS} segments were read"));
                break;
            }
            let (Some(start), Some(dur)) = (s.target_timerange.start.and_then(ticks), s.target_timerange.duration.and_then(ticks)) else {
                bad_ranges += 1;
                continue;
            };
            if start < Tick::ZERO || dur <= Tick::ZERO || start.0.checked_add(dur.0).is_none_or(|e| e > Tick::MAX.0) {
                bad_ranges += 1;
                continue;
            }
            if !s.common_keyframes.is_empty() {
                keyframed += 1;
            }
            if is_text {
                let Some(doc) = texts.get(s.material_id.as_str()) else {
                    missing += 1;
                    continue;
                };
                let text = text_of(doc.content.as_ref());
                if text.is_empty() {
                    continue;
                }
                has_text_style |= doc.content.as_ref().and_then(Value::as_str).is_some_and(|c| c.contains("\"styles\""));
                if captions.last().is_some_and(|c| c.start + c.duration > start) {
                    caption_overlaps += 1;
                    continue;
                }
                captions.push(Caption {
                    id: ClipId(b.alloc()),
                    start,
                    duration: dur,
                    text,
                    speaker: None,
                    cue_id: None,
                    settings: String::new(),
                    words: Vec::new(),
                });
                continue;
            }
            let Some(kind) = kind else { continue };
            let video = kind == TrackKind::Video;
            let (index, doc) = match if video {
                videos.get(&s.material_id).map(|i| (*i, &draft.materials.videos[*i]))
            } else {
                audios.get(&s.material_id).map(|i| (*i, &draft.materials.audios[*i]))
            } {
                Some(x) => x,
                None => {
                    missing += 1;
                    continue;
                }
            };
            let item = match media_items.get(&(video, index)) {
                Some(id) => *id,
                None => {
                    let id = media_item(&mut b, doc, video, rate, base);
                    media_items.insert((video, index), id);
                    id
                }
            };
            let mut speed = s.speed.filter(|v| v.is_finite()).unwrap_or(1.0);
            if speed < 0.0 {
                reverse_speed += 1;
            }
            speed = speed.abs();
            if !(1e-3..=1e3).contains(&speed) {
                report.warn("a segment speed outside 0.001x to 1000x was read as 100%");
                speed = 1.0;
            }
            let source_in = s.source_timerange.and_then(|r| r.start).and_then(ticks).filter(|t| *t >= Tick::ZERO).unwrap_or(Tick::ZERO);
            let mut ti = b.clip(item, kind, &doc_name(doc), start, dur, source_in);
            ti.speed = speed;
            ti.reverse = s.speed.is_some_and(|v| v < 0.0);
            ti.enabled = s.visible.unwrap_or(true);
            if let Some(v) = s.volume.filter(|v| v.is_finite() && *v >= 0.0)
                && !video
            {
                ti.gain_db = gain_to_db(v);
            }
            if let Some(c) = &s.clip
                && is_transformed(c)
            {
                transformed += 1;
            }
            // CapCut tracks hold sequential segments: only the last item can collide, so the check
            // is O(1); a segment that does not fit within a few extra tracks is skipped.
            let mut placed = false;
            for i in preferred..(preferred + MAX_EXTRA_TRACKS).min(MAX_TRACKS) {
                b.ensure_tracks(&mut seq, kind, i + 1);
                if let Some(track) = seq.tracks_mut(kind).get_mut(i)
                    && track.items.last().is_none_or(|l| l.end() <= ti.start)
                {
                    track.items.push(ti);
                    placed = true;
                    break;
                }
            }
            if !placed {
                overlapping += 1;
            }
        }
        if is_text && !captions.is_empty() {
            let name = t.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Text {}", seq.caption_tracks.len() + 1));
            let mut ct = CaptionTrack::new(TrackId(b.alloc()), name, CaptionFormat::Subtitle);
            ct.captions = captions;
            ct.sort();
            seq.caption_tracks.push(ct);
        }
        if segments_read > MAX_SEGMENTS {
            break;
        }
    }

    for (kind, n) in &unsupported_tracks {
        for _ in 0..*n {
            report.warn(format!("{kind} tracks were not imported"));
        }
    }
    for (key, label) in UNSUPPORTED {
        if let Some(n) = draft.materials.other.get(key).and_then(Value::as_array).map(Vec::len).filter(|n| *n > 0) {
            for _ in 0..n.min(u32::MAX as usize) {
                report.warn(format!("{label} were not imported"));
            }
        }
    }
    let counted = [
        (bad_ranges, "segments with a negative, empty or out-of-range time range were skipped"),
        (missing, "segments naming a material that is not in the draft were skipped"),
        (transformed, "segments with position, scale or rotation: the transform was not imported"),
        (keyframed, "segments with keyframes: the keyframes were not imported"),
        (reverse_speed, "segments with negative speed were read as reverse playback (unverified against CapCut)"),
        (overlapping, "segments overlapping earlier ones on the same track were skipped"),
        (caption_overlaps, "text segments overlapping an earlier one on the same track were skipped"),
    ];
    for (n, msg) in counted {
        for _ in 0..n {
            report.warn(msg);
        }
    }
    if has_text_style {
        report.info("text styling (font, colour, position) was not imported; captions use the default style");
    }
    if draft.materials.videos.iter().any(|m| m.kind.as_deref() != Some("photo")) {
        report.info("the audio inside video segments is not placed on an audio track");
    }

    b.put_sequence(seq_id, seq);
    b.top.push(seq_id);
    Ok((b.finish(), report))
}

fn doc_name(m: &MediaDoc) -> String {
    m.material_name
        .clone()
        .or_else(|| m.name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| m.path.as_deref().map(|p| file_name(p).to_string()))
        .unwrap_or_else(|| "Media".into())
}

fn media_item(b: &mut Builder, m: &MediaDoc, video: bool, rate: FrameRate, base: Option<&str>) -> ItemId {
    let raw = m.path.as_deref().unwrap_or("");
    let path = resolve_draft_path(raw, base);
    let name = doc_name(m);
    let still = m.kind.as_deref() == Some("photo");
    let size = u32::try_from(m.width.unwrap_or(0)).ok().zip(u32::try_from(m.height.unwrap_or(0)).ok()).filter(|(w, h)| validate_frame_size(*w, *h).is_ok());
    let spec = MediaSpec {
        duration: m.duration.and_then(ticks).filter(|t| *t > Tick::ZERO && !still),
        video: if video { size.map(|(w, h)| (w, h, rate)) } else { None },
        audio: (!video).then_some((48_000, 2)),
        start_tc: None,
        kind: if still {
            Some(MediaKind::Still)
        } else if video {
            Some(MediaKind::Movie)
        } else {
            Some(MediaKind::AudioOnly)
        },
    };
    let key = if path.is_empty() { format!("capcut:{}", m.id) } else { path.clone() };
    b.file_media(&key, &name, &path, &spec, None)
}

/// A material path: CapCut writes `##_draftpath_placeholder_<id>_##/...` for files inside the
/// draft folder; other relative paths resolve against the draft folder too.
fn resolve_draft_path(raw: &str, base: Option<&str>) -> String {
    let rest = raw.strip_prefix("##_draftpath_placeholder_").and_then(|r| r.split_once("_##")).map(|(_, tail)| tail.trim_start_matches(['/', '\\']));
    resolve_path(rest.unwrap_or(raw), base)
}

/// The text of a `texts` material: `content` is a JSON string with a `text` field (or plain text).
fn text_of(content: Option<&Value>) -> String {
    let Some(Value::String(s)) = content else { return String::new() };
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(o)) => o.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        _ => s.clone(),
    }
}

/// Whether a segment's `clip` block moves, scales or rotates the picture.
fn is_transformed(c: &Value) -> bool {
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let scale = c.get("scale").is_some_and(|s| num(s, "x").is_some_and(|x| (x - 1.0).abs() > 1e-6) || num(s, "y").is_some_and(|y| (y - 1.0).abs() > 1e-6));
    let moved = c.get("transform").is_some_and(|s| num(s, "x").is_some_and(|x| x.abs() > 1e-6) || num(s, "y").is_some_and(|y| y.abs() > 1e-6));
    let rotated = num(c, "rotation").is_some_and(|r| r.abs() > 1e-6);
    scale || moved || rotated
}
