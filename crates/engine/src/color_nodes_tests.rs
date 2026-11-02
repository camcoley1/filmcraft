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

#[test]
fn node_commands_are_atomic_and_need_lumetri() {
    let mut s = demo();
    let (c, _) = clips(&mut s);
    let has = |s: &Session| s.active_sequence().unwrap().find_item(c).unwrap().1.effects.iter().any(|e| e.effect == "lumetri");
    // commands that edit an existing grade refuse a clip without Lumetri and add nothing
    for (id, p) in [
        ("color.removeNode", json!({"node": 5})),
        ("color.selectNode", json!({"node": 0})),
        ("color.setNodeMix", json!({"node": 0, "mix": 0.5})),
        ("color.version.delete", json!({})),
        ("color.resetGrade", json!({})),
    ] {
        assert!(s.execute(id, p).is_err(), "{id}");
        assert!(!has(&s), "{id} left no Lumetri behind");
    }
    // adding a node to a clip without Lumetri is one undo step, Lumetri included
    s.execute("color.addNode", json!({})).unwrap();
    assert!(has(&s));
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!has(&s), "one undo removes the node and the Lumetri it needed");
    // a failing add (after = not an index) changes nothing either
    assert!(s.execute("color.addNode", json!({"after": "x"})).is_err());
    assert!(!has(&s));
    s.execute("color.version.add", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!has(&s));
}

#[test]
fn versions_are_addressed_by_index_or_name() {
    let mut s = demo();
    clips(&mut s);
    s.execute("color.version.add", json!({})).unwrap();
    s.execute("color.version.add", json!({})).unwrap();
    // C is active: deleting by name deletes B, not the active version
    let r = s.execute("color.version.delete", json!({"name": "B"})).unwrap();
    assert_eq!(r["versions"], json!(["A", "C"]));
    assert_eq!(r["active"], 1);
    let r = s.execute("color.version.rename", json!({"version": "A", "name": "Day"})).unwrap();
    assert_eq!((r["versions"].clone(), r["active"].clone()), (json!(["Day", "C"]), json!(1)), "renaming A leaves C active");
    let r = s.execute("color.version.switch", json!({"version": "Day"})).unwrap();
    assert_eq!(r["active"], 0);
    assert!(s.execute("color.version.delete", json!({"version": "Nope"})).is_err());
    assert!(s.execute("color.version.switch", json!({"name": "Nope"})).is_err());
    let r = s.execute("color.version.delete", json!({"version": 1})).unwrap();
    assert_eq!(r["versions"], json!(["Day"]));
}

#[test]
fn clicking_through_nodes_and_dragging_mix_are_one_undo_step_each() {
    let mut s = demo();
    let (c, _) = clips(&mut s);
    s.execute("color.addNode", json!({})).unwrap();
    s.execute("color.addNode", json!({})).unwrap();
    let selected = |s: &mut Session| s.execute("color.nodes", json!({})).unwrap()["nodes"].as_array().unwrap().iter().position(|n| n["selected"] == true);
    assert_eq!(selected(&mut s), Some(2));
    for n in [0, 1, 0, 2, 1] {
        s.execute("color.selectNode", json!({"node": n, "merge": true})).unwrap();
    }
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(selected(&mut s), Some(2), "one undo returns to the node selected before the clicks");
    for (k, m) in [0.9, 0.7, 0.5, 0.3].into_iter().enumerate() {
        s.execute("color.setNodeMix", json!({"node": 2, "mix": m, "merge": true, "begin": k == 0})).unwrap();
    }
    assert_eq!(cg::active_nodes(&lumetri(&s, c))[2].mix, 0.3);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(cg::active_nodes(&lumetri(&s, c))[2].mix, 1.0, "the whole drag undoes at once");
}

#[test]
fn node_masks_follow_mask_renames_and_removal() {
    let mut s = demo();
    let (c, _) = clips(&mut s);
    s.execute("color.addNode", json!({})).unwrap();
    let li = s.active_sequence().unwrap().find_item(c).unwrap().1.effects.iter().position(|e| e.effect == "lumetri").unwrap();
    let r = s.execute("masks.add", json!({"clip": c.0, "effect": li, "shape": "ellipse"})).unwrap();
    let name = r["name"].as_str().unwrap().to_string();
    s.execute("color.setNodeMask", json!({"node": 1, "mask": name})).unwrap();
    s.execute("masks.set", json!({"clip": c.0, "effect": li, "mask": 0, "name": "Face"})).unwrap();
    assert_eq!(cg::active_nodes(&lumetri(&s, c))[1].mask, Some("Face"));
    s.execute("masks.remove", json!({"clip": c.0, "effect": li, "mask": 0})).unwrap();
    assert_eq!(cg::active_nodes(&lumetri(&s, c))[1].mask, None, "no dangling name a later mask could take over");
}

/// Keyframe upkeep: a keyframed node that is not selected must come out of every edit exactly as
/// it does when it is the selected node (whose parameters are the effect's own). Each case also
/// says whether the edit moves the keyframes at all (`moves`), so every case checks something:
/// the moving ones fail if the other nodes stop following; speed changes and slips keep keyframes
/// in media time for every node and must leave them where they were.
#[test]
fn keyframes_of_other_nodes_follow_every_edit() {
    use filmcraft_time::{FrameRate, Tick};
    type Edit = fn(&mut Session, ClipId, ClipId) -> ClipId;
    let edits: [(&str, bool, Edit); 8] = [
        ("paste attributes", true, |s, a, b| {
            s.execute("timeline.select", json!({"clips": [a.0]})).unwrap();
            s.execute("edit.copy", json!({})).unwrap();
            s.execute("edit.pasteAttributes", json!({"clips": [b.0]})).unwrap();
            b
        }),
        ("paste grade", true, |s, a, b| {
            s.execute("color.copyGrade", json!({"clip": a.0})).unwrap();
            s.execute("color.pasteGrade", json!({"clips": [b.0]})).unwrap();
            b
        }),
        ("preset with keyframes", true, |s, a, b| {
            let li = s.active_sequence().unwrap().find_item(a).unwrap().1.effects.iter().position(|e| e.effect == "lumetri").unwrap();
            s.execute("presets.save", json!({"clip": a.0, "name": "grade-k", "keyframes": "anchorIn", "effects": [li]})).unwrap();
            s.execute("presets.apply", json!({"preset": "grade-k", "clips": [b.0]})).unwrap();
            b
        }),
        ("preset without keyframes (baked)", true, |s, a, b| {
            let li = s.active_sequence().unwrap().find_item(a).unwrap().1.effects.iter().position(|e| e.effect == "lumetri").unwrap();
            s.execute("presets.save", json!({"clip": a.0, "name": "grade-still", "keyframes": "none", "effects": [li]})).unwrap();
            s.execute("presets.apply", json!({"preset": "grade-still", "clips": [b.0]})).unwrap();
            b
        }),
        ("media start shift (relink / project manager)", true, |s, a, _| {
            let item = s.active_sequence().unwrap().find_item(a).unwrap().1.item;
            s.edit("Shift", |p, _| {
                p.shift_media_time(item, Tick(254_016_000_000 / 4));
                Ok(())
            })
            .unwrap();
            a
        }),
        ("source timecode rebase (interchange)", true, |s, a, _| {
            let item = s.active_sequence().unwrap().find_item(a).unwrap().1.item;
            s.edit("Rebase", |p, _| {
                filmcraft_interchange::rebase_source_timecode(p, item, 48, FrameRate::FPS_24);
                Ok(())
            })
            .unwrap();
            a
        }),
        ("speed change", false, |s, a, _| {
            s.execute("clip.speedDuration", json!({"clips": [a.0], "speed": 200})).unwrap();
            a
        }),
        ("slip", false, |s, a, _| {
            s.execute("timeline.slip", json!({"clip": a.0, "deltaFrames": 12})).unwrap();
            a
        }),
    ];
    // node 0 keyframed (exposure 0 -> 1 over the clip); `keyed_selected`: node 0 is the selected
    // one. Returns the keyframes (time, value) before and after the edit, plus the static value.
    type Keys = Vec<(Tick, Option<f64>)>;
    let run = |edit: Edit, keyed_selected: bool| -> (Keys, Keys) {
        let mut s = demo();
        let (a, b) = clips(&mut s);
        s.execute("color.addNode", json!({"clip": a.0})).unwrap();
        s.execute("color.selectNode", json!({"clip": a.0, "node": 0})).unwrap();
        let it = s.active_sequence().unwrap().find_item(a).unwrap().1.clone();
        let (k1, k2) = (it.start + Tick(254_016_000_000 / 2), it.end() - Tick(254_016_000_000));
        s.set_playhead(k1);
        let k1 = s.playhead();
        let li = it.effects.iter().position(|e| e.effect == "lumetri").unwrap();
        s.execute("effects.toggleAnimation", json!({"clip": a.0, "effect": li, "param": "exposure", "time": k1.0})).unwrap();
        s.execute("effects.setParam", json!({"clip": a.0, "effect": li, "param": "exposure", "value": 0.0, "time": k1.0})).unwrap();
        s.execute("effects.setParam", json!({"clip": a.0, "effect": li, "param": "exposure", "value": 1.0, "time": k2.0})).unwrap();
        let keys = |s: &Session, c: ClipId| -> Keys {
            let e = lumetri(s, c);
            let p = e.param("exposure").unwrap();
            let mut out: Keys = p.keyframes.iter().map(|k| (k.time, k.value.as_f64())).collect();
            out.push((Tick(-1), p.value.as_f64()));
            out
        };
        let before = keys(&s, a);
        assert_eq!(before.len(), 3, "two keyframes set");
        if !keyed_selected {
            s.execute("color.selectNode", json!({"clip": a.0, "node": 1})).unwrap();
        }
        let c = edit(&mut s, a, b);
        s.execute("color.selectNode", json!({"clip": c.0, "node": 0})).unwrap();
        (before, keys(&s, c))
    };
    for (name, moves, edit) in edits {
        let (before, selected) = run(edit, true);
        let (_, other) = run(edit, false);
        assert_eq!(other, selected, "{name}: the unselected node's keyframes went their own way");
        assert_eq!(selected != before, moves, "{name}: keyframes {} expected to move: {before:?} -> {selected:?}", if moves { "were" } else { "were not" });
    }
}

/// Paste Grade moves the grade's keyframes from the copied clip's media time to the target's.
#[test]
fn paste_grade_shifts_the_keyframes_of_every_node() {
    use filmcraft_time::Tick;
    let mut s = demo();
    let (a, b) = clips(&mut s);
    // b's media starts one second later than a's (trim its tail first so the slip has media to use)
    s.execute("timeline.trim", json!({"clip": b.0, "edge": "out", "deltaFrames": -24})).unwrap();
    s.execute("timeline.slip", json!({"clip": b.0, "deltaFrames": 24})).unwrap();
    let si = |s: &Session, c: ClipId| s.active_sequence().unwrap().find_item(c).unwrap().1.source_in;
    let shift = si(&s, b).0 - si(&s, a).0;
    assert_ne!(shift, 0, "the clips' media start at different times");
    // node 0 keyframed, node 1 selected
    s.execute("color.addNode", json!({"clip": a.0})).unwrap();
    s.execute("color.selectNode", json!({"clip": a.0, "node": 0})).unwrap();
    let li = s.active_sequence().unwrap().find_item(a).unwrap().1.effects.iter().position(|e| e.effect == "lumetri").unwrap();
    let it = s.active_sequence().unwrap().find_item(a).unwrap().1.clone();
    let t = it.start + Tick(254_016_000_000 / 2);
    s.execute("effects.toggleAnimation", json!({"clip": a.0, "effect": li, "param": "exposure", "time": t.0})).unwrap();
    s.execute("effects.setParam", json!({"clip": a.0, "effect": li, "param": "exposure", "value": 0.7, "time": t.0})).unwrap();
    let at_a: Vec<Tick> = lumetri(&s, a).param("exposure").unwrap().keyframes.iter().map(|k| k.time).collect();
    s.execute("color.selectNode", json!({"clip": a.0, "node": 1})).unwrap();
    s.execute("color.copyGrade", json!({"clip": a.0})).unwrap();
    s.execute("color.pasteGrade", json!({"clips": [b.0]})).unwrap();
    s.execute("color.selectNode", json!({"clip": b.0, "node": 0})).unwrap();
    let at_b: Vec<Tick> = lumetri(&s, b).param("exposure").unwrap().keyframes.iter().map(|k| k.time).collect();
    assert_eq!(at_b, at_a.iter().map(|k| Tick(k.0 + shift)).collect::<Vec<_>>(), "a keyframe at media time t lands at t + the start difference");
}

#[test]
fn merging_is_opt_in_and_per_sequence() {
    let mut s = demo();
    let (c, _) = clips(&mut s);
    s.execute("color.addNode", json!({})).unwrap();
    let mix = |s: &Session| cg::active_nodes(&lumetri(s, c))[1].mix;
    // two separate calls without `merge` are two undo steps
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.8})).unwrap();
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.6})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(mix(&s), 0.8);
    // a drag (merge) after them is its own step, and `begin` starts the next drag anew
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.5, "merge": true, "begin": true})).unwrap();
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.4, "merge": true})).unwrap();
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.3, "merge": true, "begin": true})).unwrap();
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.2, "merge": true})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(mix(&s), 0.4, "the second drag undoes alone");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(mix(&s), 0.8, "then the first drag");
    // an unrelated edit between merged edits ends the merge
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.5, "merge": true, "begin": true})).unwrap();
    s.execute("color.setNodeLabel", json!({"node": 0, "label": "Base"})).unwrap();
    s.execute("color.setNodeMix", json!({"node": 1, "mix": 0.1, "merge": true})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(mix(&s), 0.5);
    // version and name together are refused for switch / delete
    s.execute("color.version.add", json!({})).unwrap();
    assert!(s.execute("color.version.switch", json!({"version": 0, "name": "B"})).is_err());
    assert!(s.execute("color.version.delete", json!({"version": 1, "name": "A"})).is_err());
}

#[test]
fn paste_grade_replaces_only_the_active_version_of_the_target() {
    let mut s = demo();
    let (a, b) = clips(&mut s);
    // source: three nodes; target: versions A (exposure 0.3) and B (active, exposure -0.3)
    s.execute("color.addNode", json!({"clip": a.0})).unwrap();
    s.execute("color.addNode", json!({"clip": a.0})).unwrap();
    s.execute("color.version.add", json!({"clip": b.0, "duplicate": false})).unwrap();
    s.execute("color.version.switch", json!({"clip": b.0, "version": 0})).unwrap();
    set_exposure(&mut s, b, 0.3);
    s.execute("color.version.switch", json!({"clip": b.0, "version": 1})).unwrap();
    set_exposure(&mut s, b, -0.3);
    s.execute("color.copyGrade", json!({"clip": a.0})).unwrap();
    let r = s.execute("color.pasteGrade", json!({"clips": [b.0]})).unwrap();
    assert_eq!(r["nodes"], 3);
    let g = s.execute("color.nodes", json!({"clip": b.0})).unwrap();
    assert_eq!((nodes(&g), g["versions"].clone(), g["active"].clone()), (3, json!(["A", "B"]), json!(1)));
    s.execute("color.version.switch", json!({"clip": b.0, "version": 0})).unwrap();
    assert_eq!(exposure(&s, b), 0.3, "version A of the target is kept");
}
