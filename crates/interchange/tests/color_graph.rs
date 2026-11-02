//! Lumetri node graphs (M8d) through interchange: OpenTimelineIO keeps the graph and its grade
//! versions (FilmCraft metadata); FCPXML and FCP7 XML carry one Lumetri per enabled node of the
//! active version (a plain graph renders the same) and report what they cannot keep; a single node
//! round-trips as before.

mod support;

use filmcraft_interchange::{ExportOptions, Format, Report, export, import};
use filmcraft_project::{ParamValue, Project, TrackKind, color_graph as cg, find_effect};
use filmcraft_time::FrameRate;
use support::*;

/// A clip whose Lumetri is `graph(e)`.
fn project(graph: impl Fn(&mut filmcraft_project::EffectInstance)) -> (Project, filmcraft_project::ItemId) {
    let r = FrameRate::FPS_24;
    let mut p = Project::new("Lib");
    let a = media(&mut p, "/m/a.mov", true, false, r);
    let s = sequence(&mut p, "Cut", r, false);
    let c = clip(&mut p, s, TrackKind::Video, 0, a, 0, 48, 0);
    let mut e = find_effect("lumetri").unwrap().instance();
    e.param_mut("exposure").unwrap().value = ParamValue::Float(0.5);
    graph(&mut e);
    p.sequence_mut(s).unwrap().find_item_mut(c).unwrap().1.effects.push(e);
    (p, s)
}

/// Export (keeping the export report) and import back.
fn through(p: &Project, s: filmcraft_project::ItemId, format: Format) -> (filmcraft_interchange::Imported, Report, String) {
    let (bytes, rep) = export(p, s, format, &ExportOptions::default()).expect("export");
    let text = String::from_utf8_lossy(&bytes).to_string();
    let (imp, _) = import(&bytes, format, None).unwrap_or_else(|e| panic!("import: {e}\n{text}"));
    (imp, rep, text)
}

fn two_plain_nodes(e: &mut filmcraft_project::EffectInstance) {
    cg::add_node(e, None, "second").unwrap();
    e.param_mut("exposure").unwrap().value = ParamValue::Float(-0.25);
}

fn lumetris(imp: &filmcraft_interchange::Imported) -> Vec<filmcraft_project::EffectInstance> {
    let s = only_seq(imp);
    imp.project.sequence(s).unwrap().video_tracks[0].items[0].effects.iter().filter(|e| e.effect == "lumetri").cloned().collect()
}

fn exposure(e: &filmcraft_project::EffectInstance) -> f64 {
    e.param("exposure").and_then(|p| p.value.as_f64()).unwrap()
}

#[test]
fn otio_keeps_the_graph_and_its_versions() {
    let (p, s) = project(|e| {
        two_plain_nodes(e);
        cg::set_mix(e, 1, 0.3).unwrap();
        cg::add_version(e, Some("B"), true).unwrap();
    });
    let (imp, _, rep) = roundtrip(&p, s, Format::Otio, &ExportOptions::default());
    assert!(!rep.has_warnings(), "{rep}");
    let want = p.sequence(s).unwrap().video_tracks[0].items[0].effects.iter().find(|e| e.effect == "lumetri").unwrap().clone();
    assert_eq!(lumetris(&imp), vec![want]);
}

#[test]
fn xml_formats_stack_the_nodes_and_say_so() {
    for format in [Format::Fcpxml, Format::Fcp7Xml] {
        // plain nodes: two stacked Lumetri effects, which render the same
        let (p, s) = project(two_plain_nodes);
        let (imp, rep, text) = through(&p, s, format);
        let got = lumetris(&imp);
        assert_eq!(got.len(), 2, "{format:?}: {rep}\n{text}");
        assert_eq!((exposure(&got[0]), exposure(&got[1])), (0.5, -0.25), "{format:?}");
        let msg = rep.to_string();
        assert!(msg.contains("stacked Lumetri effects") && !msg.contains("not kept"), "{format:?}: {msg}");
        // Mix below 100 %: also stacked, and the report says what is lost
        let (p, s) = project(|e| {
            two_plain_nodes(e);
            cg::set_mix(e, 1, 0.5).unwrap();
        });
        let (_, rep, _) = through(&p, s, format);
        assert!(rep.to_string().contains("not kept: node Mix"), "{format:?}: {rep}");
        // a bypassed node: exported as a disabled Lumetri (renders the same), and said so
        let (p, s) = project(|e| {
            two_plain_nodes(e);
            cg::set_enabled(e, 0, false).unwrap();
        });
        let (imp, rep, text) = through(&p, s, format);
        let got = lumetris(&imp);
        assert_eq!(got.iter().map(|e| e.enabled).collect::<Vec<_>>(), vec![false, true], "{format:?}: {text}");
        let msg = rep.to_string();
        assert!(msg.contains("1 of them off") && !msg.contains("not kept"), "{format:?}: {msg}");
        // the effect's own masks are not carried by the stacked effects: reported
        let (p, s) = project(|e| {
            two_plain_nodes(e);
            e.masks.push(filmcraft_project::Mask::new("frame", filmcraft_project::MaskPath::rect(0.0, 0.0, 100.0, 100.0)));
        });
        let (_, rep, _) = through(&p, s, format);
        assert!(rep.to_string().contains("not kept: the effect's masks"), "{format:?}: {rep}");
        // one node: as before, no warning; several versions: only the active one, reported
        let (p, s) = project(|_| {});
        let (imp, rep, _) = through(&p, s, format);
        assert_eq!(lumetris(&imp).len(), 1);
        let msg = rep.to_string();
        assert!(!msg.contains("node graph") && !msg.contains("grade version"), "{format:?}: {msg}");
        let (p, s) = project(|e| {
            cg::add_version(e, None, true).unwrap();
        });
        let (_, rep, _) = through(&p, s, format);
        assert!(rep.to_string().contains("only the active grade version"), "{format:?}: {rep}");
    }
}

#[test]
fn otio_refuses_a_grade_over_the_caps_and_says_so() {
    let (p, s) = project(two_plain_nodes);
    let (bytes, _) = export(&p, s, Format::Otio, &ExportOptions::default()).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    // the node list as written, repeated past the cap
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut done = false;
    fn walk(v: &mut serde_json::Value, done: &mut bool) {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(nodes) = m.get_mut("nodes").and_then(|n| n.as_array_mut())
                    && let Some(first) = nodes.first().cloned()
                {
                    *nodes = vec![first; cg::MAX_NODES + 1];
                    *done = true;
                }
                m.values_mut().for_each(|x| walk(x, done));
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, done)),
            _ => {}
        }
    }
    walk(&mut v, &mut done);
    assert!(done, "the graph is in the OTIO metadata");
    let (imp, rep) = import(&serde_json::to_vec(&v).unwrap(), Format::Otio, None).unwrap();
    assert!(lumetris(&imp).is_empty(), "no grade cut short");
    assert!(rep.to_string().contains("could not be read") && rep.to_string().contains("at most 32"), "{rep}");
}

/// Every object of the JSON tree, depth first.
fn objects(v: &mut serde_json::Value, f: &mut dyn FnMut(&mut serde_json::Map<String, serde_json::Value>)) {
    match v {
        serde_json::Value::Object(m) => {
            f(m);
            m.values_mut().for_each(|x| objects(x, f));
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| objects(x, f)),
        _ => {}
    }
}

#[test]
fn otio_track_and_master_stacks_keep_the_effects_that_read() {
    let (p, s) = project(two_plain_nodes);
    let (bytes, _) = export(&p, s, Format::Otio, &ExportOptions::default()).unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let good = serde_json::to_value(filmcraft_project::find_effect("gaussian_blur").unwrap().instance()).unwrap();
    let mut bad = serde_json::to_value(lumetris(&import(&bytes, Format::Otio, None).unwrap().0).remove(0)).unwrap();
    let node = bad["color"]["versions"][0]["nodes"][0].clone();
    bad["color"]["versions"][0]["nodes"] = serde_json::Value::Array(vec![node; cg::MAX_NODES + 1]);
    let (mut masters, mut tracks) = (0, 0);
    objects(&mut v, &mut |m| {
        if m.contains_key("master_effects") {
            m.insert("master_effects".into(), serde_json::json!([good, bad]));
            masters += 1;
        } else if m.contains_key("volume_db") && m.contains_key("effects") {
            m.insert("effects".into(), serde_json::json!([good, bad]));
            tracks += 1;
        }
    });
    assert!(masters == 1 && tracks > 0, "{masters} {tracks}");
    let (imp, rep) = import(&serde_json::to_vec(&v).unwrap(), Format::Otio, None).unwrap();
    let q = imp.project.sequence(only_seq(&imp)).unwrap();
    // the stacks keep what reads; the grade over the cap is reported, not imported
    assert_eq!(q.master_effects.iter().map(|e| e.effect.as_str()).collect::<Vec<_>>(), vec!["gaussian_blur"]);
    let stacks: Vec<usize> = q.video_tracks.iter().chain(&q.audio_tracks).map(|t| t.effects.len()).collect();
    assert!(stacks.iter().all(|n| *n == 1), "{stacks:?}");
    let msg = rep.to_string();
    assert!(msg.contains("(master)") && msg.contains("track \"") && msg.contains("at most 32"), "{msg}");
}

#[test]
fn a_bypassed_nodes_mix_and_mask_are_reported_as_lost() {
    for format in [Format::Fcpxml, Format::Fcp7Xml] {
        let (p, s) = project(|e| {
            two_plain_nodes(e);
            e.masks.push(filmcraft_project::Mask::new("face", filmcraft_project::MaskPath::rect(0.0, 0.0, 50.0, 50.0)));
            cg::set_mix(e, 1, 0.5).unwrap();
            cg::set_mask(e, 1, Some("face")).unwrap();
            cg::set_enabled(e, 1, false).unwrap();
        });
        let (imp, rep, _) = through(&p, s, format);
        let msg = rep.to_string();
        assert!(msg.contains("node Mix") && msg.contains("node masks"), "{format:?}: a bypassed node's Mix and mask are lost too: {msg}");
        // turning the exported node back on gives the full node, not the 50 % one inside the mask
        let got = lumetris(&imp);
        let off = got.iter().find(|e| !e.enabled).expect("the bypassed node, disabled");
        assert!(off.color.is_none() && off.masks.is_empty(), "{format:?}");
        assert_eq!(exposure(off), -0.25);
    }
}
