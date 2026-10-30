//! `ai.indexProject` and `ai.search` against the scripted fake helper: the index on disk,
//! incremental indexing, stale entries, transcript and visual hits, cancel and the disabled states.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use filmcraft_media::DemoScene;
use filmcraft_project::{ItemId, ItemKind, MediaRef, Transcript, Word};
use filmcraft_sidecar::Limits;
use filmcraft_sidecar::fake::{FakeHelper, ResultMode, Script};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use crate::Session;
use crate::media_test_util::{make_movie, tmp_dir};

struct Fx {
    s: Session,
    helper: FakeHelper,
    a: ItemId,
    b: ItemId,
    files: [PathBuf; 2],
    dir: PathBuf,
}

fn word(t: &str, start: f64) -> Word {
    Word::new(t, Tick((start * TICKS_PER_SECOND as f64) as i64), Tick(((start + 0.4) * TICKS_PER_SECOND as f64) as i64))
}

/// A canned helper: it answers an index job with two indexed items and a search with `hits`.
fn script(hits: Value, steps: u32) -> Script {
    let meta = json!({
        "items": [
            {"index": 0, "id": 0, "frames": 2, "duration": 1.0, "f16": "idx_0.f16", "times": "idx_0.json"},
            {"index": 1, "id": 0, "frames": 2, "duration": 1.0, "f16": "idx_1.f16", "times": "idx_1.json"},
        ],
        "failed": [],
        "hits": hits,
    });
    let files = (0..2).flat_map(|k| [(format!("idx_{k}.f16"), vec![0u8; 2048]), (format!("idx_{k}.json"), b"{}".to_vec())]).collect();
    Script { features: vec!["searchIndex".into(), "search".into()], steps, result: ResultMode::Files { files, meta }, ..Default::default() }
}

/// Two small movies (items a and b), a transcript on b, a connected fake helper.
fn fixture(name: &str, hits_for: impl Fn(ItemId, ItemId) -> Value, steps: u32) -> Fx {
    fixture_with(name, hits_for, steps, &|_| {})
}

fn fixture_with(name: &str, hits_for: impl Fn(ItemId, ItemId) -> Value, steps: u32, tweak: &dyn Fn(&mut Value)) -> Fx {
    let dir = tmp_dir(name);
    let files = [dir.join("a.mp4"), dir.join("b.mp4")];
    make_movie(&files[0], DemoScene::Aurora, 160, 90, 24);
    make_movie(&files[1], DemoScene::CityNight, 160, 90, 24);
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [files[0].to_string_lossy(), files[1].to_string_lossy()]})).unwrap();
    let items: Vec<ItemId> = r["items"].as_array().unwrap().iter().map(|v| ItemId(v.as_u64().unwrap())).collect();
    let (a, b) = (items[0], items[1]);
    let mut p = (*s.project).clone();
    let t = Transcript { words: vec![word("Hello,", 0.1), word("Ocean", 1.0), word("waves.", 1.5)], ..Default::default() };
    p.transcripts.insert(b, std::sync::Arc::new(t));
    s.project = std::sync::Arc::new(p);
    s.ai.cache_root = Some(dir.join("cache"));
    s.ai.limits = Limits { poll_interval: Duration::from_millis(5), max_poll_failures: 3, io_timeout: Duration::from_secs(2), ..Default::default() };
    // the canned index meta names item ids; the ids are known only now: rewrite the script's ids
    let mut sc = script(hits_for(a, b), steps);
    if let ResultMode::Files { meta, .. } = &mut sc.result {
        meta["items"][0]["id"] = json!(a.0);
        meta["items"][1]["id"] = json!(b.0);
        tweak(meta);
    }
    let helper = FakeHelper::start(sc).unwrap();
    s.execute("ai.connect", json!({"port": helper.port()})).unwrap();
    Fx { s, helper, a, b, files, dir }
}

fn visual(a: ItemId, b: ItemId) -> Value {
    json!([{"id": b.0, "time": 4.0, "score": 0.31}, {"id": a.0, "time": 0.25, "score": 0.27}, {"id": 999_999, "time": 1.0, "score": 0.2}])
}

fn index_dir(f: &Fx) -> PathBuf {
    let pid = f.s.project.id.clone();
    f.dir.join("cache").join("AI Index").join(pid)
}

fn index_json(f: &Fx) -> Value {
    serde_json::from_slice(&std::fs::read(index_dir(f).join("index.json")).unwrap()).unwrap()
}

#[test]
fn indexing_writes_the_index_beside_the_cache_and_is_incremental() {
    let mut f = fixture("idx", visual, 1);
    let r = f.s.execute("ai.indexProject", json!({"wait": true, "intervalSeconds": 1.5})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(r["remaining"], 0);
    let req = &f.helper.started()[0];
    assert_eq!(req.kind, "searchIndex");
    assert_eq!(req.inputs.len(), 2);
    assert_eq!(req.params["intervalSeconds"], 1.5);
    assert_eq!(req.params["ids"].as_array().unwrap().len(), 2);
    // outside the pruned AI job folders, per project
    let dir = index_dir(&f);
    assert!(dir.starts_with(f.dir.join("cache").join("AI Index")));
    let doc = index_json(&f);
    assert_eq!(doc["version"], 1);
    let ids: Vec<u64> = doc["items"].as_array().unwrap().iter().map(|e| e["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [f.a.0, f.b.0]);
    for it in [f.a, f.b] {
        assert_eq!(std::fs::metadata(dir.join(format!("i_{}_1.f16", it.0))).unwrap().len(), 2048);
        assert!(dir.join(format!("i_{}_1.json", it.0)).is_file());
    }
    // nothing changed: no job
    let again = f.s.execute("ai.indexProject", json!({"wait": true, "intervalSeconds": 1.5})).unwrap();
    assert_eq!(again["upToDate"], true, "{again}");
    assert_eq!(f.helper.started().len(), 1);
    // one file changes: only it is embedded again
    std::fs::write(&f.files[1], b"x").unwrap();
    {
        // the file is no longer a movie, but its fingerprint changed: the item is sent again
        let mut p = (*f.s.project).clone();
        if let Some(ItemKind::Media(m)) = p.item_mut(f.b).map(|i| &mut i.kind) {
            m.info.duration = Tick(m.info.duration.0 + 1);
        }
        f.s.project = std::sync::Arc::new(p);
    }
    f.s.execute("ai.indexProject", json!({"wait": true, "intervalSeconds": 1.5})).unwrap();
    let started = f.helper.started();
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].inputs, [f.files[1].to_string_lossy().to_string()]);
}

#[test]
fn at_most_sixty_items_go_in_one_job() {
    let mut f = fixture("idx-many", visual, 1);
    // 62 more items on the same file
    let mut p = (*f.s.project).clone();
    let Some(ItemKind::Media(m)) = p.item(f.a).map(|i| i.kind.clone()) else { panic!() };
    for k in 0..62 {
        p.add_item(&format!("copy {k}"), filmcraft_project::Label::Iris, ItemKind::Media(m.clone()), None);
    }
    f.s.project = std::sync::Arc::new(p);
    let r = f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    assert_eq!(r["remaining"], 4, "{r}");
    assert_eq!(f.helper.started()[0].inputs.len(), 60);
}

#[test]
fn search_lists_transcript_hits_first_then_visual_ones_and_marks_stale_media() {
    let mut f = fixture("search", visual, 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let r = f.s.execute("ai.search", json!({"query": "  OCEAN  ", "wait": true})).unwrap();
    assert_eq!(r["pending"], false, "{r}");
    let hits = r["hits"].as_array().unwrap();
    // the transcript word, then the visual hits of items that exist (999999 is not in the project)
    assert_eq!(hits.len(), 3, "{r}");
    assert_eq!((hits[0]["kind"].as_str(), hits[0]["item"].as_u64()), (Some("transcript"), Some(f.b.0)));
    assert!((hits[0]["seconds"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert_eq!((hits[1]["kind"].as_str(), hits[1]["item"].as_u64()), (Some("visual"), Some(f.b.0)));
    assert!((hits[1]["seconds"].as_f64().unwrap() - 4.0).abs() < 1e-6);
    assert_eq!(hits[2]["item"].as_u64(), Some(f.a.0));
    assert!(hits.iter().all(|h| h["stale"] == false));
    assert_eq!(r["indexed"], 2);
    assert_eq!(f.helper.started()[1].kind, "search");
    assert_eq!(f.helper.started()[1].params["query"], "OCEAN");
    // the same result is on the session for the Project panel
    assert_eq!(f.s.search_results.hits.len(), 3);
    // a hit's media changes after it was indexed: the hit is marked stale
    {
        let mut p = (*f.s.project).clone();
        if let Some(ItemKind::Media(m)) = p.item_mut(f.a).map(|i| &mut i.kind) {
            m.info.duration = Tick(m.info.duration.0 + 7);
        }
        f.s.project = std::sync::Arc::new(p);
    }
    let r = f.s.execute("ai.search", json!({"query": "ocean", "wait": true})).unwrap();
    assert_eq!(r["staleItems"], 1, "{r}");
    let stale: Vec<bool> = r["hits"].as_array().unwrap().iter().map(|h| h["stale"].as_bool().unwrap()).collect();
    assert_eq!(stale, [false, false, true]);
    // limit
    let r = f.s.execute("ai.search", json!({"query": "ocean", "limit": 1, "wait": true})).unwrap();
    assert_eq!(r["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn transcript_phrases_ignore_case_and_punctuation() {
    let mut f = fixture("phrase", |_, _| json!([]), 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let r = f.s.execute("ai.search", json!({"query": "hello waves", "wait": true})).unwrap();
    assert!(r["hits"].as_array().unwrap().is_empty(), "the words are not next to each other: {r}");
    let r = f.s.execute("ai.search", json!({"query": "Ocean WAVES!", "wait": true})).unwrap();
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{r}");
    assert!((hits[0]["seconds"].as_f64().unwrap() - 1.0).abs() < 1e-6, "the phrase starts at its first word");
}

#[test]
fn a_project_without_an_index_says_so_and_still_finds_transcript_words() {
    let mut f = fixture("noindex", visual, 1);
    let r = f.s.execute("ai.search", json!({"query": "ocean", "wait": true})).unwrap();
    assert_eq!(r["hits"].as_array().unwrap().len(), 1);
    assert!(r["note"].as_str().unwrap().contains("ai.indexProject"), "{r}");
    assert!(f.helper.started().is_empty(), "no search job without an index");
}

#[test]
fn cancelling_an_index_keeps_the_old_index_and_leaves_nothing_pending() {
    let mut f = fixture("cancel", visual, 100_000);
    let r = f.s.execute("ai.indexProject", json!({})).unwrap();
    let id = r["job"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    let t0 = std::time::Instant::now();
    loop {
        crate::ai::poll(&mut f.s);
        if f.s.jobs.iter().find(|j| j.id == id).is_none_or(|j| j.progress.finished.load(Ordering::Relaxed)) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    let err = f.s.jobs.iter().find(|j| j.id == id).unwrap().result.lock().unwrap().clone().unwrap().unwrap_err();
    assert_eq!(err, "stopped");
    assert_eq!(f.helper.cancelled().len(), 1);
    assert!(!index_dir(&f).join("index.json").exists(), "nothing was written");
    assert!(f.s.ai_search.pending_is_empty());
}

#[test]
fn deleted_items_are_removed_from_the_index() {
    let mut f = fixture("gone", visual, 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let dir = index_dir(&f);
    assert!(dir.join(format!("i_{}_1.f16", f.a.0)).exists());
    let mut p = (*f.s.project).clone();
    p.items.remove(&f.a);
    f.s.project = std::sync::Arc::new(p);
    let r = f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    assert_eq!(r["upToDate"], true, "{r}");
    let ids: Vec<u64> = index_json(&f)["items"].as_array().unwrap().iter().map(|e| e["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [f.b.0]);
    // and so are their files, in the same run
    assert!(!dir.join(format!("i_{}_1.f16", f.a.0)).exists() && !dir.join(format!("i_{}_1.json", f.a.0)).exists());
    assert!(dir.join(format!("i_{}_1.f16", f.b.0)).exists());
}

#[test]
fn disabled_states_and_parameters() {
    let reason = |s: &Session, id: &str| (crate::find_command(id).unwrap().enabled)(s).unwrap_err();
    let mut s = Session::default();
    s.ai.port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let _ = s.execute("ai.status", json!({}));
    assert_eq!(reason(&s, "ai.indexProject"), crate::ai::NOT_CONNECTED);
    // transcript search needs no helper
    assert!(s.is_enabled("ai.search"));
    // a helper without the models
    let mut f = fixture("states", visual, 1);
    let other = FakeHelper::start(Script { features: vec!["echo".into()], ..Default::default() }).unwrap();
    f.s.execute("ai.connect", json!({"port": other.port()})).unwrap();
    assert!(reason(&f.s, "ai.indexProject").contains("does not provide `searchIndex`"));
    f.s.execute("ai.connect", json!({"port": f.helper.port()})).unwrap();
    assert!(f.s.is_enabled("ai.search") && f.s.is_enabled("ai.indexProject"));
    // an empty project has nothing to index
    let mut empty = Session::default();
    empty.execute("ai.connect", json!({"port": f.helper.port()})).unwrap();
    assert_eq!(reason(&empty, "ai.indexProject"), "the project has no video or image files to index");
    for bad in [json!({}), json!({"query": "   "}), json!({"query": "x".repeat(201)}), json!({"query": "a", "limit": 0}), json!({"query": "a", "limit": 101})] {
        assert!(f.s.execute("ai.search", bad.clone()).is_err(), "{bad}");
    }
    for bad in [json!({"intervalSeconds": 0.1}), json!({"intervalSeconds": 61}), json!({"maxFrames": 0}), json!({"maxFrames": 5001})] {
        assert!(f.s.execute("ai.indexProject", bad.clone()).is_err(), "{bad}");
    }
    assert!(f.helper.started().is_empty());
    let _ = MediaRef::File { path: String::new() };
}

#[test]
fn in_the_app_the_visual_hits_arrive_later() {
    let mut f = fixture("background", visual, 2);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    f.s.ai.background = true;
    let r = f.s.execute("ai.search", json!({"query": "ocean", "wait": true})).unwrap();
    assert_eq!(r["pending"], true, "wait is ignored: {r}");
    assert_eq!(r["hits"].as_array().unwrap().len(), 1, "the transcript hit is there at once");
    let t0 = std::time::Instant::now();
    while f.s.search_results.pending {
        crate::ai::poll(&mut f.s);
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(f.s.search_results.hits.len(), 3);
}

fn wait_idle(f: &mut Fx) {
    let t0 = std::time::Instant::now();
    while !f.s.ai_search.pending_is_empty() {
        crate::ai::poll(&mut f.s);
        assert!(t0.elapsed() < Duration::from_secs(20), "jobs did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn set_duration(f: &mut Fx, item: ItemId, d: Tick) {
    let mut p = (*f.s.project).clone();
    if let Some(ItemKind::Media(m)) = p.item_mut(item).map(|i| &mut i.kind) {
        m.info.duration = d;
    }
    f.s.project = std::sync::Arc::new(p);
}

#[test]
fn a_changed_file_gets_a_new_file_pair_and_the_old_one_is_removed() {
    let mut f = fixture("pair", visual, 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let dir = index_dir(&f);
    let d = f.s.project.item(f.b).and_then(|i| if let ItemKind::Media(m) = &i.kind { Some(m.info.duration) } else { None }).unwrap();
    let b = f.b;
    set_duration(&mut f, b, Tick(d.0 + 1));
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let doc = index_json(&f);
    let b = doc["items"].as_array().unwrap().iter().find(|e| e["id"] == f.b.0).unwrap();
    assert_eq!(b["f16"], format!("i_{}_2.f16", f.b.0), "{doc}");
    assert!(dir.join(format!("i_{}_2.f16", f.b.0)).is_file() && dir.join(format!("i_{}_2.json", f.b.0)).is_file());
    assert!(!dir.join(format!("i_{}_1.f16", f.b.0)).exists() && !dir.join(format!("i_{}_1.json", f.b.0)).exists(), "the replaced pair is gone");
    assert!(dir.join(format!("i_{}_1.f16", f.a.0)).is_file(), "the untouched item keeps its pair");
    // stray files the manifest does not name go too
    std::fs::write(dir.join("i_999_7.f16"), b"x").unwrap();
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    assert!(!dir.join("i_999_7.f16").exists());
}

#[test]
fn the_fingerprint_sees_subsecond_changes_and_a_new_sampling_rebuilds() {
    let mut f = fixture("fp", visual, 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    // a rewrite 300 ms later with the same size: the whole-second mtime of before would not see it
    let file = std::fs::OpenOptions::new().write(true).open(&f.files[0]).unwrap();
    let m = std::fs::metadata(&f.files[0]).unwrap().modified().unwrap();
    file.set_modified(m + Duration::from_millis(300)).unwrap();
    drop(file);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let started = f.helper.started();
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].inputs, [f.files[0].to_string_lossy().to_string()]);
    // the same sampling: nothing to do; another interval or frame cap: everything again
    assert_eq!(f.s.execute("ai.indexProject", json!({"wait": true})).unwrap()["upToDate"], true);
    f.s.execute("ai.indexProject", json!({"wait": true, "intervalSeconds": 3})).unwrap();
    assert_eq!(f.helper.started().len(), 3);
    assert_eq!(f.helper.started()[2].inputs.len(), 2);
    f.s.execute("ai.indexProject", json!({"wait": true, "intervalSeconds": 3, "maxFrames": 100})).unwrap();
    assert_eq!(f.helper.started().len(), 4);
}

#[test]
fn items_the_helper_puts_off_are_not_recorded_and_are_asked_for_again() {
    let tweak = |m: &mut Value| {
        let first = m["items"][0].clone();
        m["items"] = json!([first]);
        m["deferred"] = json!([{"index": 1, "id": 0, "error": "the job's limit of 50000 frames would be passed"}]);
    };
    let mut f = fixture_with("deferred", visual, 1, &tweak);
    let r = f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    assert_eq!((r["remaining"].as_u64(), r["deferred"].as_u64(), r["failed"].as_u64()), (Some(1), Some(1), Some(0)), "{r}");
    let doc = index_json(&f);
    assert_eq!(doc["items"].as_array().unwrap().len(), 1);
    assert!(doc["failed"].as_array().unwrap().is_empty(), "put off is not failed: {doc}");
    // the next run sends only the item that is missing
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let started = f.helper.started();
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].inputs, [f.files[1].to_string_lossy().to_string()]);
}

#[test]
fn a_job_stays_within_the_frame_budget() {
    let mut f = fixture("budget", visual, 1);
    let mut p = (*f.s.project).clone();
    let Some(ItemKind::Media(m)) = p.item(f.a).map(|i| i.kind.clone()) else { panic!() };
    let mut long = m.clone();
    long.info.duration = Tick(36_000 * TICKS_PER_SECOND);
    for k in 0..40 {
        p.add_item(&format!("long {k}"), filmcraft_project::Label::Iris, ItemKind::Media(long.clone()), None);
    }
    f.s.project = std::sync::Arc::new(p);
    // each long item is 1800 frames at the defaults: 27 of them fit in 50 000 beside the two short ones
    let r = f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let sent = f.helper.started()[0].inputs.len();
    assert_eq!(sent, 2 + 27, "{r}");
    assert_eq!(r["remaining"], 40 - 27);
}

#[test]
fn only_one_indexing_job_runs_at_a_time() {
    let mut f = fixture("one", visual, 100_000);
    let r = f.s.execute("ai.indexProject", json!({})).unwrap();
    assert_eq!(r["background"], true);
    assert!(!f.s.is_enabled("ai.indexProject"));
    let e = f.s.execute("ai.indexProject", json!({})).unwrap_err().to_string();
    assert!(e.contains("being indexed"), "{e}");
    f.s.execute("jobs.cancel", json!({"job": r["job"]})).unwrap();
    wait_idle(&mut f);
}

#[test]
fn enabling_the_index_command_does_not_touch_the_media_files() {
    let f = fixture("noio", visual, 1);
    for file in &f.files {
        std::fs::remove_file(file).unwrap();
    }
    // the menu asks every frame: it must not stat the files
    assert!(f.s.is_enabled("ai.indexProject"));
}

#[test]
fn a_newer_search_replaces_an_older_one_whichever_answers_first() {
    let mut f = fixture("race", visual, 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    f.s.ai.background = true;
    let a = f.s.execute("ai.search", json!({"query": "dog"})).unwrap();
    let b = f.s.execute("ai.search", json!({"query": "ocean"})).unwrap();
    assert_ne!(a["job"], b["job"]);
    wait_idle(&mut f);
    // the answer of the first search (finished or not) is not shown; the second one is
    assert_eq!(f.s.search_results.query, "ocean");
    assert!(!f.s.search_results.pending && f.s.search_results.note.is_none(), "{:?}", f.s.search_results);
    assert_eq!(f.s.search_results.hits.len(), 3);
    // a failing search that is no longer current does not clear the current one's state
    let mut f = fixture_with("race-fail", visual, 1, &|_| {});
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    f.s.ai.background = true;
    f.s.execute("ai.search", json!({"query": "dog"})).unwrap();
    f.helper.cancelled();
    f.s.execute("ai.search", json!({"query": "ocean"})).unwrap();
    wait_idle(&mut f);
    assert_eq!(f.s.search_results.query, "ocean");
    assert!(f.s.search_results.note.is_none());
}

#[test]
fn search_results_do_not_outlive_the_project_they_belong_to() {
    let mut f = fixture("switch", visual, 1);
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    f.s.execute("ai.search", json!({"query": "ocean", "wait": true})).unwrap();
    assert!(!f.s.search_results.hits.is_empty());
    f.s.execute("file.newProject", json!({})).unwrap();
    assert_eq!(f.s.search_results, Default::default());
}

#[test]
fn transcript_search_works_without_the_helper_and_says_why_pictures_are_left_out() {
    let mut f = fixture("nohelper", visual, 1);
    let other = FakeHelper::start(Script { features: vec!["echo".into()], ..Default::default() }).unwrap();
    f.s.execute("ai.connect", json!({"port": other.port()})).unwrap();
    let r = f.s.execute("ai.search", json!({"query": "ocean", "wait": true})).unwrap();
    assert_eq!(r["hits"].as_array().unwrap().len(), 1, "{r}");
    assert!(r["note"].as_str().unwrap().contains("does not provide `search`"), "{r}");
    assert_eq!(r["pending"], false);
}

#[test]
fn a_search_does_not_give_a_project_an_id() {
    let mut f = fixture("noid", visual, 1);
    let mut p = (*f.s.project).clone();
    p.id = String::new();
    f.s.project = std::sync::Arc::new(p);
    let r = f.s.execute("ai.search", json!({"query": "ocean", "wait": true})).unwrap();
    assert_eq!(r["hits"].as_array().unwrap().len(), 1);
    assert!(r["note"].as_str().unwrap().contains("ai.indexProject"), "{r}");
    assert!(f.s.project.id.is_empty(), "reading minted an id");
}

#[test]
fn copies_of_a_project_keep_their_own_index() {
    let mut f = fixture("copies", visual, 1);
    let id = crate::ai::ensure_project_id(&mut f.s);
    f.s.path = Some("/projects/one.fcraft".into());
    let one = super::index_dir_of(&f.s, &id).unwrap();
    f.s.path = Some("/projects/copy of one.fcraft".into());
    let two = super::index_dir_of(&f.s, &id).unwrap();
    assert_ne!(one, two);
    assert_eq!(one.parent(), two.parent());
    assert!(one.file_name().unwrap().to_string_lossy().starts_with(&id));
}

#[test]
fn a_hostile_index_is_read_within_its_bounds() {
    let f = fixture("hostile", visual, 1);
    let dir = f.dir.join("hostile-index");
    std::fs::create_dir_all(&dir).unwrap();
    // too large: ignored without being read
    let file = std::fs::File::create(dir.join("index.json")).unwrap();
    file.set_len(super::MAX_INDEX_JSON + 1).unwrap();
    assert!(super::read_index(&dir).is_empty());
    // more entries than the helper reads: the extra ones are not taken
    let items: Vec<Value> = (0..(super::MAX_ENTRIES as u64 + 500)).map(|i| json!({"id": i, "fingerprint": "x", "frames": 1})).collect();
    std::fs::write(dir.join("index.json"), serde_json::to_vec(&json!({"version": 1, "items": items, "failed": []})).unwrap()).unwrap();
    assert_eq!(super::read_index(&dir).len(), super::MAX_ENTRIES);
    // nonsense
    std::fs::write(dir.join("index.json"), br#"{"version":1,"items":[7,{"id":"x"},{"id":1}],"failed":"no"}"#).unwrap();
    assert!(super::read_index(&dir).is_empty());
}

#[test]
fn the_helper_and_the_engine_agree_on_the_limits() {
    assert_eq!(super::MAX_ENTRIES, 4000);
    assert_eq!(super::MAX_INDEX_FRAMES * 1024, 2 << 30);
    assert_eq!(super::JOB_FRAMES, 50_000);
}

#[test]
fn the_frame_estimate_follows_the_helpers_sampling_rule() {
    let est = |secs: f64, interval: f64, max: u64| super::estimate_frames(Tick::from_seconds_f64(secs), interval, max);
    assert_eq!(est(0.0, 2.0, 100), 1);
    assert_eq!(est(1.0, 2.0, 100), 1);
    assert_eq!(est(7.0, 2.0, 100), 3); // 1, 3, 5
    assert_eq!(est(3600.0, 2.0, 1800), 1800);
    assert_eq!(est(3600.0, 2.0, 100), 100); // the interval grows to fit the cap
}

fn hit(item: u64, t: f64, kind: &'static str, score: f32) -> crate::search_results::SearchHit {
    crate::search_results::SearchHit { item: ItemId(item), time: Tick::from_seconds_f64(t), score, kind, stale: false }
}

#[test]
fn transcript_hits_cannot_crowd_out_picture_hits_and_duplicates_collapse() {
    let words: Vec<_> = (0..20).map(|i| hit(1, i as f64 * 10.0, "transcript", 1.0)).collect();
    let pics: Vec<_> = (0..20).map(|i| hit(2, i as f64 * 10.0, "visual", 0.3)).collect();
    let merged = super::merge_hits(words.clone(), pics.clone(), 20, 2.0);
    assert_eq!(merged.len(), 20);
    assert_eq!(merged.iter().filter(|h| h.kind == "transcript").count(), 10);
    assert_eq!(merged.iter().filter(|h| h.kind == "visual").count(), 10);
    // few picture hits: the words fill the rest; few words: the pictures do
    let merged = super::merge_hits(words.clone(), pics[..3].to_vec(), 20, 2.0);
    assert_eq!((merged.iter().filter(|h| h.kind == "transcript").count(), merged.len()), (17, 20));
    let merged = super::merge_hits(words[..2].to_vec(), pics.clone(), 20, 2.0);
    assert_eq!((merged.iter().filter(|h| h.kind == "visual").count(), merged.len()), (18, 20));
    // a picture within `dedupe` seconds of a word of the same item is the same place
    let merged = super::merge_hits(
        vec![hit(1, 5.0, "transcript", 1.0)],
        vec![hit(1, 6.0, "visual", 0.3), hit(1, 9.0, "visual", 0.29), hit(2, 5.0, "visual", 0.28)],
        10,
        2.0,
    );
    let places: Vec<(u64, &str)> = merged.iter().map(|h| (h.item.0, h.kind)).collect();
    assert_eq!(places, [(1, "transcript"), (1, "visual"), (2, "visual")]);
    assert!((merged[1].time.seconds() - 9.0).abs() < 1e-9);
}

#[test]
fn weak_pictures_are_left_out_and_the_rest_come_best_first() {
    let mut f = fixture(
        "floor",
        |a, b| json!([{"id": a.0, "time": 0.1, "score": 0.25}, {"id": b.0, "time": 0.3, "score": 0.4}, {"id": a.0, "time": 0.9, "score": 0.15}]),
        1,
    );
    f.s.execute("ai.indexProject", json!({"wait": true})).unwrap();
    let r = f.s.execute("ai.search", json!({"query": "nothing in the transcript", "wait": true})).unwrap();
    let scores: Vec<f64> = r["hits"].as_array().unwrap().iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert_eq!(scores.len(), 2, "{r}");
    assert!((scores[0] - 0.4).abs() < 1e-6 && (scores[1] - 0.25).abs() < 1e-6, "{scores:?}");
}
