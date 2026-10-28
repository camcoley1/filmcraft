//! `ai.*` against the scripted fake helper (`filmcraft_sidecar::fake`): handshake, the echo job
//! end to end, cancel, malformed replies, version mismatch, bad result files, a helper that dies,
//! stale results and the atomic apply.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{DemoScene, Generator, MediaSource};
use filmcraft_project::{ClipId, ItemId, ItemKind, Label, MediaClip, MediaRef, SequenceSettings, TrackKind};
use filmcraft_sidecar::Limits;
use filmcraft_sidecar::fake::{FakeHelper, Malformed, ResultMode, Script};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::json;

use super::NOT_CONNECTED;
use crate::{Event, Session};

const FEATURES: [&str; 9] =
    ["ai.reframe", "ai.stems", "ai.enhanceSpeech", "ai.beatGrid", "ai.musicFit", "ai.search", "ai.upscale", "ai.interpolate", "ai.generativeExtend"];

struct Fixture {
    s: Session,
    helper: FakeHelper,
    clip: ClipId,
    item: ItemId,
    cache: PathBuf,
}

fn cache_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("filmcraft-ai-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A session with one 2 s clip of `/media/interview.mov` on V1 (selected), connected to a fake
/// helper running `script`.
fn fixture(name: &str, script: Script) -> Fixture {
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
    s.ai.limits = Limits { poll_interval: Duration::from_millis(5), max_poll_failures: 3, io_timeout: Duration::from_secs(2), ..Default::default() };
    let helper = FakeHelper::start(script).unwrap();
    let _ = s.execute("ai.connect", json!({"port": helper.port()}));
    Fixture { s, helper, clip, item, cache }
}

/// Wait for job `id` to finish, then apply results.
fn finish(s: &mut Session, id: u64) {
    let t0 = std::time::Instant::now();
    loop {
        let done = s.jobs.iter().find(|j| j.id == id).is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        if done {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "job {id} never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    super::poll(s);
}

fn job_error(s: &Session, id: u64) -> Option<String> {
    let j = s.jobs.iter().find(|j| j.id == id)?;
    j.result.lock().unwrap().clone()?.err()
}

/// The project as JSON (motion's automatic position is NaN, so `Project` never equals itself).
fn snapshot(s: &Session) -> serde_json::Value {
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

fn reason(s: &Session, id: &str) -> String {
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
    let mut f = fixture("hello", Script { features: vec!["echo".into(), "reframe".into()], ..Default::default() });
    let st = f.s.execute("ai.status", json!({})).unwrap();
    assert_eq!(st["connected"], true, "{st}");
    assert_eq!(st["features"], json!(["echo", "reframe"]));
    assert_eq!(reason(&f.s, "ai.reframe"), "not built yet (arrives in M6a)");
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
    assert_eq!(job_error(&f.s, id), None);
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
    assert_eq!(job_error(&f.s, id), None, "{name}: the job itself succeeded");
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
    stale(
        "newproject",
        |f| {
            f.s.execute("file.newProject", json!({"name": "Other", "force": true})).unwrap();
        },
        "another project was opened",
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
    f.s.execute("ai.echo", json!({"wait": true, "failStep": 2})).unwrap();
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
    // written as schema 16, the last without the id
    let mut v: serde_json::Value = serde_json::from_slice(&filmcraft_format::encode(&f.s.project, false)).unwrap();
    v["schema_version"] = json!(16);
    std::fs::write(&old, serde_json::to_vec(&v).unwrap()).unwrap();
    let r = f.s.execute("file.open", json!({"path": old.to_string_lossy()})).unwrap();
    assert_eq!(r["migrated"], true);
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
    assert!(toasts(&mut f.s).iter().any(|m| m.contains("another project was opened")));
}
