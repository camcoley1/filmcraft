//! Sequence ▸ Transcript ▸ Label Speakers (`transcript.diarize`): re-run speaker labelling on an
//! existing transcript without transcribing again.
//!
//! `transcript.diarize {item?, maxSpeakers=6, threshold=0.4, wait=false}` decodes the item's audio
//! to mono 16 kHz in a background job (`jobs.list` shows the progress, `jobs.cancel` stops it
//! without changing anything), runs [`filmcraft_speech::diarize::diarize`] on a copy of the
//! transcript and replaces the transcript's speakers and every word's speaker in one undo step
//! ("Label Speakers"). The words and their times are not touched. If the transcript was changed
//! while the job ran, the labels are dropped with a toast instead of overwriting the change.

use std::sync::{Arc, Mutex};

use filmcraft_project::{ItemId, ItemKind, Transcript};
use filmcraft_speech::SAMPLE_RATE;
use filmcraft_speech::diarize::{Params, diarize_report};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, f64_p, u64_p};
use crate::{EngineError, Result, Session};

/// Longest audio labelled (2 hours of 16 kHz mono floats is about 460 MB).
const MAX_SECONDS: i64 = 2 * 3600;
/// Seconds of audio decoded per step (progress and cancel points).
const CHUNK_SECONDS: i64 = 60;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "transcript.diarize",
        label: "Label Speakers",
        menu: &["Sequence", "Transcript"],
        shortcut: None,
        params: r#"{"item":id?,"maxSpeakers":1..32=6,"threshold":0.05..2=0.4,"wait":bool=false}"#,
        enabled: has_transcripts,
        run,
        journal: true,
    }]
}

fn has_transcripts(s: &Session) -> std::result::Result<(), String> {
    if s.project.transcripts.values().any(|t| !t.words.is_empty()) { Ok(()) } else { Err("there is no transcript to label (Transcribe first)".into()) }
}

/// A running Label Speakers job; applied by [`poll`] when it finishes.
pub struct PendingDiarize {
    pub job: u64,
    pub item: ItemId,
    /// The transcript the job started from.
    pub base: Arc<Transcript>,
    pub result: Arc<Mutex<Option<Transcript>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The media items whose transcripts a nested or multi-camera clip plays, first the one that gives
/// the most words.
fn nested_media(s: &Session, nested: ItemId) -> Vec<ItemId> {
    let Some(seq) = s.project.sequence(nested) else { return Vec::new() };
    let mut count: Vec<(ItemId, usize)> = Vec::new();
    for w in filmcraft_edit::transcript::sequence_words_nested(seq, &s.project.transcripts, &|id| s.project.sequence(id)) {
        match count.iter_mut().find(|c| c.0 == w.item) {
            Some(c) => c.1 += 1,
            None => count.push((w.item, 1)),
        }
    }
    count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    count.into_iter().map(|c| c.0).collect()
}

/// The transcript to label: `item`, else the transcribed audio of a selected multi-camera or
/// nested clip, else the media of the selection, else the only transcript.
fn target(s: &Session, p: &Value) -> Result<ItemId> {
    let cmd = "transcript.diarize";
    let media = |i: ItemId| s.project.resolve_media(i).map(|(root, _, _)| root);
    let has = |m: &ItemId| s.project.transcripts.get(m).is_some_and(|t| !t.words.is_empty());
    if let Some(i) = u64_p(p, "item") {
        let m = media(ItemId(i)).ok_or_else(|| bad(cmd, "no such media item"))?;
        return if has(&m) { Ok(m) } else { Err(bad(cmd, "that item has no transcript")) };
    }
    let clip_items: Vec<ItemId> =
        s.active_sequence().into_iter().flat_map(|q| s.state.selection.iter().filter_map(move |c| q.find_item(*c).map(|x| x.1.item))).collect();
    // a selected multi-camera or nested clip: the audio it plays
    for item in &clip_items {
        if s.project.sequence(*item).is_some()
            && let Some(m) = nested_media(s, *item).into_iter().find(has)
        {
            return Ok(m);
        }
    }
    let candidates: Vec<ItemId> = s.state.project_selection.iter().copied().chain(clip_items).filter_map(media).collect();
    if let Some(m) = candidates.into_iter().find(has) {
        return Ok(m);
    }
    let all: Vec<ItemId> = s.project.transcripts.iter().filter(|(_, t)| !t.words.is_empty()).map(|(i, _)| *i).collect();
    match all.as_slice() {
        [one] => Ok(*one),
        [] => Err(bad(cmd, "there is no transcript to label (Transcribe first)")),
        _ => Err(bad(cmd, "several items have transcripts: select the clip or media whose speakers to label, or pass `item`")),
    }
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "transcript.diarize";
    let item = target(s, p)?;
    if s.diarize_jobs.iter().any(|j| j.item == item) {
        return Err(EngineError::Other("Label Speakers is already running on this item".into()));
    }
    let base = s.project.transcripts.get(&item).cloned().ok_or_else(|| bad(cmd, "no transcript"))?;
    let dur = match &s.project.item(item).ok_or_else(|| bad(cmd, "no such media item"))?.kind {
        ItemKind::Media(m) => m.duration(),
        _ => return Err(bad(cmd, "not a media item")),
    };
    let src = s.source(item).filter(|src| src.info().has_audio()).ok_or_else(|| bad(cmd, "the item has no audio to label speakers from"))?;
    let len = dur.to_units_floor(SAMPLE_RATE as i64).max(0);
    if len > MAX_SECONDS * SAMPLE_RATE as i64 {
        return Err(bad(cmd, format!("the audio is longer than {} hours", MAX_SECONDS / 3600)));
    }
    let mut params = Params::default();
    if let Some(n) = u64_p(p, "maxSpeakers") {
        params.max_speakers = n.clamp(1, 32) as usize;
    }
    if let Some(t) = f64_p(p, "threshold") {
        if !t.is_finite() {
            return Err(bad(cmd, "`threshold` must be a number"));
        }
        params.threshold = t.clamp(0.05, 2.0) as f32;
    }
    let steps = (len + CHUNK - 1) / CHUNK + 1;
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let name = s.project.item(item).map(|i| i.name.clone()).unwrap_or_default();
    let job = crate::Job { id, label: format!("Label Speakers ({name})"), progress: Default::default(), result: Default::default() };
    job.progress.total.store(steps.max(1) as u64, std::sync::atomic::Ordering::Relaxed);
    let result: Arc<Mutex<Option<Transcript>>> = Arc::default();
    let (prog, res, out, base_run) = (job.progress.clone(), job.result.clone(), result.clone(), base.clone());
    let work = move || {
        use std::sync::atomic::Ordering;
        let t0 = web_time::Instant::now();
        let r = label(&*src, len, &base_run, &params, &prog);
        let secs = t0.elapsed().as_secs_f64();
        *lock(&prog.status) = match &r {
            Ok((_, n)) => {
                let coarse = lock(&prog.status).starts_with("Very long");
                format!("Found {n} speaker(s) ({secs:.1}s){}", if coarse { "; the recording is very long, so speech was grouped coarsely" } else { "" })
            }
            Err(e) if e == "stopped" => "Stopped: nothing was changed".into(),
            Err(e) => e.clone(),
        };
        let r = match r {
            Ok((t, _)) => {
                *lock(&out) = Some(t);
                Ok(filmcraft_export::Report { path: String::new(), frames: 0, seconds: secs, bytes: 0, render_fps: 0.0, extra_files: Vec::new() })
            }
            Err(e) => Err(e),
        };
        *lock(&res) = Some(r);
        prog.finished.store(true, Ordering::Relaxed);
    };
    let work = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), work);
    s.jobs.push(job);
    s.diarize_jobs.push(PendingDiarize { job: id, item, base, result: result.clone() });
    let wait = bool_p(p, "wait").unwrap_or(false);
    let mut out = json!({"job": id, "item": item.0});
    if wait || cfg!(target_arch = "wasm32") {
        work();
        let speakers = lock(&result).as_ref().map(|t| t.speakers.len());
        poll(s);
        if let Some(Err(e)) = s.jobs.iter().find(|j| j.id == id).and_then(|j| lock(&j.result).clone()) {
            return Err(EngineError::Other(e));
        }
        out["speakers"] = json!(speakers);
    } else if let Err(e) = spawn_worker(work) {
        // no thread: the job never ran, so it must not stay registered as pending
        s.diarize_jobs.retain(|j| j.job != id);
        s.jobs.retain(|j| j.id != id);
        return Err(EngineError::Other(format!("can't start the Label Speakers job: {e}")));
    }
    Ok(out)
}

/// Start `work` on a thread of its own.
fn spawn_worker(work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    SPAWN.with(|hook| match hook.get() {
        Some(fail) if fail => Err(std::io::Error::other("test: thread limit")),
        _ => std::thread::Builder::new().name("filmcraft-diarize".into()).spawn(work).map(|_| ()),
    })
}

thread_local! {
    /// Test hook: make [`spawn_worker`] fail on this thread.
    static SPAWN: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Make [`spawn_worker`] fail on this thread (tests of the thread-limit path).
#[cfg(test)]
pub(crate) fn fail_spawns(on: bool) {
    SPAWN.with(|c| c.set(Some(on)));
}

/// Samples of 16 kHz audio decoded per step (progress and cancel points).
const CHUNK: i64 = CHUNK_SECONDS * SAMPLE_RATE as i64;

/// `len` samples of `src` as mono 16 kHz, read in chunks (progress and cancel between them).
pub(crate) fn decode_chunked(src: &dyn filmcraft_media::MediaSource, len: i64, prog: &filmcraft_export::Progress) -> std::result::Result<Vec<f32>, String> {
    use std::sync::atomic::Ordering;
    let mut audio: Vec<f32> = Vec::with_capacity(len.max(0) as usize);
    let mut start = 0i64;
    while start < len {
        if prog.cancel.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        let frames = CHUNK.min(len - start) as usize;
        let buf = src.audio(start, frames, SAMPLE_RATE).map_err(|e| format!("can't decode audio at {} s: {e}", start / SAMPLE_RATE as i64))?;
        let mut mono = filmcraft_speech::downmix(&buf.channels);
        mono.resize(frames, 0.0);
        audio.extend(mono);
        start += CHUNK;
        prog.done.store((start / CHUNK).max(0) as u64, Ordering::Relaxed);
        *lock(&prog.status) = format!("Decoding audio: {} of {} s", start.min(len) / SAMPLE_RATE as i64, len / SAMPLE_RATE as i64);
    }
    Ok(audio)
}

/// Decode `len` samples of `src` and label the speakers of a copy of `base`. `Err("stopped")`
/// when the job was cancelled; other errors are messages.
pub(crate) fn label(
    src: &dyn filmcraft_media::MediaSource,
    len: i64,
    base: &Transcript,
    params: &Params,
    prog: &filmcraft_export::Progress,
) -> std::result::Result<(Transcript, usize), String> {
    use std::sync::atomic::Ordering;
    let cancelled = || prog.cancel.load(Ordering::Relaxed);
    let audio = decode_chunked(src, len, prog)?;
    *lock(&prog.status) = "Clustering voices".into();
    let mut t = base.clone();
    let out = diarize_report(&audio, &mut t, params, &cancelled).ok_or_else(|| "stopped".to_string())?;
    prog.done.store(prog.total.load(Ordering::Relaxed), Ordering::Relaxed);
    if out.coarse {
        *lock(&prog.status) = "Very long recording: speech was grouped coarsely".into();
    }
    Ok((t, out.speakers))
}

/// Apply finished jobs (one undo step each) and drop finished / cancelled ones. Called once per UI
/// frame from [`Session::poll_persistence`] and after synchronous runs.
pub fn poll(s: &mut Session) {
    use std::sync::atomic::Ordering;
    let mut i = 0;
    while i < s.diarize_jobs.len() {
        let job = s.jobs.iter().find(|j| j.id == s.diarize_jobs[i].job);
        let finished = job.is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        let cancelled = job.is_some_and(|j| j.progress.cancel.load(Ordering::Relaxed));
        if !finished {
            i += 1;
            continue;
        }
        let pj = s.diarize_jobs.remove(i);
        let Some(t) = lock(&pj.result).take().filter(|_| !cancelled) else { continue };
        let unchanged = s.project.transcripts.get(&pj.item).is_some_and(|cur| Arc::ptr_eq(cur, &pj.base));
        if !unchanged {
            s.toast("Label Speakers: the transcript changed while it ran, so the labels were dropped");
            continue;
        }
        let item = pj.item;
        let r = s.edit("Label Speakers", move |pr, _| {
            pr.transcripts.insert(item, Arc::new(t));
            Ok(())
        });
        if let Err(e) = r {
            s.error_toast("transcript.diarize", format!("Label Speakers: {e}"));
        }
    }
}
