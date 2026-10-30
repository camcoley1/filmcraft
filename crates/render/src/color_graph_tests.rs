//! Lumetri node graphs (M8d) on the CPU: a one-node graph renders exactly as the single Lumetri it
//! replaces, nodes apply in order, Mix lerps against the node's input, a bypassed node and Mix 0
//! change nothing, a node mask limits its node only, versions switch the picture, the GPU plan's op
//! (`FxOp::Nodes`) is the CPU render, and HDR graphs (CPU only) render rather than drop.

use filmcraft_color::WorkingSpace;
use filmcraft_project::color_graph as cg;
use filmcraft_project::{EffectInstance, Mask, MaskPath, ParamValue, find_effect};
use filmcraft_time::Tick;

use crate::Image;
use crate::effects::{FxCtx, apply};
use crate::gpufx::FxOp;

fn cx(working: WorkingSpace) -> FxCtx<'static> {
    FxCtx { t: Tick::ZERO, px_scale: 1.0, seconds: 0.0, timecode: "", clip_name: "", project: None, env: None, frame_rate: 24.0, working }
}

fn lumetri(params: &[(&str, ParamValue)]) -> EffectInstance {
    let mut e = find_effect("lumetri").unwrap().instance();
    set(&mut e, params);
    e
}

fn set(e: &mut EffectInstance, params: &[(&str, ParamValue)]) {
    for (k, v) in params {
        e.params.get_mut(*k).unwrap_or_else(|| panic!("no param {k}")).value = v.clone();
    }
}

/// Hue / lightness sweep with partial alpha.
fn picture() -> Image {
    let (w, h) = (48usize, 32usize);
    let mut img = Image::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let c = filmcraft_color::hsl_to_rgb(x as f32 / w as f32, 0.7, 0.1 + 0.8 * y as f32 / h as f32).map(filmcraft_color::srgb_to_linear);
            let a = if y % 5 == 0 { 0.5 } else { 1.0 };
            img.px[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[c[0] * a, c[1] * a, c[2] * a, a]);
        }
    }
    img
}

fn render(e: &EffectInstance, working: WorkingSpace) -> Vec<f32> {
    let mut img = picture();
    apply(&mut img, e, &cx(working));
    img.px
}

fn warm() -> Vec<(&'static str, ParamValue)> {
    vec![("temperature", ParamValue::Float(40.0)), ("contrast", ParamValue::Float(30.0)), ("curve_luma", ParamValue::Curve(vec![[0.0, 0.05], [1.0, 0.9]]))]
}
fn cool() -> Vec<(&'static str, ParamValue)> {
    vec![
        ("temperature", ParamValue::Float(-30.0)),
        ("saturation", ParamValue::Float(150.0)),
        ("wheel_shadows", ParamValue::Vec2(filmcraft_geom::Vec2::new(0.3, -0.2))),
    ]
}

/// A graph of two nodes, warm then cool, with the first selected.
fn two_nodes() -> EffectInstance {
    let mut e = lumetri(&warm());
    cg::add_node(&mut e, None, "cool").unwrap();
    set(&mut e, &cool());
    cg::select_node(&mut e, 0).unwrap();
    e
}

#[test]
fn a_one_node_graph_renders_exactly_as_the_single_lumetri() {
    let plain = lumetri(&warm());
    let mut graph = plain.clone();
    // an explicit graph of one plain node (what a grade version holds)
    cg::add_version(&mut graph, None, true).unwrap();
    assert!(graph.color.is_some() && cg::is_single(&graph));
    for ws in [WorkingSpace::Rec709, WorkingSpace::Rec2100Pq] {
        assert_eq!(render(&graph, ws), render(&plain, ws), "{ws:?}");
    }
    let c = cx(WorkingSpace::Rec709);
    assert_eq!(FxOp::eval(&graph, &c, 48, 32), FxOp::eval(&plain, &c, 48, 32), "the same GPU op");
}

#[test]
fn nodes_apply_in_order() {
    let e = two_nodes();
    for ws in [WorkingSpace::Rec709, WorkingSpace::Rec2100Pq] {
        let mut want = picture();
        apply(&mut want, &lumetri(&warm()), &cx(ws));
        apply(&mut want, &lumetri(&cool()), &cx(ws));
        assert_eq!(render(&e, ws), want.px, "{ws:?}: warm then cool");
    }
    // reordered: cool then warm (a different picture)
    let mut moved = e.clone();
    cg::move_node(&mut moved, 0, 1).unwrap();
    assert_ne!(render(&moved, WorkingSpace::Rec709), render(&e, WorkingSpace::Rec709));
}

#[test]
fn mix_and_bypass() {
    let base = render(&lumetri(&warm()), WorkingSpace::Rec709);
    let full = render(&two_nodes(), WorkingSpace::Rec709);
    // the second node off, or at mix 0: the first node alone
    let mut off = two_nodes();
    cg::set_enabled(&mut off, 1, false).unwrap();
    assert_eq!(render(&off, WorkingSpace::Rec709), base);
    let mut zero = two_nodes();
    cg::set_mix(&mut zero, 1, 0.0).unwrap();
    let z = render(&zero, WorkingSpace::Rec709);
    assert!(z.iter().zip(&base).all(|(a, b)| (a - b).abs() < 1e-6));
    // mix 0.5: halfway (premultiplied) between the node's input and its result
    let mut half = two_nodes();
    cg::set_mix(&mut half, 1, 0.5).unwrap();
    let hv = render(&half, WorkingSpace::Rec709);
    for ((h, b), f) in hv.iter().zip(&base).zip(&full) {
        assert!((h - (b + (f - b) * 0.5)).abs() < 1e-5);
    }
    // every node bypassed: the picture unchanged
    let mut none = two_nodes();
    cg::set_enabled(&mut none, 0, false).unwrap();
    cg::set_enabled(&mut none, 1, false).unwrap();
    assert_eq!(render(&none, WorkingSpace::Rec709), picture().px);
}

#[test]
fn a_node_mask_limits_that_node_only() {
    let mut e = two_nodes();
    e.masks = vec![Mask::new("left", MaskPath::rect(0.0, 0.0, 20.0, 32.0)), Mask::new("all", MaskPath::rect(-20.0, -20.0, 68.0, 52.0))];
    cg::set_mask(&mut e, 1, Some("left")).unwrap();
    // the cool node inside `left` only; `all` still limits the whole graph (it covers the whole
    // picture, past the edges, so it changes nothing here)
    let mut want = picture();
    apply(&mut want, &lumetri(&warm()), &cx(WorkingSpace::Rec709));
    let mut node = lumetri(&cool());
    node.masks = vec![e.masks[0].clone()];
    crate::mask::apply_effect(&mut want, &node, &cx(WorkingSpace::Rec709));
    let mut got = picture();
    crate::mask::apply_effect(&mut got, &e, &cx(WorkingSpace::Rec709));
    let worst = got.px.iter().zip(&want.px).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst < 1e-5, "{worst}");
    assert_eq!(cg::effect_wide_masks(&e).iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["all"]);
    // the GPU plan's op agrees (masked node inside the graph, the remaining mask around it)
    let op = FxOp::eval_masked(&e, &cx(WorkingSpace::Rec709), 48, 32).unwrap();
    let mut planned = picture();
    op.apply(&mut planned);
    let worst = planned.px.iter().zip(&got.px).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst < 1e-5, "{worst}");
}

#[test]
fn versions_switch_the_picture() {
    let mut e = lumetri(&warm());
    let a = render(&e, WorkingSpace::Rec709);
    cg::add_version(&mut e, None, false).unwrap();
    set(&mut e, &cool());
    let b = render(&e, WorkingSpace::Rec709);
    assert_ne!(a, b);
    cg::switch_version(&mut e, 0).unwrap();
    assert_eq!(render(&e, WorkingSpace::Rec709), a);
    cg::switch_version(&mut e, 1).unwrap();
    assert_eq!(render(&e, WorkingSpace::Rec709), b);
}

#[test]
fn the_planned_op_is_the_cpu_render_and_hdr_graphs_render() {
    let mut e = two_nodes();
    cg::set_mix(&mut e, 0, 0.7).unwrap();
    let c = cx(WorkingSpace::Rec709);
    let op = FxOp::eval(&e, &c, 48, 32).unwrap();
    let FxOp::Nodes(ops) = &op else { panic!("a graph op: {op:?}") };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], FxOp::Blend { .. }));
    assert!(op.gpu_ok());
    let mut planned = picture();
    op.apply(&mut planned);
    assert_eq!(planned.px, render(&e, WorkingSpace::Rec709));
    // HDR: the GPU stage does not take it (each node's Lumetri is CPU-only there), the CPU renders
    // the graph's steps; nothing is dropped
    let hdr = cx(WorkingSpace::Rec2100Pq);
    assert!(!FxOp::eval(&e, &hdr, 48, 32).unwrap().gpu_ok());
    let got = render(&e, WorkingSpace::Rec2100Pq);
    let mut want = picture();
    let input = want.clone();
    apply(&mut want, &lumetri(&warm()), &hdr);
    let cov = vec![0.7; want.w * want.h];
    crate::mask::mix(&mut want, &input, &cov);
    apply(&mut want, &lumetri(&cool()), &hdr);
    assert_eq!(got, want.px);
    assert_ne!(got, picture().px);
}

#[test]
fn a_dangling_node_mask_applies_nowhere() {
    let mut e = two_nodes();
    e.masks = vec![Mask::new("left", MaskPath::rect(0.0, 0.0, 20.0, 32.0))];
    cg::set_mask(&mut e, 1, Some("left")).unwrap();
    // the mask goes away behind the graph's back (a hostile file): the node grades nothing
    e.masks.clear();
    let want = render(&lumetri(&warm()), WorkingSpace::Rec709);
    let got = {
        let mut img = picture();
        crate::mask::apply_effect(&mut img, &e, &cx(WorkingSpace::Rec709));
        img.px
    };
    assert_eq!(got, want, "only the first node");
    let op = FxOp::eval_masked(&e, &cx(WorkingSpace::Rec709), 48, 32).unwrap();
    let mut planned = picture();
    op.apply(&mut planned);
    let worst = planned.px.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst < 1e-5, "{worst}");
}

#[test]
fn a_keyframed_node_that_is_not_selected_animates() {
    use filmcraft_project::Keyframe;
    let mut e = two_nodes();
    // keyframe node 0 (exposure 0 at tick 0, 1.5 at one second), then select node 1
    cg::select_node(&mut e, 0).unwrap();
    let sec = Tick(254_016_000_000);
    e.params.get_mut("exposure").unwrap().keyframes = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(0.0)), Keyframe::new(sec, ParamValue::Float(1.5))];
    let keyed = e.clone();
    cg::select_node(&mut e, 1).unwrap();
    assert!(e.is_animated());
    for t in [Tick::ZERO, sec] {
        let at = |e: &EffectInstance| {
            let mut img = picture();
            apply(&mut img, e, &FxCtx { t, ..cx(WorkingSpace::Rec709) });
            img.px
        };
        assert_eq!(at(&e), at(&keyed), "at {t:?}: the same picture whichever node is selected");
    }
    let frame = |t: Tick| {
        let mut img = picture();
        apply(&mut img, &e, &FxCtx { t, ..cx(WorkingSpace::Rec709) });
        img.px
    };
    assert_ne!(frame(Tick::ZERO), frame(sec), "the unselected node's keyframes animate the picture");
}
