// Mask coverage shared by the mask shader (`GpuMask`) and the effect stage (`fx.wgsl`, masked
// effects): the same math as filmcraft_render::mask. Each shader defines `mask_pt(i)`, the i-th
// polygon point of its point list.

struct MaskInfo {
    start: u32,
    len: u32,
    mode: u32,
    inverted: u32,
    feather: f32,
    expansion: f32,
    opacity: f32,
    _pad: f32,
};

fn falloff(s: f32, feather: f32) -> f32 {
    let w = max(feather, 1.0);
    let u = clamp(s / w + 0.5, 0.0, 1.0);
    let smooth_u = u * u * (3.0 - 2.0 * u);
    return u + (smooth_u - u) * min(feather, 1.0);
}

// MaskMode index: None 0, Add 1, Subtract 2, Intersect 3, Lighten 4, Darken 5, Difference 6.
fn mode_start(mode: u32) -> f32 {
    if (mode == 2u || mode == 3u || mode == 5u) {
        return 1.0;
    }
    return 0.0;
}

fn combine(mode: u32, a: f32, m: f32) -> f32 {
    switch mode {
        case 1u: { return a + m - a * m; }
        case 2u: { return a * (1.0 - m); }
        case 3u: { return a * m; }
        case 4u: { return max(a, m); }
        case 5u: { return min(a, m); }
        case 6u: { return a + m - 2.0 * a * m; }
        default: { return a; }
    }
}

fn seg_dist2(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let d = b - a;
    let q = p - a;
    let l2 = dot(d, d);
    var t = 0.0;
    if (l2 > 0.0) {
        t = clamp(dot(q, d) / l2, 0.0, 1.0);
    }
    let e = q - d * t;
    return dot(e, e);
}

// Distance beyond which a mask's coverage is saturated (`mask::band`).
fn mask_band(m: MaskInfo) -> f32 {
    return abs(m.expansion) + max(m.feather, 1.0) * 0.5 + 1.0;
}

// Coverage of a mask at signed distance `sd` (inside > 0).
fn mask_level(m: MaskInfo, sd: f32) -> f32 {
    let c = falloff(sd + m.expansion, m.feather);
    if (m.inverted != 0u) {
        return m.opacity * (1.0 - c);
    }
    return m.opacity * c;
}

fn mask_value(m: MaskInfo, p: vec2<f32>) -> f32 {
    let band = mask_band(m);
    var sd = -band;
    if (m.len >= 3u) {
        var wind = 0;
        var d2 = band * band;
        for (var i = 0u; i < m.len; i = i + 1u) {
            let a = mask_pt(m.start + i);
            let c = mask_pt(m.start + (i + 1u) % m.len);
            if ((a.y <= p.y) != (c.y <= p.y)) {
                let t = (p.y - a.y) / (c.y - a.y);
                if (a.x + (c.x - a.x) * t < p.x) {
                    if (c.y > a.y) { wind = wind + 1; } else { wind = wind - 1; }
                }
            }
            d2 = min(d2, seg_dist2(p, a, c));
        }
        let d = sqrt(d2);
        if (wind != 0) { sd = d; } else { sd = -d; }
    }
    return mask_level(m, sd);
}
