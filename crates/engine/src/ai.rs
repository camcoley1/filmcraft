//! AI features through the optional `craft-ai` helper (feature `sidecar`, ADR 0002).
//!
//! | command | menu | state |
//! |---|---|---|
//! | `ai.status` | (none) | probes the helper; `{connected, probing, port, version, protocol, features, reason}` |
//! | `ai.connect {port=9877}` | Clip ▸ AI | connects to the helper on another port |
//! | `ai.reframe` `ai.enhanceSpeech` `ai.stems` `ai.upscale` `ai.interpolate` `ai.generativeExtend` | Clip ▸ AI | M6a to M6f |
//! | `ai.beatGrid` `ai.musicFit` | Sequence ▸ AI | M6d |
//! | `ai.search {query}` | (Project panel search) | M6e |
//! | `ai.echo` | (none) | the protocol test kind |
//!
//! Every `ai.*` feature is disabled with a reason while no compatible helper is connected
//! ([`NOT_CONNECTED`], a protocol mismatch, a helper without the feature), like
//! `transcript.generate` without Whisper. `enabled()` only reads the cached handshake; in a host
//! that must not block ([`AiState::background`], the desktop app) the handshake itself runs on a
//! worker thread and `wait` is ignored, so no socket I/O ever happens on the UI thread.
//!
//! A job ([`start_job`]) records a [`Fingerprint`] of its targets, asks the helper to write its
//! result files into `<media cache>/AI/<project id>/<job>/` and runs as an engine [`crate::Job`]
//! (progress, `jobs.cancel`). The job stays unfinished until [`poll`] has applied the result: only
//! if the fingerprint still holds, never during a gesture or dynamic trim, in one undo step that is
//! rolled back whole if any step fails ([`apply_atomic`]). A stale or failed apply fails the job
//! with the reason and leaves the files in the cache, which is kept under [`AiState::cache_cap`].

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filmcraft_project::{ClipId, ItemId, Label, Marker, MarkerId, MarkerKind, TrackId};
use filmcraft_sidecar::{DEFAULT_PORT, Hello, JobRequest, JobResult, Limits, SidecarClient, SidecarError};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, bool_p, clips_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

/// Why the AI commands are disabled when no helper answers.
pub(crate) const NOT_CONNECTED: &str = "AI helper not running (start craft-ai: craft-suite/sidecar/run.sh, then Clip ▸ AI ▸ Connect AI Helper)";

/// Finished job folders younger than this are never pruned (a job of another window).
const PRUNE_MIN_AGE: Duration = Duration::from_secs(600);

/// The helper connection's state, kept by the session.
pub struct AiState {
    /// The last handshake: `Ok` = a compatible helper, `Err` = why AI commands are disabled.
    /// `None` = never asked.
    pub hello: Option<std::result::Result<Hello, String>>,
    pub port: u16,
    pub limits: Limits,
    /// Where the `AI` cache folder goes; `None` = the media cache.
    pub cache_root: Option<PathBuf>,
    /// Size the AI cache may take; beyond it the oldest finished job folders are deleted when a
    /// job starts.
    pub cache_cap: u64,
    /// The host must never wait on the helper (the desktop app, whose session lives on the UI
    /// thread): handshakes run on a worker thread and `wait` is ignored.
    pub background: bool,
    /// A handshake running on a worker thread.
    pub probe: Option<Probe>,
    /// The id this session gave (or checked on) the project, with its project generation: an undo
    /// to a state from before it puts it back.
    pub stamp: Option<(u64, String)>,
    /// Jobs whose results are applied when they finish.
    pub jobs: Vec<PendingAi>,
}

impl Default for AiState {
    fn default() -> Self {
        Self {
            hello: None,
            port: DEFAULT_PORT,
            limits: Limits::default(),
            cache_root: None,
            cache_cap: 32 << 30,
            background: false,
            probe: None,
            stamp: None,
            jobs: Vec::new(),
        }
    }
}

/// A background handshake; `announce` = say how it went (it came from `ai.connect`).
pub struct Probe {
    result: Arc<Mutex<Option<std::result::Result<Hello, String>>>>,
    announce: bool,
}

/// What a target clip was when its job started.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipPrint {
    pub clip: ClipId,
    pub track: TrackId,
    pub item: ItemId,
    pub media: Option<String>,
    /// Hash of the whole clip (placement, source range, speed, effects, enabled…) and of the media
    /// it shows (file, subclip range, interpretation).
    pub state: u64,
}

/// What a job's result depends on; [`Fingerprint::check`] tells whether it still holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    pub generation: u64,
    pub project: String,
    pub seq: ItemId,
    /// Hash of the sequence settings (frame size, rate…).
    pub settings: u64,
    pub clips: Vec<ClipPrint>,
}

fn hash_json<T: serde::Serialize + ?Sized>(v: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(v).unwrap_or_default().hash(&mut h);
    h.finish()
}

impl Fingerprint {
    fn capture(s: &Session, seq: ItemId, clips: &[ClipId]) -> Vec<ClipPrint> {
        let Some(q) = s.project.sequence(seq) else { return Vec::new() };
        clips
            .iter()
            .filter_map(|c| {
                let (track, it) = q.find_item(*c)?;
                let media = s.project.resolve_media(it.item).map(|(_, m, range)| (&m.media, &m.interpret, range));
                let path = media.and_then(|(m, _, _)| match m {
                    filmcraft_project::MediaRef::File { path } => Some(path.clone()),
                    filmcraft_project::MediaRef::Generator(_) => None,
                });
                Some(ClipPrint { clip: *c, track, item: it.item, media: path, state: hash_json(&(it, media)) })
            })
            .collect()
    }

    /// `Err(why)` when the project, the sequence or a target clip changed since the job started,
    /// or a target track is locked now.
    pub fn check(&self, s: &Session) -> std::result::Result<(), String> {
        if s.project_generation != self.generation || s.project.id != self.project {
            return Err("another project was opened".into());
        }
        let Some(q) = s.project.sequence(self.seq) else { return Err("the sequence was deleted".into()) };
        if hash_json(&q.settings) != self.settings {
            return Err("the sequence settings changed".into());
        }
        let ids: Vec<ClipId> = self.clips.iter().map(|c| c.clip).collect();
        let now = Self::capture(s, self.seq, &ids);
        for c in &self.clips {
            match now.iter().find(|n| n.clip == c.clip) {
                None => return Err("a target clip was deleted".into()),
                Some(n) if n != c => return Err("a target clip changed".into()),
                Some(_) => {}
            }
            if q.track(c.track).is_some_and(|t| t.locked) {
                return Err("a target track is locked".into());
            }
        }
        Ok(())
    }
}

/// A job's checked result, waiting to be applied.
pub struct Ready {
    pub result: JobResult,
    pub files: Vec<PathBuf>,
    pub report: filmcraft_export::Report,
}

/// A running AI job.
pub struct PendingAi {
    pub job: u64,
    pub kind: &'static str,
    pub label: String,
    pub fingerprint: Fingerprint,
    pub out_dir: PathBuf,
    pub outcome: Arc<Mutex<Option<Ready>>>,
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
/// configured port, and cache the outcome for the commands' `enabled`. With
/// [`AiState::background`] it runs on a worker thread and [`poll`] picks it up.
pub fn refresh(s: &mut Session, announce: bool) {
    if s.sidecar.is_none() {
        s.sidecar = tcp_client(s.ai.port, s.ai.limits);
    }
    let Some(client) = s.sidecar.clone() else {
        s.ai.hello = Some(Err(NOT_CONNECTED.into()));
        return;
    };
    if !s.ai.background {
        s.ai.hello = Some(handshake(&*client));
        return;
    }
    if let Some(p) = s.ai.probe.as_mut() {
        p.announce |= announce;
        return;
    }
    let result: Arc<Mutex<Option<std::result::Result<Hello, String>>>> = Arc::default();
    let out = result.clone();
    let run = move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handshake(&*client)));
        *lock(&out) = Some(r.unwrap_or_else(|_| Err("internal error while contacting the AI helper (see the crash log)".into())));
    };
    match std::thread::Builder::new().name("filmcraft-ai-probe".into()).spawn(run) {
        Ok(_) => s.ai.probe = Some(Probe { result, announce }),
        Err(e) => s.ai.hello = Some(Err(format!("cannot contact the AI helper: {e}"))),
    }
}

fn handshake(c: &dyn SidecarClient) -> std::result::Result<Hello, String> {
    match c.hello() {
        Ok(h) => h.check().map(|_| h).map_err(|e| e.to_string()),
        Err(SidecarError::NotRunning { .. }) => Err(NOT_CONNECTED.into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Take a finished background handshake.
fn poll_probe(s: &mut Session) {
    let Some(p) = &s.ai.probe else { return };
    let Some(r) = lock(&p.result).take() else { return };
    let announce = p.announce;
    s.ai.probe = None;
    if announce {
        match &r {
            Ok(h) => s.toast(format!("AI helper connected (version {})", h.version)),
            Err(e) => s.error_toast("ai.connect", e.clone()),
        }
    }
    s.ai.hello = Some(r);
}

#[cfg(not(target_arch = "wasm32"))]
fn tcp_client(port: u16, limits: Limits) -> Option<Arc<dyn SidecarClient>> {
    Some(Arc::new(filmcraft_sidecar::TcpClient::new(port, limits)))
}

#[cfg(target_arch = "wasm32")]
fn tcp_client(_: u16, _: Limits) -> Option<Arc<dyn SidecarClient>> {
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
        "probing": s.ai.probe.is_some(),
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
    refresh(s, false);
    Ok(status_json(s))
}

fn connect(s: &mut Session, p: &Value) -> Result<Value> {
    if let Some(port) = p.get("port").filter(|v| !v.is_null()) {
        let port = port.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|n| *n > 0);
        s.ai.port = port.ok_or_else(|| bad("ai.connect", "`port` must be 1..65535"))?;
    }
    s.sidecar = None;
    s.ai.probe = None;
    refresh(s, true);
    match &s.ai.hello {
        Some(Err(e)) if s.ai.probe.is_none() => Err(EngineError::Other(e.clone())),
        _ => Ok(status_json(s)),
    }
}

// ---------- project id and cache ----------

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in hex digits: the only project id used as a folder name.
pub fn is_uuid(id: &str) -> bool {
    id.len() == 36 && id.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() })
}

/// A random (version 4) UUID; no dependency, good enough to name a cache folder.
fn new_uuid() -> String {
    use std::hash::BuildHasher;
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

/// The project's id. A project without one, or with anything but a UUID (a hand-edited or
/// hostile file), gets a new one now, and the project becomes modified so it is saved. The
/// session remembers it, so undoing to a state from before keeps it ([`restamp`]); the undo
/// snapshots are not rewritten.
pub fn ensure_project_id(s: &mut Session) -> String {
    let id = if is_uuid(&s.project.id) {
        s.project.id.clone()
    } else {
        let id = new_uuid();
        Arc::make_mut(&mut s.project).id = id.clone();
        s.bump();
        id
    };
    s.ai.stamp = Some((s.project_generation, id.clone()));
    id
}

/// Put the session's project id back after undo / redo restored a snapshot from before it.
pub(crate) fn restamp(s: &mut Session) {
    if let Some((g, id)) = &s.ai.stamp
        && *g == s.project_generation
        && s.project.id != *id
    {
        let id = id.clone();
        Arc::make_mut(&mut s.project).id = id;
    }
}

fn cache_root(s: &Session) -> PathBuf {
    let root = s.ai.cache_root.clone().or_else(|| s.media_cache_dir()).or_else(|| crate::autosave::default_data_dir().map(|d| d.join("Media Cache")));
    root.unwrap_or_else(|| crate::temp_dir().join("FilmCraft Media Cache"))
}

/// `<media cache>/AI/<project id>`; refused unless `project_id` is a UUID (it comes from the project
/// file) and the folder lies directly inside `<media cache>/AI`.
pub fn project_cache_dir(s: &Session, project_id: &str) -> Result<PathBuf> {
    if !is_uuid(project_id) {
        return Err(EngineError::Other(format!("project id `{}` is not a UUID", project_id.chars().take(40).collect::<String>())));
    }
    let ai = cache_root(s).join("AI");
    let dir = ai.join(project_id);
    if dir.parent() != Some(ai.as_path()) {
        return Err(EngineError::Other("the project's AI cache folder is outside the cache".into()));
    }
    Ok(dir)
}

/// Bytes in the files under `dir` (symbolic links not followed), bounded in depth and entries.
fn dir_size(dir: &Path, depth: u32, budget: &mut u32) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0u64;
    for e in rd.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        if m.is_file() {
            n = n.saturating_add(m.len());
        } else if m.is_dir() && depth > 0 {
            n = n.saturating_add(dir_size(&e.path(), depth - 1, budget));
        }
    }
    n
}

/// Keep the AI cache under `cap` bytes: delete the oldest job folders (`<ai root>/<project>/<job>/`)
/// first, never one in `keep` (jobs still running or waiting to be applied) nor one changed within
/// `min_age`. Returns how many were deleted.
pub fn prune_cache(ai_root: &Path, cap: u64, keep: &[PathBuf], min_age: Duration) -> usize {
    let dirs = |p: &Path| -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
        rd.flatten().take(10_000).map(|e| e.path()).filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())).collect()
    };
    let mut jobs = Vec::new();
    let mut total = 0u64;
    let mut budget = 200_000u32;
    for project in dirs(ai_root) {
        for job in dirs(&project) {
            let size = dir_size(&job, 4, &mut budget);
            let modified = std::fs::metadata(&job).and_then(|m| m.modified()).ok();
            total = total.saturating_add(size);
            jobs.push((modified, size, job));
        }
    }
    if total <= cap {
        return 0;
    }
    jobs.sort_by_key(|j| j.0);
    let now = std::time::SystemTime::now();
    let mut deleted = 0;
    for (modified, size, job) in jobs {
        if total <= cap {
            break;
        }
        let young = modified.is_none_or(|t| now.duration_since(t).unwrap_or_default() < min_age);
        if young || keep.contains(&job) {
            continue;
        }
        if std::fs::remove_dir_all(&job).is_ok() {
            total = total.saturating_sub(size);
            deleted += 1;
        }
    }
    deleted
}

// ---------- jobs ----------

/// Start a `kind` job on `clips` of the active sequence (see the module docs). `wait` runs it to
/// the end, result applied, before returning (not in a [`AiState::background`] host).
pub fn start_job(s: &mut Session, kind: &'static str, label: &str, clips: &[ClipId], params: Value, wait: bool) -> Result<Value> {
    let client = s.sidecar.clone().ok_or_else(|| EngineError::Other(NOT_CONNECTED.into()))?;
    helper_ready(s, kind).map_err(EngineError::Other)?;
    let wait = wait && !s.ai.background;
    let seq = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let prints = Fingerprint::capture(s, seq, clips);
    if prints.is_empty() {
        return Err(EngineError::Other("none of the clips is in the open sequence".into()));
    }
    let project = ensure_project_id(s);
    let mut inputs: Vec<String> = Vec::new();
    for m in prints.iter().filter_map(|c| c.media.clone()) {
        if !inputs.contains(&m) {
            inputs.push(m);
        }
    }
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let ms = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let out_dir = project_cache_dir(s, &project)?.join(format!("{kind}-{ms}-{id}"));
    std::fs::create_dir_all(&out_dir).map_err(|e| EngineError::Other(format!("AI cache {}: {e}", out_dir.display())))?;
    let req = JobRequest { kind: kind.into(), inputs, params, out_dir: out_dir.to_string_lossy().into() };
    let settings = s.project.sequence(seq).map(|q| hash_json(&q.settings)).unwrap_or_default();
    let fingerprint = Fingerprint { generation: s.project_generation, project, seq, settings, clips: prints };
    let keep: Vec<PathBuf> = s.ai.jobs.iter().map(|j| j.out_dir.clone()).chain([out_dir.clone()]).collect();
    let (ai_root, cap) = (cache_root(s).join("AI"), s.ai.cache_cap);

    let job = crate::Job { id, label: label.to_string(), progress: Default::default(), result: Default::default() };
    job.progress.total.store(1000, Ordering::Relaxed);
    let outcome: Arc<Mutex<Option<Ready>>> = Arc::default();
    let (prog, res, out, limits) = (job.progress.clone(), job.result.clone(), outcome.clone(), s.ai.limits);
    let run = move || {
        prune_cache(&ai_root, cap, &keep, PRUNE_MIN_AGE);
        let t0 = web_time::Instant::now();
        let cancelled = || prog.cancel.load(Ordering::Relaxed);
        let mut on_progress = |f: f64, st: &str| {
            prog.done.store((f * 1000.0) as u64, Ordering::Relaxed);
            *lock(&prog.status) = st.to_string();
        };
        match filmcraft_sidecar::drive(&*client, &req, &limits, &cancelled, &mut on_progress) {
            // finished by `poll` once the result is applied (or dropped)
            Ok((result, files)) => {
                *lock(&prog.status) = format!("{} result file(s) ready; applying", files.len());
                let bytes = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
                let extra_files = files.iter().map(|f| f.to_string_lossy().to_string()).collect();
                let report =
                    filmcraft_export::Report { path: req.out_dir.clone(), frames: 0, seconds: t0.elapsed().as_secs_f64(), bytes, render_fps: 0.0, extra_files };
                *lock(&out) = Some(Ready { result, files, report });
                return;
            }
            Err(SidecarError::Cancelled) => {
                *lock(&prog.status) = "Stopped: nothing was changed".into();
                *lock(&res) = Some(Err("stopped".to_string()));
            }
            Err(e) => {
                *lock(&prog.status) = e.to_string();
                *lock(&res) = Some(Err(e.to_string()));
            }
        }
        prog.finished.store(true, Ordering::Relaxed);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    let (prog, res) = (job.progress.clone(), job.result.clone());
    s.jobs.push(job);
    s.ai.jobs.push(PendingAi { job: id, kind, label: label.to_string(), fingerprint, out_dir: out_dir.clone(), outcome });
    if wait || cfg!(target_arch = "wasm32") {
        run();
        poll(s);
    } else if let Err(e) = std::thread::Builder::new().name(format!("filmcraft-ai-{kind}")).spawn(run) {
        // no worker: the job must not stay "running" forever
        let msg = format!("cannot start the job: {e}");
        *lock(&prog.status) = msg.clone();
        *lock(&res) = Some(Err(msg.clone()));
        prog.finished.store(true, Ordering::Relaxed);
        s.ai.jobs.retain(|j| j.job != id);
        return Err(EngineError::Other(msg));
    }
    let out = json!({"job": id, "outDir": out_dir.to_string_lossy(), "background": !wait});
    match lock(&res).clone() {
        Some(Err(e)) => Err(EngineError::Other(e)),
        Some(Ok(_)) => Ok(json!({"job": id, "outDir": out_dir.to_string_lossy(), "applied": true})),
        None => Ok(out),
    }
}

/// Apply results that are ready, one undo step each, and finish their jobs; a result whose
/// fingerprint no longer holds fails its job with the reason. Results wait while a gesture or a
/// dynamic trim is in progress. Jobs of a project that was replaced are cancelled. Called once
/// per UI frame from [`Session::poll_persistence`] and after `wait` runs.
pub fn poll(s: &mut Session) {
    poll_probe(s);
    for pj in &s.ai.jobs {
        if pj.fingerprint.generation != s.project_generation
            && let Some(j) = s.jobs.iter().find(|j| j.id == pj.job)
        {
            j.progress.cancel.store(true, Ordering::Relaxed);
        }
    }
    let busy = s.gesture_in_progress || s.trim_play.dynamic.is_some();
    let mut i = 0;
    while let Some(pj) = s.ai.jobs.get(i) {
        let Some(job) = s.jobs.iter().find(|j| j.id == pj.job).cloned() else {
            s.ai.jobs.remove(i);
            continue;
        };
        // failed or stopped in the worker: nothing to apply
        if lock(&job.result).is_some() {
            s.ai.jobs.remove(i);
            continue;
        }
        let cancelled = job.progress.cancel.load(Ordering::Relaxed);
        if lock(&pj.outcome).is_none() || (busy && !cancelled) {
            i += 1;
            continue;
        }
        let pj = s.ai.jobs.remove(i);
        let Some(ready) = lock(&pj.outcome).take() else { continue };
        let kept = format!("the result was not applied (kept in {})", pj.out_dir.display());
        let r = if cancelled {
            let m = format!("stopped; {kept}");
            s.log.push(crate::panels::Level::Warning, "ai", format!("{}: {m}", pj.label));
            Err(m)
        } else if let Err(why) = pj.fingerprint.check(s) {
            let m = format!("{why} while the AI job ran; {kept}");
            let message = format!("{}: {m}", pj.label);
            s.log.push(crate::panels::Level::Warning, "ai", message.clone());
            s.events.push(crate::Event::Toast { message, error: false });
            Err(m)
        } else {
            match apply(s, &pj, &ready.result, &ready.files) {
                Ok(()) => Ok(ready.report),
                Err(e) => {
                    let m = format!("{e}; nothing was changed");
                    s.error_toast("ai", format!("{}: {m}", pj.label));
                    Err(m)
                }
            }
        };
        *lock(&job.progress.status) = match &r {
            Ok(_) => "Applied".into(),
            Err(e) => e.clone(),
        };
        *lock(&job.result) = Some(r);
        job.progress.finished.store(true, Ordering::Relaxed);
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
    let q = s.project.sequence(seq).ok_or(EngineError::NoSequence)?;
    let starts: Vec<Tick> = fp.clips.iter().filter_map(|c| q.find_item(c.clip).map(|(_, it)| it.start)).collect();
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
