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
        assert!(rep.to_string().contains("not kept"), "{format:?}: {rep}");
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
