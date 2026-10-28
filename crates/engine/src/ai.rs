//! AI features through the optional `craft-ai` helper (feature `sidecar`, ADR 0002).
//!
//! | command | menu | state |
//! |---|---|---|
//! | `ai.status` | (none) | probes the helper; `{connected, port, version, protocol, features, reason}` |
//! | `ai.connect {port=9877}` | Clip ▸ AI | connects to the helper on another port |
//! | `ai.reframe` `ai.enhanceSpeech` `ai.stems` `ai.upscale` `ai.interpolate` `ai.generativeExtend` | Clip ▸ AI | M6a to M6f |
//! | `ai.beatGrid` `ai.musicFit` | Sequence ▸ AI | M6d |
//! | `ai.search {query}` | (Project panel search) | M6e |
//! | `ai.echo` | (none) | the protocol test kind |
//!
//! Every `ai.*` feature is disabled with a reason while no compatible helper is connected
//! ([`NOT_CONNECTED`], a protocol mismatch, a helper without the feature), like
//! `transcript.generate` without Whisper.
//!
//! A job ([`start_job`]) records a [`Fingerprint`] of its targets, asks the helper to write its
//! result files into `<media cache>/AI/<project id>/<job>/` and runs as an engine [`crate::Job`]
//! (progress, `jobs.cancel`). [`poll`] applies a finished result only if the fingerprint still
//! holds, in one undo step that is rolled back whole if any step fails ([`apply_atomic`]);
//! otherwise the result is dropped with an event and stays in the cache.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use filmcraft_project::{ClipId, ItemId, Label, Marker, MarkerId, MarkerKind, MediaRef, TrackId};
use filmcraft_sidecar::{DEFAULT_PORT, Hello, JobRequest, JobResult, Limits, SidecarError};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, bool_p, clips_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

/// Why the AI commands are disabled when no helper answers.
pub(crate) const NOT_CONNECTED: &str = "AI helper not running (start craft-ai: craft-suite/sidecar/run.sh, then Clip ▸ AI ▸ Connect AI Helper)";

/// The helper connection's state, kept by the session.
pub struct AiState {
    /// The last handshake: `Ok` = a compatible helper, `Err` = why AI commands are disabled.
    /// `None` = never asked.
    pub hello: Option<std::result::Result<Hello, String>>,
    pub port: u16,
    pub limits: Limits,
    /// Where the `AI` cache folder goes; `None` = the media cache.
    pub cache_root: Option<PathBuf>,
    /// Jobs whose results are applied when they finish.
    pub jobs: Vec<PendingAi>,
}

impl Default for AiState {
    fn default() -> Self {
        Self { hello: None, port: DEFAULT_PORT, limits: Limits::default(), cache_root: None, jobs: Vec::new() }
    }
}

/// What a target clip was when its job started.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipPrint {
    pub clip: ClipId,
    pub track: TrackId,
    pub item: ItemId,
    pub media: Option<String>,
    pub start: Tick,
    pub duration: Tick,
    pub source_in: Tick,
    pub speed_bits: u64,
    pub reverse: bool,
}

/// What a job's result depends on; [`Fingerprint::check`] tells whether it still holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    pub generation: u64,
    pub project: String,
    pub seq: ItemId,
    pub clips: Vec<ClipPrint>,
}

impl Fingerprint {
    fn capture(s: &Session, seq: ItemId, clips: &[ClipId]) -> Vec<ClipPrint> {
        let Some(q) = s.project.sequence(seq) else { return Vec::new() };
        clips
            .iter()
            .filter_map(|c| {
                let (track, it) = q.find_item(*c)?;
                let media = s.project.resolve_media(it.item).and_then(|(_, m, _)| match &m.media {
                    MediaRef::File { path } => Some(path.clone()),
                    MediaRef::Generator(_) => None,
                });
                Some(ClipPrint {
                    clip: *c,
                    track,
                    item: it.item,
                    media,
                    start: it.start,
                    duration: it.duration,
                    source_in: it.source_in,
                    speed_bits: it.speed.to_bits(),
                    reverse: it.reverse,
                })
            })
            .collect()
    }

    /// `Err(why)` when the project or a target clip changed since the job started.
    pub fn check(&self, s: &Session) -> std::result::Result<(), String> {
        if s.project_generation != self.generation || s.project.id != self.project {
            return Err("another project was opened".into());
        }
        let ids: Vec<ClipId> = self.clips.iter().map(|c| c.clip).collect();
        let now = Self::capture(s, self.seq, &ids);
        for c in &self.clips {
            match now.iter().find(|n| n.clip == c.clip) {
                None => return Err("a target clip was deleted".into()),
                Some(n) if n != c => return Err("a target clip changed".into()),
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// A running AI job.
pub struct PendingAi {
    pub job: u64,
    pub kind: &'static str,
    pub label: String,
    pub fingerprint: Fingerprint,
    pub out_dir: PathBuf,
    pub outcome: Arc<Mutex<Option<(JobResult, Vec<PathBuf>)>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The helper is connected, speaks our protocol and offers `feature`.
fn helper_ready(s: &Session, feature: &str) -> std::result::Result<(), String> {
    if s.sidecar.is_none() {
        return Err(NOT_CONNECTED.into());
    }
    match &s.ai.hello {
        None => Err(NOT_CONNECTED.into()),
        Some(Err(why)) => Err(why.clone()),
        Some(Ok(h)) if !h.has(feature) => Err(format!("the AI helper (version {}) does not provide `{feature}` yet", h.version)),
        Some(Ok(_)) => Ok(()),
    }
}

/// What a feature works on.
#[derive(Clone, Copy)]
enum Needs {
    Clips,
    Sequence,
    Project,
}

fn feature_enabled(s: &Session, feature: &str, milestone: &str, needs: Needs) -> std::result::Result<(), String> {
    helper_ready(s, feature)?;
    match needs {
        Needs::Clips => {
            has_seq(s)?;
            if s.state.selection.is_empty() {
                return Err("select a clip in the timeline".into());
            }
        }
        Needs::Sequence => has_seq(s)?,
        Needs::Project => {}
    }
    Err(format!("not built yet (arrives in {milestone})"))
}

macro_rules! feature {
    ($id:literal, $label:literal, [$($m:literal),*], $feature:literal, $milestone:literal, $needs:expr, $params:literal) => {
        CommandSpec {
            id: $id,
            label: $label,
            menu: &[$($m),*],
            shortcut: None,
            params: $params,
            enabled: |s| feature_enabled(s, $feature, $milestone, $needs),
            run: |_, _| Err(EngineError::Other(format!("{} is not built yet (arrives in {})", $label, $milestone))),
            journal: true,
        }
    };
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec { id: "ai.status", label: "AI Helper Status", menu: &[], shortcut: None, params: "{}", enabled: always, run: status, journal: false },
        CommandSpec {
            id: "ai.connect",
            label: "Connect AI Helper",
            menu: &["Clip", "AI"],
            shortcut: None,
            params: r#"{"port":1..65535=9877}"#,
            enabled: always,
            run: connect,
            journal: false,
        },
        feature!(
            "ai.reframe",
            "Auto Reframe (Tracked)…",
            ["Clip", "AI"],
            "reframe",
            "M6a",
            Needs::Clips,
            r#"{"clips":[id]?,"aspect":"9:16"?,"wait":bool=false}"#
        ),
        feature!(
            "ai.enhanceSpeech",
            "Enhance Speech…",
            ["Clip", "AI"],
            "enhanceSpeech",
            "M6c",
            Needs::Clips,
            r#"{"clips":[id]?,"strength":0..1=1,"wait":bool=false}"#
        ),
        feature!("ai.stems", "Extract Stems…", ["Clip", "AI"], "stems", "M6b", Needs::Clips, r#"{"clips":[id]?,"wait":bool=false}"#),
        feature!("ai.upscale", "Upscale…", ["Clip", "AI"], "upscale", "M6f", Needs::Clips, r#"{"clips":[id]?,"factor":2|4=2,"wait":bool=false}"#),
        feature!(
            "ai.interpolate",
            "Interpolate Frames…",
            ["Clip", "AI"],
            "interpolate",
            "M6f",
            Needs::Clips,
            r#"{"clips":[id]?,"factor":2|4=2,"wait":bool=false}"#
        ),
        feature!(
            "ai.generativeExtend",
            "Generative Extend…",
            ["Clip", "AI"],
            "generativeExtend",
            "a later milestone",
            Needs::Clips,
            r#"{"clips":[id]?,"seconds":f?}"#
        ),
        feature!("ai.beatGrid", "Add Beat Markers", ["Sequence", "AI"], "beatGrid", "M6d", Needs::Clips, r#"{"clips":[id]?,"wait":bool=false}"#),
        feature!(
            "ai.musicFit",
            "Fit Music to Length…",
            ["Sequence", "AI"],
            "musicFit",
            "M6d",
            Needs::Sequence,
            r#"{"clip":id?,"seconds":f?,"duck":bool=true}"#
        ),
        feature!("ai.search", "Search Media by Content", [], "search", "M6e", Needs::Project, r#"{"query":str}"#),
        CommandSpec {
            id: "ai.echo",
            label: "AI Helper Echo Test",
            menu: &[],
            shortcut: None,
            params: r#"{"clips":[id]?,"steps":n?,"failStep":n?,"wait":bool=false}"#,
            enabled: |s| {
                helper_ready(s, "echo")?;
                has_seq(s)?;
                if s.state.selection.is_empty() { Err("select a clip in the timeline".into()) } else { Ok(()) }
            },
            run: echo,
            journal: true,
        },
    ]
}

// ---------- connection ----------

/// Handshake again with the installed client, or (none installed) with a helper on the
/// configured port. Caches the outcome for the commands' `enabled`.
pub fn refresh(s: &mut Session) {
    if s.sidecar.is_none() {
        s.sidecar = tcp_client(s.ai.port, s.ai.limits);
    }
    let Some(client) = s.sidecar.clone() else {
        s.ai.hello = Some(Err(NOT_CONNECTED.into()));
        return;
    };
    s.ai.hello = Some(match client.hello() {
        Ok(h) => h.check().map(|_| h).map_err(|e| e.to_string()),
        Err(SidecarError::NotRunning { .. }) => Err(NOT_CONNECTED.into()),
        Err(e) => Err(e.to_string()),
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn tcp_client(port: u16, limits: Limits) -> Option<Arc<dyn filmcraft_sidecar::SidecarClient>> {
    Some(Arc::new(filmcraft_sidecar::TcpClient::new(port, limits)))
}

#[cfg(target_arch = "wasm32")]
fn tcp_client(_: u16, _: Limits) -> Option<Arc<dyn filmcraft_sidecar::SidecarClient>> {
    None
}

fn status_json(s: &Session) -> Value {
    let (h, reason) = match &s.ai.hello {
        Some(Ok(h)) => (Some(h), None),
        Some(Err(e)) => (None, Some(e.clone())),
        None => (None, Some(NOT_CONNECTED.to_string())),
    };
    json!({
        "connected": h.is_some(),
        "port": s.ai.port,
        "version": h.map(|h| h.version.clone()),
        "protocol": h.map(|h| h.protocol),
        "needProtocol": filmcraft_sidecar::PROTOCOL_VERSION,
        "features": h.map(|h| h.features.clone()).unwrap_or_default(),
        "reason": reason,
        "jobs": s.ai.jobs.len(),
    })
}

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    refresh(s);
    Ok(status_json(s))
}

fn connect(s: &mut Session, p: &Value) -> Result<Value> {
    if let Some(port) = p.get("port").filter(|v| !v.is_null()) {
        let port = port.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|n| *n > 0);
        s.ai.port = port.ok_or_else(|| bad("ai.connect", "`port` must be 1..65535"))?;
    }
    s.sidecar = None;
    refresh(s);
    match &s.ai.hello {
        Some(Err(e)) => Err(EngineError::Other(e.clone())),
        _ => Ok(status_json(s)),
    }
}

// ---------- project id and cache ----------

/// A random (version 4) UUID; no dependency, good enough to name a cache folder.
fn new_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let word = |k: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(t);
        h.write_u64(n);
        h.write_u64(k);
        h.finish()
    };
    let (a, b) = (word(0), word(1));
    let a = (a & !0xf000) | 0x4000; // version 4
    let b = (b & !(0xc << 60)) | (0x8 << 60); // RFC 4122 variant
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
}

/// The project's id, given now if it has none. The id is written into the undo and redo
/// snapshots too (once per project), so undoing past this point keeps it; the project becomes
/// modified so the id is saved.
pub fn ensure_project_id(s: &mut Session) -> String {
    if !s.project.id.is_empty() {
        return s.project.id.clone();
    }
    let id = new_uuid();
    Arc::make_mut(&mut s.project).id = id.clone();
    for (_, snap) in s.history.undo.iter_mut().chain(s.history.redo.iter_mut()) {
        Arc::make_mut(snap).id = id.clone();
    }
    s.bump();
    id
}

/// `<media cache>/AI/<project id>`.
pub fn project_cache_dir(s: &Session, project_id: &str) -> PathBuf {
    let root = s.ai.cache_root.clone().or_else(|| s.media_cache_dir()).or_else(|| crate::autosave::default_data_dir().map(|d| d.join("Media Cache")));
    root.unwrap_or_else(|| crate::temp_dir().join("FilmCraft Media Cache")).join("AI").join(project_id)
}

// ---------- jobs ----------

/// Start a `kind` job on `clips` of the active sequence (see the module docs). `wait` (and wasm)
/// runs it to the end before returning, applying the result.
pub fn start_job(s: &mut Session, kind: &'static str, label: &str, clips: &[ClipId], params: Value, wait: bool) -> Result<Value> {
    let client = s.sidecar.clone().ok_or_else(|| EngineError::Other(NOT_CONNECTED.into()))?;
    helper_ready(s, kind).map_err(EngineError::Other)?;
    let seq = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let project = ensure_project_id(s);
    let prints = Fingerprint::capture(s, seq, clips);
    if prints.is_empty() {
        return Err(EngineError::Other("none of the clips is in the open sequence".into()));
    }
    let mut inputs: Vec<String> = Vec::new();
    for m in prints.iter().filter_map(|c| c.media.clone()) {
        if !inputs.contains(&m) {
            inputs.push(m);
        }
    }
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let ms = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let out_dir = project_cache_dir(s, &project).join(format!("{kind}-{ms}-{id}"));
    std::fs::create_dir_all(&out_dir).map_err(|e| EngineError::Other(format!("AI cache {}: {e}", out_dir.display())))?;
    let req = JobRequest { kind: kind.into(), inputs, params, out_dir: out_dir.to_string_lossy().into() };
    let fingerprint = Fingerprint { generation: s.project_generation, project, seq, clips: prints };

    let job = crate::Job { id, label: label.to_string(), progress: Default::default(), result: Default::default() };
    job.progress.total.store(1000, Ordering::Relaxed);
    let outcome: Arc<Mutex<Option<(JobResult, Vec<PathBuf>)>>> = Arc::default();
    let (prog, res, out, limits) = (job.progress.clone(), job.result.clone(), outcome.clone(), s.ai.limits);
    let run = move || {
        let t0 = web_time::Instant::now();
        let cancelled = || prog.cancel.load(Ordering::Relaxed);
        let mut on_progress = |f: f64, st: &str| {
            prog.done.store((f * 1000.0) as u64, Ordering::Relaxed);
            *lock(&prog.status) = st.to_string();
        };
        let r = filmcraft_sidecar::drive(&*client, &req, &limits, &cancelled, &mut on_progress);
        let r = match r {
            Ok((result, files)) => {
                *lock(&prog.status) = format!("{} result file(s) ready", files.len());
                let bytes = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
                let extra_files = files.iter().map(|f| f.to_string_lossy().to_string()).collect();
                *lock(&out) = Some((result, files));
                Ok(filmcraft_export::Report { path: req.out_dir.clone(), frames: 0, seconds: t0.elapsed().as_secs_f64(), bytes, render_fps: 0.0, extra_files })
            }
            Err(SidecarError::Cancelled) => {
                *lock(&prog.status) = "Stopped: nothing was changed".into();
                Err("stopped".to_string())
            }
            Err(e) => {
                *lock(&prog.status) = e.to_string();
                Err(e.to_string())
            }
        };
        *lock(&res) = Some(r);
        prog.finished.store(true, Ordering::Relaxed);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    s.jobs.push(job);
    s.ai.jobs.push(PendingAi { job: id, kind, label: label.to_string(), fingerprint, out_dir: out_dir.clone(), outcome });
    if wait || cfg!(target_arch = "wasm32") {
        run();
        poll(s);
    } else {
        std::thread::Builder::new().name(format!("filmcraft-ai-{kind}")).spawn(run).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    if let Some(Err(e)) = s.jobs.iter().find(|j| j.id == id).and_then(|j| lock(&j.result).clone()) {
        return Err(EngineError::Other(e));
    }
    Ok(json!({"job": id, "outDir": out_dir.to_string_lossy()}))
}

/// Apply finished jobs whose fingerprint still holds (one undo step each), drop the others with
/// an event. Called once per UI frame from [`Session::poll_persistence`] and after `wait` runs.
pub fn poll(s: &mut Session) {
    let mut i = 0;
    while i < s.ai.jobs.len() {
        let job = s.jobs.iter().find(|j| j.id == s.ai.jobs[i].job);
        let finished = job.is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        let cancelled = job.is_some_and(|j| j.progress.cancel.load(Ordering::Relaxed));
        if !finished {
            i += 1;
            continue;
        }
        let pj = s.ai.jobs.remove(i);
        let Some((result, files)) = lock(&pj.outcome).take().filter(|_| !cancelled) else { continue };
        if let Err(why) = pj.fingerprint.check(s) {
            let message = format!("{}: {why} while the AI job ran; the result was not applied (kept in {})", pj.label, pj.out_dir.display());
            s.log.push(crate::panels::Level::Warning, "ai", message.clone());
            s.events.push(crate::Event::Toast { message, error: false });
            continue;
        }
        if let Err(e) = apply(s, &pj, &result, &files) {
            s.error_toast("ai", format!("{}: {e}; nothing was changed", pj.label));
        }
    }
}

/// One step of applying a result.
pub type Step = Box<dyn FnOnce(&mut Session) -> Result<()>>;

/// Run `steps` as one undo step named `label`: if any step fails, the project and its history
/// return to how they were before the first one.
pub fn apply_atomic(s: &mut Session, label: &str, steps: Vec<Step>) -> Result<()> {
    s.edit_group(label, |s| {
        for step in steps {
            step(s)?;
        }
        Ok(())
    })
}

fn apply(s: &mut Session, pj: &PendingAi, result: &JobResult, files: &[PathBuf]) -> Result<()> {
    match pj.kind {
        "echo" => apply_echo(s, &pj.fingerprint, result, files),
        k => Err(EngineError::Other(format!("results of `{k}` jobs cannot be applied yet"))),
    }
}

// ---------- echo (the protocol test kind) ----------

fn echo(s: &mut Session, p: &Value) -> Result<Value> {
    let clips = clips_p(s, p);
    let mut params = json!({});
    for k in ["steps", "failStep"] {
        if let Some(n) = u64_p(p, k) {
            params[k] = json!(n);
        }
    }
    start_job(s, "echo", "AI Echo", &clips, params, bool_p(p, "wait").unwrap_or(false))
}

/// Echo results: (1) a sequence marker at each target clip, (2) the result file's name in each
/// target item's metadata. `failStep: 2` in the helper's meta fails step 2 (tests of
/// [`apply_atomic`]).
fn apply_echo(s: &mut Session, fp: &Fingerprint, result: &JobResult, files: &[PathBuf]) -> Result<()> {
    let seq = fp.seq;
    let starts: Vec<Tick> = fp.clips.iter().map(|c| c.start).collect();
    let items: Vec<ItemId> = fp.clips.iter().map(|c| c.item).collect();
    let name = files.first().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let fail = result.meta.get("failStep").and_then(Value::as_u64) == Some(2);
    let mark: Step = Box::new(move |s: &mut Session| {
        s.edit("AI Echo", move |pr, _| {
            let ids: Vec<u64> = starts.iter().map(|_| pr.alloc_id()).collect();
            let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            for (t, id) in starts.iter().zip(ids) {
                q.markers.push(Marker {
                    id: MarkerId(id),
                    start: *t,
                    duration: Tick::ZERO,
                    name: "AI Echo".into(),
                    comment: String::new(),
                    kind: MarkerKind::Comment,
                    color: Label::Green,
                });
            }
            q.markers.sort_by_key(|m| m.start);
            Ok(())
        })
    });
    let note: Step = Box::new(move |s: &mut Session| {
        if fail {
            return Err(EngineError::Other("injected failure in step 2".into()));
        }
        s.edit("AI Echo", move |pr, _| {
            for i in &items {
                if let Some(it) = pr.item_mut(*i) {
                    it.metadata.insert("AI Echo".into(), name.clone());
                }
            }
            Ok(())
        })
    });
    apply_atomic(s, "AI Echo", vec![mark, note])
}

#[cfg(test)]
#[path = "ai_tests.rs"]
mod tests;
