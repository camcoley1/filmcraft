//! Lumetri node graph and grade version commands: each edit is one undo step, Lumetri controls
//! edit the selected node, grades copy between clips, and hostile parameters are refused.

use serde_json::{Value, json};

use crate::Session;
use filmcraft_project::{ClipId, ParamValue, color_graph as cg};

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

/// Two V1 clips: the first under the playhead (selected), and the next one.
fn clips(s: &mut Session) -> (ClipId, ClipId) {
    let q = s.active_sequence().unwrap();
    let items = &q.video_tracks[0].items;
    let first = q.video_tracks[0].item_at(s.playhead()).unwrap().id;
    let other = items.iter().find(|i| i.id != first).unwrap().id;
    s.state.selection = vec![first];
    (first, other)
}

fn lumetri(s: &Session, c: ClipId) -> filmcraft_project::EffectInstance {
    let q = s.active_sequence().unwrap();
    q.find_item(c).unwrap().1.effects.iter().find(|e| e.effect == "lumetri").unwrap().clone()
}

fn exposure(s: &Session, c: ClipId) -> f64 {
    lumetri(s, c).param("exposure").and_then(|p| p.value.as_f64()).unwrap()
}

fn set_exposure(s: &mut Session, c: ClipId, v: f64) {
    let q = s.active_sequence().unwrap();
    let idx = q.find_item(c).unwrap().1.effects.iter().position(|e| e.effect == "lumetri").unwrap();
    s.execute("effects.setParam", json!({"clip": c.0, "effect": idx, "param": "exposure", "value": v})).unwrap();
}

fn nodes(v: &Value) -> usize {
    v["nodes"].as_array().map_or(0, Vec::len)
}

#[test]
fn nodes_are_added_mixed_bypassed_moved_and_removed_with_undo() {
    let mut s = demo();
    let (c, _) = clips(&mut s);
    // a clip without Lumetri lists no nodes; the first node command adds Lumetri
    assert_eq!(nodes(&s.execute("color.nodes", json!({})).unwrap()), 0);
    let r = s.execute("color.addNode", json!({"label": "warm"})).unwrap();
    assert_eq!(nodes(&r), 2);
    assert_eq!(r["nodes"][1]["selected"], true);
    assert_eq!(r["nodes"][1]["label"], "warm");
    // Lumetri's own parameters are the selected node's
    set_exposure(&mut s, c, 1.0);
    s.execute("color.selectNode", json!({"node": 0})).unwrap();
    assert_eq!(exposure(&s, c), 0.0);
    s.execute("color.selectNode", json!({"node": 1})).unwrap();
    assert_eq!(exposure(&s, c), 1.0);
    let r = s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.4})).unwrap();
    assert_eq!(r["nodes"][1]["mix"], 0.4);
    let r = s.execute("color.bypassNode", json!({"node": 0})).unwrap();
    assert_eq!(r["nodes"][0]["enabled"], false);
    let r = s.execute("color.bypassNode", json!({"node": 0, "bypass": false})).unwrap();
    assert_eq!(r["nodes"][0]["enabled"], true);
    let r = s.execute("color.moveNode", json!({"from": 1, "to": 0})).unwrap();
    assert_eq!((r["nodes"][0]["label"].as_str(), r["nodes"][0]["selected"].as_bool()), (Some("warm"), Some(true)));
    s.execute("color.setNodeLabel", json!({"node": 0, "label": "Warm look"})).unwrap();
    // every step undoes on its own
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.execute("color.nodes", json!({})).unwrap()["nodes"][0]["label"], "warm");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.execute("color.nodes", json!({})).unwrap()["nodes"][1]["label"], "warm", "the move undone");
    let r = s.execute("color.removeNode", json!({"node": 1})).unwrap();
    assert_eq!(nodes(&r), 1);
    assert!(s.execute("color.removeNode", json!({"node": 0})).is_err(), "the last node stays");
}

#[test]
fn versions_switch_rename_delete_and_reset() {
    let mut s = demo();
    let (c, _) = clips(&mut s);
    s.execute("color.addNode", json!({})).unwrap();
    set_exposure(&mut s, c, 0.5);
    let r = s.execute("color.version.add", json!({})).unwrap();
    assert_eq!((r["versions"].clone(), r["active"].clone()), (json!(["A", "B"]), json!(1)));
    assert_eq!(nodes(&r), 2, "a duplicate of A");
    set_exposure(&mut s, c, -0.5);
    s.execute("color.version.switch", json!({"name": "A"})).unwrap();
    assert_eq!(exposure(&s, c), 0.5);
    s.execute("color.version.switch", json!({"version": 1})).unwrap();
    assert_eq!(exposure(&s, c), -0.5);
    let r = s.execute("color.version.add", json!({"name": "Night", "duplicate": false})).unwrap();
    assert_eq!(nodes(&r), 1);
    s.execute("color.version.rename", json!({"version": 0, "name": "Day"})).unwrap();
    let r = s.execute("color.version.delete", json!({})).unwrap();
    assert_eq!(r["versions"], json!(["Day", "B"]));
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.execute("color.nodes", json!({})).unwrap()["versions"], json!(["Day", "B", "Night"]));
    let r = s.execute("color.resetGrade", json!({})).unwrap();
    assert_eq!(nodes(&r), 1);
    assert_eq!(exposure(&s, c), 0.0);
    assert!(s.execute("color.version.switch", json!({"name": "nope"})).is_err());
}

#[test]
fn grades_copy_and_paste_between_clips() {
    let mut s = demo();
    let (c, other) = clips(&mut s);
    assert!(s.execute("color.pasteGrade", json!({"clips": [other.0]})).is_err(), "nothing copied yet");
    s.execute("color.addNode", json!({"label": "two"})).unwrap();
    set_exposure(&mut s, c, 0.75);
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.5})).unwrap();
    s.execute("color.copyGrade", json!({})).unwrap();
    // the other clip has no Lumetri: pasting adds it, in one undo step
    let r = s.execute("color.pasteGrade", json!({"clips": [other.0]})).unwrap();
    assert_eq!(r["nodes"], 2);
    let (a, b) = (lumetri(&s, c), lumetri(&s, other));
    let (na, nb) = (cg::active_nodes(&a), cg::active_nodes(&b));
    assert_eq!(na.len(), nb.len());
    for (x, y) in na.iter().zip(&nb) {
        assert_eq!((x.params, x.mix, x.enabled), (y.params, y.mix, y.enabled));
    }
    s.execute("edit.undo", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    assert!(!q.find_item(other).unwrap().1.effects.iter().any(|e| e.effect == "lumetri"), "one undo removes the pasted Lumetri");
}

#[test]
fn hostile_parameters_are_refused() {
    let mut s = demo();
    clips(&mut s);
    s.execute("color.addNode", json!({})).unwrap();
    for (id, p) in [
        ("color.selectNode", json!({"node": 9})),
        ("color.selectNode", json!({"node": -1})),
        ("color.selectNode", json!({"node": "x"})),
        ("color.selectNode", json!({})),
        ("color.setNodeMix", json!({"node": 0, "mix": "half"})),
        ("color.setNodeMix", json!({"node": 0})),
        ("color.moveNode", json!({"from": 0, "to": 5})),
        ("color.removeNode", json!({"node": 1e30})),
        ("color.setNodeMask", json!({"node": 0, "mask": "no such mask"})),
        ("color.version.switch", json!({"version": 3})),
        ("color.version.rename", json!({"name": "   "})),
        ("color.setNodeLabel", json!({"node": 0})),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p}");
    }
    // mix out of range clamps
    assert_eq!(s.execute("color.setNodeMix", json!({"node": 0, "mix": 7.5})).unwrap()["nodes"][0]["mix"], 1.0);
    // many nodes / versions: capped
    for _ in 0..40 {
        let _ = s.execute("color.addNode", json!({}));
    }
    assert_eq!(nodes(&s.execute("color.nodes", json!({})).unwrap()), cg::MAX_NODES);
    let _ = ParamValue::Bool(true);
}
