//! CapCut desktop drafts (`draft_content.json`, and `draft_info.json` of newer versions), import only.
//!
//! Source: the community draft schema (pyJianYingDraft and the CapCut draft notes). No CapCut code
//! or binary was consulted. The file is plain JSON; recent CapCut builds encrypt it, which is
//! reported as a parse error that says so.
//!
//! Read: the canvas size and frame rate, video and audio tracks with their segments (timeline and
//! source ranges, speed, volume, visibility, the reverse flag), the media they use (path, size; a
//! missing file is offline and goes through the normal relink flow) and text tracks, which become
//! one caption track each. Every time is in microseconds, converted to [`Tick`] exactly
//! (`us * 254016`). A video segment of a file with sound gets a linked audio clip with the same
//! ranges, speed and reverse; the engine drops it again when the linked file has no audio.
//!
//! Reported, not imported (a warning with a count): effects, filters, transitions, stickers,
//! animations and speed curves, keyframes, text styling, and position / scale / rotation.
//!
//! Hostile input: the file is capped at [`MAX_BYTES`]; tracks, segments and materials are capped
//! while the JSON is read (the rest is skipped without being stored, and the truncation is
//! reported); a segment with a negative, empty, overflowing or inconsistent range is reported and
//! skipped, never rewritten; a material path that climbs out of the draft folder stays offline;
//! unknown fields are ignored and a wrong type is a parse error.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::marker::PhantomData;

use filmcraft_media::MediaKind;
use filmcraft_project::{Caption, CaptionFormat, CaptionTrack, ClipId, ItemId, TrackId, TrackKind, validate_frame_size};
use filmcraft_time::{FrameRate, Tick};
use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::common::{Builder, MediaSpec, empty_sequence, file_name, gain_to_db, is_absolute, media_info, rate_from_f64, resolve_path, settings_for};
use crate::{Error, Format, ImportOptions, Imported, Level, Report, Result};

/// Largest draft read (64 MiB; 16 MiB on the web).
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
#[cfg(target_arch = "wasm32")]
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Most tracks read.
const MAX_TRACKS: usize = 256;
/// Most segments read in all tracks together.
const MAX_SEGMENTS: usize = 200_000;
/// Most materials read per kind.
const MAX_MATERIALS: usize = 200_000;
/// Most sequence tracks of one kind (CapCut tracks, linked audio and overlap lanes).
const MAX_LANES: usize = 512;
/// Most lanes a segment overlapping its own track may move down to.
const MAX_EXTRA_LANES: usize = 8;
/// Ticks per microsecond (254 016 000 000 / 1 000 000).
const TICKS_PER_US: i64 = 254_016;
/// Draft file names inside a draft folder, in the order tried.
pub const DRAFT_FILES: [&str; 2] = ["draft_content.json", "draft_info.json"];

thread_local! {
    /// Segments the current read may still keep (shared by every track of one draft).
    static SEGMENTS_LEFT: Cell<usize> = const { Cell::new(0) };
}

// ---------------------------------------------------------------------------------------------
// Bounded deserialisation
// ---------------------------------------------------------------------------------------------

/// A JSON array read into at most `N` elements (and, with `SHARED`, at most what is left of the
/// draft-wide segment budget). The rest is skipped without being stored; `total` counts all.
struct Capped<T, const N: usize, const SHARED: bool> {
    items: Vec<T>,
    total: usize,
}

impl<T, const N: usize, const SHARED: bool> Default for Capped<T, N, SHARED> {
    fn default() -> Self {
        Capped { items: Vec::new(), total: 0 }
    }
}

impl<'de, T: Deserialize<'de>, const N: usize, const SHARED: bool> Deserialize<'de> for Capped<T, N, SHARED> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V<T, const N: usize, const SHARED: bool>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize, const SHARED: bool> Visitor<'de> for V<T, N, SHARED> {
            type Value = Capped<T, N, SHARED>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Capped { items: Vec::new(), total: 0 };
                loop {
                    if out.items.len() < N && (!SHARED || SEGMENTS_LEFT.with(Cell::get) > 0) {
                        match seq.next_element::<T>()? {
                            Some(v) => {
                                out.items.push(v);
                                out.total += 1;
                                if SHARED {
                                    SEGMENTS_LEFT.with(|c| c.set(c.get().saturating_sub(1)));
                                }
                            }
                            None => break,
                        }
                    } else {
                        match seq.next_element::<IgnoredAny>()? {
                            Some(_) => out.total += 1,
                            None => break,
                        }
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_seq(V::<T, N, SHARED>(PhantomData))
    }
}

/// An array read only for its length.
#[derive(Default, Clone, Copy)]
struct Len(usize);

impl<'de> Deserialize<'de> for Len {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Len;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Len, A::Error> {
                let mut n = 0usize;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    n += 1;
                }
                Ok(Len(n))
            }
        }
        d.deserialize_seq(V)
    }
}

/// A JSON object whose content is skipped.
struct AnyObject;

impl<'de> Deserialize<'de> for AnyObject {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = AnyObject;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<AnyObject, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(AnyObject)
            }
        }
        d.deserialize_map(V)
    }
}

/// Whether a field holds a value other than `null`.
#[derive(Default, Clone, Copy)]
struct Present(bool);

impl<'de> Deserialize<'de> for Present {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Option::<IgnoredAny>::deserialize(d).map(|o| Present(o.is_some()))
    }
}

/// What the entries of an array of materials that are not read say about them.
#[derive(Default)]
struct Tally {
    total: u32,
    /// Entries with a speed curve (`mode` other than 0, or a `curve_speed`).
    curved: u32,
    /// Entries with at least one animation.
    animated: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TallyEntry {
    mode: Option<i64>,
    curve_speed: Present,
    animations: Option<Len>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TallyElem {
    Entry(TallyEntry),
    Other(IgnoredAny),
}

impl<'de> Deserialize<'de> for Tally {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Tally;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Tally, A::Error> {
                let mut t = Tally::default();
                while let Some(e) = seq.next_element::<TallyElem>()? {
                    t.total = t.total.saturating_add(1);
                    if let TallyElem::Entry(e) = e {
                        if e.mode.is_some_and(|m| m != 0) || e.curve_speed.0 {
                            t.curved = t.curved.saturating_add(1);
                        }
                        if e.animations.is_some_and(|a| a.0 > 0) {
                            t.animated = t.animated.saturating_add(1);
                        }
                    }
                }
                Ok(t)
            }
        }
        d.deserialize_seq(V)
    }
}

// ---------------------------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------------------------

/// Whether `bytes` look like a CapCut draft: a JSON object with a `canvas_config` object, `tracks`
/// whose entries have a `type` and `segments`, and a `materials` object holding at least one of the
/// material arrays a draft has. Nothing is stored while reading (a fixed handful of tracks).
pub fn sniff(bytes: &[u8]) -> bool {
    let b = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if b.iter().find(|c| !c.is_ascii_whitespace()) != Some(&b'{') || b.len() > MAX_BYTES {
        return false;
    }
    #[derive(Deserialize)]
    struct TrackShape {
        #[serde(rename = "type")]
        _kind: String,
        #[serde(rename = "segments")]
        _segments: Len,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct MaterialShape {
        videos: Option<Len>,
        audios: Option<Len>,
        texts: Option<Len>,
        speeds: Option<Len>,
        canvases: Option<Len>,
        sound_channel_mappings: Option<Len>,
    }
    #[derive(Deserialize)]
    struct Shape {
        #[serde(rename = "canvas_config")]
        _canvas: AnyObject,
        tracks: Capped<TrackShape, 4, false>,
        materials: MaterialShape,
    }
    let m = |s: &Shape| {
        let m = &s.materials;
        m.videos.is_some() || m.audios.is_some() || m.texts.is_some() || m.speeds.is_some() || m.canvases.is_some() || m.sound_channel_mappings.is_some()
    };
    serde_json::from_slice::<Shape>(b).is_ok_and(|s| m(&s) && !s.tracks.items.is_empty())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Draft {
    canvas_config: Canvas,
    fps: Option<f64>,
    tracks: Capped<TrackDoc, MAX_TRACKS, false>,
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
    segments: Capped<Segment, MAX_SEGMENTS, true>,
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
    reverse: Option<bool>,
    clip: Option<ClipDoc>,
    common_keyframes: Len,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default)]
struct Range {
    start: Option<i64>,
    duration: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ClipDoc {
    scale: Option<Xy>,
    transform: Option<Xy>,
    rotation: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Xy {
    x: Option<f64>,
    y: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Materials {
    videos: Capped<MediaDoc, MAX_MATERIALS, false>,
    audios: Capped<MediaDoc, MAX_MATERIALS, false>,
    texts: Capped<TextDoc, MAX_MATERIALS, false>,
    effects: Tally,
    video_effects: Tally,
    audio_effects: Tally,
    filters: Tally,
    transitions: Tally,
    stickers: Tally,
    speeds: Tally,
    material_animations: Tally,
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
    content: Option<Content>,
}

/// `content` of a text material: markup or a JSON string with a `text` field, or an object.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Str(String),
    Obj(TextObj),
    Other(IgnoredAny),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TextObj {
    text: Option<String>,
}

fn perr(message: impl Into<String>) -> Error {
    Error::parse(Format::CapCut, message)
}

/// Microseconds to ticks, `None` when they overflow the supported time range.
fn ticks(us: i64) -> Option<Tick> {
    let t = us.checked_mul(TICKS_PER_US).map(Tick)?;
    (t >= Tick::MIN && t <= Tick::MAX).then_some(t)
}

/// Counted report lines (merged in one pass at the end, so a huge count costs nothing per item).
#[derive(Default)]
struct Counter(BTreeMap<&'static str, u32>);

impl Counter {
    fn bump(&mut self, msg: &'static str) {
        let n = self.0.entry(msg).or_insert(0);
        *n = n.saturating_add(1);
    }
}

pub(crate) fn import(bytes: &[u8], opts: &ImportOptions) -> Result<(Imported, Report)> {
    if bytes.len() > MAX_BYTES {
        return Err(perr(format!("the draft is larger than {} MiB", MAX_BYTES / (1024 * 1024))));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    SEGMENTS_LEFT.with(|c| c.set(MAX_SEGMENTS));
    let draft: Draft = serde_json::from_slice(bytes).map_err(|e| {
        let hint = if bytes.first().is_some_and(|c| *c != b'{') {
            " (newer CapCut versions encrypt their drafts; export the project as FCPXML or XML from CapCut instead)"
        } else {
            ""
        };
        perr(format!("{e}{hint}"))
    })?;
    let mut report = Report::default();
    if draft.tracks.items.is_empty() {
        return Err(Error::Empty);
    }
    let base = opts.base_dir.as_deref();
    let seq_name = opts.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("CapCut Draft");
    let mut b = Builder::new(seq_name);
    let mut warns = Counter::default();
    let mut infos = Counter::default();

    let rate = match draft.fps {
        Some(f) if f.is_finite() && f > 0.0 && f <= 1000.0 => rate_from_f64(f),
        _ => {
            report.warn("the draft has no usable frame rate; 30 fps was used");
            FrameRate::FPS_30
        }
    };
    let frame = rate.frame_duration();
    let videos = &draft.materials.videos.items;
    let audios = &draft.materials.audios.items;
    let video_ids: HashMap<&str, usize> = videos.iter().enumerate().map(|(i, m)| (m.id.as_str(), i)).collect();
    let audio_ids: HashMap<&str, usize> = audios.iter().enumerate().map(|(i, m)| (m.id.as_str(), i)).collect();
    let texts: HashMap<&str, &TextDoc> = draft.materials.texts.items.iter().map(|t| (t.id.as_str(), t)).collect();
    for (name, total, kept) in [
        ("tracks", draft.tracks.total, draft.tracks.items.len()),
        ("video materials", draft.materials.videos.total, videos.len()),
        ("audio materials", draft.materials.audios.total, audios.len()),
        ("text materials", draft.materials.texts.total, draft.materials.texts.items.len()),
    ] {
        if total > kept {
            report.warn(format!("only the first {kept} of {total} {name} were read"));
        }
    }
    let seg_total: usize = draft.tracks.items.iter().map(|t| t.segments.total).sum();
    let seg_kept: usize = draft.tracks.items.iter().map(|t| t.segments.items.len()).sum();
    if seg_total > seg_kept {
        report.warn(format!("only the first {seg_kept} of {seg_total} segments were read"));
    }

    // canvas: the draft's own, else the first video material's
    let size_of = |w: Option<i64>, h: Option<i64>| -> Option<(u32, u32)> {
        let (w, h) = (u32::try_from(w?).ok()?, u32::try_from(h?).ok()?);
        validate_frame_size(w, h).ok().map(|()| (w, h))
    };
    let canvas = size_of(draft.canvas_config.width, draft.canvas_config.height).or_else(|| videos.iter().find_map(|m| size_of(m.width, m.height)));
    let (width, height) = canvas.unwrap_or_else(|| {
        report.warn("the draft has no usable canvas size; 1920x1080 was used");
        (1920, 1080)
    });
    let mut seq = empty_sequence(settings_for(rate, width, height, false));
    let seq_id = b.reserve_sequence(seq_name, seq.settings.clone(), None);

    // sequence lanes: video track k -> V(k) and its linked audio A(k); CapCut audio track m ->
    // A(video tracks + m); overlap spill lanes come after all of those
    let kinds: Vec<&str> = draft.tracks.items.iter().map(|t| t.kind.as_str()).collect();
    let n_video = kinds.iter().filter(|k| **k == "video").count();
    let n_audio = kinds.iter().filter(|k| **k == "audio").count();
    let (mut vi, mut ai) = (0usize, 0usize);
    let mut media_items: HashMap<(bool, usize), ItemId> = HashMap::new();
    let mut has_text_style = false;
    let mut unsupported_tracks: BTreeMap<String, u32> = BTreeMap::new();

    for t in &draft.tracks.items {
        let kind = match t.kind.as_str() {
            "video" => Some(TrackKind::Video),
            "audio" => Some(TrackKind::Audio),
            "text" => None,
            other => {
                let n = unsupported_tracks.entry(other.chars().take(40).collect()).or_default();
                *n = n.saturating_add(1);
                continue;
            }
        };
        let (preferred, audio_preferred) = match kind {
            Some(TrackKind::Video) => {
                vi += 1;
                (vi - 1, vi - 1)
            }
            Some(TrackKind::Audio) => {
                ai += 1;
                (n_video + ai - 1, 0)
            }
            None => (0, 0),
        };
        // segments in timeline order (drafts written by tools are not sorted)
        let mut order: Vec<usize> = (0..t.segments.items.len()).collect();
        order.sort_by_key(|i| t.segments.items.get(*i).and_then(|s| s.target_timerange.start).unwrap_or(i64::MAX));
        let mut captions: Vec<Caption> = Vec::new();
        for s in order.into_iter().filter_map(|i| t.segments.items.get(i)) {
            let (Some(start), Some(dur)) = (s.target_timerange.start.and_then(ticks), s.target_timerange.duration.and_then(ticks)) else {
                warns.bump("segments with a negative, empty or out-of-range time range were skipped");
                continue;
            };
            if start < Tick::ZERO || dur <= Tick::ZERO || start.0.checked_add(dur.0).is_none_or(|e| e > Tick::MAX.0) {
                warns.bump("segments with a negative, empty or out-of-range time range were skipped");
                continue;
            }
            if s.common_keyframes.0 > 0 {
                warns.bump("segments with keyframes: the keyframes were not imported");
            }
            if t.kind == "text" {
                let Some(doc) = texts.get(s.material_id.as_str()) else {
                    warns.bump("segments naming a material that is not in the draft were skipped");
                    continue;
                };
                let (text, styled) = text_of(doc.content.as_ref());
                if text.is_empty() {
                    warns.bump("text segments with no text were skipped");
                    continue;
                }
                has_text_style |= styled;
                if captions.last().is_some_and(|c: &Caption| c.start + c.duration > start) {
                    warns.bump("text segments overlapping an earlier one on the same track were skipped");
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
            let found = if video {
                video_ids.get(s.material_id.as_str()).and_then(|i| videos.get(*i).map(|d| (*i, d)))
            } else {
                audio_ids.get(s.material_id.as_str()).and_then(|i| audios.get(*i).map(|d| (*i, d)))
            };
            let Some((index, doc)) = found else {
                warns.bump("segments naming a material that is not in the draft were skipped");
                continue;
            };
            let Some((source_in, speed)) = media_times(s, dur, frame, &mut warns) else { continue };
            // media past its own length
            if let Some(len) = doc.duration.and_then(ticks).filter(|l| *l > Tick::ZERO)
                && source_in.0.saturating_add((dur.0 as f64 * speed) as i64) > len.0.saturating_add(frame.0)
            {
                warns.bump("segments whose source range runs past the length of the media");
            }
            let photo = doc.kind.as_deref() == Some("photo");
            let item = match media_items.get(&(video, index)) {
                Some(id) => *id,
                None => {
                    let id = media_item(&mut b, doc, video, rate, (width, height), base, &mut warns);
                    media_items.insert((video, index), id);
                    id
                }
            };
            let name = doc_name(doc);
            let volume = s.volume.filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(1.0);
            let make = |b: &mut Builder, kind: TrackKind, link: Option<u64>| {
                let mut ti = b.clip(item, kind, &name, start, dur, source_in);
                ti.speed = speed;
                ti.reverse = s.reverse == Some(true);
                ti.enabled = s.visible.unwrap_or(true);
                ti.link = link;
                if kind == TrackKind::Audio {
                    if volume == 0.0 {
                        ti.enabled = false;
                    } else {
                        ti.gain_db = gain_to_db(volume);
                    }
                }
                ti
            };
            if s.reverse == Some(true) {
                warns.bump("reversed segments: the source range was kept as written (CapCut may store a reversed copy of the file)");
            }
            if let Some(c) = &s.clip
                && is_transformed(c)
            {
                warns.bump("segments with position, scale or rotation: the transform was not imported");
            }
            let with_audio = video && !photo;
            let v_lane = lane_for(&mut b, &mut seq, kind, preferred, if video { n_video } else { n_video + n_audio }, start);
            let a_lane = if with_audio { lane_for(&mut b, &mut seq, TrackKind::Audio, audio_preferred, n_video + n_audio, start) } else { None };
            let ok = v_lane.is_some() && (!with_audio || a_lane.is_some());
            if !ok {
                warns.bump("segments overlapping earlier ones on the same track were skipped");
                continue;
            }
            let link = with_audio.then(|| b.link_id());
            if let Some(i) = v_lane {
                let ti = make(&mut b, kind, link);
                if let Some(tr) = seq.tracks_mut(kind).get_mut(i) {
                    tr.items.push(ti);
                }
            }
            if let Some(i) = a_lane {
                let ti = make(&mut b, TrackKind::Audio, link);
                if let Some(tr) = seq.tracks_mut(TrackKind::Audio).get_mut(i) {
                    tr.items.push(ti);
                }
                infos.bump("video segments got a linked audio clip; it is dropped when the file has no sound");
            }
        }
        if t.kind == "text" && !captions.is_empty() {
            let name = t.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Text {}", seq.caption_tracks.len() + 1));
            let mut ct = CaptionTrack::new(TrackId(b.alloc()), name, CaptionFormat::Subtitle);
            ct.captions = captions;
            ct.sort();
            seq.caption_tracks.push(ct);
        }
    }

    for (kind, n) in &unsupported_tracks {
        report.push_n(Level::Warning, format!("{kind} tracks were not imported"), *n);
    }
    let m = &draft.materials;
    for (label, n) in [
        ("effects", m.effects.total),
        ("video effects", m.video_effects.total),
        ("audio effects", m.audio_effects.total),
        ("filters", m.filters.total),
        ("transitions", m.transitions.total),
        ("stickers", m.stickers.total),
        ("speed curves (the constant speed of each segment was kept)", m.speeds.curved),
        ("animations", m.material_animations.animated),
    ] {
        report.push_n(Level::Warning, format!("{label} were not imported"), n);
    }
    for (msg, n) in warns.0 {
        report.push_n(Level::Warning, msg, n);
    }
    for (msg, n) in infos.0 {
        report.push_n(Level::Info, msg, n);
    }
    if has_text_style {
        report.info("text styling (font, colour, position) was not imported; captions use the default style");
    }

    b.put_sequence(seq_id, seq);
    b.top.push(seq_id);
    Ok((b.finish(), report))
}

/// A lane of the sequence for a segment starting at `start`: `preferred` when its last item ends
/// by then, else the first spill lane (from `spill_base`) that does. `None`: none fits.
fn lane_for(b: &mut Builder, seq: &mut filmcraft_project::Sequence, kind: TrackKind, preferred: usize, spill_base: usize, start: Tick) -> Option<usize> {
    let spill = spill_base.max(preferred + 1);
    for i in std::iter::once(preferred).chain(spill..spill + MAX_EXTRA_LANES) {
        if i >= MAX_LANES {
            break;
        }
        b.ensure_tracks(seq, kind, i + 1);
        if seq.tracks(kind).get(i).is_some_and(|t| t.items.last().is_none_or(|l| l.end() <= start)) {
            return Some(i);
        }
    }
    None
}

/// Source in point and speed of a media segment, or `None` (counted) when its ranges are invalid.
/// The source range, the target duration and the speed must agree; the ranges win over a speed
/// field that contradicts them.
fn media_times(s: &Segment, dur: Tick, frame: Tick, warns: &mut Counter) -> Option<(Tick, f64)> {
    let (source_in, source_dur) = match s.source_timerange {
        Some(r) => {
            let st = r.start.and_then(ticks).filter(|t| *t >= Tick::ZERO);
            let sd = r.duration.and_then(ticks).filter(|d| *d > Tick::ZERO);
            let (Some(st), Some(sd)) = (st, sd) else {
                warns.bump("segments with a negative, empty or out-of-range source range were skipped");
                return None;
            };
            (st, Some(sd))
        }
        None => (Tick::ZERO, None),
    };
    let explicit = match s.speed {
        Some(v) if v.is_finite() && (1e-3..=1e3).contains(&v) => Some(v),
        Some(_) => {
            warns.bump("segments with a speed outside 0.001x to 1000x were skipped");
            return None;
        }
        None => None,
    };
    let implied = source_dur.map(|sd| sd.0 as f64 / dur.0 as f64);
    let speed = match (explicit, implied) {
        (Some(e), Some(i)) => {
            if (sd_of(source_dur) - dur.0 as f64 * e).abs() > frame.0 as f64 {
                warns.bump("segments whose speed contradicts their source and target durations: the speed implied by the durations was used");
                i
            } else {
                e
            }
        }
        (Some(e), None) => e,
        (None, Some(i)) => i,
        (None, None) => 1.0,
    };
    if !(1e-3..=1e3).contains(&speed) {
        warns.bump("segments with a speed outside 0.001x to 1000x were skipped");
        return None;
    }
    // the source range the clip plays must stay inside the supported time range
    let consumed = dur.0 as f64 * speed;
    if !consumed.is_finite() || consumed >= Tick::MAX.0 as f64 || source_in.0.checked_add(consumed.round() as i64).is_none_or(|e| e > Tick::MAX.0) {
        warns.bump("segments with a negative, empty or out-of-range source range were skipped");
        return None;
    }
    Some((source_in, speed))
}

fn sd_of(d: Option<Tick>) -> f64 {
    d.map_or(0.0, |d| d.0 as f64)
}

fn doc_name(m: &MediaDoc) -> String {
    m.material_name
        .clone()
        .or_else(|| m.name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| m.path.as_deref().map(|p| file_name(p).to_string()))
        .unwrap_or_else(|| "Media".into())
}

fn media_item(b: &mut Builder, m: &MediaDoc, video: bool, rate: FrameRate, canvas: (u32, u32), base: Option<&str>, warns: &mut Counter) -> ItemId {
    let raw = m.path.as_deref().unwrap_or("").trim();
    let path = if raw.is_empty() {
        warns.bump("materials with no file path (CapCut library or cloud media) are offline");
        String::new()
    } else {
        resolve_draft_path(raw, base).unwrap_or_else(|| {
            warns.bump("materials whose path climbs out of the draft folder were left offline");
            String::new()
        })
    };
    let name = doc_name(m);
    let still = m.kind.as_deref() == Some("photo");
    let size = u32::try_from(m.width.unwrap_or(0)).ok().zip(u32::try_from(m.height.unwrap_or(0)).ok()).filter(|(w, h)| validate_frame_size(*w, *h).is_ok());
    let spec = MediaSpec {
        duration: m.duration.and_then(ticks).filter(|t| *t > Tick::ZERO && !still),
        video: if video { Some(size.map_or((canvas.0, canvas.1, rate), |(w, h)| (w, h, rate))) } else { None },
        // the sound of a video file: assumed until the engine opens the file
        audio: (!video || !still).then_some((48_000, 2)),
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
    if let Some(id) = b.find_media(&key) {
        // the same file used as video and as audio: one item with both streams
        let add = media_info(&name, &spec);
        if let Some(mc) = b.p.item_mut(id).and_then(|i| i.as_media_mut()) {
            if mc.info.video.is_none() {
                mc.info.video = add.video;
                if video {
                    mc.info.kind = add.kind;
                }
            }
            if mc.info.audio_streams.is_empty() {
                mc.info.audio_streams = add.audio_streams;
            }
        }
        return id;
    }
    b.file_media(&key, &name, &path, &spec, None)
}

/// A material path: CapCut writes `##_draftpath_placeholder_<id>_##/...` for files inside the
/// draft folder; other relative paths resolve against the draft folder too. A relative path may
/// not climb out of it (`None`); an absolute path is the file's own location and is kept.
fn resolve_draft_path(raw: &str, base: Option<&str>) -> Option<String> {
    let rest = raw.strip_prefix("##_draftpath_placeholder_").and_then(|r| r.split_once("_##")).map(|(_, tail)| tail.trim_start_matches(['/', '\\']));
    let owned = rest.is_some() || (!is_absolute(raw) && !raw.starts_with("file:"));
    let rel = rest.unwrap_or(raw);
    if owned && rel.split(['/', '\\']).any(|c| c == "..") {
        return None;
    }
    Some(resolve_path(rel, base))
}

/// The text of a `texts` material and whether it carried styling. `content` is a JSON string with a
/// `text` field, legacy markup (`<font ...>[text]</font>`) or plain text.
fn text_of(content: Option<&Content>) -> (String, bool) {
    match content {
        Some(Content::Obj(o)) => (o.text.clone().unwrap_or_default(), false),
        Some(Content::Str(s)) => {
            if s.trim_start().starts_with('{')
                && let Ok(Content::Obj(o)) = serde_json::from_str::<Content>(s)
            {
                return (o.text.unwrap_or_default(), s.contains("\"styles\""));
            }
            let styled = s.contains('<');
            (strip_markup(s), styled)
        }
        _ => (String::new(), false),
    }
}

/// Plain text out of legacy markup: tags removed, one outer `[...]` unwrapped.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let t = out.trim();
    t.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(t).to_string()
}

/// Whether a segment's `clip` block moves, scales or rotates the picture.
fn is_transformed(c: &ClipDoc) -> bool {
    let off = |v: Option<f64>, rest: f64| v.is_some_and(|x| (x - rest).abs() > 1e-6);
    let scaled = c.scale.as_ref().is_some_and(|s| off(s.x, 1.0) || off(s.y, 1.0));
    let moved = c.transform.as_ref().is_some_and(|s| off(s.x, 0.0) || off(s.y, 0.0));
    scaled || moved || off(c.rotation, 0.0)
}
