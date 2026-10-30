//! `ai.stems` (M6b): split a clip's audio into vocals, drums, bass and other.
//!
//! `ai.stems {clips?, place=false, bandwidth="16k", wait=false}` starts one helper job per
//! selected clip with audio (a linked video and audio pair counts once). The job's thread decodes
//! the part of the source the clip plays, plus [`PAD_S`] on each side (the clip's audio stream and
//! channels; at most [`MAX_SECONDS`]), to a 44.1 kHz float WAV in the job folder with FilmCraft's
//! own decoders, and the helper splits that. The decoded range starts on a 44.1 kHz sample, so a
//! stem's media time is the source's media time minus an exact offset: sample-accurate alignment
//! whatever the source's rate.
//!
//! The result is applied in one undo step ("Extract Stems"): the four WAV files are moved out of
//! the AI cache into the project's AI media folder ([`super::media_dir`]), imported into a new bin
//! "Stems: <clip>" and, with `place`, put on four new audio tracks under the clip with its start,
//! duration, speed and direction and its source In shifted by the offset. A stem whose length
//! differs from the decoded range's fails the apply and nothing is added. When the clip's
//! sequence is not the open one, the stems are imported and not placed.

use std::time::Duration;

use filmcraft_project::{BinId, ClipId, ItemId, MediaRef, TrackId, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{bool_p, clips_p, has_seq};
use crate::{EngineError, Result, Session};

pub(crate) const PARAMS: &str = r#"{"clips":[id]?,"place":bool=false (also put the stems on new audio tracks under the clip),"bandwidth":"16k"|"11k"="16k" (how high the model separates; above it the stems are silent),"wait":bool=false}"#;

/// The stems, in track order.
pub const STEMS: [&str; 4] = ["vocals", "drums", "bass", "other"];
/// Longest range separated (20 minutes): the clip's used source range with its padding.
pub const MAX_SECONDS: f64 = 1200.0;
/// Source decoded on each side of what the clip plays (context for the separation, and room to
/// trim the placed stems a little).
pub const PAD_S: f64 = 2.0;
/// The model's sample rate.
pub const RATE: u32 = 44_100;
/// Ticks per sample at [`RATE`] (exact: 254 016 000 000 / 44 100).
const TICKS_PER_SAMPLE: i64 = TICKS_PER_SECOND / RATE as i64;
/// Largest stem file read (20 minutes of 44.1 kHz stereo float is about 430 MB).
const MAX_STEM_BYTES: u64 = 2 << 30;
/// A stem may differ from the decoded length by this much.
const LENGTH_SLACK_S: f64 = 0.05;

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    super::helper_ready(s, "stems")?;
    has_seq(s)?;
    // a clip with audio that cannot be split still enables the command, which then says why
    if s.state.selection.iter().any(|c| !matches!(target(s, *c), Err(Refused::Skip))) { Ok(()) } else { Err("select a clip with audio in the timeline".into()) }
}

/// What a stems job reads for one clip.
#[derive(Clone, Debug, PartialEq)]
struct Target {
    clip: ClipId,
    name: String,
    item: ItemId,
    path: String,
    stream: usize,
    /// Source channels decoded (one or two).
    channels: Vec<usize>,
    /// First decoded sample (at [`RATE`]) and how many.
    offset: i64,
    samples: i64,
    /// The media's end, in samples at [`RATE`].
    end: i64,
    audio_track: bool,
    link: Option<u64>,
}

impl Target {
    /// Another clip that this one's stems would duplicate: the same media, stream, channels and
    /// range, or (a video clip) its linked audio clip playing the same range.
    fn covered_by(&self, o: &Target) -> bool {
        let range = (&self.path, self.offset, self.samples) == (&o.path, o.offset, o.samples);
        let input = range && (self.stream, &self.channels) == (o.stream, &o.channels);
        input || (range && !self.audio_track && o.audio_track && self.link.is_some() && self.link == o.link)
    }
}

/// Why a clip is not split: `Skip` (no audio here: other selected clips may still be split) or
/// `Hard` (it has audio but cannot be split: the command fails).
#[derive(Debug)]
enum Refused {
    Skip,
    Hard(String),
}

/// The clip's decode target, or why it cannot be split.
fn target(s: &Session, c: ClipId) -> std::result::Result<Target, Refused> {
    target_or(s, c).map_err(|(hard, e)| if hard { Refused::Hard(e) } else { Refused::Skip })
}

fn target_or(s: &Session, c: ClipId) -> std::result::Result<Target, (bool, String)> {
    let skip = |e: String| (false, e);
    let q = s.active_sequence().ok_or_else(|| skip("no sequence is open".into()))?;
    let (t, it) = q.find_item(c).ok_or_else(|| skip("the clip is not in the open sequence".into()))?;
    let (_, m, _) = s.project.resolve_media(it.item).ok_or_else(|| skip(format!("{}: no media", it.name)))?;
    let MediaRef::File { path } = &m.media else { return Err(skip(format!("{}: generated media has no audio to split", it.name))) };
    let stream = it.audio_stream;
    let info = m.info.audio_streams.get(stream).ok_or_else(|| skip(format!("{}: the media has no audio (stream {stream})", it.name)))?;
    let have = info.channels.max(1) as usize;
    let channels: Vec<usize> = match it.source_channels.as_slice() {
        [] => (0..have.min(2)).collect(),
        chosen => chosen.iter().take(2).map(|c| usize::from(*c).min(have - 1)).collect(),
    };
    // the media the clip plays, padded, on the 44.1 kHz sample grid (positions come from the
    // project file: checked arithmetic, never a wrap)
    let pad = (PAD_S * TICKS_PER_SECOND as f64) as i64;
    let out_of_range = || (true, format!("{}: the clip's source position is out of range; reset its In and Out points", it.name));
    let (lo, hi) = (it.source_in.0.min(it.source_out().0), it.source_in.0.max(it.source_out().0));
    let duration = m.info.duration.0.max(0);
    let a = lo.checked_sub(pad).ok_or_else(out_of_range)?.max(0);
    let b = hi.checked_add(pad).ok_or_else(out_of_range)?.min(duration).max(a);
    let offset = a / TICKS_PER_SAMPLE;
    let samples =
        b.checked_sub(offset * TICKS_PER_SAMPLE).and_then(|n| n.checked_add(TICKS_PER_SAMPLE - 1)).map(|n| n / TICKS_PER_SAMPLE).ok_or_else(out_of_range)?;
    let end = duration / TICKS_PER_SAMPLE + i64::from(duration % TICKS_PER_SAMPLE != 0);
    if samples as f64 > MAX_SECONDS * RATE as f64 {
        return Err((
            true,
            format!(
                "{}: the clip uses {:.0} min of its source; stems are made for up to {:.0} min",
                it.name,
                samples as f64 / RATE as f64 / 60.0,
                MAX_SECONDS / 60.0
            ),
        ));
    }
    if samples <= 0 {
        return Err(skip(format!("{}: the clip plays no audio", it.name)));
    }
    let audio_track = q.track(t).is_some_and(|t| t.kind == TrackKind::Audio);
    Ok(Target { clip: c, name: it.name.clone(), item: it.item, path: path.clone(), stream, channels, offset, samples, end, audio_track, link: it.link })
}

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let place = bool_p(p, "place").unwrap_or(false);
    let bandwidth = match p.get("bandwidth").and_then(Value::as_str) {
        None => "16k",
        Some(b @ ("16k" | "11k")) => b,
        Some(b) => return Err(crate::commands::bad("ai.stems", format!("`bandwidth` is \"16k\" or \"11k\", not `{b}`"))),
    };
    let wait = bool_p(p, "wait").unwrap_or(false);
    // every clip is checked before any job starts; a linked video and audio pair (same media and
    // range) is split once, from its audio clip; clips playing other channels or streams of the
    // same range (dual mono split to two tracks) are split each
    let mut targets: Vec<Target> = Vec::new();
    let mut picked: Vec<ClipId> = clips_p(s, p);
    picked.sort_by_key(|c| target(s, *c).map_or(1, |t| u8::from(!t.audio_track)));
    for c in picked {
        match target(s, c) {
            Ok(t) => {
                if !targets.iter().any(|o| t.covered_by(o)) {
                    targets.push(t);
                }
            }
            // a clip that has audio but cannot be split stops the command; others are skipped
            Err(Refused::Hard(e)) => return Err(EngineError::Other(e)),
            Err(Refused::Skip) => {}
        }
    }
    if targets.is_empty() {
        return Err(EngineError::Other("select a clip with audio in the timeline".into()));
    }
    let mut jobs = Vec::new();
    for t in targets {
        let seconds = t.samples as f64 / RATE as f64;
        // CPU separation runs at a few times real time: allow 6x the range plus 10 minutes
        let timeout = Duration::from_secs_f64(seconds * 6.0 + 600.0);
        let src = s.source(t.item).ok_or_else(|| EngineError::Other(format!("{}: the media cannot be read", t.name)))?;
        let (stream, channels, offset, samples, end) = (t.stream, t.channels.clone(), t.offset, t.samples, t.end);
        let spec = super::JobSpec {
            kind: "stems",
            label: "Extract Stems",
            clips: &[t.clip],
            params: json!({"stems": STEMS, "maxSeconds": MAX_SECONDS, "bandwidth": bandwidth}),
            context: json!({"name": t.name, "place": place, "offset": offset, "samples": samples}),
            timeout: Some(timeout),
            prepare: Some(Box::new(move |dir: &std::path::Path, stop: &dyn Fn() -> bool, progress: &mut dyn FnMut(f64)| {
                let path = dir.join("source.wav");
                decode_to_wav(&*src, stream, &channels, (offset, samples, end), &path, stop, progress)?;
                Ok(vec![path.to_string_lossy().to_string()])
            })),
            loose: !place,
            media: true,
        };
        jobs.push(super::start_job(s, spec, wait)?);
    }
    Ok(if jobs.len() == 1 { jobs.remove(0) } else { json!({"jobs": jobs}) })
}

/// Write `samples` samples of `src`'s audio stream `stream` from sample `offset` (at [`RATE`],
/// resampled), channels `channels`, as a 32-bit float WAV, ten seconds at a time; stops when
/// `stop` says so. `end` is the media's end (samples, for messages). A read that comes back short
/// (the stream ended) is padded with silence; a decode failure is an error naming the stream and
/// the range ([`read_chunk`]).
fn decode_to_wav(
    src: &dyn filmcraft_media::MediaSource,
    stream: usize,
    channels: &[usize],
    (offset, samples, end): (i64, i64, i64),
    path: &std::path::Path,
    stop: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(f64),
) -> std::result::Result<(), String> {
    use std::io::Write;
    let ch = channels.len().clamp(1, 2);
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
        &RATE.to_le_bytes(),
        &(RATE * u32::from(c16) * 4).to_le_bytes(),
        &(c16 * 4).to_le_bytes(),
        &32u16.to_le_bytes(),
    ] {
        head.extend_from_slice(v);
    }
    head.extend_from_slice(b"data");
    head.extend_from_slice(&size.to_le_bytes());
    w.write_all(&head).map_err(|e| e.to_string())?;
    let step = RATE as u64 * 10;
    let mut done = 0u64;
    while done < total {
        if stop() {
            return Err("stopped".into());
        }
        let n = step.min(total - done) as usize;
        let buf = read_chunk(src, stream, offset.saturating_add(done as i64), n, end)?;
        let mut out = Vec::with_capacity(n * ch * 4);
        for i in 0..n {
            for c in channels.iter().take(ch) {
                let v = buf.channels.get(*c).or(buf.channels.first()).and_then(|x| x.get(i)).copied().unwrap_or(0.0);
                out.extend_from_slice(&(if v.is_finite() { v } else { 0.0 }).to_le_bytes());
            }
        }
        w.write_all(&out).map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done as f64 / total.max(1) as f64);
    }
    w.flush().map_err(|e| e.to_string())
}

/// `n` samples of audio stream `stream` from sample `pos`. FilmCraft's decoders answer reads past
/// the end of a stream with silence or a short buffer (the stream's own end-of-stream signal),
/// which is padded with silence; an error is never taken for the end: it fails the job with the
/// stream and the range. A cancelled read stops.
fn read_chunk(
    src: &dyn filmcraft_media::MediaSource,
    stream: usize,
    pos: i64,
    n: usize,
    end: i64,
) -> std::result::Result<filmcraft_frame::AudioBuffer, String> {
    match src.audio_stream(stream, pos, n, RATE) {
        Ok(b) => Ok(b),
        Err(filmcraft_media::MediaError::Cancelled) => Err("stopped".into()),
        Err(e) => {
            let t = |x: i64| x as f64 / f64::from(RATE);
            Err(format!(
                "decoding audio stream {stream} failed reading {:.3} to {:.3} s (the media lasts {:.3} s): {e}",
                t(pos),
                t(pos.saturating_add(n as i64)),
                t(end)
            ))
        }
    }
}

pub(crate) fn apply(s: &mut Session, pj: &super::PendingAi, files: &[std::path::PathBuf]) -> Result<()> {
    let mut stems = Vec::with_capacity(STEMS.len());
    for stem in STEMS {
        let f = files
            .iter()
            .find(|f| f.file_name().is_some_and(|n| n.to_string_lossy() == format!("{stem}.wav")))
            .ok_or_else(|| EngineError::Other(format!("the helper wrote no {stem}.wav")))?;
        // checked again on open (a file swapped after the job's check is refused)
        filmcraft_sidecar::open_result(f, pj.result_dir(), MAX_STEM_BYTES).map_err(|e| EngineError::Other(e.to_string()))?;
        stems.push(f.clone());
    }
    let name = pj.context.get("name").and_then(Value::as_str).unwrap_or("Clip").to_string();
    let seq = pj.fingerprint.seq;
    let want_place = pj.context.get("place").and_then(Value::as_bool).unwrap_or(false);
    let place = want_place && s.state.active_sequence == Some(seq);
    let offset = Tick(pj.context.get("offset").and_then(Value::as_i64).unwrap_or(0).saturating_mul(TICKS_PER_SAMPLE));
    let seconds = pj.context.get("samples").and_then(Value::as_i64).unwrap_or(0) as f64 / RATE as f64;
    let clip = pj.fingerprint.clips.first().map(|c| c.clip).ok_or_else(|| EngineError::Other("no target clip".into()))?;
    let bin_name = format!("Stems: {name}");
    let r = super::apply_with_media(s, pj, "Extract Stems", &stems, move |moved| {
        let paths: Vec<String> = moved.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let import: super::Step = Box::new(move |s: &mut Session| {
            let bin: BinId = s.edit("Extract Stems", |pr, _| Ok(pr.add_bin(&bin_name, None)))?;
            let r = s.execute("file.import", json!({"paths": paths, "bin": bin.0}))?;
            let errors = r.get("errors").and_then(Value::as_array).map_or(0, Vec::len);
            let items: Vec<ItemId> = r.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).map(ItemId).collect();
            if errors > 0 || items.len() != STEMS.len() {
                return Err(EngineError::Other(format!("importing the stems failed: {}", r.get("errors").cloned().unwrap_or_default())));
            }
            for (stem, item) in STEMS.iter().zip(&items) {
                let d = s.project.item(*item).and_then(|i| i.as_media()).map(|m| m.info.duration.0 as f64 / TICKS_PER_SECOND as f64);
                if d.is_none_or(|d| (d - seconds).abs() > LENGTH_SLACK_S) {
                    return Err(EngineError::Other(format!("the {stem} stem is not as long as the decoded source")));
                }
            }
            if !place {
                return Ok(());
            }
            let added = s.execute("sequence.addTracks", json!({"video": 0, "audio": STEMS.len()}))?;
            let tracks: Vec<TrackId> = added["audio"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(TrackId).collect();
            if tracks.len() != STEMS.len() {
                return Err(EngineError::Other("could not add the stem tracks".into()));
            }
            s.edit("Extract Stems", move |pr, _| {
                let rate = pr.sequence(seq).map(|q| q.settings.frame_rate).ok_or(EngineError::NoSequence)?;
                let it =
                    pr.sequence(seq).and_then(|q| q.find_item(clip)).map(|(_, it)| it.clone()).ok_or_else(|| EngineError::Other("the clip is gone".into()))?;
                // a stem's media time is the source's minus the offset of the decoded range
                let source_in = it.source_in - offset;
                for ((stem, item), track) in STEMS.iter().zip(&items).zip(&tracks) {
                    let range = TimeRange::new(source_in.max(Tick::ZERO), it.duration.max(Tick(1)));
                    let mut ti = pr
                        .make_track_item(*item, TrackKind::Audio, it.start, range, rate)
                        .ok_or_else(|| EngineError::Other(format!("cannot place the {stem} stem")))?;
                    (ti.start, ti.duration, ti.source_in, ti.speed, ti.reverse) = (it.start, it.duration, source_in, it.speed, it.reverse);
                    ti.name = format!("{name} ({stem})");
                    let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                    q.audio_tracks.iter_mut().find(|t| t.id == *track).ok_or_else(|| EngineError::Other("a stem track is gone".into()))?.items.push(ti);
                }
                Ok(())
            })
        });
        vec![import]
    });
    if r.is_ok() && want_place && !place {
        s.toast("Stems imported; open the clip's sequence to place them");
    }
    r
}

#[cfg(test)]
#[path = "ai_stems_tests.rs"]
mod tests;
