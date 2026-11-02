//! Video input and output for AI jobs on frames (upscale, interpolate): which source frames a clip
//! plays, the decoder that writes them as PNG files on the job's thread, the import of the PNG
//! sequence a helper returns, and the relink that keeps media-time data with the picture.
//!
//! Only the frames the clip plays are decoded (its source range, on the media's frame grid), with
//! FilmCraft's own decoder at full resolution (never a proxy), so frame `k` of the result is source
//! frame `first + k` exactly. A frame that cannot be decoded fails the job. Image-sequence sources
//! are refused: their reader substitutes a neighbouring frame for one that fails to decode, which
//! would silently repeat frames. So are sources whose colour cannot go through 8-bit sRGB PNG
//! unchanged (PQ, HLG or linear transfer, or a colour space set in Interpret Footage).
//!
//! The result is checked and opened on the job's thread ([`prepare_sequence`]: every file decoded
//! strictly and its size checked, then the numbered sequence opened), so applying it
//! ([`import_prepared`]) only edits the project.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use filmcraft_project::{BinId, ClipId, EffectInstance, ItemId, ItemKind, MediaClip, MediaIdentity, MediaRef, TrackItem, TrackKind};
use filmcraft_time::{FrameRate, Tick};

use crate::{EngineError, Session};

/// What a frame job reads for one clip.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VideoTarget {
    pub clip: ClipId,
    pub name: String,
    pub item: ItemId,
    /// The media's frame rate (its frame grid).
    pub rate: FrameRate,
    /// First source frame and how many.
    pub first: i64,
    pub frames: i64,
    /// Frames the media has.
    pub total: i64,
    /// Storage size and pixel aspect ratio of the source.
    pub size: (u32, u32),
    pub par: (u32, u32),
    /// Interpret Footage alpha of the source: (ignore, invert), carried to the result.
    pub alpha: (bool, bool),
}

impl VideoTarget {
    /// Media time of the first frame decoded: the result's media time is the source's minus this.
    pub fn offset(&self) -> Tick {
        self.rate.tick_of(self.first)
    }
}

/// Why a clip gets no job: `Skip` (no video here) or `Hard` (it has video but cannot be used).
#[derive(Debug)]
pub(crate) enum Refused {
    Skip,
    Hard(String),
}

/// The frames clip `c` plays (its source range on the media's frame grid), at most `max_frames`.
pub(crate) fn video_target(s: &Session, c: ClipId, max_frames: i64) -> std::result::Result<VideoTarget, Refused> {
    let q = s.active_sequence().ok_or(Refused::Skip)?;
    let (t, it) = q.find_item(c).ok_or(Refused::Skip)?;
    if !q.track(t).is_some_and(|t| t.kind == TrackKind::Video) {
        return Err(Refused::Skip);
    }
    let (_, m, _) = s.project.resolve_media(it.item).ok_or(Refused::Skip)?;
    let MediaRef::File { .. } = &m.media else { return Err(Refused::Skip) };
    let Some(v) = m.info.video.as_ref() else { return Err(Refused::Skip) };
    let hard = |why: &str| Refused::Hard(format!("{}: {why}", it.name));
    if m.info.kind == filmcraft_media::MediaKind::Still {
        return Err(hard("a still has no frames to process"));
    }
    if m.interpret.frame_rate.is_some() {
        return Err(hard("the footage is interpreted at another frame rate; reset Interpret Footage first"));
    }
    if it.frame_hold.is_some() {
        return Err(hard("the clip has a frame hold"));
    }
    if m.info.kind == filmcraft_media::MediaKind::ImageSequence {
        return Err(hard(
            "image-sequence sources are not processed (a frame that fails to decode would be replaced by its neighbour); render the clip to a movie first",
        ));
    }
    if m.interpret.color_space.is_some() {
        return Err(hard("Interpret Footage sets a colour space for this media; frames are processed as 8-bit sRGB, so reset it first"));
    }
    if matches!(v.color.transfer, filmcraft_color::Transfer::Pq | filmcraft_color::Transfer::Hlg | filmcraft_color::Transfer::Linear) {
        return Err(hard("HDR or linear footage is not processed (frames are processed as 8-bit sRGB)"));
    }
    let rate = v.frame_rate.sane();
    let out_of_range = || hard("the clip's source position is out of range; reset its In and Out points");
    let (lo, hi) = (it.source_in.0.min(it.source_out().0), it.source_in.0.max(it.source_out().0));
    let total = rate.frame_at(Tick(m.info.duration.0.max(0)).0.checked_add(rate.frame_duration().0 - 1).map(Tick).ok_or_else(out_of_range)?);
    let first = rate.frame_at(Tick(lo.max(0)));
    let last = rate.frame_at(Tick(hi.checked_sub(1).ok_or_else(out_of_range)?.max(0)));
    if total <= 0 {
        return Err(Refused::Skip);
    }
    if first > total.saturating_sub(1) {
        return Err(hard("the clip plays a part past the end of its media; trim it first"));
    }
    let first = first.max(0);
    let frames = last.min(total.saturating_sub(1)).saturating_sub(first).saturating_add(1);
    if frames <= 0 {
        return Err(out_of_range());
    }
    if frames > max_frames {
        return Err(hard(&format!("the clip plays {frames} frames; at most {max_frames} are processed")));
    }
    Ok(VideoTarget {
        clip: c,
        name: it.name.clone(),
        item: it.item,
        rate,
        first,
        frames,
        total,
        size: (v.width, v.height),
        par: m.pixel_aspect(),
        alpha: (m.interpret.ignore_alpha, m.interpret.invert_alpha),
    })
}

/// Decode source frames `first .. first + count` of `src` at `rate` as 8-bit RGBA PNG files
/// `<prefix><k:06>.png` in `dir`; a frame index past the media's `total` repeats the last frame.
/// Stops when `stop` says so; a frame that cannot be decoded, or of another size, is an error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_frames(
    src: &dyn filmcraft_media::MediaSource,
    rate: FrameRate,
    (first, count, total): (i64, i64, i64),
    size: (u32, u32),
    dir: &std::path::Path,
    prefix: &str,
    stop: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(f64),
) -> std::result::Result<Vec<String>, String> {
    let mut paths = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for k in 0..count.max(0) {
        if stop() {
            return Err("stopped".into());
        }
        let n = first.saturating_add(k).min(total.saturating_sub(1)).max(0);
        let t = rate.tick_of(n);
        let frame = src.video_frame(filmcraft_media::FrameRequest::full(t)).map_err(|e| {
            if matches!(e, filmcraft_media::MediaError::Cancelled) {
                "stopped".to_string()
            } else {
                format!("decoding frame {n} ({:.3} s): {e}", t.0 as f64 / filmcraft_time::TICKS_PER_SECOND as f64)
            }
        })?;
        if (frame.width, frame.height) != size {
            return Err(format!("frame {n} is {}x{}, not {}x{}", frame.width, frame.height, size.0, size.1));
        }
        let path = dir.join(format!("{prefix}{k:06}.png"));
        let png = filmcraft_export::encode_png(frame.to_rgba8(), frame.width, frame.height).map_err(|e| format!("encoding frame {n}: {e}"))?;
        std::fs::write(&path, png).map_err(|e| format!("writing {}: {e}", path.display()))?;
        paths.push(path.to_string_lossy().to_string());
        progress((k + 1) as f64 / count.max(1) as f64);
    }
    Ok(paths)
}

/// Relink clip `it` to `item`, whose media time 0 is the source's `offset`: the source In and
/// everything else stored in media time (effect keyframes, clip markers) move by `offset`.
pub(crate) fn shift_media_time(it: &mut TrackItem, item: ItemId, offset: Tick) {
    it.item = item;
    it.source_in -= offset;
    for e in &mut it.effects {
        for p in e.params.values_mut() {
            for k in &mut p.keyframes {
                k.time -= offset;
            }
        }
        // a mask's path, feather, opacity and expansion keyframes are in media time too
        for m in &mut e.masks {
            for p in m.params_mut() {
                for k in &mut p.keyframes {
                    k.time -= offset;
                }
            }
        }
    }
    for m in &mut it.markers {
        m.start -= offset;
    }
}

/// A result image sequence checked and opened on the job's thread ([`prepare_sequence`]).
pub(crate) struct PreparedSequence {
    /// The first frame's path (the sequence's media path).
    pub path: String,
    pub name: String,
    pub src: filmcraft_media::SharedSource,
    pub identity: MediaIdentity,
}

/// On the job's thread: the result files `<prefix><k:06>.png` for `k` in `0..count`, each a
/// regular file inside `dir` of at most `max_bytes`, opened as a sequence at `rate` whose frames
/// are `size`. Everything that reads the disk is done here, so applying only edits the project.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_sequence(
    files: &[PathBuf],
    dir: &Path,
    prefix: &str,
    count: i64,
    rate: FrameRate,
    size: (u32, u32),
    max_bytes: u64,
    services: &dyn crate::Services,
    stop: &dyn Fn() -> bool,
) -> std::result::Result<PreparedSequence, String> {
    // one pass over the names (thousands of frames: no search per frame)
    let by_name: HashMap<String, &PathBuf> = files.iter().filter_map(|f| Some((f.file_name()?.to_string_lossy().to_string(), f))).collect();
    let mut first = None;
    for k in 0..count.max(0) {
        if stop() {
            return Err("stopped".into());
        }
        let name = format!("{prefix}{k:06}.png");
        let f = by_name.get(&name).ok_or_else(|| format!("the helper wrote no {name}"))?;
        // every frame decoded here, strictly: the sequence reader would show a neighbour in the
        // place of one that does not decode
        let mut file = filmcraft_sidecar::open_result(f, dir, max_bytes).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(&mut file, max_bytes), &mut bytes).map_err(|e| format!("{name}: {e}"))?;
        let frame = filmcraft_media::sequence::decode_file(&name, &bytes).map_err(|e| format!("{name} does not decode: {e}"))?;
        if (frame.width, frame.height) != size {
            return Err(format!("{name} is {}x{}, not {}x{}", frame.width, frame.height, size.0, size.1));
        }
        first.get_or_insert_with(|| f.to_string_lossy().to_string());
    }
    let path = first.ok_or("the helper wrote no frames")?;
    let frames = crate::media_pool::image_sequence_frames(&path, services).map_err(|e| format!("{path}: {e}"))?;
    let missing = frames.iter().filter(|f| f.is_none()).count();
    if i64::try_from(frames.len()).ok() != Some(count) || missing > 0 {
        return Err(format!("the result has {} frames ({missing} missing), not {count}", frames.len()));
    }
    let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.clone());
    let src = crate::media_pool::open_image_sequence(&path, services, rate, &name).map_err(|e| format!("{path}: {e}"))?;
    let got = src.info().video.as_ref().map(|v| (v.width, v.height));
    if got != Some(size) {
        return Err(format!("the result is {got:?}, not {}x{}", size.0, size.1));
    }
    let identity = crate::relink::identity_of(services, &path).map_err(|e| format!("{path}: {e}"))?;
    Ok(PreparedSequence { path, name, src, identity })
}

/// Add a prepared result to the project in a new bin `bin_name`, with pixel aspect `par` and the
/// source's alpha interpretation; no disk access.
pub(crate) fn import_prepared(s: &mut Session, prep: &PreparedSequence, bin_name: &str, par: (u32, u32), alpha: (bool, bool)) -> crate::Result<ItemId> {
    let bin: BinId = s.edit("Import", |pr, _| Ok(pr.add_bin(bin_name, None)))?;
    let info = prep.src.info().clone();
    let label = s.prefs.labels.for_media(info.kind, info.has_video(), info.has_audio());
    let (path, name, identity) = (prep.path.clone(), prep.name.clone(), prep.identity);
    s.media.allow(&path);
    let id = s.edit(&format!("Import {name}"), |pr, _| {
        let mut interpret = filmcraft_project::Interpretation { par: Some(par), ..Default::default() };
        (interpret.ignore_alpha, interpret.invert_alpha) = alpha;
        Ok(pr.add_item(
            &name,
            label,
            ItemKind::Media(MediaClip {
                media: MediaRef::File { path: path.clone() },
                info,
                interpret,
                mark_in: None,
                mark_out: None,
                markers: vec![],
                offline: false,
                proxy: None,
                identity: Some(identity),
            }),
            Some(bin),
        ))
    })?;
    s.media.insert_file(id, &prep.path, prep.src.clone());
    Ok(id)
}

/// The prepared result of a job, or why there is none.
pub(crate) fn prepared(pj: &super::PendingAi) -> crate::Result<&PreparedSequence> {
    pj.prepared
        .as_ref()
        .and_then(|p| p.downcast_ref::<PreparedSequence>())
        .ok_or_else(|| EngineError::Other("the result was not prepared on the job's thread".into()))
}

/// Video effects whose look does not depend on the source's pixel size: per-pixel colour work,
/// and Crop (in percent). Anything else (blurs, distortions, points, radii in source pixels)
/// would change on an upscaled source.
const RESOLUTION_INDEPENDENT: &[&str] = &[
    "brightness_contrast",
    "tint",
    "black_white",
    "color_pass",
    "gamma_correction",
    "invert",
    "levels",
    "proc_amp",
    "extract",
    "asc_cdl",
    "video_limiter",
    "vignette",
    "channel_mix",
    "color_replace",
    "crop",
    "lumetri",
];

/// Lumetri parameters measured in source pixels (Sharpen, HSL Secondary Denoise and Blur).
const LUMETRI_PIXEL_PARAMS: &[&str] = &["sharpen", "hsl_denoise", "hsl_blur"];

/// The name of the first enabled effect of `it` whose look depends on the source's pixel size
/// (see [`RESOLUTION_INDEPENDENT`]); intrinsic effects are handled by the caller.
pub(crate) fn pixel_space_effect(it: &TrackItem) -> Option<String> {
    let used = |e: &EffectInstance, id: &str| {
        e.param(id).is_some_and(|p| {
            let nonzero = |v: &filmcraft_project::ParamValue| v.as_f64().is_some_and(|x| x != 0.0);
            nonzero(&p.value) || p.keyframes.iter().any(|k| nonzero(&k.value))
        })
    };
    it.effects.iter().filter(|e| e.enabled).find_map(|e| {
        let def = e.def();
        if def.is_some_and(|d| d.intrinsic) {
            return None;
        }
        let name = || def.map_or_else(|| e.effect.clone(), |d| d.name.to_string());
        if !RESOLUTION_INDEPENDENT.contains(&e.effect.as_str()) {
            return Some(name());
        }
        (e.effect == "lumetri" && LUMETRI_PIXEL_PARAMS.iter().any(|p| used(e, p))).then(|| format!("{} (Sharpen, Denoise or Blur)", name()))
    })
}

/// Remove every file of job folder `dir` but the heartbeat: a frame job that did not finish
/// leaves frames nothing will ever use (bounded, in case the folder is not what it seems).
pub(crate) fn remove_frames(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten().take(1_000_000) {
        let p = e.path();
        let png = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png"));
        if png && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
            let _ = std::fs::remove_file(p);
        }
    }
}
