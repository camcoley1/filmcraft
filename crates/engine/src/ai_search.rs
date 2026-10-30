//! `ai.indexProject` and `ai.search` (M6e): find media by what it shows and says.
//!
//! **Index.** `ai.indexProject {items?, intervalSeconds=2, maxFrames=1800, wait}` asks the helper
//! (`searchIndex`) to sample frames of the project's video and still files (one per interval, a
//! short clip gets one in the middle) and embed them with OpenAI CLIP ViT-B/32. The index lives in
//! `<media cache>/AI Index/<project id>[-<project file hash>]/` (a copy of a project that keeps its
//! id has its own folder once it is saved elsewhere): `index.json` (per item: its fingerprint, the
//! sampling it was made with, the file names) and per item a pair `i_<item>_<n>.f16` (frames x 512
//! half floats) and `.json` (the frames' times). A new pair is written under a new `n` and
//! `index.json` is switched to it with one rename, so a search never sees half of a replacement;
//! files `index.json` does not name are deleted. The folder is outside the AI job cache, so pruning
//! never takes it. Incremental: an item is embedded only when it has no entry or its fingerprint
//! (path, size, modification time in nanoseconds, duration) or the sampling (interval, frames per
//! file, model) differs; an item that could not be read is recorded as failed under its
//! fingerprint and not tried again until it changes; an item the helper only put off (its file
//! missing for the moment, the job's frame budget) is not recorded and counts in `remaining`;
//! entries of items that left the project are deleted. One job takes at most [`ITEMS_PER_JOB`]
//! items and [`JOB_FRAMES`] frames (`remaining` says how many are left: run it again), and the
//! index at most [`MAX_ENTRIES`] items and [`MAX_INDEX_FRAMES`] frames (what does not fit is
//! reported in `skipped`). Only one indexing job runs at a time.
//!
//! **Search.** `ai.search {query, limit=20, wait}` finds transcript matches here (a word or phrase,
//! case and punctuation ignored: kind `transcript`, score 1) and asks the helper (`search`) to embed
//! the query with CLIP's text encoder and rank the indexed frames (kind `visual`). Hits are items
//! with a media time; transcript and picture hits share the list (neither fills it alone) and
//! pictures below [`MIN_SCORE`] are left out. A hit of an item that changed since it was indexed is
//! marked `stale`. Transcript matching needs no helper. A newer search replaces an older one: the
//! older job is stopped and its answer, or failure, is ignored. In a host that must not block (the
//! app) the visual part arrives later (`pending` in the state).
//!
//! **Threads.** Everything that touches the disk (reading and parsing the index, fingerprinting
//! files, moving result files, writing the manifest) runs on the job's thread; the session thread
//! only starts jobs and swaps finished results in.
//!
//! The index is read-only for the project: nothing here changes the project or its history (an
//! index of a project without an id first gives it one).

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filmcraft_project::{ItemId, ItemKind, MediaRef};
use filmcraft_sidecar::{JobRequest, JobResult, Limits, SidecarClient, SidecarError};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::ai::{HEARTBEAT, HEARTBEAT_EVERY, PRUNE_MIN_AGE, cache_root, ensure_project_id, helper_ready, is_uuid, lock, project_cache_dir, prune_cache};
use crate::commands::{CommandSpec, bad, bool_p, f64_p, str_p, u64_p};
use crate::search_results::{SearchHit, SearchResults};
use crate::{EngineError, Result, Session};

/// Items one indexing job takes.
pub const ITEMS_PER_JOB: usize = 60;
/// Frames one indexing job takes (the helper's own limit).
pub const JOB_FRAMES: u64 = 50_000;
/// Largest `index.json` read.
const MAX_INDEX_JSON: u64 = 32 << 20;
/// Most items an index holds (the helper reads no more).
pub const MAX_ENTRIES: usize = 4000;
/// Most frames an index holds (the helper reads at most 2 GiB of half floats: 1 KiB a frame).
pub const MAX_INDEX_FRAMES: u64 = (2 << 30) / 1024;
/// Longest query.
const MAX_QUERY: usize = 200;
/// Transcript hits kept per item.
const PER_ITEM_TRANSCRIPT: usize = 5;
/// Pictures scoring below this are noise (matches score about 0.28 and up).
pub const MIN_SCORE: f32 = 0.22;
/// The model the index is made with (part of the sampling key).
const MODEL: &str = "ViT-B-32-openai";
/// How long a search may take.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "ai.indexProject",
            label: "Index Project for Search",
            menu: &["Clip", "AI"],
            shortcut: None,
            params: r#"{"items":[id]?,"intervalSeconds":0.25..60=2,"maxFrames":1..5000=1800,"wait":bool=false}"#,
            enabled: |s| {
                helper_ready(s, "searchIndex")?;
                if s.ai_search.pending.iter().any(|p| matches!(p.ctx, Ctx::Index)) {
                    return Err("the project is being indexed".into());
                }
                if candidates(s, None).is_empty() { Err("the project has no video or image files to index".into()) } else { Ok(()) }
            },
            run: index_project,
            journal: false,
        },
        CommandSpec {
            id: "ai.search",
            label: "Search Media by Content",
            menu: &[],
            shortcut: None,
            params: r#"{"query":str,"limit":1..100=20,"wait":bool=false}"#,
            enabled: |_| Ok(()),
            run: search,
            journal: false,
        },
    ]
}

/// AI search jobs in flight.
#[derive(Default)]
pub struct State {
    pending: Vec<Pending>,
    /// The job whose answer the Project panel is waiting for; answers of others are dropped.
    current_search: u64,
    /// What the last finished indexing job did (for a caller that waited for it).
    last_index: Option<Value>,
}

impl State {
    /// No search or index job is waiting to be applied.
    pub fn pending_is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

enum Ctx {
    Index,
    Search { query: String, limit: usize, transcript: Vec<SearchHit> },
}

/// What a job's thread hands back.
enum Done {
    Index(Value),
    Search(SearchDone),
}

struct SearchDone {
    visual: Vec<SearchHit>,
    indexed: usize,
    stale_items: usize,
    note: Option<String>,
    dedupe: f64,
}

/// Why a job's thread stopped.
enum Stop {
    Cancelled,
    Msg(String),
}

impl From<SidecarError> for Stop {
    fn from(e: SidecarError) -> Self {
        match e {
            SidecarError::Cancelled => Stop::Cancelled,
            e => Stop::Msg(e.to_string()),
        }
    }
}

struct Pending {
    job: u64,
    generation: u64,
    project: String,
    out_dir: PathBuf,
    ctx: Ctx,
    outcome: Arc<Mutex<Option<Done>>>,
}

/// What a job's thread drives the helper with.
struct Runner {
    client: Arc<dyn SidecarClient>,
    limits: Limits,
    cancelled: Box<dyn Fn() -> bool + Send>,
    progress: Box<dyn FnMut(f64, &str) + Send>,
}

impl Runner {
    fn drive(&mut self, req: &JobRequest, timeout: Duration) -> std::result::Result<(JobResult, Vec<PathBuf>), Stop> {
        let limits = Limits { timeout: timeout.min(self.limits.timeout), ..self.limits };
        Ok(filmcraft_sidecar::drive(&*self.client, req, &limits, &*self.cancelled, &mut *self.progress)?)
    }

    fn report(&mut self, f: f64, status: &str) {
        (self.progress)(f, status);
    }

    fn stopped(&self) -> bool {
        (self.cancelled)()
    }
}

/// A media item that can be indexed (what the project says; nothing is read from disk to make it).
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    item: u64,
    path: String,
    duration: Tick,
}

/// A candidate with what its file looked like when the job started.
#[derive(Clone, Debug, PartialEq)]
struct Target {
    item: u64,
    path: String,
    duration: Tick,
    fingerprint: String,
}

/// The files of a project that can be indexed (and, with `only`, just those items). No disk access.
fn candidates(s: &Session, only: Option<&[ItemId]>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (id, it) in &s.project.items {
        if only.is_some_and(|o| !o.contains(id)) {
            continue;
        }
        let ItemKind::Media(m) = &it.kind else { continue };
        let MediaRef::File { path } = &m.media else { continue };
        if m.offline || !m.info.has_video() || m.info.kind == filmcraft_media::MediaKind::ImageSequence {
            continue;
        }
        out.push(Candidate { item: id.0, path: path.clone(), duration: m.info.duration });
    }
    out
}

/// What identifies a file's content for the index: its path, size, modification time (nanoseconds)
/// and the duration the project knows. `None`: the file is not there. Reads the disk.
fn fingerprint(path: &str, duration: Tick) -> Option<String> {
    let m = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let nanos = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    Some(format!("{path}\u{1f}{}:{nanos}:{}", m.len(), duration.0))
}

fn targets_of(cands: &[Candidate]) -> Vec<Target> {
    cands
        .iter()
        .filter_map(|c| fingerprint(&c.path, c.duration).map(|fingerprint| Target { item: c.item, path: c.path.clone(), duration: c.duration, fingerprint }))
        .collect()
}

/// What the index was made with, besides the file.
fn sampling_key(interval: f64, max_frames: u64) -> String {
    format!("{interval}:{max_frames}:{MODEL}")
}

/// Frames the helper will take from a file of `duration` (its own rule, rounded up).
fn estimate_frames(duration: Tick, interval: f64, max_frames: u64) -> u64 {
    let secs = duration.seconds();
    if !secs.is_finite() || secs <= 0.0 {
        return 1;
    }
    let iv = interval.max(secs / max_frames.max(1) as f64);
    if secs <= iv { 1 } else { (((secs - 0.001 - iv / 2.0) / iv).ceil().max(1.0) as u64).min(max_frames) }
}

// ---------- the index on disk ----------

#[derive(Clone, Debug, Default, PartialEq)]
struct Entry {
    item: u64,
    /// The file as it was indexed.
    fingerprint: String,
    /// The interval, frames per file and model it was indexed with.
    sampling: String,
    frames: u64,
    /// Which pair of files holds the frames.
    ver: u64,
    error: Option<String>,
}

/// FNV-1a of a project file's path: copies of a project (same id) keep apart.
fn path_hash(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

fn index_dir_of(s: &Session, project: &str) -> Result<PathBuf> {
    // the AI cache folder's name check (a UUID) also guards this one
    project_cache_dir(s, project)?;
    let name = match s.path.as_deref() {
        Some(p) => format!("{project}-{:016x}", path_hash(p)),
        None => project.to_string(),
    };
    Ok(cache_root(s).join("AI Index").join(name))
}

/// `i_<item>_<n>` for item ids and versions (digits only: no path can come out of a name).
fn stem(item: u64, ver: u64) -> String {
    format!("i_{item}_{ver}")
}

/// A JSON file of at most `cap` bytes (checked before and while reading).
fn read_json(path: &Path, cap: u64) -> Option<Value> {
    let f = std::fs::File::open(path).ok()?;
    if f.metadata().ok()?.len() > cap {
        return None;
    }
    let mut buf = Vec::new();
    f.take(cap + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > cap { None } else { serde_json::from_slice(&buf).ok() }
}

/// Every entry of `index.json` (items and failed ones), read once.
fn read_index(dir: &Path) -> BTreeMap<u64, Entry> {
    let Some(doc) = read_json(&dir.join("index.json"), MAX_INDEX_JSON) else { return BTreeMap::new() };
    if doc["version"] != 1 {
        return BTreeMap::new();
    }
    let mut out = BTreeMap::new();
    for (key, listed) in [("items", true), ("failed", false)] {
        for e in doc[key].as_array().into_iter().flatten().take(MAX_ENTRIES) {
            let (Some(item), Some(fingerprint)) = (e["id"].as_u64(), e["fingerprint"].as_str()) else { continue };
            let frames = if listed { e["frames"].as_u64().unwrap_or(0) } else { 0 };
            if listed && frames == 0 {
                continue;
            }
            out.insert(
                item,
                Entry {
                    item,
                    fingerprint: fingerprint.to_string(),
                    sampling: e["sampling"].as_str().unwrap_or_default().to_string(),
                    frames,
                    ver: e["ver"].as_u64().unwrap_or(0),
                    error: e["error"].as_str().map(str::to_string),
                },
            );
        }
    }
    out
}

/// Write `index.json` (the helper reads `id`, `f16` and `times`; entries without frames are kept
/// under `failed`) by writing a temporary file and renaming it over the old one.
fn write_index(dir: &Path, entries: &BTreeMap<u64, Entry>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let items: Vec<Value> = entries
        .values()
        .filter(|e| e.frames > 0)
        .map(|e| {
            json!({"id": e.item, "fingerprint": e.fingerprint, "sampling": e.sampling, "ver": e.ver, "frames": e.frames,
                "f16": format!("{}.f16", stem(e.item, e.ver)), "times": format!("{}.json", stem(e.item, e.ver))})
        })
        .collect();
    let failed: Vec<Value> = entries
        .values()
        .filter(|e| e.frames == 0)
        .map(|e| json!({"id": e.item, "fingerprint": e.fingerprint, "sampling": e.sampling, "error": e.error}))
        .collect();
    let text = serde_json::to_vec(&json!({"version": 1, "items": items, "failed": failed}))?;
    let tmp = dir.join("index.json.part");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, dir.join("index.json"))
}

/// Delete the `i_*` files `entries` do not name (replaced pairs, items that left the project).
fn sweep(dir: &Path, entries: &BTreeMap<u64, Entry>) {
    let keep: HashSet<String> =
        entries.values().filter(|e| e.frames > 0).flat_map(|e| [format!("{}.f16", stem(e.item, e.ver)), format!("{}.json", stem(e.item, e.ver))]).collect();
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for f in rd.flatten().take(100_000) {
        let name = f.file_name().to_string_lossy().to_string();
        if name.starts_with("i_") && (name.ends_with(".f16") || name.ends_with(".json")) && !keep.contains(&name) {
            let _ = std::fs::remove_file(f.path());
        }
    }
}

/// Move `from` to `to` (a copy when they are on different volumes).
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

// ---------- jobs ----------

/// Start a project-level helper job (no clips, no sequence): the job record, its thread and the
/// pending entry that [`poll`] swaps in. `work` runs on the job's thread with the job's folder.
fn start<F>(s: &mut Session, kind: &'static str, label: &str, ctx: Ctx, wait: bool, work: F) -> Result<Value>
where
    F: FnOnce(&mut Runner, &Path) -> std::result::Result<Done, Stop> + Send + 'static,
{
    let client = s.sidecar.clone().ok_or_else(|| EngineError::Other(crate::ai::NOT_CONNECTED.into()))?;
    helper_ready(s, kind).map_err(EngineError::Other)?;
    let wait = wait && !s.ai.background;
    let project = ensure_project_id(s);
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let ms = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let out_dir = project_cache_dir(s, &project)?.join(format!("{kind}-{ms}-{id}"));
    std::fs::create_dir_all(&out_dir).map_err(|e| EngineError::Other(format!("AI cache {}: {e}", out_dir.display())))?;
    if matches!(ctx, Ctx::Search { .. }) {
        // a newer search replaces the older one: stop it, and only this one's answer counts
        for p in s.ai_search.pending.iter().filter(|p| matches!(p.ctx, Ctx::Search { .. })) {
            if let Some(j) = s.jobs.iter().find(|j| j.id == p.job) {
                j.progress.cancel.store(true, Ordering::Relaxed);
            }
        }
        s.ai_search.current_search = id;
    }
    let keep: Vec<PathBuf> =
        s.ai.jobs.iter().map(|j| j.out_dir.clone()).chain(s.ai_search.pending.iter().map(|p| p.out_dir.clone())).chain([out_dir.clone()]).collect();
    let (ai_root, cap, limits) = (cache_root(s).join("AI"), s.ai.cache_cap, s.ai.limits);
    let job = crate::Job { id, label: label.to_string(), progress: Default::default(), result: Default::default() };
    job.progress.total.store(1000, Ordering::Relaxed);
    let outcome: Arc<Mutex<Option<Done>>> = Arc::default();
    let (prog, res, out, dir) = (job.progress.clone(), job.result.clone(), outcome.clone(), out_dir.clone());
    let run = move || {
        let beat = dir.join(HEARTBEAT);
        let _ = std::fs::write(&beat, b"running\n");
        prune_cache(&ai_root, cap, &keep, PRUNE_MIN_AGE);
        let mut last_beat = web_time::Instant::now();
        let (p1, p2) = (prog.clone(), prog.clone());
        let mut runner = Runner {
            client,
            limits,
            cancelled: Box::new(move || p1.cancel.load(Ordering::Relaxed)),
            progress: Box::new(move |f, st| {
                p2.done.store((f.clamp(0.0, 1.0) * 1000.0) as u64, Ordering::Relaxed);
                *lock(&p2.status) = st.to_string();
                if last_beat.elapsed() >= HEARTBEAT_EVERY {
                    last_beat = web_time::Instant::now();
                    let _ = std::fs::write(&beat, b"running\n");
                }
            }),
        };
        let r = work(&mut runner, &dir);
        drop(runner);
        let _ = std::fs::remove_dir_all(&dir);
        match r {
            Ok(done) => {
                *lock(&prog.status) = "Applying".into();
                *lock(&out) = Some(done);
                return;
            }
            Err(Stop::Cancelled) => {
                *lock(&prog.status) = "Stopped: nothing was changed".into();
                *lock(&res) = Some(Err("stopped".to_string()));
            }
            Err(Stop::Msg(e)) => {
                *lock(&prog.status) = e.clone();
                *lock(&res) = Some(Err(e));
            }
        }
        prog.finished.store(true, Ordering::Relaxed);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    let (prog, res) = (job.progress.clone(), job.result.clone());
    s.jobs.push(job);
    s.ai_search.pending.push(Pending { job: id, generation: s.project_generation, project, out_dir: out_dir.clone(), ctx, outcome });
    if wait || cfg!(target_arch = "wasm32") {
        run();
        poll(s);
    } else if let Err(e) = std::thread::Builder::new().name(format!("filmcraft-ai-{kind}")).spawn(run) {
        let msg = format!("cannot start the job: {e}");
        *lock(&prog.status) = msg.clone();
        *lock(&res) = Some(Err(msg.clone()));
        prog.finished.store(true, Ordering::Relaxed);
        s.ai_search.pending.retain(|p| p.job != id);
        return Err(EngineError::Other(msg));
    }
    match lock(&res).clone() {
        Some(Err(e)) => Err(EngineError::Other(e)),
        Some(Ok(_)) => Ok(json!({"job": id, "applied": true})),
        None => Ok(json!({"job": id, "background": !wait})),
    }
}

fn index_project(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.indexProject";
    let only: Option<Vec<ItemId>> = p.get("items").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(ItemId).collect());
    let interval = f64_p(p, "intervalSeconds").unwrap_or(2.0);
    if !interval.is_finite() || !(0.25..=60.0).contains(&interval) {
        return Err(bad(cmd, "`intervalSeconds` must be between 0.25 and 60"));
    }
    let max_frames = u64_p(p, "maxFrames").unwrap_or(1800);
    if !(1..=5000).contains(&max_frames) {
        return Err(bad(cmd, "`maxFrames` must be between 1 and 5000"));
    }
    if s.ai_search.pending.iter().any(|p| matches!(p.ctx, Ctx::Index)) {
        return Err(bad(cmd, "the project is already being indexed"));
    }
    let project = ensure_project_id(s);
    let dir = index_dir_of(s, &project)?;
    let args = IndexArgs { dir, cands: candidates(s, only.as_deref()), project_items: s.project.items.keys().map(|k| k.0).collect(), interval, max_frames };
    s.ai_search.last_index = None;
    let wait = bool_p(p, "wait").unwrap_or(false);
    let mut out = start(s, "searchIndex", "Index Project for Search", Ctx::Index, wait, move |run, out_dir| index_work(run, out_dir, args))?;
    if let (Some(summary), Some(o)) = (s.ai_search.last_index.take(), out.as_object_mut()) {
        o.extend(summary.as_object().cloned().unwrap_or_default());
    }
    Ok(out)
}

struct IndexArgs {
    dir: PathBuf,
    cands: Vec<Candidate>,
    project_items: HashSet<u64>,
    interval: f64,
    max_frames: u64,
}

/// The indexing job's work, on its thread: decide what is stale, have the helper embed it, put the
/// results in the index.
fn index_work(run: &mut Runner, out_dir: &Path, a: IndexArgs) -> std::result::Result<Done, Stop> {
    let io = |e: std::io::Error| Stop::Msg(format!("{}: {e}", a.dir.display()));
    run.report(0.0, "Reading the index");
    let mut entries = read_index(&a.dir);
    let sampling = sampling_key(a.interval, a.max_frames);
    let targets = targets_of(&a.cands);
    // entries of items that left the project are dropped when this job lands
    entries.retain(|k, _| a.project_items.contains(k));
    let stale: Vec<&Target> =
        targets.iter().filter(|t| entries.get(&t.item).is_none_or(|e| e.fingerprint != t.fingerprint || e.sampling != sampling)).collect();
    // what fits in one job and in the index
    let (mut take, mut skipped, mut job_frames) = (Vec::<&Target>::new(), 0usize, 0u64);
    let mut indexed = entries.values().filter(|e| e.frames > 0).count();
    let mut index_frames: u64 = entries.values().map(|e| e.frames).sum();
    let mut full = false;
    let mut left = 0usize;
    for t in &stale {
        let est = estimate_frames(t.duration, a.interval, a.max_frames);
        let old = entries.get(&t.item).map_or(0, |e| e.frames);
        let grows = old == 0;
        if (grows && indexed >= MAX_ENTRIES) || index_frames.saturating_sub(old) + est > MAX_INDEX_FRAMES {
            skipped += 1;
            full = true;
            continue;
        }
        if take.len() >= ITEMS_PER_JOB || (!take.is_empty() && job_frames + est > JOB_FRAMES) {
            left += 1;
            continue;
        }
        job_frames += est;
        index_frames = index_frames.saturating_sub(old) + est;
        if grows {
            indexed += 1;
        }
        take.push(t);
    }
    let summary = |entries: &BTreeMap<u64, Entry>, new: usize, remaining: usize, deferred: usize, failed: usize, up_to_date: bool| {
        let frames: u64 = entries.values().map(|e| e.frames).sum();
        json!({"indexed": entries.values().filter(|e| e.frames > 0).count(), "new": new, "remaining": remaining, "deferred": deferred, "failed": failed,
            "skipped": skipped, "full": full, "frames": frames, "bytes": frames * 1024, "upToDate": up_to_date})
    };
    if take.is_empty() {
        write_index(&a.dir, &entries).map_err(io)?;
        sweep(&a.dir, &entries);
        return Ok(Done::Index(summary(&entries, 0, 0, 0, 0, skipped == 0)));
    }
    let req = JobRequest {
        kind: "searchIndex".into(),
        inputs: take.iter().map(|t| t.path.clone()).collect(),
        params: json!({"ids": take.iter().map(|t| t.item).collect::<Vec<_>>(), "intervalSeconds": a.interval, "maxFrames": a.max_frames}),
        out_dir: out_dir.to_string_lossy().into(),
    };
    let timeout = Duration::from_secs(600 + job_frames / 4);
    let (result, files) = run.drive(&req, timeout)?;
    if run.stopped() {
        return Err(Stop::Cancelled);
    }
    run.report(1.0, "Applying");
    std::fs::create_dir_all(&a.dir).map_err(io)?;
    let find = |name: &str| files.iter().find(|f| f.file_name().is_some_and(|n| n == name));
    let (mut new, mut failed) = (0usize, 0usize);
    for it in result.meta["items"].as_array().into_iter().flatten() {
        let (Some(k), Some(frames)) = (it["index"].as_u64(), it["frames"].as_u64()) else { continue };
        let Some(t) = take.get(k as usize) else { continue };
        // the file changed while it was indexed: leave it for the next run
        if fingerprint(&t.path, t.duration).as_ref() != Some(&t.fingerprint) {
            continue;
        }
        let (Some(f16), Some(js)) = (find(&format!("idx_{k}.f16")), find(&format!("idx_{k}.json"))) else { continue };
        // a new pair under a new version; the manifest switches to it below
        let ver = entries.get(&t.item).map_or(1, |e| e.ver + 1);
        let st = stem(t.item, ver);
        let (to16, tojs) = (a.dir.join(format!("{st}.f16")), a.dir.join(format!("{st}.json")));
        if let Err(e) = move_file(f16, &to16).and_then(|()| move_file(js, &tojs)) {
            let _ = std::fs::remove_file(&to16);
            let _ = std::fs::remove_file(&tojs);
            return Err(io(e));
        }
        if entries.get(&t.item).is_none_or(|e| e.frames == 0) {
            new += 1;
        }
        entries.insert(t.item, Entry { item: t.item, fingerprint: t.fingerprint.clone(), sampling: sampling.clone(), frames, ver, error: None });
    }
    for f in result.meta["failed"].as_array().into_iter().flatten() {
        let Some(t) = f["index"].as_u64().and_then(|k| take.get(k as usize)) else { continue };
        failed += 1;
        let ver = entries.get(&t.item).map_or(0, |e| e.ver);
        entries.insert(
            t.item,
            Entry {
                item: t.item,
                fingerprint: t.fingerprint.clone(),
                sampling: sampling.clone(),
                frames: 0,
                ver,
                error: f["error"].as_str().map(|e| e.chars().take(200).collect()),
            },
        );
    }
    // items the helper put off (file missing for the moment, the job's budget) are asked for again
    let deferred = result.meta["deferred"].as_array().map_or(0, Vec::len);
    // failed entries do not outgrow the index
    let mut failed_kept = 0;
    entries.retain(|_, e| {
        if e.frames > 0 {
            return true;
        }
        failed_kept += 1;
        failed_kept <= MAX_ENTRIES
    });
    write_index(&a.dir, &entries).map_err(io)?;
    sweep(&a.dir, &entries);
    Ok(Done::Index(summary(&entries, new, left + deferred, deferred, failed, false)))
}

// ---------- applying results ----------

/// Swap finished jobs in (cheap: no disk access); drop those of another project. Called from
/// [`crate::ai::poll`].
pub fn poll(s: &mut Session) {
    let mut i = 0;
    while let Some(pj) = s.ai_search.pending.get(i) {
        let Some(job) = s.jobs.iter().find(|j| j.id == pj.job).cloned() else {
            s.ai_search.pending.remove(i);
            continue;
        };
        if pj.generation != s.project_generation {
            job.progress.cancel.store(true, Ordering::Relaxed);
        }
        let current = |s: &Session, pj: &Pending| pj.job == s.ai_search.current_search && pj.generation == s.project_generation && s.project.id == pj.project;
        if lock(&job.result).is_some() {
            // failed or stopped on the job's thread
            let pj = s.ai_search.pending.remove(i);
            let why = lock(&job.result).clone().and_then(|r| r.err()).unwrap_or_default();
            if why != "stopped" {
                s.log.push(crate::panels::Level::Warning, "ai", format!("{}: {why}", job.label));
                if matches!(pj.ctx, Ctx::Index) {
                    s.error_toast("ai", format!("{}: {why}", job.label));
                }
            }
            if matches!(pj.ctx, Ctx::Search { .. }) && current(s, &pj) {
                s.search_results.pending = false;
                s.search_results.note = Some(if why == "stopped" { "the search was stopped".into() } else { why });
            }
            continue;
        }
        if lock(&pj.outcome).is_none() {
            i += 1;
            continue;
        }
        let pj = s.ai_search.pending.remove(i);
        let Some(done) = lock(&pj.outcome).take() else { continue };
        let cancelled = job.progress.cancel.load(Ordering::Relaxed);
        let r: std::result::Result<(), String> = match (done, &pj.ctx) {
            (Done::Index(summary), Ctx::Index) => {
                s.ai_search.last_index = Some(summary);
                Ok(())
            }
            (Done::Search(sd), Ctx::Search { query, limit, transcript }) => {
                if cancelled || !current(s, &pj) {
                    Err("a newer search or another project replaced this one; its answer was dropped".to_string())
                } else {
                    apply_search(s, query, *limit, transcript.clone(), sd);
                    Ok(())
                }
            }
            _ => Err("unexpected job result".to_string()),
        };
        *lock(&job.progress.status) = match &r {
            Ok(_) => "Applied".into(),
            Err(e) => e.clone(),
        };
        *lock(&job.result) = Some(r.map(|_| filmcraft_export::Report {
            path: String::new(),
            frames: 0,
            seconds: 0.0,
            bytes: 0,
            render_fps: 0.0,
            extra_files: Vec::new(),
            warnings: Vec::new(),
        }));
        job.progress.finished.store(true, Ordering::Relaxed);
    }
}

// ---------- search ----------

/// Transcript matches of the words of `query` (consecutive, punctuation and case ignored).
fn transcript_hits(s: &Session, query: &str, limit: usize) -> Vec<SearchHit> {
    let words: Vec<String> = query.split_whitespace().map(filmcraft_project::transcript::normalize_word).filter(|w| !w.is_empty()).collect();
    let mut out = Vec::new();
    if words.is_empty() {
        return out;
    }
    for (item, t) in &s.project.transcripts {
        let norm: Vec<String> = t.words.iter().map(|w| w.normalized()).collect();
        let mut found = 0;
        for i in 0..norm.len().saturating_sub(words.len() - 1) {
            if norm[i..i + words.len()] == words[..] {
                out.push(SearchHit { item: *item, time: t.words[i].start, score: 1.0, kind: "transcript", stale: false });
                found += 1;
                if found >= PER_ITEM_TRANSCRIPT || out.len() >= limit {
                    break;
                }
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.search";
    let query = str_p(p, "query").map(str::trim).filter(|q| !q.is_empty()).ok_or_else(|| bad(cmd, "`query` is required"))?;
    if query.chars().count() > MAX_QUERY {
        return Err(bad(cmd, format!("`query` is longer than {MAX_QUERY} characters")));
    }
    let limit = u64_p(p, "limit").unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(bad(cmd, "`limit` must be between 1 and 100"));
    }
    let limit = limit as usize;
    let transcript = transcript_hits(s, query, limit);
    s.search_results = SearchResults { query: query.to_string(), hits: transcript.clone(), pending: false, note: None, indexed: 0, stale_items: 0 };
    // the picture search needs the helper and an index (a project without an id has none: reading
    // must not give it one)
    if let Err(why) = helper_ready(s, "search") {
        s.search_results.note = Some(why);
        return Ok(results_json(s));
    }
    if !is_uuid(&s.project.id) {
        s.search_results.note = Some("the project has no search index yet (ai.indexProject)".into());
        return Ok(results_json(s));
    }
    let dir = index_dir_of(s, &s.project.id.clone())?;
    let snapshot = candidates(s, None);
    s.search_results.pending = true;
    let ctx = Ctx::Search { query: query.to_string(), limit, transcript };
    let (q, wait) = (query.to_string(), bool_p(p, "wait").unwrap_or(false));
    let r = start(s, "search", "Search Media by Content", ctx, wait, move |run, out_dir| search_work(run, out_dir, &dir, &q, limit, &snapshot));
    match r {
        Ok(started) => {
            let mut out = results_json(s);
            out["job"] = started["job"].clone();
            Ok(out)
        }
        Err(e) => {
            s.search_results.pending = false;
            s.search_results.note = Some(e.to_string());
            Err(e)
        }
    }
}

/// The search job's work, on its thread: read the index, have the helper rank it, mark the hits
/// whose files changed.
fn search_work(run: &mut Runner, out_dir: &Path, dir: &Path, query: &str, limit: usize, snapshot: &[Candidate]) -> std::result::Result<Done, Stop> {
    run.report(0.0, "Reading the index");
    let entries = read_index(dir);
    let indexed = entries.values().filter(|e| e.frames > 0).count();
    let by_item: BTreeMap<u64, &Candidate> = snapshot.iter().map(|c| (c.item, c)).collect();
    // an item is stale when its file is not what was indexed (one stat each, here, off the UI thread)
    let stale_of = |e: &Entry| by_item.get(&e.item).and_then(|c| fingerprint(&c.path, c.duration)).is_none_or(|fp| fp != e.fingerprint);
    let stale_items = entries.values().filter(|e| e.frames > 0).filter(|e| stale_of(e)).count();
    let interval = entries.values().find_map(|e| e.sampling.split(':').next().and_then(|v| v.parse::<f64>().ok())).filter(|v| v.is_finite()).unwrap_or(2.0);
    let dedupe = interval.clamp(0.25, 60.0);
    if indexed == 0 {
        return Ok(Done::Search(SearchDone {
            visual: Vec::new(),
            indexed,
            stale_items,
            note: Some("the project has no search index yet (ai.indexProject)".into()),
            dedupe,
        }));
    }
    let req = JobRequest {
        kind: "search".into(),
        inputs: Vec::new(),
        params: json!({"query": query, "limit": limit, "index": dir.to_string_lossy(), "dedupe": dedupe}),
        out_dir: out_dir.to_string_lossy().into(),
    };
    let (result, _) = run.drive(&req, SEARCH_TIMEOUT)?;
    let mut visual: Vec<SearchHit> = Vec::new();
    for h in result.meta["hits"].as_array().into_iter().flatten().take(limit) {
        let (Some(item), Some(t), Some(score)) = (h["id"].as_u64(), h["time"].as_f64(), h["score"].as_f64()) else { continue };
        if !t.is_finite() || !score.is_finite() || (score as f32) < MIN_SCORE {
            continue;
        }
        let stale = entries.get(&item).is_none_or(stale_of);
        visual.push(SearchHit { item: ItemId(item), time: Tick::from_seconds_f64(t.max(0.0)), score: score as f32, kind: "visual", stale });
    }
    visual.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(Done::Search(SearchDone { visual, indexed, stale_items, note: None, dedupe }))
}

/// Transcript and picture hits in one list: neither fills it alone (a share of at least half the
/// list stays open for each when both have enough), and a picture right at a transcript match of
/// the same item is not listed twice.
fn merge_hits(transcript: Vec<SearchHit>, visual: Vec<SearchHit>, limit: usize, dedupe: f64) -> Vec<SearchHit> {
    let visual: Vec<SearchHit> =
        visual.into_iter().filter(|v| !transcript.iter().any(|t| t.item == v.item && (t.time.seconds() - v.time.seconds()).abs() < dedupe)).collect();
    let t_keep = transcript.len().min(limit.saturating_sub(visual.len()).max(limit.div_ceil(2)));
    let v_keep = visual.len().min(limit - t_keep);
    let mut hits = transcript;
    hits.truncate(t_keep);
    hits.extend(visual.into_iter().take(v_keep));
    hits
}

fn apply_search(s: &mut Session, query: &str, limit: usize, transcript: Vec<SearchHit>, sd: SearchDone) {
    let visual: Vec<SearchHit> = sd.visual.into_iter().filter(|h| s.project.items.contains_key(&h.item)).collect();
    let hits = merge_hits(transcript, visual, limit, sd.dedupe);
    s.search_results = SearchResults { query: query.to_string(), hits, pending: false, note: sd.note, indexed: sd.indexed, stale_items: sd.stale_items };
}

fn results_json(s: &Session) -> Value {
    json!({
        "query": s.search_results.query,
        "hits": s.search_results.hits.iter().map(|h| json!({"item": h.item.0, "time": h.time.0, "seconds": h.time.seconds(), "score": h.score, "kind": h.kind, "stale": h.stale})).collect::<Vec<_>>(),
        "pending": s.search_results.pending,
        "note": s.search_results.note,
        "indexed": s.search_results.indexed,
        "staleItems": s.search_results.stale_items,
    })
}

#[cfg(test)]
#[path = "ai_search_tests.rs"]
mod tests;
