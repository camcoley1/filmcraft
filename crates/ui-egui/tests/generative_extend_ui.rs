//! Clip ▸ AI ▸ Generative Extend from the menu asks first (a paid hosted service): the dialog says
//! what is sent, to whom and the estimated cost; Cancel sends nothing; Generate runs the command
//! with `confirm: true` (checked on the scripted helper); "Don't ask again for this project" skips
//! the dialog afterwards.
#![cfg(feature = "sidecar")]

use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_sidecar::fake::{FakeHelper, ResultMode, Script};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn new(session: Session) -> Self {
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        d
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(r) = reply.try_recv() {
                assert_eq!(r["ok"], true, "{method} {params}: {r}");
                return r["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }
    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }
    fn labels(&mut self, prefix: &str) -> String {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.to_string()
    }
}

/// A sequence of colour bars with its video clip selected, connected to a scripted helper whose
/// jobs fail at once (nothing is generated; the test is about what is asked for).
fn session(tag: &str) -> (Session, FakeHelper) {
    let dir = std::env::temp_dir().join(format!("fc-extend-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Session::default();
    let id = s.execute("file.newBarsAndTone", json!({"seconds": 2})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequenceFromClip", json!({"items": [id]})).unwrap();
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
    s.state.selection = vec![clip];
    s.ai.cache_root = Some(dir.join("cache"));
    s.ai.media_root = Some(dir.join("media"));
    let helper = FakeHelper::start(Script {
        features: vec!["generativeExtend".into()],
        result: ResultMode::Fail("the scripted helper makes no video".into()),
        ..Default::default()
    })
    .unwrap();
    s.execute("ai.connect", json!({"port": helper.port()})).unwrap();
    (s, helper)
}

fn wait_started(helper: &FakeHelper, n: usize) {
    let t0 = std::time::Instant::now();
    while helper.started().len() < n {
        assert!(t0.elapsed() < Duration::from_secs(20), "the helper got {} jobs, not {n}", helper.started().len());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_menu_asks_first_and_generate_confirms() {
    let (s, helper) = session("confirm");
    let mut d = Driver::new(s);
    // the menu opens the dialog; nothing is sent yet
    let r = d.ok("ui.menu.invoke", json!({"id": "ai.generativeExtend"}));
    assert_eq!(r["dialog"], "generativeExtend", "{r}");
    d.frames(3);
    let ids = d.ids("generativeExtend.");
    for id in ["seconds", "side.end", "side.start", "prompt", "dontAskAgain", "cancel", "ok"] {
        assert!(ids.contains(&format!("generativeExtend.{id}")), "no {id} in {ids:?}");
    }
    assert!(d.labels("generativeExtend.ok").contains("Generate"));
    assert!(helper.started().is_empty());
    // Cancel: closed, nothing sent
    d.ok("ui.click", json!({"id": "generativeExtend.cancel"}));
    d.frames(3);
    assert!(d.ids("generativeExtend.").is_empty());
    assert!(helper.started().is_empty());
    // again, start side, then Generate: the command runs with confirm: true
    d.ok("ui.menu.invoke", json!({"id": "ai.generativeExtend"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "generativeExtend.side.start"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "generativeExtend.ok"}));
    d.frames(3);
    wait_started(&helper, 1);
    let req = &helper.started()[0];
    assert_eq!(req.kind, "generativeExtend");
    assert_eq!((req.params["side"].as_str(), req.params["seconds"].as_f64()), (Some("start"), Some(3.0)));
    assert!(req.inputs.len() == 1 && req.inputs[0].ends_with("frame.png"), "the frame was made: {:?}", req.inputs);
    assert!(d.ids("generativeExtend.").is_empty(), "the dialog closed");
    assert!(!d.harness.state().session.project.settings.generative_extend_no_confirm);
}

#[test]
fn dont_ask_again_skips_the_dialog_in_this_project() {
    let (s, helper) = session("dont-ask");
    let mut d = Driver::new(s);
    d.ok("ui.menu.invoke", json!({"id": "ai.generativeExtend"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "generativeExtend.dontAskAgain"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "generativeExtend.ok"}));
    d.frames(3);
    wait_started(&helper, 1);
    assert!(d.harness.state().session.project.settings.generative_extend_no_confirm);
    // the job failed in the helper; the next menu click runs at once, without the dialog
    for _ in 0..200 {
        d.frames(1);
    }
    let r = d.ok("ui.menu.invoke", json!({"id": "ai.generativeExtend"}));
    assert!(r.get("dialog").is_none(), "{r}");
    wait_started(&helper, 2);
    assert!(d.ids("generativeExtend.").is_empty());
}
