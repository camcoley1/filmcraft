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
use filmcraft_speech::diarize::{Params, diarize};
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

/// The transcript to label: `item`, else the media of the selected clips, else the only transcript.
fn target(s: &Session, p: &Value) -> Result<ItemId> {
    let cmd = "transcript.diarize";
    let media = |i: ItemId| s.project.resolve_media(i).map(|(root, _, _)| root);
    if let Some(i) = u64_p(p, "item") {
        let m = media(ItemId(i)).ok_or_else(|| bad(cmd, "no such media item"))?;
        return if s.project.transcripts.get(&m).is_some_and(|t| !t.words.is_empty()) { Ok(m) } else { Err(bad(cmd, "that item has no transcript")) };
    }
    let q = s.active_sequence();
    let selected = q.into_iter().flat_map(|q| s.state.selection.iter().filter_map(move |c| q.find_item(*c).map(|x| x.1.item)));
    let candidates: Vec<ItemId> = s.state.project_selection.iter().copied().chain(selected).filter_map(media).collect();
    if let Some(m) = candidates.into_iter().find(|m| s.project.transcripts.get(m).is_some_and(|t| !t.words.is_empty())) {
        return Ok(m);
    }
    let all: Vec<ItemId> = s.project.transcripts.iter().filter(|(_, t)| !t.words.is_empty()).map(|(i, _)| *i).collect();
    match all.as_slice() {
        [one] => Ok(*one),
        [] => Err(bad(cmd, "there is no transcript to label (Transcribe first)")),
        _ => Err(bad(cmd, "several items have transcripts: pass `item`")),
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
    let chunk = CHUNK_SECONDS * SAMPLE_RATE as i64;
    let steps = (len + chunk - 1) / chunk.max(1) + 1;
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let name = s.project.item(item).map(|i| i.name.clone()).unwrap_or_default();
    let job = crate::Job { id, label: format!("Label Speakers ({name})"), progress: Default::default(), result: Default::default() };
    job.progress.total.store(steps.max(1) as u64, std::sync::atomic::Ordering::Relaxed);
    let result: Arc<Mutex<Option<Transcript>>> = Arc::default();
    let (prog, res, out, base_run) = (job.progress.clone(), job.result.clone(), result.clone(), base.clone());
    let work = move || {
        use std::sync::atomic::Ordering;
        let t0 = web_time::Instant::now();
        let mut audio: Vec<f32> = Vec::with_capacity(len.max(0) as usize);
        let mut err = None;
        let mut start = 0i64;
        while start < len && err.is_none() {
            if prog.cancel.load(Ordering::Relaxed) {
                err = Some("stopped".to_string());
                break;
            }
            let frames = chunk.min(len - start) as usize;
            match src.audio(start, frames, SAMPLE_RATE) {
                Ok(buf) => {
                    let mut mono = filmcraft_speech::downmix(&buf.channels);
                    mono.resize(frames, 0.0);
                    audio.extend(mono);
                }
                Err(e) => err = Some(format!("can't decode audio at {} s: {e}", start / SAMPLE_RATE as i64)),
            }
            start += chunk;
            prog.done.store((start / chunk).max(0) as u64, Ordering::Relaxed);
            *lock(&prog.status) = format!("Decoding audio: {} of {} s", (start.min(len)) / SAMPLE_RATE as i64, len / SAMPLE_RATE as i64);
        }
        let mut speakers = 0;
        if err.is_none() && prog.cancel.load(Ordering::Relaxed) {
            err = Some("stopped".to_string());
        }
        if err.is_none() {
            *lock(&prog.status) = "Clustering voices".into();
            let mut t = (*base_run).clone();
            speakers = diarize(&audio, &mut t, &params);
            *lock(&out) = Some(t);
            prog.done.store(prog.total.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        let secs = t0.elapsed().as_secs_f64();
        *lock(&prog.status) = match &err {
            Some(e) if e == "stopped" => "Stopped: nothing was changed".into(),
            Some(e) => e.clone(),
            None => format!("Found {speakers} speaker(s) ({secs:.1}s)"),
        };
        let r = match err {
            Some(e) => Err(e),
            None => Ok(filmcraft_export::Report { path: String::new(), frames: 0, seconds: secs, bytes: 0, render_fps: 0.0, extra_files: Vec::new() }),
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
    } else {
        std::thread::Builder::new().name("filmcraft-diarize".into()).spawn(work).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    Ok(out)
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
