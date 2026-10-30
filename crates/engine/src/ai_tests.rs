//! `ai.*` against the scripted fake helper (`filmcraft_sidecar::fake`): handshake, the echo job
//! end to end, cancel, malformed replies, version mismatch, bad result files, a helper that dies,
//! stale results and the atomic apply.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{DemoScene, Generator, MediaSource};
use filmcraft_project::{ClipId, ItemId, ItemKind, Label, MediaClip, MediaRef, SequenceSettings, TrackKind};
use filmcraft_sidecar::Limits;
use filmcraft_sidecar::fake::{FakeHelper, Malformed, ResultMode, Script};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::{Value, json};

use super::NOT_CONNECTED;
use crate::{Event, Session};

const FEATURES: [&str; 9] =
    ["ai.reframe", "ai.stems", "ai.enhanceSpeech", "ai.beatGrid", "ai.musicFit", "ai.search", "ai.upscale", "ai.interpolate", "ai.generativeExtend"];

pub(super) struct Fixture {
    pub(super) s: Session,
    pub(super) helper: FakeHelper,
    pub(super) clip: ClipId,
    pub(super) item: ItemId,
    pub(super) cache: PathBuf,
}

fn cache_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("filmcraft-ai-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A session with one 2 s clip of `/media/interview.mov` on V1 (selected), connected to a fake
/// helper running `script`.
pub(super) fn fixture(name: &str, script: Script) -> Fixture {
    let mut s = Session::default();
    let rate = FrameRate::FPS_24;
    let dur = rate.tick_of(48);
    let info = GeneratorSource::new(Generator::Demo(DemoScene::Plasma), 64, 36, rate, dur).info().clone();
    let media = MediaClip {
        media: MediaRef::File { path: "/media/interview.mov".into() },
        info,
        interpret: Default::default(),
        mark_in: None,
        mark_out: None,
        markers: vec![],
        offline: false,
        proxy: None,
        identity: None,
    };
    let mut p = (*s.project).clone();
    let item = p.add_item("interview", Label::Iris, ItemKind::Media(media), None);
    let seq = p.new_sequence("Seq", SequenceSettings { width: 64, height: 36, frame_rate: rate, ..Default::default() }, 1, 1, None);
    let v = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, dur), rate).unwrap();
    let clip = v.id;
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(v);
    s.project = std::sync::Arc::new(p);
    s.state.active_sequence = Some(seq);
    s.state.open_sequences = vec![seq];
    s.state.selection = vec![clip];
    let cache = cache_dir(name);
    s.ai.cache_root = Some(cache.clone());
    // tests never write under the real data directory or read the real helper's token file
    s.ai.media_root = Some(cache.join("media"));
    s.ai.token = Some("test".into());
    s.ai.limits = Limits { poll_interval: Duration::from_millis(5), max_poll_failures: 3, io_timeout: Duration::from_secs(2), ..Default::default() };
    let helper = FakeHelper::start(script).unwrap();
    let _ = s.execute("ai.connect", json!({"port": helper.port()}));
    Fixture { s, helper, clip, item, cache }
}

/// Poll (apply results) until job `id` is finished.
pub(super) fn finish(s: &mut Session, id: u64) {
    let t0 = std::time::Instant::now();
    loop {
        super::poll(s);
        let done = s.jobs.iter().find(|j| j.id == id).is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        if done {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "job {id} never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn job_error(s: &Session, id: u64) -> Option<String> {
    let j = s.jobs.iter().find(|j| j.id == id)?;
    j.result.lock().unwrap().clone()?.err()
}

/// The project as JSON (motion's automatic position is NaN, so `Project` never equals itself).
pub(super) fn snapshot(s: &Session) -> serde_json::Value {
    serde_json::to_value(&*s.project).unwrap()
}

fn markers(s: &Session) -> usize {
    s.active_sequence().unwrap().markers.len()
}

fn toasts(s: &mut Session) -> Vec<String> {
    s.drain_events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Toast { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

pub(super) fn reason(s: &Session, id: &str) -> String {
    (crate::find_command(id).unwrap().enabled)(s).unwrap_err()
}

#[test]
fn no_helper_disables_every_ai_command_with_a_reason() {
    let mut s = Session::default();
    s.ai.port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let st = s.execute("ai.status", json!({})).unwrap();
    assert_eq!(st["connected"], false);
    assert_eq!(st["reason"], NOT_CONNECTED);
    for id in FEATURES.iter().chain(&["ai.echo"]) {
        assert_eq!(reason(&s, id), NOT_CONNECTED, "{id}");
    }
    let e = s.execute("ai.reframe", json!({})).unwrap_err().to_string();
    assert!(e.contains("AI helper not running"), "{e}");
    assert!(s.execute("ai.connect", json!({"port": 0})).is_err());
}

#[test]
fn the_ai_commands_sit_in_existing_menus() {
    for id in ["ai.connect", "ai.reframe", "ai.stems", "ai.enhanceSpeech", "ai.upscale", "ai.interpolate", "ai.generativeExtend"] {
        assert_eq!(crate::find_command(id).unwrap().menu, ["Clip", "AI"], "{id}");
    }
    for id in ["ai.beatGrid", "ai.musicFit"] {
        assert_eq!(crate::find_command(id).unwrap().menu, ["Sequence", "AI"], "{id}");
    }
    assert!(crate::find_command("ai.search").unwrap().menu.is_empty());
}

#[test]
fn handshake_reports_the_helper_and_features_wait_for_their_milestone() {
    let mut f = fixture("hello", Script { features: vec!["echo".into(), "upscale".into()], ..Default::default() });
    let st = f.s.execute("ai.status", json!({})).unwrap();
    assert_eq!(st["connected"], true, "{st}");
    assert_eq!(st["features"], json!(["echo", "upscale"]));
    assert_eq!(reason(&f.s, "ai.upscale"), "not built yet (arrives in M6f)");
    assert!(reason(&f.s, "ai.stems").contains("does not provide `stems`"));
    assert!(f.s.is_enabled("ai.echo"));
    f.s.state.selection.clear();
    assert_eq!(reason(&f.s, "ai.echo"), "select a clip in the timeline");
}

#[test]
fn version_mismatch_disables_with_both_versions() {
    let mut f = fixture("version", Script { protocol: 99, version: "9.9.0".into(), ..Default::default() });
    let e = f.s.execute("ai.connect", json!({"port": f.helper.port()})).unwrap_err().to_string();
    assert!(e.contains("version 9.9.0 (protocol 99)") && e.contains("needs protocol 1"), "{e}");
    for id in FEATURES.iter().chain(&["ai.echo"]) {
        assert!(reason(&f.s, id).contains("protocol 99"), "{id}");
    }
    assert_eq!(f.s.execute("ai.status", json!({})).unwrap()["connected"], false);
}

#[test]
fn malformed_hello_disables() {
    let f = fixture("badhello", Script { malformed: Malformed::Hello, ..Default::default() });
    assert!(reason(&f.s, "ai.echo").contains("malformed"), "{}", reason(&f.s, "ai.echo"));
}

#[test]
fn echo_job_applies_in_one_undo_step() {
    let mut f = fixture("ok", Script::default());
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.echo", json!({"wait": true, "steps": 3})).unwrap();
    let id = r["job"].as_u64().unwrap();
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(job_error(&f.s, id), None);
    assert!(f.s.jobs.iter().find(|j| j.id == id).unwrap().progress.finished.load(Ordering::Relaxed));
    // the request the helper saw: the clip's media, the params, a folder of the project's cache
    let req = &f.helper.started()[0];
    assert_eq!(req.inputs, ["/media/interview.mov"]);
    assert_eq!(req.params, json!({"steps": 3}));
    let pid = f.s.project.id.clone();
    assert_eq!(pid.len(), 36);
    assert!(PathBuf::from(&req.out_dir).starts_with(f.cache.join("AI").join(&pid)), "{}", req.out_dir);
    // applied: one marker, the item's metadata, one undo step
    assert_eq!(markers(&f.s), 1);
    assert_eq!(f.s.project.item(f.item).unwrap().metadata.get("AI Echo").map(String::as_str), Some("echo.json"));
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "AI Echo");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(markers(&f.s), 0);
    assert!(!f.s.project.item(f.item).unwrap().metadata.contains_key("AI Echo"));
    // the id given at the job's start survives undo, and the next job uses the same folder
    assert_eq!(f.s.project.id, pid);
}

#[test]
fn cancel_mid_job_changes_nothing() {
    let mut f = fixture("cancel", Script { steps: 100_000, ..Default::default() });
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert_eq!(f.helper.cancelled(), ["j1"]);
    assert_eq!(markers(&f.s), 0);
    assert!(f.s.ai.jobs.is_empty());
}

#[test]
fn helper_failures_fail_the_job_and_change_nothing() {
    let cases: Vec<(&str, Script, &str)> = vec![
        ("malformed", Script { malformed: Malformed::Status, ..Default::default() }, "stopped answering"),
        ("missing", Script { result: ResultMode::Missing, ..Default::default() }, "missing"),
        ("oversize", Script { result: ResultMode::Oversize(4096), ..Default::default() }, "larger than 1000 bytes"),
        ("outside", Script { result: ResultMode::Outside, ..Default::default() }, "outside"),
        ("killed", Script { steps: 100_000, die_at_poll: Some(3), ..Default::default() }, "stopped answering"),
        ("helperror", Script { result: ResultMode::Fail("model missing".into()), ..Default::default() }, "model missing"),
    ];
    for (name, script, want) in cases {
        let mut f = fixture(name, script);
        f.s.ai.limits.max_result_bytes = 1000;
        super::ensure_project_id(&mut f.s);
        let before = snapshot(&f.s);
        let e = f.s.execute("ai.echo", json!({"wait": true})).unwrap_err().to_string();
        assert!(e.contains(want), "{name}: {e}");
        assert_eq!(snapshot(&f.s), before, "{name}");
        assert!(f.s.ai.jobs.is_empty(), "{name}");
    }
}

/// Start an echo job, change the project with `change`, then let the job finish: nothing is
/// applied and an event says why.
fn stale(name: &str, change: impl FnOnce(&mut Fixture), why: &str) {
    let mut f = fixture(name, Script { steps: 3, ..Default::default() });
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    change(&mut f);
    let before = snapshot(&f.s);
    let _ = toasts(&mut f.s);
    finish(&mut f.s, id);
    let e = job_error(&f.s, id).unwrap_or_default();
    assert!(e.contains(why) && e.contains("kept in"), "{name}: the job fails with the reason: {e}");
    assert_eq!(snapshot(&f.s), before, "{name}: nothing applied");
    let t = toasts(&mut f.s);
    assert!(t.iter().any(|m| m.contains(why) && m.contains("not applied")), "{name}: {t:?}");
    assert!(f.s.log.entries.iter().any(|e| e.source == "ai" && e.message.contains("kept in")), "{name}");
}

#[test]
fn stale_results_are_dropped() {
    stale(
        "moved",
        |f| {
            f.s.edit_sequence("Move", |q, _, _| {
                q.video_tracks[0].items[0].start = Tick(254_016_000_000);
                Ok(())
            })
            .unwrap();
        },
        "a target clip changed",
    );
    stale(
        "deleted",
        |f| {
            let c = f.clip;
            f.s.edit_sequence("Delete", |q, _, _| {
                q.video_tracks[0].items.retain(|it| it.id != c);
                Ok(())
            })
            .unwrap();
        },
        "a target clip was deleted",
    );
}

#[test]
fn undo_during_the_job_drops_the_result() {
    let mut f = fixture("undo", Script { steps: 3, ..Default::default() });
    f.s.edit_sequence("Move", |q, _, _| {
        q.video_tracks[0].items[0].start = Tick(254_016_000_000);
        Ok(())
    })
    .unwrap();
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    f.s.execute("edit.undo", json!({})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(markers(&f.s), 0);
    assert!(toasts(&mut f.s).iter().any(|m| m.contains("a target clip changed")));
    // an undo that does not touch the clip keeps the result
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    finish(&mut f.s, id);
    assert_eq!(markers(&f.s), 1);
}

#[test]
fn a_failing_step_reverts_the_whole_apply() {
    let mut f = fixture("atomic", Script::default());
    // the project id is given when the job starts; compare from there
    super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let (undo, redo) = (f.s.history.undo.len(), f.s.history.redo.len());
    let _ = toasts(&mut f.s);
    let e = f.s.execute("ai.echo", json!({"wait": true, "failStep": 2})).unwrap_err().to_string();
    assert!(e.contains("injected failure in step 2") && e.contains("nothing was changed"), "the caller learns it: {e}");
    assert_eq!(snapshot(&f.s), before, "step 1's marker was rolled back");
    assert_eq!((f.s.history.undo.len(), f.s.history.redo.len()), (undo, redo));
    let t = toasts(&mut f.s);
    assert!(t.iter().any(|m| m.contains("injected failure in step 2") && m.contains("nothing was changed")), "{t:?}");
}

#[test]
fn the_helper_is_disconnected_mid_session() {
    let mut f = fixture("gone", Script::default());
    assert!(f.s.is_enabled("ai.echo"));
    f.helper.kill();
    let st = f.s.execute("ai.status", json!({})).unwrap();
    assert_eq!(st["connected"], false);
    assert_eq!(reason(&f.s, "ai.echo"), NOT_CONNECTED);
}

/// An older project (no id) opens with none, gets one on its first AI job, keeps it through save
/// and reopen, and a different project opened meanwhile drops the job's result.
#[test]
fn project_id_through_save_open_and_switch() {
    let mut f = fixture("file", Script { steps: 3, ..Default::default() });
    let dir = cache_dir("file-proj");
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.fcproj");
    // a project without an id is written as schema 16, as before the id existed
    let bytes = filmcraft_format::encode(&f.s.project, false);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["schema_version"], 16);
    std::fs::write(&old, bytes).unwrap();
    let r = f.s.execute("file.open", json!({"path": old.to_string_lossy()})).unwrap();
    assert_eq!(r["migrated"], false);
    assert!(f.s.project.id.is_empty());
    let seq = f.s.state.active_sequence.unwrap();
    f.s.state.selection = vec![f.s.project.sequence(seq).unwrap().video_tracks[0].items[0].id];
    f.s.execute("ai.echo", json!({"wait": true})).unwrap();
    let id = f.s.project.id.clone();
    assert_eq!(id.len(), 36);
    assert!(f.s.is_dirty(), "the new id is saved");
    let path = dir.join("new.fcproj");
    f.s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    f.s.execute("file.newProject", json!({"name": "Other"})).unwrap();
    assert!(f.s.project.id.is_empty());
    f.s.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(f.s.project.id, id);
    // a job started here, then the same file opened again: another project generation
    f.s.state.selection = vec![f.s.project.sequence(seq).unwrap().video_tracks[0].items[0].id];
    let job = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    f.s.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
    let before = snapshot(&f.s);
    finish(&mut f.s, job);
    assert_eq!(snapshot(&f.s), before);
    // the job of the replaced project was stopped (or its result dropped), never applied
    assert!(job_error(&f.s, job).is_some_and(|e| e.starts_with("stopped")), "{:?}", job_error(&f.s, job));
}

/// Wait until job `id`'s result is ready to apply (not applied: nobody polls).
fn ready(s: &Session, id: u64) {
    let t0 = std::time::Instant::now();
    while !s.ai.jobs.iter().any(|j| j.job == id && j.outcome.lock().unwrap().is_some()) {
        assert!(t0.elapsed() < Duration::from_secs(10), "job {id} never got its result");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The desktop app: handshakes run on a worker and `wait` is ignored, so a silent helper never
/// holds the caller.
#[test]
fn a_background_host_never_waits_on_the_helper() {
    let mut f = fixture("background", Script::default());
    f.s.ai.background = true;
    f.s.ai.hello = None;
    let st = f.s.execute("ai.status", json!({})).unwrap();
    assert_eq!(st["probing"], true, "{st}");
    let t0 = std::time::Instant::now();
    while f.s.ai.hello.is_none() {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
        super::poll(&mut f.s);
    }
    assert!(f.s.is_enabled("ai.echo"));
    let r = f.s.execute("ai.echo", json!({"wait": true})).unwrap();
    assert_eq!(r["background"], true, "wait is ignored: {r}");
    let id = r["job"].as_u64().unwrap();
    finish(&mut f.s, id);
    assert_eq!(markers(&f.s), 1);

    let silent = FakeHelper::start(Script { malformed: Malformed::Silent, ..Default::default() }).unwrap();
    let t0 = std::time::Instant::now();
    let st = f.s.execute("ai.connect", json!({"port": silent.port()})).unwrap();
    assert!(t0.elapsed() < Duration::from_millis(500), "ai.connect held the caller for {:?}", t0.elapsed());
    assert_eq!(st["probing"], true);
    while f.s.ai.probe.is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
        super::poll(&mut f.s);
    }
    assert!(reason(&f.s, "ai.echo").contains("no reply within"), "{}", reason(&f.s, "ai.echo"));
    assert!(toasts(&mut f.s).iter().any(|m| m.contains("no reply within")));
}

/// A result waits while a gesture (or dynamic trim) is in progress, then lands as one step.
#[test]
fn results_wait_for_gestures() {
    let mut f = fixture("gesture", Script::default());
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    ready(&f.s, id);
    f.s.gesture_in_progress = true;
    super::poll(&mut f.s);
    assert_eq!(markers(&f.s), 0);
    // a caller that waited is told the result is still pending
    let r = f.s.execute("ai.echo", json!({"wait": true})).unwrap();
    assert_eq!((&r["applied"], &r["pending"]), (&json!(false), &json!(true)), "{r}");
    assert!(job_error(&f.s, id).is_none() && !f.s.jobs.iter().find(|j| j.id == id).unwrap().progress.finished.load(Ordering::Relaxed));
    f.s.gesture_in_progress = false;
    super::poll(&mut f.s);
    assert_eq!(markers(&f.s), 2, "both results land once the gesture ends");
}

#[test]
fn more_stale_causes_are_caught() {
    stale(
        "locked",
        |f| {
            f.s.edit_sequence("Lock", |q, _, _| {
                q.video_tracks[0].locked = true;
                Ok(())
            })
            .unwrap();
        },
        "a target track is locked",
    );
    stale(
        "settings",
        |f| {
            f.s.edit_sequence("Settings", |q, _, _| {
                q.settings.width = 36;
                q.settings.height = 64;
                Ok(())
            })
            .unwrap();
        },
        "the sequence settings changed",
    );
    stale(
        "interpret",
        |f| {
            let item = f.item;
            f.s.edit("Interpret", |pr, _| {
                if let Some(ItemKind::Media(m)) = pr.item_mut(item).map(|i| &mut i.kind) {
                    m.interpret.frame_rate = Some(FrameRate::FPS_25);
                }
                Ok(())
            })
            .unwrap();
        },
        "a target clip changed",
    );
    stale(
        "effects",
        |f| {
            f.s.edit_sequence("Disable", |q, _, _| {
                q.video_tracks[0].items[0].enabled = false;
                Ok(())
            })
            .unwrap();
        },
        "a target clip changed",
    );
}

/// Opening another project stops its jobs on the helper.
#[test]
fn switching_projects_cancels_running_jobs() {
    let mut f = fixture("switchcancel", Script { steps: 100_000, ..Default::default() });
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    f.s.execute("file.newProject", json!({"name": "Other"})).unwrap();
    super::poll(&mut f.s);
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert_eq!(f.helper.cancelled(), ["j1"]);
}

/// A cancel that arrives after the result is in says so, and nothing is applied.
#[test]
fn a_late_cancel_reports_the_kept_result() {
    let mut f = fixture("latecancel", Script::default());
    let id = f.s.execute("ai.echo", json!({})).unwrap()["job"].as_u64().unwrap();
    ready(&f.s, id);
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    super::poll(&mut f.s);
    let e = job_error(&f.s, id).unwrap();
    assert!(e.starts_with("stopped; the result was not applied (kept in"), "{e}");
    assert_eq!(markers(&f.s), 0);
}

/// Project ids come from project files: anything but a UUID is replaced before it names a folder.
#[test]
fn hostile_project_ids_never_leave_the_cache() {
    let mut f = fixture("hostile", Script::default());
    let root = f.cache.join("AI");
    for bad in ["", "/etc", "../../escape", "..", "a/b", "0b8e7d2c-5f1a-4c3e-9d6b-2a7f8e1c4b9/", "C:\\x", &"f".repeat(5000)] {
        assert!(super::project_cache_dir(&f.s, bad).is_err(), "{bad:?}");
        std::sync::Arc::make_mut(&mut f.s.project).id = bad.to_string();
        let id = super::ensure_project_id(&mut f.s);
        assert!(super::is_uuid(&id) && id != bad, "{bad:?} -> {id}");
        let dir = super::project_cache_dir(&f.s, &id).unwrap();
        assert_eq!(dir.parent(), Some(root.as_path()));
    }
    let r = f.s.execute("ai.echo", json!({"wait": true})).unwrap();
    assert!(PathBuf::from(r["outDir"].as_str().unwrap()).starts_with(&root), "{r}");
}

/// Giving the project its id neither rewrites the undo history nor breaks a held undo mark; undo
/// to a state from before keeps the id.
#[test]
fn the_project_id_leaves_history_alone() {
    let mut f = fixture("history", Script::default());
    f.s.execute("sequence.addEdit", json!({"seconds": 1.0})).unwrap();
    let mark = f.s.undo_mark();
    let snap = f.s.history.undo.last().unwrap().1.clone();
    let id = super::ensure_project_id(&mut f.s);
    assert_eq!(f.s.undo_mark(), mark);
    assert!(std::sync::Arc::ptr_eq(&snap, &f.s.history.undo.last().unwrap().1));
    assert!(snap.id.is_empty(), "the snapshot is untouched");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(f.s.project.id, id);
    f.s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(f.s.project.id, id);
}

/// The cache stays under its cap: oldest finished job folders go first; running ones and young
/// ones stay.
#[test]
fn the_cache_is_bounded() {
    let root = cache_dir("prune").join("AI");
    let proj = root.join("0b8e7d2c-5f1a-4c3e-9d6b-2a7f8e1c4b90");
    let mut dirs = Vec::new();
    for k in 0..4 {
        let d = proj.join(format!("echo-{k}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("r.bin"), vec![0u8; 1000]).unwrap();
        dirs.push(d);
        std::thread::sleep(Duration::from_millis(20));
    }
    // under the cap: nothing goes
    assert_eq!(super::prune_cache(&root, 4000, &[], Duration::ZERO), 0);
    // over it: the oldest finished folder goes, the running oldest-but-one is kept
    let n = super::prune_cache(&root, 2500, &[dirs[1].clone()], Duration::ZERO);
    assert_eq!(n, 2);
    assert!(!dirs[0].exists() && dirs[1].exists() && !dirs[2].exists() && dirs[3].exists());
    // young folders are never pruned
    assert_eq!(super::prune_cache(&root, 0, &[], Duration::from_secs(600)), 0);
    // a running job of another instance keeps its folder through its heartbeat, even when the
    // folder itself has not changed for a while
    let other = proj.join("echo-other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(super::HEARTBEAT), b"running").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(other.join(super::HEARTBEAT), b"running").unwrap();
    assert_eq!(super::prune_cache(&root, 0, &[], Duration::from_millis(200)), 2, "the two stale folders go");
    assert!(other.exists() && !dirs[1].exists() && !dirs[3].exists());
}

/// A finished job leaves no heartbeat behind; an applied job's folder goes, one whose result was
/// not applied stays (in the prunable cache).
#[test]
fn the_heartbeat_ends_with_the_job() {
    let mut f = fixture("beat", Script::default());
    let r = f.s.execute("ai.echo", json!({"wait": true})).unwrap();
    assert!(!PathBuf::from(r["outDir"].as_str().unwrap()).exists(), "applied: the job folder is removed");
    let _ = f.s.execute("ai.echo", json!({"wait": true, "failStep": 2}));
    let dir = PathBuf::from(&f.helper.started()[1].out_dir);
    assert!(dir.join("echo.json").exists() && !dir.join(super::HEARTBEAT).exists());
}

/// Reconnecting while a probe is in flight: one probe at a time, the old client's answer is
/// dropped, and no old capabilities show for the new connection.
#[test]
fn repeated_connects_keep_one_probe_and_no_stale_capabilities() {
    let mut f = fixture("reconnect", Script::default());
    f.s.ai.background = true;
    let slow =
        FakeHelper::start(Script { features: vec!["echo".into(), "reframe".into()], hello_delay: Duration::from_millis(400), ..Default::default() }).unwrap();
    let fast = FakeHelper::start(Script { features: vec!["echo".into()], ..Default::default() }).unwrap();
    f.s.execute("ai.connect", json!({"port": slow.port()})).unwrap();
    assert!(f.s.ai.hello.is_none(), "the old capabilities are dropped at once");
    assert_eq!(reason(&f.s, "ai.echo"), "contacting the AI helper…");
    for _ in 0..3 {
        f.s.execute("ai.connect", json!({"port": fast.port()})).unwrap();
        assert!(f.s.ai.hello.is_none());
    }
    let t0 = std::time::Instant::now();
    while f.s.ai.probe.is_some() || f.s.ai.hello.is_none() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the probes never settled");
        std::thread::sleep(Duration::from_millis(10));
        super::poll(&mut f.s);
    }
    let st = f.s.execute("ai.status", json!({})).unwrap();
    // `ai.status` in background mode probes again; settle that one too
    while f.s.ai.probe.is_some() {
        std::thread::sleep(Duration::from_millis(10));
        super::poll(&mut f.s);
    }
    assert_eq!(f.s.ai.port, fast.port());
    assert_eq!(f.s.ai.hello.as_ref().unwrap().as_ref().unwrap().features, ["echo"], "{st}");
    assert!(reason(&f.s, "ai.reframe").contains("does not provide `reframe`"), "the slow helper's reframe never shows");
    assert_eq!(slow.hellos(), 1);
    assert_eq!(fast.hellos(), 2, "one probe after the slow one, one for ai.status: never one per connect");
}

/// A background host asks the helper again after a while: features that became ready later (a
/// model downloaded or converted) show up without a click.
#[test]
fn a_background_host_reprobes() {
    let mut f = fixture("reprobe", Script::default());
    f.s.ai.background = true;
    f.s.ai.last_probe = Some(web_time::Instant::now() - super::REPROBE - Duration::from_secs(1));
    let before = f.helper.hellos();
    super::poll(&mut f.s);
    assert!(f.s.ai.probe.is_some(), "a probe started");
    let t0 = std::time::Instant::now();
    while f.s.ai.probe.is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
        super::poll(&mut f.s);
    }
    assert_eq!(f.helper.hellos(), before + 1);
    super::poll(&mut f.s);
    assert!(f.s.ai.probe.is_none(), "not again until REPROBE has passed");
}

/// Moving a result across volumes: a copy in pieces with progress; one that stops (or fails)
/// removes what it wrote and keeps the source.
#[test]
fn a_cross_volume_copy_reports_progress_and_cleans_up() {
    let d = cache_dir("copy");
    std::fs::create_dir_all(&d).unwrap();
    let (from, to) = (d.join("big.wav"), d.join("moved.wav"));
    let data: Vec<u8> = (0..(9u32 << 20)).map(|i| i as u8).collect();
    std::fs::write(&from, &data).unwrap();
    let seen = std::cell::Cell::new(0u64);
    let e = super::copy_and_remove(&from, &to, &|| seen.get() > 0, &mut |n| seen.set(n)).unwrap_err();
    assert_eq!(e.to_string(), "stopped");
    assert_eq!(seen.get(), 8 << 20, "stopped after the first piece");
    assert!(!to.exists(), "the partial copy is removed");
    assert_eq!(std::fs::read(&from).unwrap(), data, "the source is kept");
    let mut last = 0;
    super::copy_and_remove(&from, &to, &|| false, &mut |n| last = n).unwrap();
    assert_eq!(last, data.len() as u64);
    assert!(!from.exists());
    assert_eq!(std::fs::read(&to).unwrap(), data);
    let _ = std::fs::remove_dir_all(&d);
}

/// Staging that fails part way moves the files already moved back and removes the media folder.
#[test]
fn a_failed_stage_rolls_back() {
    let d = cache_dir("stage");
    let (cache, media) = (d.join("cache"), d.join("AI Media").join("stems").join("job"));
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("a.wav"), b"a").unwrap();
    let files = [cache.join("a.wav"), cache.join("missing.wav")];
    assert!(super::stage(&files, &media, &|| false, &mut |_| {}).is_err());
    assert_eq!(std::fs::read(cache.join("a.wav")).unwrap(), b"a");
    assert!(!d.join("AI Media").join("stems").exists(), "the media folders are removed");
    let moved = super::stage(&files[..1], &media, &|| false, &mut |_| {}).unwrap();
    assert_eq!(moved, [media.join("a.wav")]);
    let _ = std::fs::remove_dir_all(&d);
}

/// A preparation stopped part way (after writing at least one chunk) leaves no partial input.
#[test]
fn a_stopped_preparation_removes_its_partial_input() {
    let mut f = fixture("prep-stop", Script::default());
    let clips = [f.clip];
    let prepare: super::Prepare = Box::new(|dir: &Path, stop: &dyn Fn() -> bool, _: &mut dyn FnMut(f64)| {
        use std::io::Write;
        let mut w = std::fs::File::create(dir.join("source.wav")).map_err(|e| e.to_string())?;
        while !stop() {
            w.write_all(&[0u8; 4096]).map_err(|e| e.to_string())?;
            w.flush().map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Err("stopped".into())
    });
    let spec = super::JobSpec {
        kind: "echo",
        label: "AI Echo",
        clips: &clips,
        params: json!({}),
        context: Value::Null,
        timeout: None,
        prepare: Some(prepare),
        loose: false,
        media: false,
    };
    let r = super::start_job(&mut f.s, spec, false).unwrap();
    let id = r["job"].as_u64().unwrap();
    let input = PathBuf::from(r["outDir"].as_str().unwrap()).join("source.wav");
    let t0 = std::time::Instant::now();
    while std::fs::metadata(&input).map_or(0, |m| m.len()) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "the preparation wrote nothing");
        std::thread::sleep(Duration::from_millis(2));
    }
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert!(!input.exists(), "the partial input is removed");
    assert!(f.helper.started().is_empty(), "the helper was never asked");
}

/// The helper's feature states: an unavailable feature is disabled with the helper's reason; one
/// whose model needs a download is offered, and ai.status says which.
#[test]
fn feature_states_reach_status_and_reasons() {
    let states = json!({
        "stems": {"state": "unavailable", "reason": "spleeter_4stems.onnx is not installed; run tools/convert_spleeter.sh"},
        "reframe": {"state": "needs download"},
        "echo": {"state": "ready"},
    });
    let f = fixture("states", Script { features: vec!["echo".into(), "reframe".into()], states, ..Default::default() });
    let st = super::status_json(&f.s);
    assert_eq!(st["featureStates"]["reframe"]["state"], "needs download", "{st}");
    assert_eq!(st["featureStates"]["stems"]["state"], "unavailable");
    let why = reason(&f.s, "ai.stems");
    assert!(why.contains("stems is unavailable in the AI helper") && why.contains("convert_spleeter"), "{why}");
    assert!(super::helper_ready(&f.s, "reframe").is_ok(), "offered: the job downloads the model");
}
