// Mask coverage + masked-effect mix (same math as filmcraft_render::mask; see its docs). The
// coverage functions are in mask_common.wgsl, shared with the effect stage.
// Masks arrive flattened to polygons in working-image pixels, active ones only (mode != None).

struct Params {
    width: u32,
    height: u32,
    count: u32,
    do_mix: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> masks: array<MaskInfo>;
@group(0) @binding(2) var<storage, read> pts: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> original: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read> effected: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read_write> out_px: array<vec4<f32>>;
@group(0) @binding(6) var<storage, read_write> out_cov: array<f32>;

fn mask_pt(i: u32) -> vec2<f32> {
    return pts[i];
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= params.width || id.y >= params.height) {
        return;
    }
    let p = vec2<f32>(f32(id.x) + 0.5, f32(id.y) + 0.5);
    var acc = mode_start(masks[0].mode);
    for (var i = 0u; i < params.count; i = i + 1u) {
        let m = masks[i];
        acc = clamp(combine(m.mode, acc, mask_value(m, p)), 0.0, 1.0);
    }
    let k = id.y * params.width + id.x;
    out_cov[k] = acc;
    if (params.do_mix == 1u) {
        let o = original[k];
        out_px[k] = o + (effected[k] - o) * acc;
    }
}
