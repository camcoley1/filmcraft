//! Audio input for AI jobs (stems, enhance, beats): which part of a clip's source to decode and
//! the decoder that writes it as a float WAV on the job's thread.
//!
//! Only the source range the clip plays is decoded, plus a pad on each side, starting on a
//! sample of the job's rate. That rate divides [`TICKS_PER_SECOND`] (22.05, 44.1 and 48 kHz all
//! do), so a result's media time is the source's media time minus an exact offset in ticks.

use filmcraft_project::{ClipId, ItemId, MediaRef, TrackItem, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick};

use crate::Session;

/// What a job reads for one clip.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AudioTarget {
    pub clip: ClipId,
    pub name: String,
    pub item: ItemId,
    pub path: String,
    pub stream: usize,
    /// Source channels decoded (one or two).
    pub channels: Vec<usize>,
    /// The decode rate, the first decoded sample and how many.
    pub rate: u32,
    pub offset: i64,
    pub samples: i64,
    /// The media's end, in samples at `rate` ([`read_chunk`]).
    pub end: i64,
    /// The source's own rate (results can be resampled back to it).
    pub source_rate: u32,
    pub audio_track: bool,
}

impl AudioTarget {
    /// The offset in ticks: a result's media time is the source's minus this.
    pub fn offset_ticks(&self) -> Tick {
        Tick(self.offset.saturating_mul(ticks_per_sample(self.rate)))
    }

    pub fn seconds(&self) -> f64 {
        self.samples as f64 / self.rate.max(1) as f64
    }

    /// Same media, stream, channels and range: one job serves both clips.
    pub fn same_input(&self, o: &AudioTarget) -> bool {
        (&self.path, self.stream, &self.channels, self.rate, self.offset, self.samples) == (&o.path, o.stream, &o.channels, o.rate, o.offset, o.samples)
    }
}

/// Ticks per sample at `rate` (exact for rates that divide [`TICKS_PER_SECOND`]).
pub(crate) fn ticks_per_sample(rate: u32) -> i64 {
    TICKS_PER_SECOND / i64::from(rate.max(1))
}

/// Why a clip gets no job: `Skip` (no audio here; other selected clips may still have some) or
/// `Hard` (it has audio but cannot be used: the command fails with the reason).
#[derive(Debug)]
pub(crate) enum Refused {
    Skip,
    Hard(String),
}

/// The decode target of clip `c` at `rate`, with `pad_s` of source on each side and at most
/// `max_s` in all. `to_end`: decode from the clip's first source frame to the end of the media
/// (music fit needs what follows the clip).
pub(crate) fn audio_target(s: &Session, c: ClipId, rate: u32, pad_s: f64, max_s: f64, to_end: bool) -> std::result::Result<AudioTarget, Refused> {
    let q = s.active_sequence().ok_or(Refused::Skip)?;
    let (t, it) = q.find_item(c).ok_or(Refused::Skip)?;
    let (_, m, _) = s.project.resolve_media(it.item).ok_or(Refused::Skip)?;
    // a remix plays pieces from anywhere in the media (absolute times in its plan)
    if filmcraft_render::remix::is_remixed(it) {
        return Err(Refused::Hard(format!("{}: the clip is remixed; revert the remix first", it.name)));
    }
    let MediaRef::File { path } = &m.media else { return Err(Refused::Skip) };
    let stream = it.audio_stream;
    let info = m.info.audio_streams.get(stream).ok_or(Refused::Skip)?;
    let have = info.channels.max(1) as usize;
    let channels: Vec<usize> = match it.source_channels.as_slice() {
        [] => (0..have.min(2)).collect(),
        chosen => chosen.iter().take(2).map(|c| usize::from(*c).min(have - 1)).collect(),
    };
    let tps = ticks_per_sample(rate);
    let pad = (pad_s * TICKS_PER_SECOND as f64) as i64;
    // positions come from the project file: checked arithmetic, never a wrap
    let out_of_range = || Refused::Hard(format!("{}: the clip's source position is out of range; reset its In and Out points", it.name));
    let (lo, hi) = (it.source_in.0.min(it.source_out().0), it.source_in.0.max(it.source_out().0));
    let duration = m.info.duration.0.max(0);
    let a = lo.checked_sub(pad).ok_or_else(out_of_range)?.max(0);
    let b = if to_end { duration } else { hi.checked_add(pad).ok_or_else(out_of_range)?.min(duration) }.max(a);
    let offset = a / tps;
    let mut samples = b.checked_sub(offset * tps).and_then(|n| n.checked_add(tps - 1)).map(|n| n / tps).ok_or_else(out_of_range)?;
    let end = duration / tps + i64::from(duration % tps != 0);
    let max = (max_s * f64::from(rate)) as i64;
    if samples > max {
        if !to_end {
            return Err(Refused::Hard(format!(
                "{}: the clip uses {:.0} min of its source; at most {:.0} min can be processed",
                it.name,
                samples as f64 / f64::from(rate) / 60.0,
                max_s / 60.0
            )));
        }
        samples = max;
    }
    if samples <= 0 {
        return Err(Refused::Skip);
    }
    let audio_track = q.track(t).is_some_and(|t| t.kind == TrackKind::Audio);
    Ok(AudioTarget {
        clip: c,
        name: it.name.clone(),
        item: it.item,
        path: path.clone(),
        stream,
        channels,
        rate,
        offset,
        samples,
        end,
        source_rate: info.sample_rate,
        audio_track,
    })
}

/// The selected clips' targets, every clip checked before any job starts; clips reading the same
/// input share one entry (audio clips first, so a linked video clip joins its audio clip).
pub(crate) fn targets(
    s: &Session,
    clips: Vec<ClipId>,
    f: impl Fn(&Session, ClipId) -> std::result::Result<AudioTarget, Refused>,
) -> crate::Result<Vec<(AudioTarget, Vec<ClipId>)>> {
    let mut picked = clips;
    picked.sort_by_key(|c| f(s, *c).map_or(1, |t| u8::from(!t.audio_track)));
    let mut out: Vec<(AudioTarget, Vec<ClipId>)> = Vec::new();
    for c in picked {
        match f(s, c) {
            Ok(t) => match out.iter_mut().find(|(o, _)| o.same_input(&t)) {
                Some((_, clips)) => clips.push(c),
                None => out.push((t, vec![c])),
            },
            Err(Refused::Hard(e)) => return Err(crate::EngineError::Other(e)),
            Err(Refused::Skip) => {}
        }
    }
    Ok(out)
}

/// Write `samples` samples of `src`'s audio stream `stream` from sample `offset` (at `rate`,
/// resampled by FilmCraft's decoder), as a 32-bit float WAV: the `channels` listed, or, with
/// `mono`, their average. Ten seconds at a time; stops when `stop` says so. A read that comes back
/// short (the stream ended) is padded with silence; a decode failure is an error naming the stream
/// and the range ([`read_chunk`]; `end` is the media's end, samples, for messages).
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_to_wav(
    src: &dyn filmcraft_media::MediaSource,
    stream: usize,
    channels: &[usize],
    mono: bool,
    (offset, samples, end): (i64, i64, i64),
    rate: u32,
    path: &std::path::Path,
    stop: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(f64),
) -> std::result::Result<(), String> {
    use std::io::Write;
    let picked: Vec<usize> = if channels.is_empty() { vec![0] } else { channels.iter().take(2).copied().collect() };
    let ch = if mono { 1 } else { picked.len() };
    let total = samples.max(0) as u64;
    let bytes = total.saturating_mul(ch as u64 * 4);
    let size = u32::try_from(bytes).map_err(|_| "the range is too long for a WAV file".to_string())?;
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut w = std::io::BufWriter::new(file);
    let c16 = ch as u16;
    let mut head = Vec::with_capacity(44);
    head.extend_from_slice(b"RIFF");
    head.extend_from_slice(&size.saturating_add(36).to_le_bytes());
    head.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &3u16.to_le_bytes(),
        &c16.to_le_bytes(),
        &rate.to_le_bytes(),
        &(rate.saturating_mul(u32::from(c16) * 4)).to_le_bytes(),
        &(c16 * 4).to_le_bytes(),
        &32u16.to_le_bytes(),
    ] {
        head.extend_from_slice(v);
    }
    head.extend_from_slice(b"data");
    head.extend_from_slice(&size.to_le_bytes());
    w.write_all(&head).map_err(|e| e.to_string())?;
    let step = u64::from(rate) * 10;
    let mut done = 0u64;
    while done < total {
        if stop() {
            return Err("stopped".into());
        }
        let n = step.min(total - done) as usize;
        let buf = read_chunk(src, stream, offset.saturating_add(done as i64), n, rate, end)?;
        let get = |c: usize, i: usize| buf.channels.get(c).or(buf.channels.first()).and_then(|x| x.get(i)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
        let mut out = Vec::with_capacity(n * ch * 4);
        for i in 0..n {
            if mono {
                let v = picked.iter().map(|c| get(*c, i)).sum::<f32>() / picked.len() as f32;
                out.extend_from_slice(&v.to_le_bytes());
            } else {
                for c in &picked {
                    out.extend_from_slice(&get(*c, i).to_le_bytes());
                }
            }
        }
        w.write_all(&out).map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done as f64 / total.max(1) as f64);
    }
    w.flush().map_err(|e| e.to_string())
}

/// `n` samples of audio stream `stream` from sample `pos` at `rate`. FilmCraft's decoders answer
/// reads past the end of a stream with silence or a short buffer (the stream's own end-of-stream
/// signal), which callers pad with silence; an error is never taken for the end: it is returned
/// with the stream and the range (`end`: the media's end, samples). A cancelled read stops.
pub(crate) fn read_chunk(
    src: &dyn filmcraft_media::MediaSource,
    stream: usize,
    pos: i64,
    n: usize,
    rate: u32,
    end: i64,
) -> std::result::Result<filmcraft_frame::AudioBuffer, String> {
    match src.audio_stream(stream, pos, n, rate) {
        Ok(b) => Ok(b),
        Err(filmcraft_media::MediaError::Cancelled) => Err("stopped".into()),
        Err(e) => {
            let t = |x: i64| x as f64 / f64::from(rate.max(1));
            Err(format!(
                "decoding audio stream {stream} failed reading {:.3} to {:.3} s (the media lasts {:.3} s): {e}",
                t(pos),
                t(pos.saturating_add(n as i64)),
                t(end)
            ))
        }
    }
}

/// The timeline time at which `it` shows media time `mt` (`None` outside the clip). The inverse
/// of [`TrackItem::source_time_at`] for clips without a frame hold.
pub(crate) fn timeline_time(it: &TrackItem, mt: Tick) -> Option<Tick> {
    if it.frame_hold.is_some() {
        return None;
    }
    let speed = it.speed.abs();
    if !(speed.is_finite() && speed > 0.0) {
        return None;
    }
    let scaled = if it.reverse { it.source_out().0.checked_sub(1)?.checked_sub(mt.0)? } else { mt.0.checked_sub(it.source_in.0)? };
    let t = Tick(it.start.0.checked_add((scaled as f64 / speed).round() as i64)?);
    (t >= it.start && t < it.end()).then_some(t)
}
