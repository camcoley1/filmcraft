//! Caption layout and burn-in.
//!
//! [`overlay`] lays out the caption showing on a track at a time with the track's
//! [`CaptionStyle`] (size relative to a 1080-line frame, colour, background box, outline,
//! alignment, anchor and margin; WebVTT `line:N%` and `align:` cue settings override the style)
//! and rasterises it into a small premultiplied linear-light RGBA [`Overlay`] positioned in the
//! frame. Renderers composite overlays over the finished picture.
//!
//! A style [`CaptionAnimation`] animates captions that carry word timings ([`Caption::words`])
//! at the timeline time being drawn: karaoke colours the words already spoken, word pop enlarges
//! the word being spoken, boxed draws a box behind it. Lines are laid out exactly as for a static
//! caption and their glyphs are split per word, so word positions never move between frames.
//! Captions without matching word timings, or with no animation, draw statically. The Program
//! monitor and export both draw through [`sequence_overlays`], so they always agree.

use filmcraft_color::srgb_to_linear;
use filmcraft_project::{Caption, CaptionAlign, CaptionAnchor, CaptionAnimation, CaptionStyle, CaptionTrack, Sequence};
use filmcraft_time::Tick;

use filmcraft_text::{ParagraphStyle, TextStyle, layout, render};

/// A rendered caption: premultiplied linear-light RGBA f32 pixels at `(x, y)` in the frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    pub x: i32,
    pub y: i32,
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Overlay {
    /// Composite over a premultiplied RGBA f32 canvas of `cw`×`ch` pixels.
    pub fn composite_onto(&self, canvas: &mut [f32], cw: usize, ch: usize) {
        for y in 0..self.h {
            let cy = self.y + y as i32;
            if cy < 0 || cy >= ch as i32 {
                continue;
            }
            for x in 0..self.w {
                let cx = self.x + x as i32;
                if cx < 0 || cx >= cw as i32 {
                    continue;
                }
                let s = &self.px[(y * self.w + x) * 4..(y * self.w + x) * 4 + 4];
                if s[3] <= 0.0 {
                    continue;
                }
                let i = (cy as usize * cw + cx as usize) * 4;
                let k = 1.0 - s[3];
                for c in 0..4 {
                    canvas[i + c] = s[c] + canvas[i + c] * k;
                }
            }
        }
    }
}

/// The caption face: the track's font family (Inter when empty or not installed) in SemiBold at `px`.
fn caption_style(font: &str, px: f32) -> TextStyle {
    let family = if font.trim().is_empty() { filmcraft_text::fonts::DEFAULT_FAMILY } else { font.trim() };
    TextStyle { family: family.into(), style: "SemiBold".into(), size: px, ..Default::default() }
}

fn lin(c: [u8; 4]) -> [f32; 4] {
    let a = c[3] as f32 / 255.0;
    [srgb_to_linear(c[0] as f32 / 255.0) * a, srgb_to_linear(c[1] as f32 / 255.0) * a, srgb_to_linear(c[2] as f32 / 255.0) * a, a]
}

/// Word-wrap one line to `max_w` pixels.
fn wrap(line: &str, ts: &TextStyle, max_w: f32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in line.split(' ') {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if !cur.is_empty() && filmcraft_text::measure(&cand, ts) > max_w {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            cur = cand;
        }
    }
    out.push(cur);
    out
}

/// Placement overrides from WebVTT cue settings.
fn vtt_overrides(settings: &str) -> (Option<f32>, Option<CaptionAlign>) {
    let mut line = None;
    let mut align = None;
    for kv in settings.split_whitespace() {
        let Some((k, v)) = kv.split_once(':') else { continue };
        match k {
            "line" => {
                let v = v.split(',').next().unwrap_or("");
                if let Some(p) = v.strip_suffix('%').and_then(|p| p.parse::<f32>().ok()) {
                    line = Some((p / 100.0).clamp(0.0, 1.0));
                }
            }
            "align" => {
                align = match v {
                    "start" | "left" => Some(CaptionAlign::Left),
                    "end" | "right" => Some(CaptionAlign::Right),
                    "center" | "middle" => Some(CaptionAlign::Center),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    (line, align)
}

/// Byte ranges of the whitespace-separated words of `s` (the same split as
/// [`filmcraft_project::caption_words`]).
fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, ch) in s.char_indices() {
        match (ch.is_whitespace(), start) {
            (true, Some(a)) => {
                out.push((a, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(a) = start {
        out.push((a, s.len()));
    }
    out
}

/// Pop scale and box pad (pixels at 1080 lines) of an animation, made safe for drawing.
fn anim_params(a: CaptionAnimation) -> (f32, f32) {
    match a {
        CaptionAnimation::WordPop { scale } if scale.is_finite() => (scale.clamp(0.5, 3.0), 0.0),
        CaptionAnimation::WordPop { .. } => (CaptionAnimation::DEFAULT_POP_SCALE, 0.0),
        CaptionAnimation::Boxed { pad, .. } if pad.is_finite() => (1.0, pad.clamp(0.0, 200.0)),
        CaptionAnimation::Boxed { .. } => (1.0, CaptionAnimation::DEFAULT_BOX_PAD),
        _ => (1.0, 0.0),
    }
}

/// Add the coverage of the rectangle `[x0, x1) x [y0, y1)` (anti-aliased edges) to `m`, an
/// `ow`x`oh` mask at `(ox, oy)` in the frame.
#[allow(clippy::too_many_arguments)]
fn fill_rect(m: &mut [f32], ow: usize, oh: usize, ox: i32, oy: i32, x0: f32, x1: f32, y0: f32, y1: f32) {
    for y in (y0.floor() as i32).max(oy)..(y1.ceil() as i32).min(oy + oh as i32) {
        let fy = ((y as f32 + 1.0).min(y1) - (y as f32).max(y0)).clamp(0.0, 1.0);
        for x in (x0.floor() as i32).max(ox)..(x1.ceil() as i32).min(ox + ow as i32) {
            let fx = ((x as f32 + 1.0).min(x1) - (x as f32).max(x0)).clamp(0.0, 1.0);
            let j = (y - oy) as usize * ow + (x - ox) as usize;
            if let Some(v) = m.get_mut(j) {
                *v = (*v + fx * fy).min(1.0);
            }
        }
    }
}

/// Lay out and rasterise one caption for a `w`×`h` frame, statically (no word animation).
pub fn render_caption(c: &Caption, style: &CaptionStyle, w: usize, h: usize) -> Option<Overlay> {
    render(c, style, None, w, h)
}

/// [`render_caption`] at timeline time `t`: the style's word animation is applied when the
/// caption has word timings that spell its text.
pub fn render_caption_at(c: &Caption, style: &CaptionStyle, t: Tick, w: usize, h: usize) -> Option<Overlay> {
    render(c, style, Some(t), w, h)
}

fn render(c: &Caption, style: &CaptionStyle, t: Option<Tick>, w: usize, h: usize) -> Option<Overlay> {
    // word animation: only with word timings that spell the text
    let anim = t.filter(|_| !style.animation.is_none() && c.words_match_text());
    draw(c, style, anim, w, h)
}

/// Draw `c`, animated at `anim` when given (the caller has checked the words).
fn draw(c: &Caption, style: &CaptionStyle, anim: Option<Tick>, w: usize, h: usize) -> Option<Overlay> {
    if w == 0 || h == 0 {
        return None;
    }
    let scale = h as f32 / 1080.0;
    let px = (style.size * scale).max(4.0);
    let max_w = w as f32 * 0.9;
    let ts = caption_style(&style.font, px);
    let mut lines: Vec<String> = Vec::new();
    // (speaker names are metadata; like Premiere, they are not burned in)
    for l in c.plain_lines() {
        lines.extend(wrap(l.trim(), &ts, max_w));
    }
    lines.retain(|l| !l.is_empty());
    if lines.is_empty() {
        return None;
    }
    let vm = filmcraft_text::fonts::face(filmcraft_text::resolve(&ts.family, &ts.style).face).metrics(px);
    let (asc, desc) = (vm.ascent, vm.descent);
    let lh = px * style.line_spacing.max(0.8);
    let pad_x = (px * 0.3).round();
    let block_h = lh * lines.len() as f32;
    let (line_override, align_override) = vtt_overrides(&c.settings);
    let align = align_override.unwrap_or(style.align);
    let margin = style.margin.clamp(0.0, 0.45) * h as f32;
    let top = match (line_override, style.anchor) {
        (Some(f), _) => (f * h as f32).min(h as f32 - block_h),
        (None, CaptionAnchor::Top) => margin,
        (None, CaptionAnchor::Middle) => (h as f32 - block_h) / 2.0,
        (None, CaptionAnchor::Bottom) => h as f32 - margin - block_h,
    }
    .max(0.0)
    .round();
    let side = w as f32 * 0.05;
    let widths: Vec<f32> = lines.iter().map(|l| filmcraft_text::measure(l, &ts)).collect();
    let xs: Vec<f32> = widths
        .iter()
        .map(|&lw| match align {
            CaptionAlign::Left => side + pad_x,
            CaptionAlign::Right => w as f32 - side - pad_x - lw,
            CaptionAlign::Center => (w as f32 - lw) / 2.0,
        })
        .collect();
    let outline = (style.outline * scale).max(0.0);
    let (pop, box_pad) = if anim.is_some() { anim_params(style.animation) } else { (1.0, 0.0) };
    let box_pad = (box_pad * scale).round();
    // room for a popped or boxed word, the same for the whole caption so the overlay never jumps:
    // a popped word grows by a share of its own width (never wider than its line) and of the
    // font's height, a boxed word by the pad
    let grow_pop = (pop - 1.0).max(0.0);
    let widest = widths.iter().copied().fold(0.0f32, f32::max);
    let extra_x = widest * grow_pop / 2.0 + box_pad;
    let extra_y = (asc + desc) * grow_pop + box_pad;
    let grow_x = outline.ceil() + 1.0 + extra_x.ceil();
    let grow_y = outline.ceil() + 1.0 + extra_y.ceil();
    // the overlay covers the caption, clipped to the frame plus that room (nothing outside it can
    // show), with a hard cap on its size
    let (fw, fh) = (w as f32, h as f32);
    let bx0 = xs.iter().map(|x| x - pad_x - grow_x).fold(f32::MAX, f32::min).max(-grow_x).floor();
    let bx1 = xs.iter().zip(&widths).map(|(x, lw)| x + lw + pad_x + grow_x).fold(f32::MIN, f32::max).min(fw + grow_x).ceil();
    let by0 = (top - grow_y).max(-grow_y).floor();
    let by1 = (top + block_h + grow_y).min(fh + grow_y).ceil();
    if !(bx1 > bx0 && by1 > by0) {
        return None;
    }
    let (ox, oy) = (bx0 as i32, by0 as i32);
    let (ow, oh) = ((bx1 - bx0) as usize, (by1 - by0) as usize);
    let cap = w.saturating_mul(h).saturating_mul(4);
    let fits = ow.checked_mul(oh).filter(|&n| n <= cap).and_then(|n| n.checked_mul(4)).is_some();
    if !fits {
        // an animated caption retries statically (less room); a static one that does not fit is
        // not drawn
        return if anim.is_some() { draw(c, style, None, w, h) } else { None };
    }
    let mut cover = vec![0.0f32; ow * oh];
    let mut bg = vec![0.0f32; ow * oh];
    // karaoke: words already spoken; boxed: the box behind the word being spoken
    let karaoke = matches!((anim, style.animation), (Some(_), CaptionAnimation::KaraokeHighlight { .. }));
    let boxed = matches!((anim, style.animation), (Some(_), CaptionAnimation::Boxed { .. }));
    let mut lit = if karaoke { vec![0.0f32; ow * oh] } else { Vec::new() };
    let mut boxm = if boxed { vec![0.0f32; ow * oh] } else { Vec::new() };
    let mut word = 0usize;
    let draw_into = |mask: &mut Vec<f32>, l: &filmcraft_text::Layout, m: &filmcraft_text::Xform| {
        let mut mk = filmcraft_text::Mask { w: ow, h: oh, a: std::mem::take(mask) };
        render::draw(l, m, &mut mk, (ox, oy));
        *mask = mk.a;
    };
    let (vis0, vis1) = (oy as f32 - extra_y - lh, (oy as f32) + oh as f32 + extra_y);
    for (i, line) in lines.iter().enumerate() {
        let lt = top + lh * i as f32;
        let (Some(&lx), Some(&lw)) = (xs.get(i), widths.get(i)) else { break };
        if lt < vis0 || lt > vis1 {
            // off the overlay: nothing to draw, but its words still count
            if anim.is_some() {
                word = word.saturating_add(word_spans(line).len());
            }
            continue;
        }
        if style.background && style.background_color[3] > 0 {
            fill_rect(&mut bg, ow, oh, ox, oy, lx - pad_x, lx + lw + pad_x, lt, lt + lh);
        }
        let baseline = lt + (lh - (asc + desc)) / 2.0 + asc;
        let l = layout(line, &ts, &ParagraphStyle::default());
        let at = render::at(lx, baseline.round());
        let Some(now) = anim else {
            draw_into(&mut cover, &l, &at);
            continue;
        };
        // one pass: glyphs in cluster order walked against the words (byte ranges, ascending);
        // glyph positions sorted once to find each word's right edge
        let mut by_cluster: Vec<&filmcraft_text::Glyph> = l.glyphs.iter().collect();
        by_cluster.sort_by_key(|g| g.cluster);
        let mut gx: Vec<f32> = l.glyphs.iter().map(|g| g.x).collect();
        gx.sort_by(f32::total_cmp);
        let mut next = 0usize;
        for (a, b) in word_spans(line) {
            while by_cluster.get(next).is_some_and(|g| g.cluster < a) {
                next += 1;
            }
            let from = next;
            while by_cluster.get(next).is_some_and(|g| g.cluster < b) {
                next += 1;
            }
            let glyphs: Vec<filmcraft_text::Glyph> = by_cluster.get(from..next).unwrap_or(&[]).iter().map(|g| **g).collect();
            let Some(g0) = glyphs.first().copied() else { continue };
            // the word spans from its leftmost glyph to the next glyph to its right (the space
            // after it) or the line end; glyph order is visual order only for left-to-right text
            let left = glyphs.iter().map(|g| g.x).fold(g0.x, f32::min);
            let last = glyphs.iter().map(|g| g.x).fold(g0.x, f32::max);
            let right = gx.get(gx.partition_point(|&x| x <= last)).copied().unwrap_or(lw).min(lw).max(last);
            let tw = (right - left).max(0.0);
            let part = filmcraft_text::Layout { glyphs, vertical: l.vertical, ..Default::default() };
            let Some(wt) = c.words.get(word) else {
                // no timing for this word: draw it as a static caption would
                draw_into(&mut cover, &part, &at);
                continue;
            };
            word += 1;
            let spoken = wt.start <= now;
            let active = spoken && now < wt.end();
            match style.animation {
                CaptionAnimation::KaraokeHighlight { .. } if spoken => draw_into(&mut lit, &part, &at),
                CaptionAnimation::WordPop { .. } if active => {
                    // scale about the word's centre on its baseline
                    let cx = left + tw / 2.0;
                    let m = [pop, 0.0, 0.0, pop, lx + cx * (1.0 - pop), baseline.round() + g0.y * (1.0 - pop)];
                    draw_into(&mut cover, &part, &m);
                }
                CaptionAnimation::Boxed { .. } if active => {
                    let x0 = lx + left;
                    fill_rect(&mut boxm, ow, oh, ox, oy, x0 - box_pad, x0 + tw + box_pad, lt, lt + lh);
                    draw_into(&mut cover, &part, &at);
                }
                _ => draw_into(&mut cover, &part, &at),
            }
        }
    }
    let stroke = if outline <= 0.0 {
        Vec::new()
    } else if lit.is_empty() {
        dilate(&cover, ow, oh, outline)
    } else {
        let all: Vec<f32> = cover.iter().zip(&lit).map(|(a, b)| (a + b).min(1.0)).collect();
        dilate(&all, ow, oh, outline)
    };
    let (tc, bc, oc) = (lin(style.color), lin(style.background_color), lin(style.outline_color));
    let (hc, xc) = match style.animation {
        CaptionAnimation::KaraokeHighlight { highlight_color } => (lin(highlight_color), [0.0; 4]),
        CaptionAnimation::Boxed { box_color, .. } => ([0.0; 4], lin(box_color)),
        _ => ([0.0; 4], [0.0; 4]),
    };
    let mut out = vec![0.0f32; ow * oh * 4];
    for j in 0..ow * oh {
        let mut p = [0.0f32; 4];
        let over = |p: &mut [f32; 4], c: [f32; 4], a: f32| {
            if a > 0.0 {
                for k in 0..4 {
                    p[k] = c[k] * a + p[k] * (1.0 - c[3] * a);
                }
            }
        };
        over(&mut p, bc, bg[j]);
        if let Some(&a) = boxm.get(j) {
            over(&mut p, xc, a);
        }
        if !stroke.is_empty() {
            over(&mut p, oc, stroke[j]);
        }
        over(&mut p, tc, cover[j]);
        if let Some(&a) = lit.get(j) {
            over(&mut p, hc, a);
        }
        out[j * 4..j * 4 + 4].copy_from_slice(&p);
    }
    Some(Overlay { x: ox, y: oy, w: ow, h: oh, px: out })
}

/// Max-filter a coverage mask with a disc of radius `r`.
fn dilate(m: &[f32], w: usize, h: usize, r: f32) -> Vec<f32> {
    let ri = r.ceil() as i32;
    let mut offs = Vec::new();
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d <= r + 0.5 {
                offs.push((dx, dy, (r + 0.5 - d).clamp(0.0, 1.0)));
            }
        }
    }
    let mut out = vec![0.0f32; w * h];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut v = 0.0f32;
            for &(dx, dy, wgt) in &offs {
                let (sx, sy) = (x + dx, y + dy);
                if sx >= 0 && sy >= 0 && sx < w as i32 && sy < h as i32 {
                    v = v.max(m[sy as usize * w + sx as usize] * wgt);
                }
            }
            out[y as usize * w + x as usize] = v;
        }
    }
    out
}

/// The overlay for a caption track at timeline time `t` (None when nothing shows).
pub fn track_overlay(track: &CaptionTrack, t: Tick, w: usize, h: usize) -> Option<Overlay> {
    if !track.enabled {
        return None;
    }
    let c = track.caption_at(t)?;
    render_caption_at(c, &track.style, t, w, h)
}

/// Overlays of every visible caption track of a sequence at `t`, bottom track first.
pub fn sequence_overlays(seq: &Sequence, t: Tick, w: usize, h: usize) -> Vec<Overlay> {
    seq.caption_tracks.iter().rev().filter_map(|tr| track_overlay(tr, t, w, h)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::{CaptionFormat, ClipId, TrackId};

    fn cap(text: &str) -> Caption {
        Caption {
            id: ClipId(1),
            start: Tick(0),
            duration: Tick(100),
            text: text.into(),
            speaker: None,
            cue_id: None,
            settings: String::new(),
            words: Vec::new(),
        }
    }

    #[test]
    fn bottom_centre_with_box() {
        let st = CaptionStyle::default();
        let o = render_caption(&cap("Hello world"), &st, 1920, 1080).unwrap();
        // centred horizontally, in the lower part of the frame
        let cx = o.x as f32 + o.w as f32 / 2.0;
        assert!((cx - 960.0).abs() < 4.0, "centre {cx}");
        assert!(o.y > 800 && (o.y as usize + o.h) < 1080, "y {} h {}", o.y, o.h);
        // white text pixels exist and the box is dark translucent
        let mut white = 0;
        let mut boxy = 0;
        for p in o.px.chunks(4) {
            if p[3] > 0.99 && p[0] > 0.9 {
                white += 1;
            } else if p[3] > 0.7 && p[0] < 0.01 {
                boxy += 1;
            }
        }
        assert!(white > 500, "{white}");
        assert!(boxy > 1000, "{boxy}");
    }

    #[test]
    fn top_anchor_and_vtt_line() {
        let st = CaptionStyle { anchor: CaptionAnchor::Top, ..Default::default() };
        let o = render_caption(&cap("Top"), &st, 1280, 720).unwrap();
        assert!(o.y < 100);
        let mut c = cap("Line");
        c.settings = "line:50% align:start".into();
        let o = render_caption(&c, &CaptionStyle::default(), 1280, 720).unwrap();
        assert!((o.y - 360).abs() < 4, "{}", o.y);
        assert!(o.x < 120, "{}", o.x);
    }

    #[test]
    fn margin_is_a_fraction_of_frame_height() {
        // #1542: on a portrait sequence, a bottom-anchored caption with margin 0.31 sits with
        // its bottom edge 0.31 of the frame height above the bottom; the top anchor mirrors it.
        let st = CaptionStyle { anchor: CaptionAnchor::Bottom, margin: 0.31, size: 78.0, ..Default::default() };
        let o = render_caption(&cap("Hello world"), &st, 1080, 1920).unwrap();
        // the overlay pads the content by the outline grow (the default outline is 0, so by 1 px)
        let bottom = (o.y as usize + o.h) as f32;
        let expected = 1920.0 - 0.31 * 1920.0;
        assert!((bottom - expected).abs() <= 2.0, "bottom edge {bottom}, expected {expected}");
        let st = CaptionStyle { anchor: CaptionAnchor::Top, margin: 0.31, size: 78.0, ..Default::default() };
        let o = render_caption(&cap("Hello world"), &st, 1080, 1920).unwrap();
        assert!((o.y as f32 - 0.31 * 1920.0).abs() <= 2.0, "top edge {}, expected {}", o.y, 0.31 * 1920.0);
        // margin 0 and the 0.45 clamp stay inside the frame
        for margin in [0.0, 0.31, 0.45, 9.0] {
            let st = CaptionStyle { anchor: CaptionAnchor::Bottom, margin, size: 78.0, ..Default::default() };
            let o = render_caption(&cap("Hello world"), &st, 1080, 1920).unwrap();
            // the outline-grow padding plus ceil rounding may reach 2 px past the edge; the
            // composite clips it
            assert!(o.y >= 0 && o.y as usize + o.h <= 1922, "margin {margin}: y {} h {}", o.y, o.h);
        }
    }

    #[test]
    fn wraps_and_composites() {
        let long = "word ".repeat(60);
        let o = render_caption(&cap(&long), &CaptionStyle::default(), 640, 360).unwrap();
        assert!(o.w <= 640 && o.h > 40, "{}x{}", o.w, o.h);
        let mut canvas = vec![0.0f32; 640 * 360 * 4];
        o.composite_onto(&mut canvas, 640, 360);
        assert!(canvas.chunks(4).any(|p| p[0] > 0.9));
    }

    #[test]
    fn outline_draws_around_text() {
        let st = CaptionStyle { background: false, outline: 4.0, outline_color: [255, 0, 0, 255], ..Default::default() };
        let o = render_caption(&cap("O"), &st, 1920, 1080).unwrap();
        assert!(o.px.chunks(4).any(|p| p[0] > 0.9 && p[1] < 0.05 && p[3] > 0.9), "red outline pixels");
    }

    #[test]
    fn track_font_is_used() {
        // the style's font used to be ignored (always Inter)
        let inter = render_caption(&cap("Hello world"), &CaptionStyle::default(), 1920, 1080).unwrap();
        let serif = CaptionStyle { font: "Noto Serif".into(), ..Default::default() };
        let serif = render_caption(&cap("Hello world"), &serif, 1920, 1080).unwrap();
        assert_ne!((inter.w, &inter.px), (serif.w, &serif.px), "Noto Serif renders like Inter");
        let mono = CaptionStyle { font: "jetbrains mono".into(), ..Default::default() };
        let mono = render_caption(&cap("Hello world"), &mono, 1920, 1080).unwrap();
        assert_ne!((inter.w, &inter.px), (mono.w, &mono.px), "JetBrains Mono renders like Inter");
    }

    #[test]
    fn unknown_or_empty_font_falls_back_to_inter() {
        let inter = render_caption(&cap("Hello"), &CaptionStyle::default(), 1280, 720).unwrap();
        for font in ["", "  ", "No Such Font 123"] {
            let st = CaptionStyle { font: font.into(), ..Default::default() };
            assert_eq!(render_caption(&cap("Hello"), &st, 1280, 720), Some(inter.clone()), "font {font:?}");
        }
    }

    use filmcraft_project::WordTiming;

    /// "aaaa bbbb" with "aaaa" spoken over 0..10 and "bbbb" over 50..60.
    fn timed() -> Caption {
        let mut c = cap("aaaa bbbb");
        c.words = vec![WordTiming::new(Tick(0), Tick(10), "aaaa"), WordTiming::new(Tick(50), Tick(10), "bbbb")];
        c
    }

    fn style(animation: CaptionAnimation) -> CaptionStyle {
        CaptionStyle { background: false, animation, ..Default::default() }
    }

    const YELLOW: [u8; 4] = [255, 255, 0, 255];

    /// Mean x (in frame pixels) of the overlay pixels `pick` accepts, and how many there are.
    fn centroid(o: &Overlay, pick: impl Fn(&[f32]) -> bool) -> (f32, usize) {
        let (mut sx, mut n) = (0.0f32, 0usize);
        for (j, p) in o.px.chunks(4).enumerate() {
            if pick(p) {
                sx += (o.x + (j % o.w) as i32) as f32;
                n += 1;
            }
        }
        (if n > 0 { sx / n as f32 } else { f32::NAN }, n)
    }

    fn yellow(p: &[f32]) -> bool {
        p[3] > 0.9 && p[0] > 0.9 && p[1] > 0.9 && p[2] < 0.1
    }

    fn white(p: &[f32]) -> bool {
        p[3] > 0.9 && p[0] > 0.9 && p[1] > 0.9 && p[2] > 0.9
    }

    #[test]
    fn karaoke_colours_the_words_already_spoken() {
        let st = style(CaptionAnimation::KaraokeHighlight { highlight_color: YELLOW });
        // before the first word: nothing highlighted
        let before = render_caption_at(&timed(), &st, Tick(-1), 1920, 1080).unwrap();
        assert_eq!(centroid(&before, yellow).1, 0);
        // during the first word: the left word is yellow, the right word white
        let first = render_caption_at(&timed(), &st, Tick(5), 1920, 1080).unwrap();
        let (yx, yn) = centroid(&first, yellow);
        let (wx, wn) = centroid(&first, white);
        assert!(yn > 200 && wn > 200, "yellow {yn} white {wn}");
        assert!(yx < 960.0 && wx > 960.0, "yellow at {yx}, white at {wx}");
        // in the pause between words the first stays lit
        let pause = render_caption_at(&timed(), &st, Tick(30), 1920, 1080).unwrap();
        assert_eq!(pause, first);
        // after the second word starts both are lit
        let both = render_caption_at(&timed(), &st, Tick(55), 1920, 1080).unwrap();
        assert_eq!(centroid(&both, white).1, 0);
        assert!(centroid(&both, yellow).1 > yn + 200);
        // the overlay never moves or resizes, and the static render has no highlight
        for o in [&before, &both] {
            assert_eq!((o.x, o.y, o.w, o.h), (first.x, first.y, first.w, first.h));
        }
        let stat = render_caption(&timed(), &st, 1920, 1080).unwrap();
        assert_eq!(centroid(&stat, yellow).1, 0);
    }

    #[test]
    fn per_word_glyphs_sit_exactly_where_the_static_line_puts_them() {
        // karaoke in the text colour must draw the same pixels as a static caption
        for t in [-1, 5, 30, 55] {
            let st =
                CaptionStyle { outline: 3.0, animation: CaptionAnimation::KaraokeHighlight { highlight_color: [255, 255, 255, 255] }, ..Default::default() };
            let animated = render_caption_at(&timed(), &st, Tick(t), 1280, 720).unwrap();
            let stat = render_caption(&timed(), &CaptionStyle { outline: 3.0, ..Default::default() }, 1280, 720).unwrap();
            assert_eq!((animated.x, animated.y, animated.w, animated.h), (stat.x, stat.y, stat.w, stat.h));
            let diff = animated.px.iter().zip(&stat.px).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(diff < 1e-5, "t {t}: max difference {diff}");
        }
    }

    #[test]
    fn word_pop_enlarges_only_the_word_being_spoken() {
        let st = style(CaptionAnimation::WordPop { scale: 1.5 });
        // the caption is centred in the 1920 wide frame
        let c = 960.0f32;
        // ink pixels, and the pixels of the region well clear of one word
        let ink = |o: &Overlay| o.px.chunks(4).filter(|p| p[3] > 0.5).count();
        let region = |o: &Overlay, left: bool| -> Vec<f32> {
            o.px.chunks(4)
                .enumerate()
                .filter(|(j, _)| {
                    let x = (o.x + (j % o.w) as i32) as f32;
                    if left { x < c - 70.0 } else { x > c + 70.0 }
                })
                .flat_map(|(_, p)| p.to_vec())
                .collect()
        };
        let pause = render_caption_at(&timed(), &st, Tick(30), 1920, 1080).unwrap();
        let first = render_caption_at(&timed(), &st, Tick(5), 1920, 1080).unwrap();
        let second = render_caption_at(&timed(), &st, Tick(55), 1920, 1080).unwrap();
        assert_eq!((first.x, first.y, first.w, first.h), (pause.x, pause.y, pause.w, pause.h), "same overlay box at every time");
        // a word drawn 1.5 times larger has about 2.25 times the ink: the total grows by more
        // than half of the pause total (two words of similar size)
        let p0 = ink(&pause) as f32;
        assert!(ink(&first) as f32 > p0 * 1.5, "{} vs {p0}", ink(&first));
        assert!(ink(&second) as f32 > p0 * 1.5, "{} vs {p0}", ink(&second));
        // the word not being spoken is untouched
        assert_eq!(region(&first, false), region(&pause, false));
        assert_eq!(region(&second, true), region(&pause, true));
        // the pop is centred on the word: its ink centroid stays put
        let (c0, _) = centroid(&pause, |p| p[3] > 0.5);
        let (c1, _) = centroid(&first, |p| p[3] > 0.5);
        assert!(c1 < c0, "the left word grew: {c1} < {c0}");
        // the popped word stays inside the overlay
        let mut canvas = vec![0.0f32; 1920 * 1080 * 4];
        first.composite_onto(&mut canvas, 1920, 1080);
        let edge = |o: &Overlay| (0..o.h).any(|y| o.px[(y * o.w) * 4 + 3] > 0.01 || o.px[(y * o.w + o.w - 1) * 4 + 3] > 0.01);
        assert!(!edge(&first), "ink touches the overlay edge");
    }

    #[test]
    fn boxed_draws_a_box_behind_the_word_being_spoken() {
        let red = [255, 0, 0, 255];
        let st = style(CaptionAnimation::Boxed { box_color: red, pad: 8.0 });
        let is_red = |p: &[f32]| p[3] > 0.9 && p[0] > 0.9 && p[1] < 0.05 && p[2] < 0.05;
        let pause = render_caption_at(&timed(), &st, Tick(30), 1920, 1080).unwrap();
        assert_eq!(centroid(&pause, is_red).1, 0, "no box between words");
        let first = render_caption_at(&timed(), &st, Tick(5), 1920, 1080).unwrap();
        let second = render_caption_at(&timed(), &st, Tick(55), 1920, 1080).unwrap();
        let (x1, n1) = centroid(&first, is_red);
        let (x2, n2) = centroid(&second, is_red);
        assert!(n1 > 1000 && n2 > 1000, "{n1} {n2}");
        assert!(x1 < 960.0 && x2 > 960.0, "{x1} {x2}");
        // the text stays white over the box
        assert!(centroid(&first, white).1 > 200);
        // after the last word nothing is boxed
        let after = render_caption_at(&timed(), &st, Tick(90), 1920, 1080).unwrap();
        assert_eq!(centroid(&after, is_red).1, 0);
    }

    #[test]
    fn captions_without_matching_words_draw_statically() {
        let st = style(CaptionAnimation::KaraokeHighlight { highlight_color: YELLOW });
        let stat = render_caption(&cap("aaaa bbbb"), &st, 1920, 1080).unwrap();
        // no words
        assert_eq!(render_caption_at(&cap("aaaa bbbb"), &st, Tick(5), 1920, 1080).unwrap(), stat);
        // words that no longer spell the text (a corrupt or hand-edited project)
        let mut c = timed();
        c.text = "aaaa cccc".into();
        let stat = render_caption(&c, &st, 1920, 1080).unwrap();
        assert_eq!(render_caption_at(&c, &st, Tick(55), 1920, 1080).unwrap(), stat);
        // and no animation means static whatever the words
        let plain = CaptionStyle { background: false, ..Default::default() };
        assert_eq!(render_caption_at(&timed(), &plain, Tick(5), 1920, 1080), render_caption(&timed(), &plain, 1920, 1080));
    }

    #[test]
    fn multi_line_words_map_across_wrapped_lines() {
        // eight words wrapped on a narrow frame: the highlight walks down the lines in order
        let text = "one two three four five six seven eight";
        let mut c = cap(text);
        c.words = text.split(' ').enumerate().map(|(i, w)| WordTiming::new(Tick(i as i64 * 10), Tick(10), w)).collect();
        let st = CaptionStyle { size: 90.0, ..style(CaptionAnimation::KaraokeHighlight { highlight_color: YELLOW }) };
        let mut last = 0;
        for i in 0..8 {
            let o = render_caption_at(&c, &st, Tick(i * 10 + 5), 480, 360).unwrap();
            let n = centroid(&o, yellow).1;
            assert!(n > last, "word {i}: {n} lit pixels after {last}");
            last = n;
        }
        assert!(centroid(&render_caption_at(&c, &st, Tick(75), 480, 360).unwrap(), white).1 == 0);
    }

    #[test]
    fn hostile_animation_values_do_not_panic() {
        let mut c = timed();
        c.words[0] = WordTiming::new(Tick(i64::MIN), Tick(i64::MIN), "aaaa");
        c.words[1] = WordTiming::new(Tick(i64::MAX), Tick(i64::MAX), "bbbb");
        for a in [
            CaptionAnimation::WordPop { scale: f32::NAN },
            CaptionAnimation::WordPop { scale: f32::INFINITY },
            CaptionAnimation::WordPop { scale: -4.0 },
            CaptionAnimation::Boxed { box_color: [1, 2, 3, 4], pad: f32::NAN },
            CaptionAnimation::Boxed { box_color: [1, 2, 3, 4], pad: 1e30 },
            CaptionAnimation::KaraokeHighlight { highlight_color: [0, 0, 0, 0] },
        ] {
            let st = CaptionStyle { animation: a, outline: 2.0, ..Default::default() };
            for t in [Tick::MIN, Tick(0), Tick(5), Tick::MAX] {
                let r = std::panic::catch_unwind(|| render_caption_at(&c, &st, t, 64, 36));
                assert!(r.is_ok(), "{a:?} at {t:?}");
            }
        }
    }

    #[test]
    fn sequence_overlays_animate_with_time() {
        // the one path the Program monitor and export share
        let mut tr = CaptionTrack::new(TrackId(1), "S".into(), CaptionFormat::Subtitle);
        tr.style = style(CaptionAnimation::KaraokeHighlight { highlight_color: YELLOW });
        tr.captions.push(timed());
        let a = track_overlay(&tr, Tick(5), 1920, 1080).unwrap();
        let b = track_overlay(&tr, Tick(55), 1920, 1080).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, render_caption_at(&timed(), &tr.style, Tick(5), 1920, 1080).unwrap());
    }

    /// `text` with one 10-tick word every 20 ticks, every word timed.
    fn spelled(text: &str) -> Caption {
        let mut c = cap(text);
        c.words = filmcraft_project::caption_words(text).into_iter().enumerate().map(|(i, w)| WordTiming::new(Tick(i as i64 * 20), Tick(10), w)).collect();
        c.duration = Tick(c.words.len() as i64 * 20 + 20);
        c
    }

    /// No ink within `n` pixels of the overlay's edges.
    fn clear_edges(o: &Overlay, n: usize) -> bool {
        (0..o.h).all(|y| {
            (0..o.w).all(|x| {
                let edge = x < n || y < n || x + n >= o.w || y + n >= o.h;
                !edge || o.px.get((y * o.w + x) * 4 + 3).is_none_or(|a| *a < 0.01)
            })
        })
    }

    #[test]
    fn huge_captions_render_in_bounded_memory_or_decline() {
        let (w, h) = (1080usize, 1920usize);
        let words20k = (0..20_000).map(|i| format!("w{i}")).collect::<Vec<_>>().join("\n");
        let one_line_words = "ab ".repeat(33_333);
        let one_word = "x".repeat(100_000);
        for text in [words20k.as_str(), one_line_words.as_str(), one_word.as_str()] {
            let c = spelled(text);
            for animation in [
                CaptionAnimation::None,
                CaptionAnimation::KaraokeHighlight { highlight_color: YELLOW },
                CaptionAnimation::WordPop { scale: 3.0 },
                CaptionAnimation::Boxed { box_color: [255, 0, 0, 255], pad: 200.0 },
            ] {
                let st = CaptionStyle { animation, outline: 4.0, ..Default::default() };
                let started = std::time::Instant::now();
                let r = std::panic::catch_unwind(|| render_caption_at(&c, &st, Tick(30), w, h));
                let o = r.unwrap_or_else(|_| panic!("{animation:?} panicked"));
                // generous: the text engine's first shaping of a 100k-character run takes seconds
                // on its own (cached afterwards), as it did before animations existed
                assert!(started.elapsed() < std::time::Duration::from_secs(60), "{animation:?}: {:?}", started.elapsed());
                if let Some(o) = o {
                    // never larger than 4 frames, and only around the frame
                    assert!(o.w * o.h <= 4 * w * h, "{animation:?}: {}x{}", o.w, o.h);
                    assert!(o.px.len() == o.w * o.h * 4);
                    let grow_x = w as i32 + 2; // the most any animation may add (pop 3 on a line as wide as the frame)
                    assert!(o.x >= -grow_x && o.x + o.w as i32 <= w as i32 + grow_x, "{animation:?}: x {} w {}", o.x, o.w);
                }
            }
        }
        // a static caption is clipped to the frame plus the outline room
        let st = CaptionStyle { outline: 4.0, ..Default::default() };
        let o = render_caption(&spelled(&words20k), &st, w, h).unwrap();
        // the outline is 4 px at 1080 lines, so 7.1 px at 1920: 8 px of room plus 1
        let grow = (4.0f32 * h as f32 / 1080.0).ceil() as i32 + 1;
        assert!(o.x >= -grow && o.y >= -grow && o.x + o.w as i32 <= w as i32 + grow && o.y + o.h as i32 <= h as i32 + grow, "{} {} {} {}", o.x, o.y, o.w, o.h);
    }

    #[test]
    fn word_pop_never_clips_a_long_word() {
        // one long word per line, the usual 9:16 short-video layout
        let c = spelled("Unbelievable\nIncredible\nExtraordinary");
        for scale in [1.25, 3.0] {
            let st = style(CaptionAnimation::WordPop { scale });
            let still = render_caption_at(&c, &st, Tick(-1), 1080, 1920).unwrap();
            for t in [5, 25, 45] {
                let o = render_caption_at(&c, &st, Tick(t), 1080, 1920).unwrap();
                assert_eq!((o.x, o.y, o.w, o.h), (still.x, still.y, still.w, still.h), "the overlay never moves");
                assert!(clear_edges(&o, 1), "pop {scale} at {t}: ink on the overlay edge");
                // the popped word really is larger
                let ink = |o: &Overlay| o.px.chunks(4).filter(|p| p[3] > 0.5).count();
                assert!(ink(&o) > ink(&still), "pop {scale} at {t}");
            }
        }
    }

    #[test]
    fn words_left_without_timings_draw_statically() {
        // bypass the word check: three words shown, two timed
        let mut c = timed();
        c.text = "aaaa bbbb cccc".into();
        let white = CaptionAnimation::KaraokeHighlight { highlight_color: [255, 255, 255, 255] };
        let st = CaptionStyle { animation: white, ..Default::default() };
        let stat = render_caption(&c, &CaptionStyle::default(), 1920, 1080).unwrap();
        for t in [-1, 5, 55] {
            let o = draw(&c, &st, Some(Tick(t)), 1920, 1080).unwrap();
            assert_eq!((o.x, o.y, o.w, o.h), (stat.x, stat.y, stat.w, stat.h));
            let diff = o.px.iter().zip(&stat.px).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(diff < 1e-5, "t {t}: the untimed word must still be drawn ({diff})");
        }
        // and a word list longer than the text never reaches past the words that are shown
        let mut c = timed();
        c.words.push(WordTiming::new(Tick(70), Tick(10), "extra"));
        assert!(std::panic::catch_unwind(|| draw(&c, &st, Some(Tick(75)), 1920, 1080)).is_ok());
    }

    #[test]
    fn word_extents_from_glyphs_match_measured_widths() {
        // the boxed word's box is the measured word plus the pad on each side, for an inner word
        // and for the last word of a line
        let c = spelled("alpha bravo charlie");
        let st = style(CaptionAnimation::Boxed { box_color: [255, 0, 0, 255], pad: 0.0 });
        let ts = caption_style("Inter", 54.0);
        for (k, word) in ["bravo", "charlie"].iter().enumerate() {
            let o = render_caption_at(&c, &st, Tick((k as i64 + 1) * 20 + 5), 1920, 1080).unwrap();
            let red: Vec<usize> = o.px.chunks(4).enumerate().filter(|(_, p)| p[3] > 0.5 && p[0] > 0.5 && p[1] < 0.1).map(|(j, _)| j % o.w).collect();
            let (x0, x1) = (red.iter().min().copied().unwrap(), red.iter().max().copied().unwrap());
            let box_w = (x1 - x0 + 1) as f32;
            let want = filmcraft_text::measure(word, &ts);
            assert!((box_w - want).abs() <= 2.0, "{word}: box {box_w} vs measured {want}");
        }
    }

    #[test]
    fn hidden_track_draws_nothing() {
        let mut t = CaptionTrack::new(TrackId(1), "S".into(), CaptionFormat::Subtitle);
        t.captions.push(cap("x"));
        assert!(track_overlay(&t, Tick(5), 100, 100).is_some());
        t.enabled = false;
        assert!(track_overlay(&t, Tick(5), 100, 100).is_none());
    }
}
