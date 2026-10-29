//! `ai.indexProject` and `ai.search` (M6e): find media by what it shows and says.
//!
//! **Index.** `ai.indexProject {items?, intervalSeconds=2, maxFrames=1800, wait}` asks the helper
//! (`searchIndex`) to sample frames of the project's video and still files (one per interval, a
//! short clip gets one in the middle) and embed them with OpenAI CLIP ViT-B/32. The index lives in
//! `<media cache>/AI Index/<project id>/`: `index.json` (per item: its fingerprint, the file names)
//! and per item `i_<item>.f16` (frames x 512 half floats) and `i_<item>.json` (the frames' times).
//! It is outside the AI job cache, so pruning never takes it. Incremental: an item is embedded only
//! when it has no entry or its fingerprint (file size, modification time, duration) differs; an item
//! that could not be read is recorded as failed under its fingerprint and not tried again until it
//! changes; entries of items that left the project are deleted. One job takes at most
//! [`ITEMS_PER_JOB`] items (`remaining` says how many are left: run it again).
//!
//! **Search.** `ai.search {query, limit=20, wait}` finds transcript matches here (a word or phrase,
//! case and punctuation ignored: kind `transcript`, score 1) and asks the helper (`search`) to embed
//! the query with CLIP's text encoder and rank the indexed frames (kind `visual`). Hits are items
//! with a media time; they are listed transcript hits first, then visual ones by score, and kept in
//! [`Session::search_results`] for the Project panel. A hit of an item that changed since it was
//! indexed is marked `stale`. In a host that must not block (the app) the visual part arrives later
//! (`pending` in the state).
//!
//! The index is read-only for the project: nothing here changes the project or its history.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use filmcraft_project::{ItemId, ItemKind, MediaRef};
use filmcraft_sidecar::{JobRequest, JobResult, SidecarError};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::ai::{HEARTBEAT, HEARTBEAT_EVERY, PRUNE_MIN_AGE, cache_root, ensure_project_id, helper_ready, lock, project_cache_dir, prune_cache};
use crate::commands::{CommandSpec, bad, bool_p, f64_p, str_p, u64_p};
use crate::search_results::{SearchHit, SearchResults};
use crate::{EngineError, Result, Session};

/// Items one indexing job takes.
pub const ITEMS_PER_JOB: usize = 60;
/// Largest `index.json` read.
const MAX_INDEX_JSON: u64 = 32 << 20;
/// Most entries an index holds.
const MAX_ENTRIES: usize = 100_000;
/// Longest query.
const MAX_QUERY: usize = 200;
/// Transcript hits kept per item.
const PER_ITEM_TRANSCRIPT: usize = 5;

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
                if targets(s, None).is_empty() { Err("the project has no video or image files to index".into()) } else { Ok(()) }
            },
            run: index_project,
            journal: true,
        },
        CommandSpec {
            id: "ai.search",
            label: "Search Media by Content",
            menu: &[],
            shortcut: None,
            params: r#"{"query":str,"limit":1..100=20,"wait":bool=false}"#,
            enabled: |s| helper_ready(s, "search"),
            run: search,
            journal: false,
        },
    ]
}

/// AI search jobs in flight.
#[derive(Default)]
pub struct State {
    pending: Vec<Pending>,
}

impl State {
    /// No search or index job is waiting to be applied.
    pub fn pending_is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

enum Ctx {
    Index { items: Vec<Target> },
    Search { query: String, limit: usize, transcript: Vec<SearchHit> },
}

struct Pending {
    job: u64,
    generation: u64,
    project: String,
    dir: PathBuf,
    out_dir: PathBuf,
    ctx: Ctx,
    outcome: Arc<Mutex<Option<(JobResult, Vec<PathBuf>)>>>,
}

/// A media item that can be indexed, with what it looked like when its job started.
#[derive(Clone, Debug, PartialEq)]
struct Target {
    item: ItemId,
    path: String,
    fingerprint: String,
}

/// The files of a project that can be indexed (and, with `only`, just those items).
fn targets(s: &Session, only: Option<&[ItemId]>) -> Vec<Target> {
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
        if let Some(fingerprint) = fingerprint(path, m.info.duration) {
            out.push(Target { item: *id, path: path.clone(), fingerprint });
        }
    }
    out
}

/// What identifies a file's content for the index: its size and modification time and the duration
/// the project knows. `None`: the file is not there.
fn fingerprint(path: &str, duration: Tick) -> Option<String> {
    let m = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    Some(format!("{}:{}:{}", m.len(), mtime, duration.0))
}

// ---------- the index on disk ----------

#[derive(Clone, Debug, Default, PartialEq)]
struct Entry {
    item: u64,
    fingerprint: String,
    frames: u64,
    error: Option<String>,
}

fn index_dir(s: &mut Session) -> Result<PathBuf> {
    let project = ensure_project_id(s);
    index_dir_of(s, &project)
}

fn index_dir_of(s: &Session, project: &str) -> Result<PathBuf> {
    // the AI cache folder's name check (a UUID) also guards this one
    project_cache_dir(s, project)?;
    Ok(cache_root(s).join("AI Index").join(project))
}

/// `i_<item>` for item ids (digits only: no path can come out of a name).
fn stem(item: u64) -> String {
    format!("i_{item}")
}

fn read_index(dir: &Path) -> BTreeMap<u64, Entry> {
    let Ok(m) = std::fs::metadata(dir.join("index.json")) else { return BTreeMap::new() };
    if m.len() > MAX_INDEX_JSON {
        return BTreeMap::new();
    }
    let doc: Value = std::fs::read(dir.join("index.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    if doc["version"] != 1 {
        return BTreeMap::new();
    }
    doc["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(MAX_ENTRIES)
                .filter_map(|e| {
                    let item = e["id"].as_u64()?;
                    let fingerprint = e["fingerprint"].as_str()?.to_string();
                    Some((item, Entry { item, fingerprint, frames: e["frames"].as_u64().unwrap_or(0), error: e["error"].as_str().map(str::to_string) }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Write `index.json` (the helper reads `id`, `f16` and `times`; entries without frames are not
/// listed for it but kept in `failed`).
fn write_index(dir: &Path, entries: &BTreeMap<u64, Entry>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let items: Vec<Value> = entries
        .values()
        .filter(|e| e.frames > 0)
        .map(|e| json!({"id": e.item, "fingerprint": e.fingerprint, "frames": e.frames, "f16": format!("{}.f16", stem(e.item)), "times": format!("{}.json", stem(e.item))}))
        .collect();
    let failed: Vec<Value> =
        entries.values().filter(|e| e.frames == 0).map(|e| json!({"id": e.item, "fingerprint": e.fingerprint, "error": e.error})).collect();
    let text = serde_json::to_vec(&json!({"version": 1, "items": items, "failed": failed}))?;
    let tmp = dir.join("index.json.part");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, dir.join("index.json"))
}

/// Merge `index.json` entries of failed items back in (they are listed under `failed`).
fn read_all(dir: &Path) -> BTreeMap<u64, Entry> {
    let mut m = read_index(dir);
    let doc: Value = std::fs::read(dir.join("index.json"))
        .ok()
        .filter(|b| b.len() as u64 <= MAX_INDEX_JSON)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if let Some(a) = doc["failed"].as_array() {
        for e in a.iter().take(MAX_ENTRIES) {
            if let (Some(item), Some(fp)) = (e["id"].as_u64(), e["fingerprint"].as_str()) {
                m.insert(item, Entry { item, fingerprint: fp.to_string(), frames: 0, error: e["error"].as_str().map(str::to_string) });
            }
        }
    }
    m
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

/// Start a project-level helper job (no clips, no sequence): the job record, its thread and
/// the pending entry that [`poll`] applies.
fn start(s: &mut Session, kind: &'static str, label: &str, inputs: Vec<String>, params: Value, ctx: Ctx, dir: PathBuf, wait: bool) -> Result<Value> {
    let client = s.sidecar.clone().ok_or_else(|| EngineError::Other(crate::ai::NOT_CONNECTED.into()))?;
    helper_ready(s, kind).map_err(EngineError::Other)?;
    let wait = wait && !s.ai.background;
    let project = ensure_project_id(s);
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let ms = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let out_dir = project_cache_dir(s, &project)?.join(format!("{kind}-{ms}-{id}"));
    std::fs::create_dir_all(&out_dir).map_err(|e| EngineError::Other(format!("AI cache {}: {e}", out_dir.display())))?;
    let req = JobRequest { kind: kind.into(), inputs, params, out_dir: out_dir.to_string_lossy().into() };
    let keep: Vec<PathBuf> =
        s.ai.jobs.iter().map(|j| j.out_dir.clone()).chain(s.ai_search.pending.iter().map(|p| p.out_dir.clone())).chain([out_dir.clone()]).collect();
    let (ai_root, cap, limits) = (cache_root(s).join("AI"), s.ai.cache_cap, s.ai.limits);
    let job = crate::Job { id, label: label.to_string(), progress: Default::default(), result: Default::default() };
    job.progress.total.store(1000, Ordering::Relaxed);
    let outcome: Arc<Mutex<Option<(JobResult, Vec<PathBuf>)>>> = Arc::default();
    let (prog, res, out) = (job.progress.clone(), job.result.clone(), outcome.clone());
    let run = move || {
        let beat = PathBuf::from(&req.out_dir).join(HEARTBEAT);
        let _ = std::fs::write(&beat, b"running\n");
        prune_cache(&ai_root, cap, &keep, PRUNE_MIN_AGE);
        let mut last_beat = web_time::Instant::now();
        let cancelled = || prog.cancel.load(Ordering::Relaxed);
        let mut on_progress = |f: f64, st: &str| {
            prog.done.store((f * 1000.0) as u64, Ordering::Relaxed);
            *lock(&prog.status) = st.to_string();
            if last_beat.elapsed() >= HEARTBEAT_EVERY {
                last_beat = web_time::Instant::now();
                let _ = std::fs::write(&beat, b"running\n");
            }
        };
        let r = filmcraft_sidecar::drive(&*client, &req, &limits, &cancelled, &mut on_progress);
        let _ = std::fs::remove_file(&beat);
        match r {
            Ok(done) => {
                *lock(&prog.status) = "Applying".into();
                *lock(&out) = Some(done);
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
    s.ai_search.pending.push(Pending { job: id, generation: s.project_generation, project, dir, out_dir: out_dir.clone(), ctx, outcome });
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
    let dir = index_dir(s)?;
    let all = targets(s, only.as_deref());
    let mut entries = read_all(&dir);
    // entries of items that left the project, or whose file is gone, are dropped when this job lands
    let stale: Vec<Target> = all.iter().filter(|t| entries.get(&t.item.0).is_none_or(|e| e.fingerprint != t.fingerprint)).cloned().collect();
    let remaining = stale.len().saturating_sub(ITEMS_PER_JOB);
    let take: Vec<Target> = stale.into_iter().take(ITEMS_PER_JOB).collect();
    entries.retain(|k, _| s.project.items.contains_key(&ItemId(*k)));
    if take.is_empty() {
        write_index(&dir, &entries).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
        let frames: u64 = entries.values().map(|e| e.frames).sum();
        return Ok(json!({"indexed": entries.values().filter(|e| e.frames > 0).count(), "new": 0, "remaining": 0, "frames": frames, "upToDate": true}));
    }
    let inputs: Vec<String> = take.iter().map(|t| t.path.clone()).collect();
    let params = json!({"ids": take.iter().map(|t| t.item.0).collect::<Vec<_>>(), "intervalSeconds": interval, "maxFrames": max_frames});
    let mut out = start(s, "searchIndex", "Index Project for Search", inputs, params, Ctx::Index { items: take }, dir, bool_p(p, "wait").unwrap_or(false))?;
    out["remaining"] = json!(remaining);
    Ok(out)
}

// ---------- applying results ----------

/// Apply finished jobs; drop those of another project. Called from [`crate::ai::poll`].
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
        if lock(&job.result).is_some() {
            // failed or stopped in the worker
            let pj = s.ai_search.pending.remove(i);
            if let Ctx::Search { .. } = pj.ctx {
                let why = lock(&job.result).clone().and_then(|r| r.err()).unwrap_or_default();
                s.search_results.pending = false;
                s.search_results.note = Some(if why == "stopped" { "the search was stopped".into() } else { why });
            }
            continue;
        }
        let cancelled = job.progress.cancel.load(Ordering::Relaxed);
        if lock(&pj.outcome).is_none() {
            i += 1;
            continue;
        }
        let pj = s.ai_search.pending.remove(i);
        let Some((result, files)) = lock(&pj.outcome).take() else { continue };
        let r = if cancelled || pj.generation != s.project_generation || s.project.id != pj.project {
            Err("another project was opened or the job was stopped; nothing was changed".to_string())
        } else {
            match pj.ctx {
                Ctx::Index { ref items } => apply_index(s, &pj.dir, items, &result, &files).map_err(|e| e.to_string()),
                Ctx::Search { ref query, limit, ref transcript } => apply_search(s, &pj.dir, query, limit, transcript.clone(), &result),
            }
        };
        if r.is_err() && matches!(pj.ctx, Ctx::Search { .. }) {
            s.search_results.pending = false;
            s.search_results.note = r.as_ref().err().cloned();
        }
        *lock(&job.progress.status) = match &r {
            Ok(_) => "Applied".into(),
            Err(e) => e.clone(),
        };
        *lock(&job.result) =
            Some(r.map(|_| filmcraft_export::Report { path: String::new(), frames: 0, seconds: 0.0, bytes: 0, render_fps: 0.0, extra_files: Vec::new() }));
        job.progress.finished.store(true, Ordering::Relaxed);
    }
}

fn apply_index(s: &mut Session, dir: &Path, items: &[Target], result: &JobResult, files: &[PathBuf]) -> Result<()> {
    let _ = s;
    let mut entries = read_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
    let find = |name: &str| files.iter().find(|f| f.file_name().is_some_and(|n| n == name));
    for it in result.meta["items"].as_array().into_iter().flatten() {
        let (Some(k), Some(frames)) = (it["index"].as_u64(), it["frames"].as_u64()) else { continue };
        let Some(t) = items.get(k as usize) else { continue };
        // the file changed while it was indexed: leave it for the next run
        if targets(s, Some(&[t.item])).first().map(|c| &c.fingerprint) != Some(&t.fingerprint) {
            continue;
        }
        let (Some(f16), Some(js)) = (find(&format!("idx_{k}.f16")), find(&format!("idx_{k}.json"))) else { continue };
        let st = stem(t.item.0);
        move_file(f16, &dir.join(format!("{st}.f16")))
            .and_then(|()| move_file(js, &dir.join(format!("{st}.json"))))
            .map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
        entries.insert(t.item.0, Entry { item: t.item.0, fingerprint: t.fingerprint.clone(), frames, error: None });
    }
    for f in result.meta["failed"].as_array().into_iter().flatten() {
        let Some(t) = f["index"].as_u64().and_then(|k| items.get(k as usize)) else { continue };
        entries.insert(
            t.item.0,
            Entry { item: t.item.0, fingerprint: t.fingerprint.clone(), frames: 0, error: f["error"].as_str().map(|e| e.chars().take(200).collect()) },
        );
    }
    // items that left the project: their files go
    let gone: Vec<u64> = entries.keys().copied().filter(|k| !s.project.items.contains_key(&ItemId(*k))).collect();
    for k in gone {
        entries.remove(&k);
        let _ = std::fs::remove_file(dir.join(format!("{}.f16", stem(k))));
        let _ = std::fs::remove_file(dir.join(format!("{}.json", stem(k))));
    }
    write_index(dir, &entries).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))
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
    let dir = index_dir(s)?;
    let entries = read_all(&dir);
    let indexed = entries.values().filter(|e| e.frames > 0).count();
    let transcript = transcript_hits(s, query, limit);
    s.search_results =
        SearchResults { query: query.to_string(), hits: transcript.clone(), pending: false, note: None, indexed, stale_items: stale_count(s, &entries) };
    if indexed == 0 {
        s.search_results.note = Some("the project has no search index yet (ai.indexProject)".into());
        return Ok(results_json(s));
    }
    s.search_results.pending = true;
    let params = json!({"query": query, "limit": limit, "index": dir.to_string_lossy(), "dedupe": 2.0});
    let ctx = Ctx::Search { query: query.to_string(), limit, transcript };
    let r = start(s, "search", "Search Media by Content", Vec::new(), params, ctx, dir, bool_p(p, "wait").unwrap_or(false));
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

fn stale_count(s: &Session, entries: &BTreeMap<u64, Entry>) -> usize {
    entries.values().filter(|e| e.frames > 0).filter(|e| targets(s, Some(&[ItemId(e.item)])).first().is_none_or(|t| t.fingerprint != e.fingerprint)).count()
}

fn apply_search(s: &mut Session, dir: &Path, query: &str, limit: usize, transcript: Vec<SearchHit>, result: &JobResult) -> std::result::Result<(), String> {
    let entries = read_all(dir);
    let mut hits = transcript;
    for h in result.meta["hits"].as_array().into_iter().flatten().take(limit) {
        let (Some(item), Some(t), Some(score)) = (h["id"].as_u64(), h["time"].as_f64(), h["score"].as_f64()) else { continue };
        let item = ItemId(item);
        if !s.project.items.contains_key(&item) || !t.is_finite() {
            continue;
        }
        let stale = entries.get(&item.0).is_none_or(|e| targets(s, Some(&[item])).first().is_none_or(|c| c.fingerprint != e.fingerprint));
        hits.push(SearchHit { item, time: Tick::from_seconds_f64(t.max(0.0)), score: score as f32, kind: "visual", stale });
    }
    hits.truncate(limit);
    let (indexed, stale_items) = (entries.values().filter(|e| e.frames > 0).count(), stale_count(s, &entries));
    s.search_results = SearchResults { query: query.to_string(), hits, pending: false, note: None, indexed, stale_items };
    Ok(())
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
