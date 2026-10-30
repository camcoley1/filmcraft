//! GPU-vs-CPU parity of the effect stage (`filmcraft_render::gpufx::FxOp::apply` is the
//! reference).
//!
//! Tolerances:
//! - Exact checks ([`gpu_effects_match_cpu_exactly`]): the working image read back as f32 from a
//!   half-float-representable source (so the source draw is lossless) must match the CPU op to
//!   [`EXACT`] per value (f32 arithmetic and the GPU's `pow` / `sqrt` differ in the last bits).
//!   Step functions (Posterize, Color Pass, Leave Color and Change to Color keys, Extract) may
//!   flip a pixel that lands on a threshold; at most [`FLIPS`] of the values may differ by more.
//! - Composited plans ([`gpu_effect_layers_match_cpu_plan`]): the compositor's criterion (p99 of
//!   the per-pixel max 8-bit channel difference ≤ 6, mean < 1.5) away from layer outlines, as for
//!   the blend modes.

use super::blend_tests::{device, interior, ramp_layer, stats8, yuv_frame};
use super::*;
use filmcraft_geom::{Affine, Vec2};
use filmcraft_project::{EffectInstance, ParamValue, find_effect};
use filmcraft_render::effects::FxCtx;
use filmcraft_render::gpufx::FxOp;
use filmcraft_render::plan::{LayerFx, execute_cpu};
use filmcraft_time::Tick;

const EXACT: f32 = 1e-4;
const FLIPS: f64 = 0.01;

fn cx(t: Tick, px_scale: f32) -> FxCtx<'static> {
    FxCtx { t, px_scale, seconds: 0.0, timecode: "", clip_name: "", project: None, env: None, frame_rate: 24.0, working: filmcraft_color::WorkingSpace::Rec709 }
}

fn effect(id: &str, params: &[(&str, ParamValue)]) -> EffectInstance {
    let mut e = find_effect(id).unwrap_or_else(|| panic!("{id}")).instance();
    for (k, v) in params {
        e.params.get_mut(*k).unwrap_or_else(|| panic!("{id}.{k}")).value = v.clone();
    }
    e
}

fn fl(v: f64) -> ParamValue {
    ParamValue::Float(v)
}
fn col(r: f32, g: f32, b: f32) -> ParamValue {
    ParamValue::Color([r, g, b, 1.0])
}
fn ch(c: u32) -> ParamValue {
    ParamValue::Choice(c)
}
fn pt(x: f64, y: f64) -> ParamValue {
    ParamValue::Vec2(Vec2::new(x, y))
}

fn half(v: f32) -> f32 {
    super::blend_tests::f16_to_f32(f32_to_f16(v))
}

/// Linear premultiplied test picture, every value representable in half floats: hue sweep along
/// x, lightness along y, a grey ramp, black and white columns, super-whites (up to 1.6), and
/// alpha 1 / partial / 0 bands.
fn picture(w: u32, h: u32) -> Vec<f32> {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / (w - 1) as f32;
            let fy = y as f32 / (h - 1) as f32;
            let c = match x % 11 {
                0 => [0.0, 0.0, 0.0],
                1 => [1.0, 1.0, 1.0],
                2 => [fy, fy, fy],
                3 => [1.6 * fy, 1.2 * fy, 0.3],
                _ => {
                    let rgb = filmcraft_color::hsl_to_rgb(fx, 0.25 + 0.75 * ((y % 5) as f32 / 4.0), 0.1 + 0.8 * fy);
                    rgb.map(filmcraft_color::srgb_to_linear)
                }
            };
            let a = match y % 7 {
                0 => 0.0,
                1 | 2 => 0.5 + 0.4 * fx,
                3 => 0.25,
                _ => 1.0,
            };
            px.extend_from_slice(&[c[0] * a, c[1] * a, c[2] * a, a].map(half));
        }
    }
    px
}

/// Max |cpu − gpu| (finite values required) and the share of values off by more than 0.02.
fn compare(cpu: &[f32], gpu: &[f32]) -> (f32, f64) {
    assert_eq!(cpu.len(), gpu.len());
    let mut worst = 0f32;
    let mut flips = 0usize;
    for (a, b) in cpu.iter().zip(gpu) {
        assert!(b.is_finite(), "non-finite GPU value");
        let d = (a - b).abs() / (1.0 + a.abs());
        if d > 0.02 {
            flips += 1;
        } else {
            worst = worst.max(d);
        }
    }
    (worst, flips as f64 / cpu.len() as f64)
}

/// Effect cases: (effect id, parameters, whether a threshold may flip pixels).
fn cases() -> Vec<(&'static str, Vec<(&'static str, ParamValue)>, bool)> {
    vec![
        ("brightness_contrast", vec![("brightness", fl(30.0)), ("contrast", fl(40.0))], false),
        ("brightness_contrast", vec![("brightness", fl(-100.0)), ("contrast", fl(-100.0))], false),
        ("brightness_contrast", vec![("brightness", fl(100.0)), ("contrast", fl(100.0))], false),
        ("brightness_contrast", vec![("brightness", fl(-20.0)), ("contrast", fl(900.0))], false),
        ("proc_amp", vec![("brightness", fl(10.0)), ("contrast", fl(130.0)), ("hue", fl(200.0)), ("saturation", fl(150.0))], false),
        ("proc_amp", vec![("hue", fl(-400.0)), ("saturation", fl(0.0))], false),
        ("tint", vec![], false),
        ("tint", vec![("black", col(0.2, 0.1, 0.5)), ("white", col(1.0, 0.9, 0.3)), ("amount", fl(60.0))], false),
        ("black_white", vec![], false),
        ("color_balance", vec![("shadow_r", fl(50.0)), ("mid_g", fl(-40.0)), ("hi_b", fl(80.0)), ("shadow_b", fl(-100.0)), ("hi_r", fl(100.0))], false),
        ("color_balance", vec![("mid_r", fl(70.0)), ("hi_g", fl(-60.0)), ("preserve", ParamValue::Bool(true))], false),
        ("leave_color", vec![("amount", fl(80.0)), ("tolerance", fl(20.0)), ("softness", fl(30.0))], true),
        ("leave_color", vec![("amount", fl(100.0)), ("color", col(0.1, 0.8, 0.2)), ("tolerance", fl(0.0)), ("softness", fl(0.0))], true),
        ("change_to_color", vec![], true),
        ("change_to_color", vec![("hue_tol", fl(30.0)), ("softness", fl(10.0)), ("to", col(0.9, 0.9, 0.1))], true),
        ("color_pass", vec![("similarity", fl(30.0))], true),
        ("color_pass", vec![("similarity", fl(45.0)), ("reverse", ParamValue::Bool(true)), ("color", col(0.2, 0.4, 0.9))], true),
        ("gamma_correction", vec![("gamma", fl(5.0))], false),
        ("gamma_correction", vec![("gamma", fl(28.0))], false),
        ("gamma_correction", vec![("gamma", fl(0.0))], false),
        ("gamma_correction", vec![("gamma", fl(-3.0))], false),
        ("levels", vec![("in_black", fl(20.0)), ("in_white", fl(230.0)), ("out_black", fl(10.0)), ("out_white", fl(240.0)), ("gamma", fl(150.0))], false),
        ("levels", vec![("in_black", fl(200.0)), ("in_white", fl(0.0)), ("gamma", fl(5.0)), ("out_white", fl(-50.0))], true),
        ("extract", vec![], true),
        ("extract", vec![("invert", ParamValue::Bool(true))], true),
        ("extract", vec![("black", fl(40.0)), ("white", fl(200.0)), ("softness", fl(30.0))], true),
        ("extract", vec![("black", fl(90.0)), ("white", fl(160.0)), ("invert", ParamValue::Bool(true))], true),
        ("extract", vec![("black", fl(90.0)), ("white", fl(160.0)), ("softness", fl(1.0))], true),
        ("extract", vec![("black", fl(40.0)), ("white", fl(200.0)), ("softness", fl(100.0))], true),
        ("extract", vec![("black", fl(160.0)), ("white", fl(90.0))], true),
        ("extract", vec![("black", fl(90.0)), ("white", fl(90.0))], true),
        ("invert", vec![("blend", fl(30.0))], false),
        ("invert", vec![("channel", ParamValue::Choice(1))], false),
        ("invert", vec![("channel", ParamValue::Choice(2)), ("blend", fl(70.0))], false),
        ("invert", vec![("channel", ParamValue::Choice(3))], false),
        ("invert", vec![("channel", ParamValue::Choice(4)), ("blend", fl(20.0))], false),
        ("invert", vec![("channel", ParamValue::Choice(9))], false),
        ("posterize", vec![("levels", fl(2.0))], true),
        ("posterize", vec![("levels", fl(7.0))], true),
        ("posterize", vec![("levels", fl(255.0))], true),
        ("gaussian_blur", vec![("blurriness", fl(0.0))], false),
        ("gaussian_blur", vec![("blurriness", fl(5.0))], false),
        ("gaussian_blur", vec![("blurriness", fl(5.0)), ("repeat_edge", ParamValue::Bool(true))], false),
        ("gaussian_blur", vec![("blurriness", fl(80.0))], false),
        ("gaussian_blur", vec![("blurriness", fl(80.0)), ("repeat_edge", ParamValue::Bool(true))], false),
        ("gaussian_blur", vec![("blurriness", fl(12.0)), ("dimensions", ParamValue::Choice(1))], false),
        ("gaussian_blur", vec![("blurriness", fl(12.0)), ("dimensions", ParamValue::Choice(2)), ("repeat_edge", ParamValue::Bool(true))], false),
        ("gaussian_blur", vec![("blurriness", fl(3000.0)), ("repeat_edge", ParamValue::Bool(true))], false),
        ("gaussian_blur", vec![("blurriness", fl(1e9))], false),
        ("gaussian_blur", vec![("blurriness", fl(-50.0))], false),
        ("camera_blur", vec![("percent", fl(20.0))], false),
        ("directional_blur", vec![("length", fl(10.0)), ("direction", fl(30.0))], false),
        ("directional_blur", vec![("length", fl(0.2))], false),
        ("directional_blur", vec![("length", fl(1000.0)), ("direction", fl(-100.0))], false),
        ("sharpen", vec![("amount", fl(80.0))], false),
        ("sharpen", vec![("amount", fl(4000.0))], false),
        ("unsharp_mask", vec![("amount", fl(150.0)), ("radius", fl(3.0)), ("threshold", fl(10.0))], true),
        ("unsharp_mask", vec![("radius", fl(0.1))], false),
        ("unsharp_mask", vec![("amount", fl(300.0)), ("radius", fl(250.0))], false),
        ("crop", vec![("left", fl(10.0)), ("top", fl(5.0)), ("right", fl(20.0)), ("bottom", fl(15.0))], false),
        ("crop", vec![("left", fl(10.0)), ("top", fl(5.0)), ("right", fl(20.0)), ("bottom", fl(15.0)), ("feather", fl(12.0))], false),
        ("crop", vec![("left", fl(12.0)), ("top", fl(5.0)), ("right", fl(20.0)), ("zoom", ParamValue::Bool(true))], false),
        ("crop", vec![("left", fl(70.0)), ("right", fl(70.0)), ("feather", fl(5000.0))], false),
        ("edge_feather", vec![("amount", fl(30.0))], false),
        ("transform", vec![("scale_height", fl(150.0)), ("rotation", fl(30.0)), ("position", pt(30.0, 20.0)), ("opacity", fl(60.0))], false),
        (
            "transform",
            vec![
                ("uniform_scale", ParamValue::Bool(false)),
                ("scale_width", fl(80.0)),
                ("scale_height", fl(120.0)),
                ("skew", fl(20.0)),
                ("skew_axis", fl(45.0)),
                ("anchor", pt(10.0, 30.0)),
            ],
            false,
        ),
        ("horizontal_flip", vec![], false),
        ("vertical_flip", vec![], false),
        ("mirror", vec![("center", pt(20.0, 10.0)), ("angle", fl(30.0))], false),
        ("mirror", vec![("angle", fl(200.0))], false),
        ("offset", vec![("shift", pt(10.0, 30.0)), ("blend", fl(25.0))], false),
        ("offset", vec![("shift", pt(-1e5, 7.25))], false),
        ("asc_cdl", vec![("r_slope", fl(1.4)), ("g_offset", fl(-0.1)), ("b_power", fl(2.2)), ("saturation", fl(1.6))], false),
        ("asc_cdl", vec![("r_power", fl(0.0)), ("g_slope", fl(0.0)), ("saturation", fl(0.0))], false),
        ("channel_mix", vec![("rr", fl(50.0)), ("rg", fl(60.0)), ("gb", fl(-80.0)), ("bc", fl(30.0))], false),
        ("channel_mix", vec![("rb", fl(120.0)), ("monochrome", ParamValue::Bool(true))], false),
        ("color_replace", vec![("similarity", fl(40.0))], true),
        ("color_replace", vec![("similarity", fl(25.0)), ("solid", ParamValue::Bool(true)), ("target", col(0.3, 0.6, 0.2))], true),
        ("color_replace", vec![("similarity", fl(0.0))], true),
        ("alpha_adjust", vec![("opacity", fl(60.0))], false),
        ("alpha_adjust", vec![("ignore", ParamValue::Bool(true)), ("invert", ParamValue::Bool(true))], false),
        ("alpha_adjust", vec![("invert", ParamValue::Bool(true)), ("mask_only", ParamValue::Bool(true)), ("opacity", fl(150.0))], false),
        ("vignette", vec![], false),
        ("vignette", vec![("amount", fl(-100.0)), ("midpoint", fl(50.0)), ("roundness", fl(0.0)), ("feather", fl(50.0))], false),
        (
            "vignette",
            vec![("amount", fl(-30.0)), ("midpoint", fl(20.0)), ("roundness", fl(-100.0)), ("feather", fl(0.0)), ("color", col(0.1, 0.4, 0.7))],
            false,
        ),
        ("vignette", vec![("amount", fl(40.0)), ("midpoint", fl(90.0)), ("roundness", fl(100.0)), ("feather", fl(100.0))], false),
        ("video_limiter", vec![], false),
        ("video_limiter", vec![("axis", ch(0)), ("clip_level", ch(0)), ("compression", ch(0))], false),
        ("video_limiter", vec![("axis", ch(1)), ("clip_level", ch(9)), ("compression", ch(4))], false),
        ("video_limiter", vec![("axis", ch(2)), ("clip_level", ch(5)), ("compression", ch(2))], false),
        ("video_limiter", vec![("axis", ch(3)), ("clip_level", ch(0)), ("compression", ch(1))], false),
        (
            "video_limiter",
            vec![
                ("axis", ch(2)),
                ("clip_level", ch(0)),
                ("compression", ch(0)),
                ("gamut_warning", ParamValue::Bool(true)),
                ("warning_color", col(0.0, 1.0, 0.0)),
            ],
            false,
        ),
        (
            "lumetri",
            vec![
                ("temperature", fl(20.0)),
                ("tint", fl(-10.0)),
                ("exposure", fl(0.5)),
                ("contrast", fl(15.0)),
                ("highlights", fl(-20.0)),
                ("shadows", fl(25.0)),
                ("whites", fl(10.0)),
                ("blacks", fl(-15.0)),
                ("saturation", fl(110.0)),
                ("creative_on", ParamValue::Bool(false)),
                ("vignette_on", ParamValue::Bool(false)),
            ],
            false,
        ),
        (
            "lumetri",
            vec![
                ("basic_on", ParamValue::Bool(false)),
                ("creative_sat", fl(120.0)),
                ("vibrance", fl(30.0)),
                ("faded_film", fl(25.0)),
                ("shadow_tint", col(0.4, 0.45, 0.6)),
                ("highlight_tint", col(0.6, 0.55, 0.4)),
                ("vignette_on", ParamValue::Bool(false)),
            ],
            false,
        ),
        (
            "lumetri",
            vec![
                ("basic_on", ParamValue::Bool(false)),
                ("creative_on", ParamValue::Bool(false)),
                ("vignette_amount", fl(-3.0)),
                ("vignette_midpoint", fl(45.0)),
                ("vignette_roundness", fl(-30.0)),
                ("vignette_feather", fl(60.0)),
            ],
            false,
        ),
        (
            "lumetri",
            vec![
                ("temperature", fl(-15.0)),
                ("tint", fl(10.0)),
                ("exposure", fl(0.3)),
                ("contrast", fl(20.0)),
                ("highlights", fl(-10.0)),
                ("shadows", fl(15.0)),
                ("whites", fl(-5.0)),
                ("blacks", fl(5.0)),
                ("saturation", fl(105.0)),
                ("creative_sat", fl(110.0)),
                ("vibrance", fl(20.0)),
                ("faded_film", fl(15.0)),
                ("shadow_tint", col(0.48, 0.5, 0.55)),
                ("highlight_tint", col(0.52, 0.5, 0.45)),
                ("vignette_amount", fl(2.0)),
                ("vignette_midpoint", fl(50.0)),
                ("vignette_roundness", fl(20.0)),
                ("vignette_feather", fl(50.0)),
            ],
            false,
        ),
        (
            "lumetri",
            vec![("exposure", fl(4.0)), ("contrast", fl(100.0)), ("temperature", fl(100.0)), ("tint", fl(100.0)), ("whites", fl(100.0)), ("blacks", fl(100.0))],
            false,
        ),
        (
            "lumetri",
            vec![
                ("exposure", fl(-4.0)),
                ("contrast", fl(-100.0)),
                ("temperature", fl(-100.0)),
                ("tint", fl(-100.0)),
                ("whites", fl(-100.0)),
                ("blacks", fl(-100.0)),
            ],
            false,
        ),
    ]
}

/// Every effect case on the GPU against its CPU reference, on the working image read back as f32.
#[test]
fn gpu_effects_match_cpu_exactly() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (67u32, 41u32);
    let px = picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let mut failures = Vec::new();
    for (id, params, steps) in cases() {
        let e = effect(id, &params);
        let op = FxOp::eval(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap_or_else(|| panic!("{id} has no GPU op"));
        assert!(op.gpu_ok(), "{id} {params:?}: not GPU-capable");
        let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
        op.apply(&mut cpu);
        let fx = LayerFx { size: (w, h), decimation: 1, ops: vec![op] };
        let (_, _, gpu) = c.effect_image(&frame, &fx).expect("effect image");
        let (worst, flips) = compare(&cpu.px, &gpu);
        eprintln!("{id} {params:?}: max rel diff {worst:.2e}, {:.3}% flipped", flips * 100.0);
        if worst > EXACT || flips > if steps { FLIPS } else { 0.0 } {
            failures.push(format!("{id} {params:?}: {worst:.2e}, flips {flips:.4}"));
        }
    }
    assert!(failures.is_empty(), "GPU effects differ from the CPU:\n{}", failures.join("\n"));
}

#[test]
fn gpu_extract_zero_softness_has_no_fractional_edge() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (67, 41);
    let px = picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let mut masks = Vec::new();
    for invert in [false, true] {
        let e = effect("extract", &[("black", fl(90.0)), ("white", fl(160.0)), ("invert", ParamValue::Bool(invert))]);
        let op = FxOp::eval(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap();
        let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).unwrap();
        for (src, out) in px.as_chunks::<4>().0.iter().zip(gpu.as_chunks::<4>().0.iter()) {
            assert_eq!(out[3], src[3], "Extract must preserve alpha");
            assert!(out[0] == 0.0 || out[0] == out[3], "zero softness produced a fractional edge: {out:?}");
            assert_eq!(out[0], out[1]);
            assert_eq!(out[0], out[2]);
        }
        masks.push(gpu);
    }
    for ((normal, inverted), src) in masks[0].as_chunks::<4>().0.iter().zip(masks[1].as_chunks::<4>().0.iter()).zip(px.as_chunks::<4>().0.iter()) {
        assert_eq!(normal[0] + inverted[0], src[3], "inversion must complement the premultiplied mask");
    }
}

/// Chains run in order (and Unsharp keeps its original while blurring, between other blurs).
#[test]
fn gpu_effect_chains_match_cpu() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (96u32, 54u32);
    let px = picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let chains: Vec<Vec<EffectInstance>> = vec![
        vec![
            effect("brightness_contrast", &[("brightness", fl(20.0)), ("contrast", fl(30.0))]),
            effect("gaussian_blur", &[("blurriness", fl(8.0))]),
            effect("tint", &[("amount", fl(50.0))]),
        ],
        vec![
            effect("gaussian_blur", &[("blurriness", fl(90.0)), ("repeat_edge", ParamValue::Bool(true))]),
            effect("unsharp_mask", &[("amount", fl(200.0)), ("radius", fl(4.0))]),
            effect("gaussian_blur", &[("blurriness", fl(3.0)), ("dimensions", ParamValue::Choice(1))]),
            effect("sharpen", &[("amount", fl(50.0))]),
        ],
        vec![
            effect("crop", &[("left", fl(10.0)), ("bottom", fl(20.0)), ("feather", fl(4.0))]),
            effect("transform", &[("rotation", fl(-15.0)), ("scale_height", fl(110.0))]),
            effect("horizontal_flip", &[]),
            effect("levels", &[("in_black", fl(15.0)), ("gamma", fl(80.0))]),
            effect("offset", &[("shift", pt(70.0, 10.0))]),
            effect("invert", &[("channel", ParamValue::Choice(4))]),
            effect("directional_blur", &[("length", fl(6.0)), ("direction", fl(90.0))]),
        ],
    ];
    for chain in chains {
        let ops: Vec<FxOp> = chain.iter().filter_map(|e| FxOp::eval(e, &cx(Tick::ZERO, 1.0), w as usize, h as usize)).collect();
        assert!(ops.iter().all(FxOp::gpu_ok));
        let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
        for op in &ops {
            op.apply(&mut cpu);
        }
        let names: Vec<&str> = chain.iter().map(|e| e.effect.as_str()).collect();
        let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops }).expect("effect image");
        let (worst, flips) = compare(&cpu.px, &gpu);
        eprintln!("{names:?}: max rel diff {worst:.2e}, {:.3}% flipped", flips * 100.0);
        assert!(worst < EXACT * 2.0 && flips == 0.0, "{names:?}: {worst}, {flips}");
    }
}

/// Keyframed parameters are evaluated at the layer's time on the CPU: the GPU result follows them.
#[test]
fn keyframed_effects_follow_time() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (48u32, 32u32);
    let px = picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let mut blur = effect("gaussian_blur", &[]);
    let mut bc = effect("brightness_contrast", &[]);
    let p = blur.params.get_mut("blurriness").expect("blurriness");
    p.put_keyframe(Tick::ZERO, fl(0.0));
    p.put_keyframe(Tick::from_seconds_f64(1.0), fl(30.0));
    let p = bc.params.get_mut("brightness").expect("brightness");
    p.put_keyframe(Tick::ZERO, fl(-50.0));
    p.put_keyframe(Tick::from_seconds_f64(1.0), fl(60.0));
    let mut last: Option<Vec<f32>> = None;
    for s in [0.0, 0.3, 0.75, 1.0] {
        let cx = cx(Tick::from_seconds_f64(s), 1.0);
        let ops: Vec<FxOp> = [&bc, &blur].iter().filter_map(|e| FxOp::eval(e, &cx, w as usize, h as usize)).collect();
        let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
        for op in &ops {
            op.apply(&mut cpu);
        }
        let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops }).expect("effect image");
        let (worst, flips) = compare(&cpu.px, &gpu);
        assert!(worst < EXACT && flips == 0.0, "t = {s}: {worst}, {flips}");
        assert_ne!(last.as_ref(), Some(&gpu), "t = {s}: the keyframes change the picture");
        last = Some(gpu);
    }
}

/// Hostile values: non-finite parameters stay on the CPU (`gpu_ok` is false), huge radii and
/// sizes are capped as on the CPU and run in bounded time on the GPU.
#[test]
fn hostile_parameters_are_bounded() {
    let (w, h) = (40usize, 24usize);
    let nan = [
        effect("brightness_contrast", &[("brightness", fl(f64::NAN))]),
        effect("gaussian_blur", &[("blurriness", fl(f64::INFINITY))]),
        effect("directional_blur", &[("length", fl(f64::NAN))]),
        effect("transform", &[("rotation", fl(f64::NAN))]),
        effect("crop", &[("left", fl(f64::NAN))]),
        effect("offset", &[("shift", pt(f64::INFINITY, 0.0))]),
        effect("levels", &[("in_black", fl(f64::NEG_INFINITY))]),
        effect("transform", &[("scale_height", fl(1e300))]),
        // minifying resamples (the CPU's mip path) stay on the CPU
        effect("transform", &[("scale_height", fl(20.0))]),
        effect("crop", &[("left", fl(-300.0)), ("zoom", ParamValue::Bool(true))]),
        effect("vignette", &[("amount", fl(f64::NAN))]),
        effect("vignette", &[("midpoint", fl(f64::INFINITY))]),
        effect("vignette", &[("roundness", fl(f64::NAN))]),
        effect("vignette", &[("feather", fl(f64::NAN))]),
        effect("video_limiter", &[("clip_level", fl(f64::NAN))]),
        effect("video_limiter", &[("compression", fl(f64::INFINITY))]),
        effect("video_limiter", &[("axis", fl(f64::NAN))]),
        effect("video_limiter", &[("warning_color", ParamValue::Color([f32::NAN, 0.0, 0.0, 1.0]))]),
        effect("lumetri", &[("exposure", fl(f64::NAN))]),
        effect("lumetri", &[("temperature", fl(f64::INFINITY))]),
        effect("lumetri", &[("contrast", fl(f64::NAN))]),
        effect("lumetri", &[("vignette_amount", fl(f64::NAN))]),
    ];
    for e in &nan {
        let op = FxOp::eval(e, &cx(Tick::ZERO, 1.0), w, h);
        // gaussian with an infinite radius is capped (no blur), which the GPU runs
        if e.effect == "gaussian_blur" {
            assert!(op.as_ref().is_some_and(FxOp::gpu_ok));
            continue;
        }
        assert!(op.as_ref().is_some_and(|o| !o.gpu_ok()), "{}: {op:?}", e.effect);
        // and the CPU reference copes
        let mut img = filmcraft_render::Image::filled(w, h, [0.3, 0.2, 0.1, 1.0]);
        if let Some(op) = op {
            op.apply(&mut img);
        }
    }
    // huge radii: box radii capped by the image size
    let op = FxOp::eval(&effect("gaussian_blur", &[("blurriness", fl(1e30))]), &cx(Tick::ZERO, 1.0), w, h);
    let Some(FxOp::Gaussian { rx, ry, .. }) = op else { panic!("{op:?}") };
    assert!(rx.iter().chain(&ry).all(|r| *r as usize <= 4 * w.max(h).max(8)), "{rx:?} {ry:?}");
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let frame = VideoFrame::rgba_f32(w as u32, h as u32, picture(w as u32, h as u32));
    let ops = vec![
        FxOp::eval(&effect("gaussian_blur", &[("blurriness", fl(1e30))]), &cx(Tick::ZERO, 1.0), w, h).expect("op"),
        FxOp::eval(&effect("unsharp_mask", &[("radius", fl(1e30)), ("amount", fl(1e30))]), &cx(Tick::ZERO, 1.0), w, h).expect("op"),
        FxOp::eval(&effect("directional_blur", &[("length", fl(1e30))]), &cx(Tick::ZERO, 1.0), w, h).expect("op"),
    ];
    let t0 = std::time::Instant::now();
    let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w as u32, h as u32), decimation: 1, ops }).expect("effect image");
    assert!(t0.elapsed().as_secs() < 30);
    assert_eq!(gpu.len(), w * h * 4);
    // a working image larger than any texture is refused, not attempted
    assert!(c.effect_image(&frame, &LayerFx { size: (1 << 20, 4), decimation: 1, ops: vec![FxOp::BlackWhite] }).is_none());
}

/// Where the GPU stage takes Lumetri: every SDR section (Basic, Input LUT, Creative incl. looks,
/// look LUTs and Sharpen, Curves, Color Wheels, HSL Secondary, Vignette); not HDR grades, LUTs
/// beyond its size limits, a Denoise radius beyond its window, or non-finite numbers.
#[test]
fn lumetri_gpu_boundary() {
    let (w, h) = (40usize, 24usize);
    let gpu = |e: &EffectInstance, c: &FxCtx| FxOp::eval(e, c, w, h).is_some_and(|o| o.gpu_ok());
    let c1 = cx(Tick::ZERO, 1.0);
    for (what, e) in [
        ("sharpen", effect("lumetri", &[("sharpen", fl(20.0))])),
        ("input LUT", effect("lumetri", &[("input_lut", ParamValue::Text("builtin:slog3-sgamut3cine-to-rec709".into()))])),
        ("unresolvable input LUT (none applied)", effect("lumetri", &[("input_lut", ParamValue::Text("custom.cube".into()))])),
        ("look LUT", effect("lumetri", &[("look_lut", ParamValue::Text("builtin:look-teal-orange".into()))])),
        ("procedural look", effect("lumetri", &[("look", ch(1))])),
        ("luma curve", effect("lumetri", &[("curve_luma", ParamValue::Curve(vec![[0.0, 0.1], [1.0, 0.9]]))])),
        ("hue vs sat", effect("lumetri", &[("hue_vs_sat", ParamValue::Curve(vec![[0.5, 0.5]]))])),
        ("wheel", effect("lumetri", &[("wheel_shadows", pt(0.5, 0.0))])),
        ("wheel lightness", effect("lumetri", &[("wheel_midtones_l", fl(20.0))])),
        ("HSL Secondary", effect("lumetri", &[("hsl_on", ParamValue::Bool(true)), ("hsl_denoise", fl(100.0)), ("hsl_blur", fl(100.0))])),
        ("heavy grade", heavy_grade()),
    ] {
        assert!(gpu(&e, &c1), "{what} must run on the GPU");
    }
    // HDR grades stay on the CPU
    let mut hdr = cx(Tick::ZERO, 1.0);
    hdr.working = filmcraft_color::WorkingSpace::Rec2100Pq;
    assert!(!gpu(&effect("lumetri", &[]), &hdr));
    // a Denoise window larger than the shader's (px_scale 2: radius 6)
    let denoise = effect("lumetri", &[("hsl_on", ParamValue::Bool(true)), ("hsl_denoise", fl(100.0))]);
    assert!(gpu(&denoise, &cx(Tick::ZERO, 1.0)));
    assert!(!gpu(&denoise, &cx(Tick::ZERO, 2.0)));
    // LUT size limits
    let lut = |n: usize| filmcraft_color::Lut { title: String::new(), shaper: None, cube: Some(filmcraft_color::Lut3d::identity(n)) };
    assert!(filmcraft_render::gpufx::gpu_lut_ok(&lut(filmcraft_render::gpufx::GPU_LUT_MAX_CUBE)));
    assert!(!filmcraft_render::gpufx::gpu_lut_ok(&lut(filmcraft_render::gpufx::GPU_LUT_MAX_CUBE + 1)));
    // non-finite numbers in the new sections are left to the CPU
    for e in [
        effect("lumetri", &[("hsl_on", ParamValue::Bool(true)), ("hsl_hue", fl(f64::NAN))]),
        effect("lumetri", &[("wheel_midtones_l", fl(f64::INFINITY))]),
        effect("lumetri", &[("look", ch(2)), ("look_intensity", fl(f64::NAN))]),
    ] {
        assert!(!gpu(&e, &c1), "{e:?}");
    }
}

/// The GPU, or None (skip) without an adapter; with `FILMCRAFT_REQUIRE_GPU=1` (a machine that has
/// one) a missing adapter fails the test instead.
fn gpu_or_skip(what: &str) -> Option<(wgpu::Device, wgpu::Queue)> {
    let d = device();
    if d.is_none() {
        assert!(std::env::var("FILMCRAFT_REQUIRE_GPU").as_deref() != Ok("1"), "FILMCRAFT_REQUIRE_GPU=1 but no GPU adapter for {what}");
        eprintln!("SKIPPED {what}: no GPU adapter");
    }
    d
}

/// The demo scene (a rendered landscape: sky gradients, sun, water, foliage) as linear
/// premultiplied RGBA in half-float-representable values: the real-picture test image.
fn demo_picture(w: u32, h: u32) -> Vec<f32> {
    let rgba = filmcraft_media::generators::render(
        &filmcraft_media::Generator::Demo(filmcraft_media::DemoScene::OceanSunset),
        w,
        h,
        1.0,
        24,
        filmcraft_time::FrameRate::FPS_24,
    );
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let a = f32::from(p[3]) / 255.0;
            [0, 1, 2].map(|k| filmcraft_color::srgb_to_linear(f32::from(p[k]) / 255.0) * a).into_iter().chain([a])
        })
        .map(half)
        .collect()
}

/// (max |cpu − gpu| over the linear premultiplied values, max difference of the 8-bit sRGB codes
/// of the straight colour).
fn diff_float_and_8bit(cpu: &[f32], gpu: &[f32]) -> (f32, u8) {
    assert_eq!(cpu.len(), gpu.len());
    let code = |p: &[f32; 4], k: usize| {
        let v = if p[3] > 1e-6 { p[k] / p[3] } else { 0.0 };
        (filmcraft_color::linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0).round() as i32
    };
    let (mut fl, mut b8) = (0f32, 0i32);
    for (a, b) in cpu.as_chunks::<4>().0.iter().zip(gpu.as_chunks::<4>().0.iter()) {
        for k in 0..4 {
            assert!(b[k].is_finite(), "GPU value not finite: {b:?}");
            fl = fl.max((a[k] - b[k]).abs());
        }
        for k in 0..3 {
            b8 = b8.max((code(a, k) - code(b, k)).abs());
        }
    }
    (fl, b8.min(255) as u8)
}

/// Each Lumetri section (and the heavy grade) on the GPU against its CPU reference
/// (`FxOp::apply`), on the synthetic chart (hue / lightness sweeps, super-whites, partial
/// alpha) and on the demo scene: max |difference| ≤ 1/255 on the linear float values and ≤ 1
/// code on the 8-bit sRGB values.
#[test]
fn lumetri_sections_match_cpu() {
    let Some((dev, q)) = gpu_or_skip("lumetri_sections_match_cpu") else { return };
    let mut c = GpuCompositor::new(&dev, &q);
    let curve = |v: &[[f32; 2]]| ParamValue::Curve(v.to_vec());
    let on = ParamValue::Bool(true);
    let cases: Vec<(&str, Vec<(&str, ParamValue)>)> = vec![
        ("luma curve", vec![("curve_luma", curve(&[[0.0, 0.05], [0.3, 0.2], [0.7, 0.85], [1.0, 0.95]]))]),
        (
            "RGB curves",
            vec![
                ("curve_red", curve(&[[0.0, 0.1], [1.0, 0.9]])),
                ("curve_green", curve(&[[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]])),
                ("curve_blue", curve(&[[0.2, 0.0], [1.0, 1.0]])),
            ],
        ),
        ("hue vs sat", vec![("hue_vs_sat", curve(&[[0.0, 0.8], [0.33, 0.2], [0.66, 0.6]]))]),
        ("hue vs hue", vec![("hue_vs_hue", curve(&[[0.1, 0.6], [0.5, 0.4], [0.9, 0.55]]))]),
        ("hue vs luma", vec![("hue_vs_luma", curve(&[[0.0, 0.7], [0.5, 0.3]]))]),
        ("luma vs sat", vec![("luma_vs_sat", curve(&[[0.2, 0.2], [0.8, 0.8]]))]),
        ("sat vs sat", vec![("sat_vs_sat", curve(&[[0.1, 0.7], [0.9, 0.3]]))]),
        ("wheels", vec![("wheel_shadows", pt(0.4, -0.3)), ("wheel_midtones", pt(-0.2, 0.5)), ("wheel_highlights", pt(0.3, 0.3))]),
        ("wheel lightness", vec![("wheel_shadows_l", fl(-30.0)), ("wheel_midtones_l", fl(25.0)), ("wheel_highlights_l", fl(40.0))]),
        ("look 1", vec![("look", ch(1)), ("look_intensity", fl(100.0))]),
        ("look 2", vec![("look", ch(2)), ("look_intensity", fl(70.0))]),
        ("look 3", vec![("look", ch(3))]),
        ("look 4", vec![("look", ch(4))]),
        ("look 5", vec![("look", ch(5))]),
        ("look 6", vec![("look", ch(6))]),
        ("look 7", vec![("look", ch(7))]),
        ("look 8", vec![("look", ch(8))]),
        ("look LUT", vec![("look_lut", ParamValue::Text("builtin:look-teal-orange".into())), ("look_intensity", fl(85.0))]),
        ("input LUT", vec![("input_lut", ParamValue::Text("builtin:slog3-sgamut3cine-to-rec709".into())), ("contrast", fl(10.0))]),
        ("sharpen", vec![("sharpen", fl(60.0))]),
        ("negative sharpen", vec![("sharpen", fl(-50.0))]),
        (
            "HSL correct",
            vec![
                ("hsl_on", on.clone()),
                ("hsl_hue", fl(200.0)),
                ("hsl_hue_range", fl(90.0)),
                ("hsl_sat", fl(150.0)),
                ("hsl_temp", fl(30.0)),
                ("hsl_tint", fl(-20.0)),
                ("hsl_hue_shift", fl(20.0)),
            ],
        ),
        ("HSL denoise + blur", vec![("hsl_on", on.clone()), ("hsl_hue", fl(30.0)), ("hsl_denoise", fl(100.0)), ("hsl_blur", fl(40.0)), ("hsl_sat", fl(40.0))]),
        ("HSL denoise only", vec![("hsl_on", on.clone()), ("hsl_hue", fl(120.0)), ("hsl_denoise", fl(30.0))]),
        ("HSL show mask grey", vec![("hsl_on", on.clone()), ("hsl_show_mask", ch(1)), ("hsl_blur", fl(20.0))]),
        ("HSL show mask black", vec![("hsl_on", on.clone()), ("hsl_show_mask", ch(2))]),
        ("HSL show mask white", vec![("hsl_on", on.clone()), ("hsl_show_mask", ch(3)), ("hsl_denoise", fl(60.0))]),
    ];
    let mut failures = Vec::new();
    for (picture_name, (w, h), px) in [("chart", (67u32, 41u32), picture(67, 41)), ("demo scene", (320, 180), demo_picture(320, 180))] {
        let frame = VideoFrame::rgba_f32(w, h, px.clone());
        let mut all: Vec<(String, EffectInstance)> = cases.iter().map(|(n, p)| (n.to_string(), effect("lumetri", p))).collect();
        all.push(("heavy grade".into(), heavy_grade()));
        for (name, e) in all {
            let op = FxOp::eval(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).expect("a Lumetri op");
            assert!(op.gpu_ok(), "{name}: not GPU-capable");
            let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
            op.apply(&mut cpu);
            let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("effect image");
            let (fl_diff, b8) = diff_float_and_8bit(&cpu.px, &gpu);
            eprintln!("{picture_name} / {name}: max float diff {fl_diff:.2e}, max 8-bit diff {b8}");
            if fl_diff > 1.0 / 255.0 || b8 > 1 {
                failures.push(format!("{picture_name} / {name}: float {fl_diff:.2e}, 8-bit {b8}"));
            }
        }
    }
    assert!(failures.is_empty(), "GPU Lumetri differs from the CPU:\n{}", failures.join("\n"));
}

/// Layers with effect chains in a composited plan — YUV and RGBA sources, a decimated working
/// image, rotated / scaled placement, partial opacity and alpha, blend modes — match the CPU plan
/// executor.
#[test]
fn gpu_effect_layers_match_cpu_plan() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (320usize, 180usize);
    let ops = |list: &[EffectInstance], lw: u32, lh: u32, px_scale: f32| -> Vec<FxOp> {
        list.iter().filter_map(|e| FxOp::eval(e, &cx(Tick::ZERO, px_scale), lw as usize, lh as usize)).collect()
    };
    let chain_a = [
        effect("brightness_contrast", &[("brightness", fl(15.0)), ("contrast", fl(25.0))]),
        effect("gaussian_blur", &[("blurriness", fl(6.0))]),
        effect("tint", &[("amount", fl(40.0))]),
    ];
    let chain_b = [
        effect("crop", &[("left", fl(8.0)), ("right", fl(12.0)), ("feather", fl(6.0))]),
        effect("unsharp_mask", &[("amount", fl(120.0)), ("radius", fl(2.0))]),
        effect("color_balance", &[("hi_b", fl(60.0)), ("shadow_r", fl(40.0))]),
    ];
    let chain_c = [effect("mirror", &[("angle", fl(90.0))]), effect("gaussian_blur", &[("blurriness", fl(50.0)), ("dimensions", ParamValue::Choice(1))])];
    let ramp = ramp_layer(160, 120, 0);
    for mode in [Blend::Normal, Blend::Screen, Blend::Multiply, Blend::Overlay] {
        let plan = FramePlan::Layers {
            width: w,
            height: h,
            layers: vec![
                // YUV source decoded at half size (decimation 2) with effects, filling the output
                PlanLayer {
                    frame: yuv_frame(640, 360),
                    matrix: Affine::IDENTITY,
                    opacity: 1.0,
                    blend: Blend::Normal,
                    fx: Some(Arc::new(LayerFx { size: (320, 180), decimation: 2, ops: ops(&chain_c, 320, 180, 0.5) })),
                    input: None,
                },
                PlanLayer {
                    frame: ramp.clone(),
                    matrix: Affine::motion(Vec2::new(170.0, 95.0), Vec2::new(1.1, 1.1), 17.0, Vec2::new(80.0, 60.0)),
                    opacity: 0.8,
                    blend: mode,
                    fx: Some(Arc::new(LayerFx { size: (160, 120), decimation: 1, ops: ops(&chain_a, 160, 120, 1.0) })),
                    input: None,
                },
                PlanLayer::new(ramp.clone(), Affine::translate(10.0, 20.0), 0.5, Blend::Normal),
                PlanLayer {
                    frame: yuv_frame(320, 180),
                    matrix: Affine::motion(Vec2::new(250.0, 70.0), Vec2::new(0.45, 0.45), -9.0, Vec2::new(160.0, 90.0)),
                    opacity: 0.7,
                    blend: mode,
                    fx: Some(Arc::new(LayerFx { size: (320, 180), decimation: 1, ops: ops(&chain_b, 320, 180, 1.0) })),
                    input: None,
                },
            ],
        };
        let cpu = execute_cpu(&plan).over_black_rgba8();
        c.composite(&plan);
        let (_, _, gpu) = c.read_output().expect("readback");
        let keep = interior(&plan);
        let (p99, mean, max) = stats8(&cpu, &gpu, &keep);
        let (ap99, amean, _) = stats8(&cpu, &gpu, &vec![true; keep.len()]);
        eprintln!("{mode:?}: interior 8-bit p99 {p99} mean {mean:.3} max {max}; with edges p99 {ap99} mean {amean:.3}");
        assert!(p99 <= 6 && mean < 1.5, "{mode:?}: p99 {p99}, mean {mean}");
    }
}

/// Plans without effects never create working textures.
#[test]
fn plain_layers_skip_the_effect_stage() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let plan = FramePlan::Layers { width: 64, height: 36, layers: vec![PlanLayer::new(yuv_frame(64, 36), Affine::IDENTITY, 1.0, Blend::Normal)] };
    c.composite(&plan);
    assert!(c.fx.as_ref().is_some_and(fx::FxStage::is_idle));
    let fx = LayerFx { size: (64, 36), decimation: 1, ops: vec![FxOp::BlackWhite] };
    let with = FramePlan::Layers {
        width: 64,
        height: 36,
        layers: vec![PlanLayer { fx: Some(Arc::new(fx)), ..PlanLayer::new(yuv_frame(64, 36), Affine::IDENTITY, 1.0, Blend::Normal) }],
    };
    c.composite(&with);
    assert!(!c.fx.as_ref().is_some_and(fx::FxStage::is_idle));
    // and they are released once no layer needs them
    c.composite(&plan);
    assert!(c.fx.as_ref().is_some_and(fx::FxStage::is_idle));
}

/// Without an effect stage (devices without compute shaders) the compositor renders a layer's
/// effects on the CPU itself: the picture is the same.
#[test]
fn layers_with_effects_fall_back_to_the_cpu_without_a_stage() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let ops: Vec<FxOp> = [effect("tint", &[("amount", fl(70.0))]), effect("gaussian_blur", &[("blurriness", fl(10.0))])]
        .iter()
        .filter_map(|e| FxOp::eval(e, &cx(Tick::ZERO, 1.0), 160, 120))
        .collect();
    let plan = FramePlan::Layers {
        width: 200,
        height: 150,
        layers: vec![
            PlanLayer::new(yuv_frame(200, 150), Affine::IDENTITY, 1.0, Blend::Normal),
            PlanLayer {
                fx: Some(Arc::new(LayerFx { size: (160, 120), decimation: 1, ops })),
                ..PlanLayer::new(
                    ramp_layer(160, 120, 2),
                    Affine::motion(Vec2::new(100.0, 75.0), Vec2::new(0.9, 0.9), 8.0, Vec2::new(80.0, 60.0)),
                    0.8,
                    Blend::Screen,
                )
            },
        ],
    };
    let cpu = execute_cpu(&plan).over_black_rgba8();
    for stage in [true, false] {
        let mut c = GpuCompositor::new(&dev, &q);
        if !stage {
            c.fx = None;
        }
        c.composite(&plan);
        let (_, _, gpu) = c.read_output().expect("readback");
        let (p99, mean, _) = stats8(&cpu, &gpu, &interior(&plan));
        eprintln!("effect stage {stage}: interior p99 {p99}, mean {mean:.3}");
        assert!(p99 <= 6 && mean < 1.5, "stage {stage}: p99 {p99}, mean {mean}");
    }
}

/// The half-float upload is lossless for this input, so a 1:1 effect source draw must be too.
/// Odd dimensions expose interpolation drift that can invent opacity before any effect runs.
#[test]
fn effect_source_identity_preserves_odd_sized_half_float_texels() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    for (w, h) in [(67u32, 41u32), (96, 54), (64, 32)] {
        let px = picture(w, h);
        let frame = VideoFrame::rgba_f32(w, h, px.clone());
        for n in [0, 1] {
            let (ow, oh, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: n, ops: Vec::new() }).expect("source image");
            assert_eq!((ow, oh, gpu.len()), (w, h, px.len()));
            for (i, (expected, actual)) in px.iter().zip(&gpu).enumerate() {
                assert_eq!(actual.to_bits(), expected.to_bits(), "{w}x{h} n{n} source component{i}: {actual} != {expected}");
            }
        }
    }
}

#[test]
fn alpha_invert_does_not_amplify_invented_opacity_at_odd_source_pixels() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (67, 41);
    let px = picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let op = FxOp::Invert { channel: 4, blend: 0.2 };
    let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
    op.apply(&mut cpu);
    let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("alpha invert image");
    assert_eq!(gpu.len(), px.len());
    let (worst, flips) = compare(&cpu.px, &gpu);
    assert!(worst <= EXACT && flips == 0.0, "alpha invert: {worst}, {flips}");
    let mut transparent = 0;
    for (source, result) in px.as_chunks::<4>().0.iter().zip(gpu.as_chunks::<4>().0.iter()) {
        if source[3] == 0.0 {
            transparent += 1;
            assert_eq!(&result[..3], &[0.0; 3], "a transparent source must not acquire neighboring RGB");
            assert!((result[3] - 0.8).abs() <= f32::EPSILON);
        }
    }
    assert_eq!(transparent, 402);
}

/// The exact source grid keeps the existing box-minification behavior at non-unit factors.
#[test]
fn effect_source_integer_decimation_preserves_cpu_box_oracle() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    for (w, h) in [(67u32, 41u32), (96, 54)] {
        let frame = VideoFrame::rgba_f32(w, h, picture(w, h));
        for n in [2, 4, 8] {
            let (ow, oh, cpu) = frame.to_linear_f32_decimated(n as usize);
            let (gw, gh, gpu) =
                c.effect_image(&frame, &LayerFx { size: (ow as u32, oh as u32), decimation: n, ops: Vec::new() }).expect("decimated source image");
            assert_eq!((gw as usize, gh as usize, gpu.len()), (ow, oh, cpu.len()));
            let (worst, flips) = compare(&cpu, &gpu);
            assert!(worst <= EXACT && flips == 0.0, "{w}x{h} n{n} source box: {worst}, {flips}");
        }
    }
}

/// A heavy Lumetri grade (Basic, Curves, Hue vs Sat, Color Wheels, HSL Secondary with Denoise and
/// Blur, a look LUT, Sharpen): what the timings in the M8c report measure.
fn heavy_grade() -> EffectInstance {
    effect(
        "lumetri",
        &[
            ("exposure", fl(0.3)),
            ("contrast", fl(25.0)),
            ("saturation", fl(110.0)),
            ("curve_luma", ParamValue::Curve(vec![[0.0, 0.0], [0.25, 0.18], [0.75, 0.85], [1.0, 1.0]])),
            ("curve_red", ParamValue::Curve(vec![[0.0, 0.02], [0.5, 0.55], [1.0, 1.0]])),
            ("hue_vs_sat", ParamValue::Curve(vec![[0.0, 0.5], [0.33, 0.7], [0.66, 0.4]])),
            ("wheel_shadows", pt(0.3, -0.2)),
            ("wheel_midtones", pt(-0.1, 0.2)),
            ("wheel_highlights", pt(0.2, 0.1)),
            ("wheel_midtones_l", fl(10.0)),
            ("hsl_on", ParamValue::Bool(true)),
            ("hsl_hue", fl(30.0)),
            ("hsl_hue_range", fl(60.0)),
            ("hsl_denoise", fl(50.0)),
            ("hsl_blur", fl(30.0)),
            ("hsl_sat", fl(130.0)),
            ("look_lut", ParamValue::Text("builtin:look-teal-orange".into())),
            ("look_intensity", fl(80.0)),
            ("sharpen", fl(30.0)),
        ],
    )
}

/// Milliseconds per frame of [`heavy_grade`] at 1080p and 4K: the CPU render (`effects::apply`)
/// and, where the plan hands the grade to the GPU, the effect stage (upload, passes, read-back).
/// `cargo test -p filmcraft-gpu --release -- --ignored lumetri_heavy_grade_timings --nocapture`
#[test]
#[ignore = "timing run for the report, not a check"]
fn lumetri_heavy_grade_timings() {
    let e = heavy_grade();
    let gpu = device();
    for (w, h) in [(1920u32, 1080u32), (3840, 2160)] {
        let px = picture(w, h);
        let c = cx(Tick::ZERO, 1.0);
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let cpu: Vec<f64> = (0..3)
            .map(|_| {
                let mut img = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
                let t0 = std::time::Instant::now();
                filmcraft_render::effects::apply(&mut img, &e, &c);
                t0.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        let op = FxOp::eval(&e, &c, w as usize, h as usize).expect("a Lumetri op");
        let gpu_ms = match (&gpu, op.gpu_ok()) {
            (Some((dev, q)), true) => {
                let mut comp = GpuCompositor::new(dev, q);
                let frame = VideoFrame::rgba_f32(w, h, px.clone());
                let fx = LayerFx { size: (w, h), decimation: 1, ops: vec![op.clone()] };
                let _ = comp.effect_image(&frame, &fx);
                let time = |fx: &LayerFx, comp: &mut GpuCompositor| {
                    median(
                        (0..5)
                            .map(|_| {
                                let t0 = std::time::Instant::now();
                                let _ = comp.effect_image(&frame, fx);
                                t0.elapsed().as_secs_f64() * 1000.0
                            })
                            .collect(),
                    )
                };
                let total = time(&fx, &mut comp);
                // the same upload and read-back with a pass that changes nothing (Black & White's cost is
                // negligible): what the compositor does not pay, since it never reads the frame back
                let transfer = time(&LayerFx { size: (w, h), decimation: 1, ops: vec![FxOp::BlackWhite] }, &mut comp);
                format!("{total:.1} ms incl. upload + read-back ({transfer:.1} ms of it for a one-pass chain): grade passes ~{:.1} ms", total - transfer)
            }
            (None, _) => "no GPU adapter".into(),
            (_, false) => "not GPU-capable (rendered on the CPU)".into(),
        };
        // what the plan and the stage do on the CPU per frame before the passes: evaluate (bake the
        // curve tables, resolve the LUT) and prepare the passes (pack tables)
        let prep_ms = gpu.as_ref().map(|(dev, _)| {
            let t0 = std::time::Instant::now();
            for _ in 0..5 {
                let op = FxOp::eval(&e, &c, w as usize, h as usize).expect("an op");
                let _ = fx::prepare(dev, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }, fx::FX_TEXTURE_BUDGET);
            }
            t0.elapsed().as_secs_f64() * 1000.0 / 5.0
        });
        eprintln!(
            "heavy grade {w}x{h}: CPU {:.1} ms, GPU {gpu_ms}; per-frame evaluation + pass preparation on the CPU {:.1} ms",
            median(cpu),
            prep_ms.unwrap_or(0.0)
        );
    }
    // a 1000-point mask covering the whole 4K frame around the heavy grade: every pixel tests every
    // edge in the mix pass, which runs in narrower bands (cost 1000 / 64)
    let Some((dev, q)) = &gpu else { return };
    let (w, h) = (3840u32, 2160u32);
    let mut comp = GpuCompositor::new(dev, q);
    let frame = VideoFrame::rgba_f32(w, h, picture(w, h));
    let n = 1000;
    let pts: Vec<[f32; 2]> = (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            [w as f32 * (0.5 + 0.49 * a.cos()), h as f32 * (0.5 + 0.49 * a.sin())]
        })
        .collect();
    let mask = filmcraft_render::mask::FlatMask { pts, feather: 20.0, expansion: 0.0, opacity: 1.0, inverted: false, mode: filmcraft_project::MaskMode::Add };
    let graded = FxOp::eval(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap();
    let masked = FxOp::Masked { op: Box::new(graded.clone()), masks: vec![mask] };
    assert!(masked.gpu_ok());
    let time = |op: &FxOp, comp: &mut GpuCompositor| {
        let fx = LayerFx { size: (w, h), decimation: 1, ops: vec![op.clone()] };
        let _ = comp.effect_image(&frame, &fx);
        let mut v: Vec<f64> = (0..3)
            .map(|_| {
                let t0 = std::time::Instant::now();
                let _ = comp.effect_image(&frame, &fx);
                t0.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        v.sort_by(f64::total_cmp);
        v[1]
    };
    let (plain_ms, masked_ms) = (time(&graded, &mut comp), time(&masked, &mut comp));
    let cost = (n as u32 / 64).max(1);
    let rows = (fx::BAND_PIXELS / cost / w).max(16).next_multiple_of(16);
    let dispatches = h.div_ceil(rows);
    eprintln!(
        "heavy grade 4K with a 1000-point full-frame mask: {masked_ms:.1} ms vs {plain_ms:.1} ms unmasked: the mask pass ~{:.1} ms in {dispatches} dispatches of {rows} rows, ~{:.1} ms per dispatch",
        masked_ms - plain_ms,
        (masked_ms - plain_ms) / f64::from(dispatches)
    );
}

/// Masks for the masked-effect tests on a `w`×`h` working image: a feathered ellipse (Add), an
/// expanded rectangle subtracted from it, and an inverted, half-opaque Difference ellipse.
fn effect_masks(w: f64, h: f64) -> Vec<filmcraft_project::Mask> {
    use filmcraft_project::{Mask, MaskMode, MaskPath};
    let mut a = Mask::new("a", MaskPath::ellipse(Vec2::new(w * 0.45, h * 0.5), Vec2::new(w * 0.3, h * 0.35)));
    a.feather.value = ParamValue::Float(12.0);
    let mut b = Mask::new("b", MaskPath::rect(w * 0.4, h * 0.1, w * 0.55, h * 0.45));
    b.mode = MaskMode::Subtract;
    b.expansion.value = ParamValue::Float(4.0);
    let mut c = Mask::new("c", MaskPath::ellipse(Vec2::new(w * 0.75, h * 0.7), Vec2::new(w * 0.15, h * 0.2)));
    c.mode = MaskMode::Difference;
    c.inverted = true;
    c.opacity.value = ParamValue::Float(50.0);
    c.feather.value = ParamValue::Float(3.0);
    vec![a, b, c]
}

/// Masked effects (`FxOp::Masked`) on the GPU against the CPU (`mask::coverage` + mix, as
/// `mask::apply_effect`): Lumetri's heavy grade (whose HSL Secondary and Sharpen save the image
/// inside the mask's own save), Unsharp Mask, and a single feathered mask.
#[test]
fn masked_effects_match_cpu() {
    let Some((dev, q)) = gpu_or_skip("masked_effects_match_cpu") else { return };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (320u32, 180u32);
    let px = demo_picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let all = effect_masks(f64::from(w), f64::from(h));
    let mut failures = Vec::new();
    for (name, mut e, masks) in [
        ("heavy grade, three masks", heavy_grade(), all.clone()),
        ("unsharp mask, three masks", effect("unsharp_mask", &[("amount", fl(150.0)), ("radius", fl(6.0))]), all.clone()),
        ("tint, one feathered ellipse", effect("tint", &[("amount", fl(100.0))]), vec![all[0].clone()]),
        ("HSL show mask, inverted ellipse", effect("lumetri", &[("hsl_on", ParamValue::Bool(true)), ("hsl_show_mask", ch(3))]), vec![all[2].clone()]),
    ] {
        e.masks = masks;
        let op = FxOp::eval_masked(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).expect("an op");
        assert!(matches!(op, FxOp::Masked { .. }), "{name}: not masked");
        assert!(op.gpu_ok(), "{name}: not GPU-capable");
        let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
        op.apply(&mut cpu);
        // the CPU render's own path (`mask::apply_effect`) gives the same picture
        let mut reference = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
        filmcraft_render::mask::apply_effect(&mut reference, &e, &cx(Tick::ZERO, 1.0));
        assert_eq!(reference.px, cpu.px, "{name}: FxOp::Masked is mask::apply_effect");
        let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("effect image");
        let (fl_diff, b8) = diff_float_and_8bit(&cpu.px, &gpu);
        eprintln!("{name}: max float diff {fl_diff:.2e}, max 8-bit diff {b8}");
        if fl_diff > 1.0 / 255.0 || b8 > 1 {
            failures.push(format!("{name}: float {fl_diff:.2e}, 8-bit {b8}"));
        }
    }
    assert!(failures.is_empty(), "GPU masked effects differ from the CPU:\n{}", failures.join("\n"));
}

/// Masks the GPU stage leaves to the CPU (too many polygon points, non-finite numbers), and masks
/// switched off (mode None), which leave the effect unmasked.
#[test]
fn masked_effects_gpu_boundary() {
    use filmcraft_project::{Mask, MaskMode, MaskPath};
    let (w, h) = (320usize, 180usize);
    let c1 = cx(Tick::ZERO, 1.0);
    let mut e = effect("tint", &[("amount", fl(100.0))]);
    let mut off = Mask::new("off", MaskPath::rect(10.0, 10.0, 50.0, 50.0));
    off.mode = MaskMode::None;
    e.masks = vec![off];
    assert!(matches!(FxOp::eval_masked(&e, &c1, w, h), Some(FxOp::Tint { .. })), "a mask set to None does not mask");
    // a polygon with more points than the stage takes
    let n = filmcraft_render::gpufx::GPU_MASK_MAX_POINTS + 1;
    let pts: Vec<[f32; 2]> = (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            [160.0 + 60.0 * a.cos(), 90.0 + 60.0 * a.sin()]
        })
        .collect();
    let flat = |pts: Vec<[f32; 2]>, feather: f32| filmcraft_render::mask::FlatMask {
        pts,
        feather,
        expansion: 0.0,
        opacity: 1.0,
        inverted: false,
        mode: MaskMode::Add,
    };
    let masked = |m| FxOp::Masked { op: Box::new(FxOp::BlackWhite), masks: vec![m] };
    assert!(!masked(flat(pts.clone(), 1.0)).gpu_ok());
    assert!(masked(flat(pts[..100].to_vec(), 1.0)).gpu_ok());
    assert!(!masked(flat(pts[..100].to_vec(), f32::NAN)).gpu_ok());
    // finite but huge coordinates (a hostile project file) stay on the CPU: the shader's squared
    // lengths would overflow
    let mut far = pts[..10].to_vec();
    far[3] = [2e7, 90.0];
    assert!(!masked(flat(far, 1.0)).gpu_ok());
    assert!(!masked(filmcraft_render::mask::FlatMask { expansion: 5e7, ..flat(pts[..10].to_vec(), 1.0) }).gpu_ok());
    // a curve table of another length than the shader's (a future change to the baking) goes to the CPU
    let mut op = FxOp::eval(&effect("lumetri", &[("curve_luma", ParamValue::Curve(vec![[0.0, 0.1], [1.0, 0.9]]))]), &c1, w, h).unwrap();
    assert!(op.gpu_ok());
    if let FxOp::Lumetri { advanced: Some(a), .. } = &mut op {
        a.curves[0] = Some(vec![0.5; 512]);
    }
    assert!(!op.gpu_ok());
    // an inner op the GPU does not take keeps the whole masked effect on the CPU
    assert!(!FxOp::Masked { op: Box::new(FxOp::Gamma { g: f32::NAN }), masks: vec![flat(pts[..10].to_vec(), 1.0)] }.gpu_ok());
}

/// A working image larger than one band (`fx::BAND_PIXELS`) runs each per-pixel pass as several
/// dispatches: every row is written once, and passes that read neighbours (Denoise, Blur, Sharpen)
/// read across the band edges, so the picture is the CPU's.
#[test]
fn banded_passes_cover_every_row() {
    let Some((dev, q)) = gpu_or_skip("banded_passes_cover_every_row") else { return };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (2048u32, 1100u32);
    assert!(w * h > fx::BAND_PIXELS && (fx::BAND_PIXELS / w) < h, "two bands at least");
    let px = picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    let op = FxOp::eval(&heavy_grade(), &cx(Tick::ZERO, 1.0), w as usize, h as usize).expect("an op");
    let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px };
    op.apply(&mut cpu);
    let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("effect image");
    let (fl_diff, b8) = diff_float_and_8bit(&cpu.px, &gpu);
    eprintln!("banded heavy grade {w}x{h}: max float diff {fl_diff:.2e}, max 8-bit diff {b8}");
    assert!(fl_diff <= 1.0 / 255.0 && b8 <= 1, "{fl_diff} / {b8}");
}

/// LUTs as `.cube` files bring them: a 1D shaper (1024-entry gamma) in front of a 17³ cube, a cube
/// over a non-unit DOMAIN (-0.1…1.5, 0…1, 0…2), and a shaper alone.
fn file_luts() -> Vec<(&'static str, filmcraft_color::Lut)> {
    use filmcraft_color::{Lut, Lut1d, Lut3d};
    // a non-linear, channel-mixing cube (not affine: tetrahedral and trilinear differ on it)
    let wild = |n: usize| {
        Lut3d::from_fn(n, |c| [(c[0] * 0.8 + c[1] * 0.3).powf(1.3), (c[1] * c[1] + 0.1 * c[2]).min(1.2), (0.5 + 0.5 * (c[2] * 3.0 + c[0]).sin()) * 0.9])
    };
    let shaper = Lut1d { domain_min: [0.0; 3], domain_max: [1.0; 3], data: (0..1024).map(|i| [(i as f32 / 1023.0).powf(0.6); 3]).collect() };
    let mut domain = wild(17);
    domain.domain_min = [-0.1, 0.0, 0.0];
    domain.domain_max = [1.5, 1.0, 2.0];
    vec![
        ("shaper + cube", Lut { title: String::new(), shaper: Some(shaper.clone()), cube: Some(wild(17)) }),
        ("non-unit DOMAIN cube", Lut { title: String::new(), shaper: None, cube: Some(domain) }),
        ("shaper alone", Lut { title: String::new(), shaper: Some(shaper), cube: None }),
    ]
}

/// Imported LUTs (shaper + cube, non-unit DOMAIN, shaper alone) as Input LUT and as a look LUT at
/// 50 %, on the GPU against the CPU, on the chart and the demo scene.
#[test]
fn file_luts_match_cpu() {
    let Some((dev, q)) = gpu_or_skip("file_luts_match_cpu") else { return };
    let mut c = GpuCompositor::new(&dev, &q);
    let mut failures = Vec::new();
    for (picture_name, (w, h), px) in [("chart", (67u32, 41u32), picture(67, 41)), ("demo scene", (320, 180), demo_picture(320, 180))] {
        let frame = VideoFrame::rgba_f32(w, h, px.clone());
        for (lut_name, lut) in file_luts() {
            let lut = std::sync::Arc::new(lut);
            assert!(filmcraft_render::gpufx::gpu_lut_ok(&lut), "{lut_name}");
            let as_input = {
                let mut op = FxOp::eval(&effect("lumetri", &[("contrast", fl(10.0))]), &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap();
                let FxOp::Lumetri { input_lut, .. } = &mut op else { panic!() };
                *input_lut = Some(lut.clone());
                op
            };
            let as_look = {
                let e = effect("lumetri", &[("look", ch(1)), ("look_intensity", fl(50.0))]);
                let mut op = FxOp::eval(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap();
                let FxOp::Lumetri { advanced: Some(a), .. } = &mut op else { panic!() };
                a.look_lut = Some(lut.clone());
                op
            };
            for (role, op) in [("input LUT", as_input), ("look LUT 50%", as_look)] {
                assert!(op.gpu_ok(), "{lut_name} as {role}");
                let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
                op.apply(&mut cpu);
                let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("effect image");
                let (fl_diff, b8) = diff_float_and_8bit(&cpu.px, &gpu);
                eprintln!("{picture_name} / {lut_name} as {role}: max float diff {fl_diff:.2e}, max 8-bit diff {b8}");
                if fl_diff > 1.0 / 255.0 || b8 > 1 {
                    failures.push(format!("{picture_name} / {lut_name} as {role}: float {fl_diff:.2e}, 8-bit {b8}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "GPU LUTs differ from the CPU:\n{}", failures.join("\n"));
}

/// The cases the first parity tests left out: an HSL key whose hue range crosses 0°/360°, playback
/// resolutions other than full (px_scale 0.5: Denoise radius 1 and the Sharpen floor; 2: Blur radii
/// beyond `BOX_PER_PIXEL_MAX`, run as running sums), the remaining mask modes (Intersect, Lighten,
/// Darken) after an inverted first mask, and a 3840-wide image whose band is not a power of two.
#[test]
fn remaining_parity_cases() {
    use filmcraft_project::{Mask, MaskMode, MaskPath};
    let Some((dev, q)) = gpu_or_skip("remaining_parity_cases") else { return };
    let mut c = GpuCompositor::new(&dev, &q);
    let on = ParamValue::Bool(true);
    let mut failures = Vec::new();
    let mut check = |name: &str, w: u32, h: u32, px: &[f32], op: FxOp, c: &mut GpuCompositor| {
        assert!(op.gpu_ok(), "{name}: not GPU-capable");
        let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.to_vec() };
        op.apply(&mut cpu);
        let frame = VideoFrame::rgba_f32(w, h, px.to_vec());
        let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("effect image");
        let (fl_diff, b8) = diff_float_and_8bit(&cpu.px, &gpu);
        eprintln!("{name}: max float diff {fl_diff:.2e}, max 8-bit diff {b8}");
        if fl_diff > 1.0 / 255.0 || b8 > 1 {
            failures.push(format!("{name}: float {fl_diff:.2e}, 8-bit {b8}"));
        }
    };
    let (w, h) = (320u32, 180u32);
    let demo = demo_picture(w, h);
    let wrap =
        effect("lumetri", &[("hsl_on", on.clone()), ("hsl_hue", fl(350.0)), ("hsl_hue_range", fl(40.0)), ("hsl_sat", fl(160.0)), ("hsl_hue_shift", fl(30.0))]);
    check("HSL hue range across 0°", w, h, &demo, FxOp::eval(&wrap, &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap(), &mut c);
    let res = effect("lumetri", &[("hsl_on", on.clone()), ("hsl_hue", fl(30.0)), ("hsl_denoise", fl(20.0)), ("hsl_blur", fl(100.0)), ("sharpen", fl(50.0))]);
    for px_scale in [0.5f32, 2.0] {
        let op = FxOp::eval(&res, &cx(Tick::ZERO, px_scale), w as usize, h as usize).unwrap();
        if px_scale == 2.0 {
            let FxOp::Lumetri { hsl: Some(hs), .. } = &op else { panic!() };
            assert!(hs.blur_radii().iter().any(|r| *r as u32 > fx::BOX_PER_PIXEL_MAX), "{:?}", hs.blur_radii());
        }
        check(&format!("HSL Denoise + Blur + Sharpen at px_scale {px_scale}"), w, h, &demo, op, &mut c);
    }
    // inverted first mask (its mode starts at 1), then Intersect, Lighten, Darken
    let (fw, fh) = (f64::from(w), f64::from(h));
    let mut a = Mask::new("a", MaskPath::ellipse(Vec2::new(fw * 0.5, fh * 0.5), Vec2::new(fw * 0.35, fh * 0.4)));
    a.mode = MaskMode::Subtract;
    a.inverted = true;
    a.feather.value = ParamValue::Float(8.0);
    let mut modes = vec![a];
    for (i, mode) in [MaskMode::Intersect, MaskMode::Lighten, MaskMode::Darken].into_iter().enumerate() {
        let x = fw * (0.3 + 0.2 * i as f64);
        let mut m = Mask::new("m", MaskPath::rect(x - 40.0, fh * 0.2, x + 40.0, fh * 0.8));
        m.mode = mode;
        m.feather.value = ParamValue::Float(5.0);
        m.opacity.value = ParamValue::Float(80.0);
        modes.push(m);
    }
    let mut e = effect("lumetri", &[("saturation", fl(0.0)), ("exposure", fl(0.5))]);
    e.masks = modes;
    check(
        "masks: inverted first, Intersect, Lighten, Darken",
        w,
        h,
        &demo,
        FxOp::eval_masked(&e, &cx(Tick::ZERO, 1.0), w as usize, h as usize).unwrap(),
        &mut c,
    );
    // 3840 wide: 546 rows per band rounded to 560, then a 40-row tail
    let (bw, bh) = (3840u32, 600u32);
    let band_rows = (fx::BAND_PIXELS / bw).max(16).next_multiple_of(16);
    assert!(!band_rows.is_power_of_two() && bh > band_rows && bh % band_rows != 0, "{band_rows}");
    let wide = picture(bw, bh);
    let blur = effect("lumetri", &[("hsl_on", on), ("hsl_hue", fl(200.0)), ("hsl_denoise", fl(60.0)), ("hsl_blur", fl(40.0)), ("sharpen", fl(30.0))]);
    check("3840x600 bands", bw, bh, &wide, FxOp::eval(&blur, &cx(Tick::ZERO, 1.0), bw as usize, bh as usize).unwrap(), &mut c);
    assert!(failures.is_empty(), "GPU differs from the CPU:\n{}", failures.join("\n"));
}

/// The working-texture budget: a layer whose saved images would need more than
/// `FX_TEXTURE_BUDGET` is not prepared for the GPU (the compositor renders its effects on the
/// CPU), one within it is; a prepared job's texture count follows its nesting.
#[test]
fn working_textures_stay_within_the_budget() {
    let Some((dev, _q)) = gpu_or_skip("working_textures_stay_within_the_budget") else { return };
    let masked = |size: (u32, u32)| {
        let e = {
            let mut e = heavy_grade();
            e.masks = effect_masks(f64::from(size.0), f64::from(size.1));
            e
        };
        let op = FxOp::eval_masked(&e, &cx(Tick::ZERO, 1.0), size.0 as usize, size.1 as usize).unwrap();
        LayerFx { size, decimation: 1, ops: vec![op] }
    };
    // 4K masked heavy grade: 4 textures × 3840 × 2160 × 16 B ≈ 531 MB, within the budget
    assert!(fx::prepare(&dev, &masked((3840, 2160)), fx::FX_TEXTURE_BUDGET).is_some());
    // 7680 × 4320 with the same nesting would need ≈ 2.1 GB: refused (CPU), or beyond the texture size
    assert!(fx::prepare(&dev, &masked((7680, 4320)), fx::FX_TEXTURE_BUDGET).is_none());
    // a plain grade at 7680 × 4320 needs two: 1 061 683 200 B, just within the 1 GiB budget
    // (where the device's texture size allows 8K at all)
    let plain = FxOp::eval(&effect("lumetri", &[("exposure", fl(0.2))]), &cx(Tick::ZERO, 1.0), 7680, 4320).unwrap();
    let big = fx::prepare(&dev, &LayerFx { size: (7680, 4320), decimation: 1, ops: vec![plain.clone()] }, fx::FX_TEXTURE_BUDGET);
    assert!(big.as_ref().is_none_or(|p| p.n == 2) && (big.is_some() || dev.limits().max_texture_dimension_2d < 7680));
    // 8K with one saved image (three textures, 1.59 GB): refused
    let unsharp = FxOp::eval(&effect("unsharp_mask", &[("amount", fl(100.0))]), &cx(Tick::ZERO, 1.0), 7680, 4320).unwrap();
    assert!(fx::prepare(&dev, &LayerFx { size: (7680, 4320), decimation: 1, ops: vec![unsharp] }, fx::FX_TEXTURE_BUDGET).is_none());
    assert!(fx::prepare(&dev, &LayerFx { size: (3840, 2160), decimation: 1, ops: vec![plain] }, fx::FX_TEXTURE_BUDGET).is_some());
}

/// Every way the GPU stage can turn a graded layer down gives the CPU render of the grade, never
/// the ungraded frame: working textures over the budget, the device refusing them (out of memory),
/// a table larger than the device's storage binding, and recording the prepared passes failing
/// (which no input reaches; forced here), which renders the frame again with the layer on the CPU.
#[test]
fn no_preparation_failure_drops_a_grade() {
    let Some((dev, q)) = gpu_or_skip("no_preparation_failure_drops_a_grade") else { return };
    let (w, h) = (320usize, 180usize);
    let grade = |lw: u32, lh: u32| {
        let mut e = heavy_grade();
        e.masks = effect_masks(f64::from(lw), f64::from(lh));
        FxOp::eval_masked(&e, &cx(Tick::ZERO, 1.0), lw as usize, lh as usize).unwrap()
    };
    // an 8-bit SDR layer, and a 10-bit PQ layer whose input transform (HDR to the SDR working
    // space) the CPU fallback must apply too
    let pq = {
        use filmcraft_color::{ColorInfo, ColorPipeline, ColorSpace, InputTransform, Primaries};
        let n = w * h;
        let y: Vec<u16> = (0..n).map(|i| (64 + (i % w) * 876 / w) as u16).collect();
        let cb: Vec<u16> = (0..n).map(|i| (300 + (i / w) * 400 / h) as u16).collect();
        let cr: Vec<u16> = (0..n).map(|i| (700 - (i % w) * 300 / w) as u16).collect();
        let color = ColorInfo { matrix: Matrix::Bt2020Ncl, transfer: Transfer::Pq, primaries: Primaries::Bt2020, range: Range::Limited };
        let data = PixelData::Yuv16 { planes: [Arc::new(y), Arc::new(cb), Arc::new(cr)], chroma: Chroma::C444, bits: 10, alpha: None };
        let frame = Arc::new(VideoFrame { width: w as u32, height: h as u32, data, color, par: (1, 1), pts: Default::default() });
        let t = Arc::new(InputTransform::new(ColorSpace::from_info(&color), color.range, &ColorPipeline::REC709, None));
        (frame, Some(t))
    };
    for (kind, (frame, input)) in [("SDR 8-bit", (yuv_frame(320, 180), None)), ("PQ 10-bit with an input transform", pq)] {
        let layer = PlanLayer {
            frame,
            matrix: Affine::IDENTITY,
            opacity: 1.0,
            blend: Blend::Normal,
            fx: Some(Arc::new(LayerFx { size: (320, 180), decimation: 1, ops: vec![grade(320, 180)] })),
            input,
        };
        let plan = FramePlan::Layers { width: w, height: h, layers: vec![layer.clone()] };
        let cpu = execute_cpu(&plan).over_black_rgba8();
        let all = vec![true; w * h];
        let ungraded = execute_cpu(&FramePlan::Layers { width: w, height: h, layers: vec![PlanLayer { fx: None, ..layer.clone() }] }).over_black_rgba8();
        let (_, graded_mean, _) = stats8(&cpu, &ungraded, &all);
        assert!(graded_mean > 5.0, "{kind}: the grade changes the picture: {graded_mean}");
        if layer.input.is_some() {
            // and the input transform matters: without it the same layer is far off
            let raw = execute_cpu(&FramePlan::Layers { width: w, height: h, layers: vec![PlanLayer { input: None, ..layer.clone() }] }).over_black_rgba8();
            let (_, off, _) = stats8(&cpu, &raw, &all);
            assert!(off > 5.0, "{kind}: the input transform changes the picture: {off}");
        }
        let check = |what: &str, c: &mut GpuCompositor| {
            c.composite(&plan);
            let (_, _, gpu) = c.read_output().expect("readback");
            let (p99, mean, _) = stats8(&cpu, &gpu, &all);
            eprintln!("{kind} / {what}: against the CPU render p99 {p99} mean {mean:.3}");
            assert!(p99 <= 6 && mean < 1.5, "{kind} / {what}: p99 {p99} mean {mean}: not the graded picture");
        };
        let mut c = GpuCompositor::new(&dev, &q);
        check("GPU stage", &mut c);
        c.fx.as_mut().unwrap().budget = 1;
        check("working textures over the budget (CPU)", &mut c);
        c.fx.as_mut().unwrap().budget = fx::FX_TEXTURE_BUDGET;
        c.fx.as_mut().unwrap().fail_reserve = true;
        check("device refuses the working textures (CPU)", &mut c);
        c.fx.as_mut().unwrap().fail_job = true;
        check("recording the passes fails (frame rendered again, layer on the CPU)", &mut c);
        assert!(!c.fx.as_ref().unwrap().fail_job, "the forced failure was reached");
        check("GPU stage again", &mut c);
        // a device whose storage bindings cannot hold the grade's tables (the curves alone are 36 KB)
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).expect("the adapter of `device()`");
        let limits = wgpu::Limits { max_storage_buffer_binding_size: 16 * 1024, ..wgpu::Limits::downlevel_defaults() }.using_resolution(adapter.limits());
        let small = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_limits: limits, ..Default::default() }));
        match small {
            Ok((sdev, sq)) => {
                assert!(sdev.limits().max_storage_buffer_binding_size <= 16 * 1024);
                assert!(fx::prepare(&sdev, &LayerFx { size: (320, 180), decimation: 1, ops: vec![grade(320, 180)] }, fx::FX_TEXTURE_BUDGET).is_none());
                let mut c = GpuCompositor::new(&sdev, &sq);
                check("tables over the storage binding (CPU)", &mut c);
            }
            Err(e) => eprintln!("SKIPPED the small-binding device: {e}"),
        }
    }
}

/// Masks without area still cost every pixel a pass of the coverage loop (and an inverted empty
/// mask still covers everything), so the stage takes at most `GPU_MASK_MAX_MASKS` of them: many
/// empty and inverted-empty masks go to the CPU, a few stay on the GPU and match the CPU.
#[test]
fn empty_masks_are_bounded_and_match_cpu() {
    use filmcraft_project::MaskMode;
    let flat =
        |inverted: bool, mode: MaskMode| filmcraft_render::mask::FlatMask { pts: Vec::new(), feather: 4.0, expansion: 0.0, opacity: 0.9, inverted, mode };
    let many: Vec<_> = (0..filmcraft_render::gpufx::GPU_MASK_MAX_MASKS + 1).map(|i| flat(i % 2 == 1, MaskMode::Add)).collect();
    assert!(!FxOp::Masked { op: Box::new(FxOp::BlackWhite), masks: many }.gpu_ok(), "too many masks: CPU");
    let Some((dev, q)) = gpu_or_skip("empty_masks_are_bounded_and_match_cpu") else { return };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (320u32, 180u32);
    let px = demo_picture(w, h);
    let frame = VideoFrame::rgba_f32(w, h, px.clone());
    for (name, masks) in [
        ("empty Add masks", (0..8).map(|_| flat(false, MaskMode::Add)).collect::<Vec<_>>()),
        ("inverted empty masks", (0..8).map(|_| flat(true, MaskMode::Add)).collect()),
        (
            "mixed empty, inverted, Intersect / Difference",
            vec![flat(true, MaskMode::Add), flat(false, MaskMode::Intersect), flat(true, MaskMode::Difference), flat(false, MaskMode::Lighten)],
        ),
        ("as many as allowed", (0..filmcraft_render::gpufx::GPU_MASK_MAX_MASKS).map(|i| flat(i % 3 == 0, MaskMode::Add)).collect()),
    ] {
        let op = FxOp::Masked { op: Box::new(FxOp::BlackWhite), masks };
        assert!(op.gpu_ok(), "{name}");
        let mut cpu = filmcraft_render::Image { w: w as usize, h: h as usize, px: px.clone() };
        op.apply(&mut cpu);
        let (_, _, gpu) = c.effect_image(&frame, &LayerFx { size: (w, h), decimation: 1, ops: vec![op] }).expect("effect image");
        let (fl_diff, b8) = diff_float_and_8bit(&cpu.px, &gpu);
        eprintln!("{name}: max float diff {fl_diff:.2e}, max 8-bit diff {b8}");
        assert!(fl_diff <= 1.0 / 255.0 && b8 <= 1, "{name}: {fl_diff} / {b8}");
    }
}
