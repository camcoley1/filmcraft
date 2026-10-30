//! Video input and output for AI jobs on frames (upscale, interpolate): which source frames a clip
//! plays, the decoder that writes them as PNG files on the job's thread, the import of the PNG
//! sequence a helper returns, and the relink that keeps media-time data with the picture.
//!
//! Only the frames the clip plays are decoded (its source range, on the media's frame grid), with
//! FilmCraft's own decoder, so frame `k` of the result is source frame `first + k` exactly. A frame
//! that cannot be decoded fails the job.

use filmcraft_project::{ClipId, ItemId, MediaRef, TrackItem, TrackKind};
use filmcraft_time::{FrameRate, Tick};

use crate::Session;

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
    let rate = v.frame_rate.sane();
    let out_of_range = || hard("the clip's source position is out of range; reset its In and Out points");
    let (lo, hi) = (it.source_in.0.min(it.source_out().0), it.source_in.0.max(it.source_out().0));
    let total = rate.frame_at(Tick(m.info.duration.0.max(0)).0.checked_add(rate.frame_duration().0 - 1).map(Tick).ok_or_else(out_of_range)?);
    let first = rate.frame_at(Tick(lo.max(0)));
    let last = rate.frame_at(Tick(hi.checked_sub(1).ok_or_else(out_of_range)?.max(0)));
    let first = first.clamp(0, total.saturating_sub(1).max(0));
    let frames = last.min(total.saturating_sub(1)).saturating_sub(first).saturating_add(1);
    if frames <= 0 || total <= 0 {
        return Err(Refused::Skip);
    }
    if frames > max_frames {
        return Err(hard(&format!("the clip plays {frames} frames; at most {max_frames} are processed")));
    }
    Ok(VideoTarget { clip: c, name: it.name.clone(), item: it.item, rate, first, frames, total, size: (v.width, v.height), par: m.pixel_aspect() })
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
    }
    for m in &mut it.markers {
        m.start -= offset;
    }
}

/// The result files `<prefix><k:06>.png` for `k` in `0..count`, in order, each checked to be a
/// regular file inside `dir` of at most `max_bytes`.
pub(crate) fn result_frames(
    files: &[std::path::PathBuf],
    dir: &std::path::Path,
    prefix: &str,
    count: i64,
    max_bytes: u64,
) -> crate::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for k in 0..count.max(0) {
        let name = format!("{prefix}{k:06}.png");
        let f = files
            .iter()
            .find(|f| f.file_name().is_some_and(|n| n.to_string_lossy() == name))
            .ok_or_else(|| crate::EngineError::Other(format!("the helper wrote no {name}")))?;
        filmcraft_sidecar::open_result(f, dir, max_bytes).map_err(|e| crate::EngineError::Other(e.to_string()))?;
        out.push(f.clone());
    }
    Ok(out)
}
