//! The GPU effect stage: standard effects of a plan layer ([`LayerFx`]) run in WGSL (`fx.wgsl`).
//!
//! A layer with effects is first drawn from its source (YUV planes or RGBA, through the
//! compositor's own `fs` shader, so decoding is the same as for a plain layer) into a working
//! texture of the size the CPU decodes it at; each [`FxOp`] then runs as compute passes that
//! ping-pong between two working textures (three for Unsharp Mask, which keeps the original);
//! the last one is drawn into the accumulator with the layer's matrix, opacity and blend mode
//! like any RGBA layer. Working textures are `Rgba32Float` (the CPU reference's precision) and
//! pooled per size; layers without effects never touch any of this.
//!
//! Box blurs (Gaussian Blur, Camera Blur, the blur inside Unsharp / Sharpen) run per pixel for
//! radii up to [`BOX_PER_PIXEL_MAX`] and as a running sum per row / column above, so a huge
//! radius costs O(1) per pixel like on the CPU (radii are capped by `gaussian_boxes`).

use std::collections::{HashMap, HashSet};

use filmcraft_render::gpufx::FxOp;
use filmcraft_render::plan::LayerFx;

/// Working texture format.
pub(crate) const FX_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;
/// Box radii up to this many pixels are summed per output pixel; larger ones as a running sum.
pub const BOX_PER_PIXEL_MAX: u32 = 32;

/// Most pixels one dispatch of a per-pixel pass covers: larger images run a pass as several
/// dispatches over bands of rows, so no single dispatch keeps the GPU busy for long (integrated
/// GPUs, whose watchdog ends a dispatch that runs too long, run 4K grades as four bands).
pub const BAND_PIXELS: u32 = 1 << 21;

const OP_BRIGHTNESS_CONTRAST: u32 = 1;
const OP_PROC_AMP: u32 = 2;
const OP_TINT: u32 = 3;
const OP_BLACK_WHITE: u32 = 4;
const OP_COLOR_BALANCE: u32 = 5;
const OP_LEAVE_COLOR: u32 = 6;
const OP_CHANGE_TO_COLOR: u32 = 7;
const OP_COLOR_PASS: u32 = 8;
const OP_GAMMA: u32 = 9;
const OP_LEVELS: u32 = 10;
const OP_EXTRACT: u32 = 11;
const OP_INVERT: u32 = 12;
const OP_INVERT_ALPHA: u32 = 13;
const OP_POSTERIZE: u32 = 14;
const OP_ASC_CDL: u32 = 15;
const OP_CHANNEL_MIX: u32 = 16;
const OP_COLOR_REPLACE: u32 = 17;
const OP_ALPHA_ADJUST: u32 = 18;
const OP_VIGNETTE: u32 = 19;
const OP_BOX: u32 = 20;
const OP_DIRECTIONAL: u32 = 22;
const OP_UNSHARP: u32 = 23;
const OP_CROP: u32 = 24;
const OP_RESAMPLE: u32 = 25;
const OP_HFLIP: u32 = 26;
const OP_VFLIP: u32 = 27;
const OP_MIRROR: u32 = 28;
const OP_OFFSET: u32 = 29;
const OP_VIDEO_LIMITER: u32 = 30;
const OP_LUMETRI: u32 = 31;
const OP_LUMETRI_ADVANCED: u32 = 32;
const OP_HSL_KEY: u32 = 33;
const OP_HSL_MEDIAN: u32 = 34;
const OP_HSL_APPLY: u32 = 35;
const OP_MASK_MIX: u32 = 36;

/// An effect's active masks as `fx.wgsl`'s `mask_coverage` reads them: the count, then per mask its
/// [`MaskInfo`](mask_common.wgsl) fields (start, length, mode, inverted, feather, expansion, opacity,
/// 0) and bounding box (min x, min y, max x, max y), then every polygon point (x, y).
fn mask_data(masks: &[filmcraft_render::mask::FlatMask]) -> Vec<f32> {
    let active: Vec<_> = masks.iter().filter(|m| m.mode.index() != 0).collect();
    let mut head = vec![active.len() as f32, 0.0, 0.0, 0.0];
    let mut pts: Vec<f32> = Vec::new();
    for m in &active {
        let start = pts.len() / 2;
        let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
        for p in &m.pts {
            for k in 0..2 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        if m.pts.is_empty() {
            (lo, hi) = ([0.0; 2], [0.0; 2]);
        }
        head.extend([start as f32, m.pts.len() as f32, m.mode.index() as f32, f32::from(u8::from(m.inverted)), m.feather, m.expansion, m.opacity, 0.0]);
        head.extend([lo[0], lo[1], hi[0], hi[1]]);
        pts.extend(m.pts.iter().flatten());
    }
    head.extend(pts);
    head
}

/// Where the advanced pass's look LUT starts in its tables (after nine curves; `fx.wgsl`'s
/// `LOOK_LUT_BASE`).
const LOOK_LUT_BASE: usize = 9 * filmcraft_render::effects::CURVE_SAMPLES;

/// A LUT as `fx.wgsl`'s `lut_apply` reads it: a header (shaper size, cube size, then the shaper's
/// and the cube's domain min / max, each padded to four), the shaper's entries, the cube's entries
/// (red fastest), three floats each.
fn lut_data(lut: &filmcraft_color::Lut) -> Vec<f32> {
    let sn = lut.shaper.as_ref().map_or(0, |s| s.data.len());
    let cn = lut.cube.as_ref().map_or(0, |c| c.size);
    let pad = |v: [f32; 3]| [v[0], v[1], v[2], 0.0];
    let mut d = vec![sn as f32, cn as f32, 0.0, 0.0];
    let (smin, smax) = lut.shaper.as_ref().map_or(([0.0; 3], [1.0; 3]), |s| (s.domain_min, s.domain_max));
    let (cmin, cmax) = lut.cube.as_ref().map_or(([0.0; 3], [1.0; 3]), |c| (c.domain_min, c.domain_max));
    for v in [smin, smax, cmin, cmax] {
        d.extend(pad(v));
    }
    if let Some(s) = &lut.shaper {
        d.extend(s.data.iter().flatten());
    }
    if let Some(c) = &lut.cube {
        d.extend(c.data.iter().flatten());
    }
    d
}

type Target = (wgpu::Texture, wgpu::TextureView);

pub(crate) struct FxStage {
    px: wgpu::ComputePipeline,
    run: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    /// Draws a layer's source into a working texture (`composite.wgsl` `fs`, no blending).
    source: wgpu::RenderPipeline,
    pool: HashMap<(u32, u32), Vec<Target>>,
    used: HashSet<(u32, u32)>,
}

/// One compute pass of a job.
struct Dispatch {
    run: bool,
    bg: wgpu::BindGroup,
    groups: (u32, u32),
}

/// The recorded work of one layer's effects (bind groups made; recorded into the frame's encoder
/// right before the layer is drawn, so layers can share pooled textures).
pub(crate) struct FxJob {
    source_bg: wgpu::BindGroup,
    source_target: wgpu::TextureView,
    dispatches: Vec<Dispatch>,
    /// The texture holding the result.
    pub(crate) result: wgpu::TextureView,
    pub(crate) result_texture: wgpu::Texture,
}

/// One pass before texture routing: entry point, op code, integer and float parameters.
///
/// Passes that need an earlier image again (Unsharp's combine needs the image before its blur)
/// use a stack of saved images: `save` keeps the image this pass reads until a later `restore`
/// pass, which reads it as `aux` and lets it go. Saved images are never overwritten meanwhile,
/// and saves nest.
struct Step {
    run: bool,
    code: u32,
    i1: [u32; 4],
    p: [f32; 24],
    /// Save the image this pass reads (before running it), this many times (nested groups that
    /// start on the same pass each save it).
    saves: u32,
    /// Read the most recently saved image as `aux`, then drop it.
    restore: bool,
    /// Tables the pass reads (`data` in the shader: curve LUTs, 3D LUTs...); empty for most.
    data: Vec<f32>,
}

fn step(code: u32, i1: [u32; 4], p: &[f32]) -> Step {
    let mut q = [0.0; 24];
    for (d, s) in q.iter_mut().zip(p) {
        *d = *s;
    }
    Step { run: false, code, i1, p: q, saves: 0, restore: false, data: Vec::new() }
}

/// Mark `group` (the passes of one op, in order) as working on a saved copy of its input: the
/// first pass saves the image it reads, the last restores it.
fn saving(mut group: Vec<Step>) -> Vec<Step> {
    if let Some(first) = group.first_mut() {
        first.saves += 1;
    }
    if let Some(last) = group.last_mut() {
        last.restore = true;
    }
    group
}

/// Box passes along x (vertical = false) or y for the radii (radius 0 is a no-op on the CPU).
fn boxes(radii: &[u32], vertical: bool, repeat: bool, out: &mut Vec<Step>) {
    for &r in radii.iter().filter(|r| **r > 0) {
        let mut s = step(OP_BOX, [r, repeat as u32, vertical as u32, 0], &[]);
        s.run = r > BOX_PER_PIXEL_MAX;
        out.push(s);
    }
}

/// Unsharp Mask: blur passes on a saved copy of the image, then the combine (blurred image as
/// `src`, the original as `aux`). Without blur passes (radius 0) the combine compares the image
/// with itself.
fn unsharp(rx: &[u32], ry: &[u32], amount: f32, threshold: f32) -> Vec<Step> {
    let mut g = Vec::new();
    boxes(rx, false, true, &mut g);
    boxes(ry, true, true, &mut g);
    g.push(step(OP_UNSHARP, [0; 4], &[amount, threshold]));
    saving(g)
}

/// Lumetri's look, Color Wheels and Curves in one pass: the curves' tables (unused slots stay 0)
/// and the look LUT in `data`, which curves are on as bits of `i1.x`.
fn advanced_step(a: &filmcraft_render::effects::LumetriAdvanced) -> Step {
    let n = filmcraft_render::effects::CURVE_SAMPLES;
    let mut data = vec![0.0f32; LOOK_LUT_BASE];
    let mut bits = 0u32;
    for (k, c) in a.curves.iter().enumerate() {
        if let Some(c) = c.as_ref().filter(|c| c.len() == n) {
            bits |= 1 << k;
            data[k * n..(k + 1) * n].copy_from_slice(c);
        }
    }
    if let Some(l) = &a.look_lut {
        data.extend(lut_data(l));
    }
    let (w, on) = match &a.wheels {
        Some(w) => (w.clone(), 1),
        None => (filmcraft_render::effects::Wheels { rgb: [[0.0; 3]; 3], light: [0.0; 3] }, 0),
    };
    let [ws, wm, wh] = w.rgb;
    let p = [a.look_k, ws[0], ws[1], ws[2], wm[0], wm[1], wm[2], wh[0], wh[1], wh[2], w.light[0], w.light[1], w.light[2]];
    let mut s = step(OP_LUMETRI_ADVANCED, [bits, a.look, a.look_lut.is_some() as u32, on], &p);
    s.data = data;
    s
}

/// HSL Secondary on a saved copy of the image: the key, Denoise (median), Blur (box passes along
/// rows, then columns, edges repeated) on the key, then the correction (key as `src`, image as `aux`).
fn hsl_steps(h: &filmcraft_render::effects::HslSecondary) -> Vec<Step> {
    let mut g = vec![step(OP_HSL_KEY, [0; 4], &[h.hc, h.hr, h.smin, h.lmin, h.lmax, h.soft])];
    if let Some(r) = h.median_radius() {
        g.push(step(OP_HSL_MEDIAN, [r as u32, 0, 0, 0], &[h.denoise.min(1.0)]));
    }
    let radii: Vec<u32> = h.blur_radii().into_iter().map(|r| r.min(u32::MAX as usize) as u32).collect();
    boxes(&radii, false, true, &mut g);
    boxes(&radii, true, true, &mut g);
    g.push(step(OP_HSL_APPLY, [h.show_mask, 0, 0, 0], &[0.0, 0.0, 0.0, 0.0, h.htemp, h.htint, h.hsat, h.hshift]));
    saving(g)
}

/// The passes of an op (empty: no-op). Unsharp's blur passes come first, then the combine.
fn steps(op: &FxOp) -> Vec<Step> {
    let mut out = Vec::new();
    match op {
        FxOp::BrightnessContrast { br, co } => out.push(step(OP_BRIGHTNESS_CONTRAST, [0; 4], &[*br, *co])),
        FxOp::ProcAmp { br, co, hue, sat } => out.push(step(OP_PROC_AMP, [0; 4], &[*br, *co, *hue, *sat])),
        FxOp::Tint { black, white, amount } => {
            out.push(step(OP_TINT, [0; 4], &[black[0], black[1], black[2], *amount, white[0], white[1], white[2]]));
        }
        FxOp::BlackWhite => out.push(step(OP_BLACK_WHITE, [0; 4], &[])),
        FxOp::ColorBalance { sh, md, hi, preserve } => {
            out.push(step(OP_COLOR_BALANCE, [*preserve as u32, 0, 0, 0], &[sh[0], sh[1], sh[2], 0.0, md[0], md[1], md[2], 0.0, hi[0], hi[1], hi[2]]))
        }
        FxOp::LeaveColor { amount, key_hue, tol, soft } => out.push(step(OP_LEAVE_COLOR, [0; 4], &[*amount, *key_hue, *tol, *soft])),
        FxOp::ChangeToColor { from_hue, to_hue, tol, soft } => out.push(step(OP_CHANGE_TO_COLOR, [0; 4], &[*from_hue, *to_hue, *tol, *soft])),
        FxOp::ColorPass { key, sim, reverse } => out.push(step(OP_COLOR_PASS, [*reverse as u32, 0, 0, 0], &[key[0], key[1], key[2], *sim])),
        FxOp::Gamma { g } => out.push(step(OP_GAMMA, [0; 4], &[*g])),
        FxOp::Levels { ib, iw, ob, ow, g } => out.push(step(OP_LEVELS, [0; 4], &[*ib, *iw, *ob, *ow, *g])),
        FxOp::Extract { lo, hi, soft, invert } => out.push(step(OP_EXTRACT, [*invert as u32, 0, 0, 0], &[*lo, *hi, *soft])),
        FxOp::Invert { channel: 4, blend } => out.push(step(OP_INVERT_ALPHA, [0; 4], &[*blend])),
        FxOp::Invert { channel, blend } => out.push(step(OP_INVERT, [*channel, 0, 0, 0], &[*blend])),
        FxOp::Posterize { n } => out.push(step(OP_POSTERIZE, [0; 4], &[*n])),
        FxOp::Gaussian { rx, ry, repeat } => {
            boxes(rx, false, *repeat, &mut out);
            boxes(ry, true, *repeat, &mut out);
        }
        FxOp::DirectionalBlur { dx, dy, steps } => {
            if *steps >= 2 {
                out.push(step(OP_DIRECTIONAL, [*steps, 0, 0, 0], &[*dx, *dy]));
            }
        }
        FxOp::Unsharp { rx, ry, amount, threshold } => out.extend(unsharp(rx, ry, *amount, *threshold)),
        FxOp::Masked { op, masks } => {
            // the effect on a saved copy of the image, then the mix with it by the masks' coverage
            let mut g = steps(op);
            if !g.is_empty() {
                let mut mix = step(OP_MASK_MIX, [0; 4], &[]);
                mix.data = mask_data(masks);
                g.push(mix);
                out.extend(saving(g));
            }
        }
        FxOp::Crop { x0, x1, y0, y1, feather } => out.push(step(OP_CROP, [0; 4], &[*x0, *x1, *y0, *y1, *feather])),
        FxOp::Resample(r) => {
            let op = if (r.opacity - 1.0).abs() < 1e-6 { 1.0 } else { r.opacity };
            match &r.inv {
                Some(m) => out.push(step(OP_RESAMPLE, r.rect, &[m.a as f32, m.b as f32, m.c as f32, m.d as f32, m.e as f32, m.f as f32, op, 1.0])),
                None => out.push(step(OP_RESAMPLE, [0; 4], &[0.0; 8])),
            }
        }
        FxOp::HFlip => out.push(step(OP_HFLIP, [0; 4], &[])),
        FxOp::VFlip => out.push(step(OP_VFLIP, [0; 4], &[])),
        FxOp::Mirror { cx, cy, nx, ny } => out.push(step(OP_MIRROR, [0; 4], &[*cx, *cy, *nx, *ny])),
        FxOp::Offset { dx, dy, blend } => out.push(step(OP_OFFSET, [0; 4], &[*dx, *dy, *blend])),
        FxOp::AscCdl { slope: s, offset: o, power: p, sat } => {
            out.push(step(OP_ASC_CDL, [0; 4], &[s[0], s[1], s[2], *sat, o[0], o[1], o[2], 0.0, p[0], p[1], p[2]]));
        }
        FxOp::ChannelMix { m } => {
            let p: Vec<f32> = m.iter().flatten().copied().collect();
            out.push(step(OP_CHANNEL_MIX, [0; 4], &p));
        }
        FxOp::ColorReplace { sim, solid, target: t, replace: r, replace_hsl: h } => {
            out.push(step(OP_COLOR_REPLACE, [*solid as u32, 0, 0, 0], &[t[0], t[1], t[2], *sim, r[0], r[1], r[2], 0.0, h[0], h[1], h[2]]))
        }
        FxOp::AlphaAdjust { opacity, ignore, invert, mask_only } => {
            out.push(step(OP_ALPHA_ADJUST, [*ignore as u32, *invert as u32, *mask_only as u32, 0], &[*opacity]));
        }
        FxOp::Vignette { amount, midpoint, roundness, feather, target } => {
            if amount.abs() >= 1e-5 {
                out.push(step(OP_VIGNETTE, [0; 4], &[*amount, *midpoint, *roundness, *feather, target[0], target[1], target[2]]));
            }
        }
        FxOp::VideoLimiter { max, comp, axis, warn, warning_color } => {
            out.push(step(OP_VIDEO_LIMITER, [*axis, *warn as u32, 0, 0], &[*max, *comp, warning_color[0], warning_color[1], warning_color[2]]));
        }
        FxOp::Lumetri {
            gains,
            exposure,
            contrast,
            hl,
            sh,
            wh,
            bl,
            sat,
            creative_on,
            faded,
            vib,
            st,
            ht,
            vignette_on,
            va,
            vmid,
            vround,
            vfeather,
            aspect,
            input_lut,
            advanced,
            hsl,
            sharpen,
            ..
        } => {
            let ge = [gains[0] * exposure, gains[1] * exposure, gains[2] * exposure];
            let b0 = -bl * 0.15;
            let w0 = 1.0 - wh * 0.15;
            let sh_k = sh * 0.35;
            let hl_k = hl * 0.35;
            let vround_aspect = filmcraft_render::gpufx::lumetri_vignette_aspect(*aspect, *vround);
            let st_k = [(st[0] - 0.5) * 0.3, (st[1] - 0.5) * 0.3, (st[2] - 0.5) * 0.3];
            let ht_k = [(ht[0] - 0.5) * 0.3, (ht[1] - 0.5) * 0.3, (ht[2] - 0.5) * 0.3];

            let p = [
                ge[0],
                ge[1],
                ge[2],
                b0,
                w0,
                sh_k,
                hl_k,
                *contrast,
                *faded,
                *sat,
                *vib,
                *va,
                *vmid,
                *vfeather,
                vround_aspect,
                0.0,
                st_k[0],
                st_k[1],
                st_k[2],
                0.0,
                ht_k[0],
                ht_k[1],
                ht_k[2],
                0.0,
            ];
            let mut basic = step(OP_LUMETRI, [*creative_on as u32, *vignette_on as u32, input_lut.is_some() as u32, 0], &p);
            if let Some(l) = input_lut {
                basic.data = lut_data(l);
            }
            out.push(basic);
            if let Some(a) = advanced {
                out.push(advanced_step(a));
            }
            if let Some(h) = hsl {
                out.extend(hsl_steps(h));
            }
            if let Some((rx, ry, amount)) = sharpen {
                out.extend(unsharp(rx, ry, *amount, 0.0));
            }
        }
    }
    out
}

impl FxStage {
    /// Whether `device` can run the stage: compute shaders with 16×16 workgroups and a storage
    /// texture (WebGL2-class devices can't; they render effects on the CPU).
    pub(crate) fn supported(device: &wgpu::Device) -> bool {
        let l = device.limits();
        l.max_storage_textures_per_shader_stage >= 1
            && l.max_storage_buffers_per_shader_stage >= 1
            && l.max_compute_invocations_per_workgroup >= 256
            && l.max_compute_workgroup_size_x >= 64
            && l.max_compute_workgroup_size_y >= 16
            && l.max_compute_workgroups_per_dimension >= 1
    }

    pub(crate) fn new(device: &wgpu::Device, composite: &wgpu::ShaderModule, layer_bgl: &wgpu::BindGroupLayout) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("filmcraft-fx"),
            source: wgpu::ShaderSource::Wgsl(concat!(include_str!("fx.wgsl"), include_str!("mask_common.wgsl")).into()),
        });
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fx"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                tex(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: FX_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                tex(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("fx"), bind_group_layouts: &[Some(&bgl)], immediate_size: 0 });
        let compute = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("fx"),
                layout: Some(&pl),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let spl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fx-source"),
            bind_group_layouts: &[Some(layer_bgl)],
            immediate_size: 0,
        });
        let source = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fx-source"),
            layout: Some(&spl),
            vertex: wgpu::VertexState { module: composite, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: composite,
                entry_point: Some("fs_fx_source"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: FX_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self { px: compute("fx_px"), run: compute("fx_run"), bgl, source, pool: HashMap::new(), used: HashSet::new() }
    }

    /// Start a frame: pooled textures of sizes no layer uses by [`end_frame`](Self::end_frame)
    /// are released then.
    pub(crate) fn begin_frame(&mut self) {
        self.used.clear();
    }

    pub(crate) fn end_frame(&mut self) {
        let used = &self.used;
        self.pool.retain(|k, _| used.contains(k));
    }

    /// No working textures held.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.pool.is_empty()
    }

    fn targets(&mut self, device: &wgpu::Device, size: (u32, u32), n: usize) -> Vec<Target> {
        self.used.insert(size);
        let v = self.pool.entry(size).or_default();
        while v.len() < n {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("filmcraft-fx"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FX_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = t.create_view(&Default::default());
            v.push((t, view));
        }
        v.iter().take(n).cloned().collect()
    }

    /// Prepare the passes of `fx`, whose source draw uses `source_bg` (a layer bind group whose
    /// uniforms map the frame onto the working image). None when the working image does not fit
    /// in a texture (the layer is then drawn without its effects — never larger than the source
    /// frame, which would not have uploaded either).
    pub(crate) fn job(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fx: &LayerFx,
        source_bg: wgpu::BindGroup,
        dummy: &wgpu::TextureView,
    ) -> Option<FxJob> {
        let (w, h) = (fx.size.0.max(1), fx.size.1.max(1));
        let max = device.limits().max_texture_dimension_2d;
        if w > max || h > max {
            return None;
        }
        let all: Vec<Step> = fx.ops.iter().flat_map(steps).collect();
        // working textures: the one being read, the one being written, and the saved images
        let mut depth = 0usize;
        let mut deepest = 0usize;
        for s in &all {
            depth += s.saves as usize;
            deepest = deepest.max(depth);
            depth = depth.saturating_sub(usize::from(s.restore));
        }
        let n = deepest + 2;
        let targets = self.targets(device, (w, h), n);
        let views: Vec<wgpu::TextureView> = targets.iter().map(|t| t.1.clone()).collect();
        // passes without tables share one (the binding needs a buffer)
        let empty = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-data-empty"),
            size: 16,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let mut dispatches = Vec::with_capacity(all.len());
        let mut cur = 0usize;
        let mut saved: Vec<usize> = Vec::new();
        for s in &all {
            for _ in 0..s.saves {
                saved.push(cur);
            }
            let dst = (0..n).find(|k| *k != cur && !saved.contains(k))?;
            let aux = if s.restore { views.get(*saved.last()?)? } else { dummy };
            let data = if s.data.is_empty() {
                empty.clone()
            } else {
                let size = (s.data.len() * 4) as u64;
                if size > device.limits().max_storage_buffer_binding_size {
                    return None;
                }
                let d = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("fx-data"),
                    size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let raw: Vec<u8> = s.data.iter().flat_map(|v| v.to_le_bytes()).collect();
                queue.write_buffer(&d, 0, &raw);
                d
            };
            // per-pixel passes over bands of rows (`BAND_PIXELS`); a running-sum pass is one
            // invocation per line already
            let band = if s.run { h } else { (BAND_PIXELS / w).max(16).next_multiple_of(16).min(h) };
            let mut y0 = 0;
            while y0 < h {
                let rows = band.min(h - y0);
                let mut bytes = Vec::with_capacity(128);
                for v in [s.code, w, h, y0].iter().chain(&s.i1) {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                for v in &s.p {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("fx-u"),
                    size: bytes.len() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                queue.write_buffer(&buf, 0, &bytes);
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("fx"),
                    layout: &self.bgl,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(views.get(cur)?) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(views.get(dst)?) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(aux) },
                        wgpu::BindGroupEntry { binding: 4, resource: data.as_entire_binding() },
                    ],
                });
                let groups = if s.run {
                    let lines = if s.i1[2] != 0 { w } else { h };
                    (lines.div_ceil(64), 1)
                } else {
                    (w.div_ceil(16), rows.div_ceil(16))
                };
                dispatches.push(Dispatch { run: s.run, bg, groups });
                y0 += rows;
            }
            cur = dst;
            if s.restore {
                saved.pop();
            }
        }
        Some(FxJob {
            source_bg,
            source_target: views.first()?.clone(),
            dispatches,
            result: views.get(cur)?.clone(),
            result_texture: targets.get(cur)?.0.clone(),
        })
    }

    /// Record a job (source draw, then its compute passes) into `enc`.
    pub(crate) fn record(&self, enc: &mut wgpu::CommandEncoder, job: &FxJob) {
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fx-source"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &job.source_target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.source);
            pass.set_bind_group(0, &job.source_bg, &[]);
            pass.draw(0..6, 0..1);
        }
        if job.dispatches.is_empty() {
            return;
        }
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("fx"), timestamp_writes: None });
        for d in &job.dispatches {
            pass.set_pipeline(if d.run { &self.run } else { &self.px });
            pass.set_bind_group(0, &d.bg, &[]);
            pass.dispatch_workgroups(d.groups.0, d.groups.1, 1);
        }
    }
}
