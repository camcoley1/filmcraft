//! Speaker labelling ("who spoke when") without a model.
//!
//! A classical, openly documented method (MFCC statistics + agglomerative clustering, as in the
//! speaker-clustering literature, e.g. Reynolds & Torres-Carrasquillo 2005; no code or data from
//! any implementation is used):
//!
//! 1. The transcript's words are grouped into **chunks**: a new chunk starts at a pause of at least
//!    [`Params::chunk_gap`] or when the chunk reaches [`Params::max_chunk`]; chunks shorter than
//!    [`Params::min_chunk`] are joined to the previous one.
//! 2. Each chunk gets a voice vector: 40-band log-mel energies → DCT → MFCC 1–19 over the chunk's
//!    voiced frames (within 30 dB of its loudest), then the mean and standard deviation of each
//!    coefficient (38 values), each scaled by that coefficient's spread over all voiced frames of
//!    the file.
//! 3. Average-linkage agglomerative clustering with RMS distance merges the closest clusters
//!    until the closest pair is farther apart than [`Params::threshold`] or only
//!    `max_speakers` remain. Clusters holding less than 5 % of the speech are folded into their
//!    nearest neighbour.
//! 4. Speakers are numbered in order of first appearance ("Speaker 1", "Speaker 2"…).
//!
//! It works for interviews and dialogue with distinct voices and clean turns; overlapping speech,
//! very similar voices or music beds confuse it. Labels are editable in the Text panel (rename a
//! speaker, or assign words to a speaker by hand), which is the fallback when it gets them wrong.

use filmcraft_project::{Speaker, Transcript};
use filmcraft_time::Tick;

use crate::{SAMPLE_RATE, TICKS_PER_SAMPLE};

/// Tunables (defaults chosen on LibriSpeech test-clean dialogues; see `docs/transcripts.md`).
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub chunk_gap: Tick,
    pub min_chunk: Tick,
    pub max_chunk: Tick,
    pub threshold: f32,
    pub max_speakers: usize,
}

impl Default for Params {
    fn default() -> Self {
        let ms = |m: i64| Tick(m * TICKS_PER_SAMPLE * SAMPLE_RATE as i64 / 1000);
        Self { chunk_gap: ms(250), min_chunk: ms(700), max_chunk: ms(4000), threshold: 0.4, max_speakers: 6 }
    }
}

const N_MELS: usize = 40;
const N_MFCC: usize = 20;
const FRAME_TICKS: i64 = TICKS_PER_SAMPLE * crate::mel::HOP as i64;

/// Word index ranges of the chunks.
pub fn chunks(t: &Transcript, p: &Params) -> Vec<std::ops::Range<usize>> {
    let w = &t.words;
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut a = 0;
    for i in 1..=w.len() {
        if i == w.len() || w[i].start - w[i - 1].end >= p.chunk_gap || w[i].end - w[a].start > p.max_chunk {
            if a < i {
                out.push(a..i);
            }
            a = i;
        }
    }
    // join short chunks to the previous one
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for c in out {
        let dur = w[c.end - 1].end - w[c.start].start;
        match merged.last_mut() {
            Some(l) if dur < p.min_chunk => l.end = c.end,
            _ => merged.push(c),
        }
    }
    // a short first chunk joins the next
    if merged.len() > 1 && w[merged[0].end - 1].end - w[0].start < p.min_chunk {
        let first = merged.remove(0);
        merged[0].start = first.start;
    }
    merged
}

/// Seconds of audio turned into MFCC frames at a time (the power spectrum and mel buffers of a
/// whole two-hour file would be about 700 MB).
const BLOCK_SECONDS: usize = 30;
/// Most chunks clustered. The distances are held as a triangle of `f32` (4000 chunks: 32 MB); more
/// chunks than this are joined in pairs until they fit (see [`plan`]).
pub const MAX_CHUNKS: usize = 4000;

/// MFCC frames of `audio`: (frame count, per-frame energy in dB, `N_MFCC` coefficients per frame).
/// Computed in blocks with enough context at each side that every frame is exactly the frame the
/// whole-file computation gives; `cancel` is asked between blocks (`None`: cancelled).
fn mfcc_frames(audio: &[f32], cancel: &dyn Fn() -> bool) -> Option<(usize, Vec<f32>, Vec<f32>)> {
    mfcc_frames_in(audio, BLOCK_SECONDS * SAMPLE_RATE as usize / crate::mel::HOP, cancel)
}

/// [`mfcc_frames`] with `block` frames per block (at least one).
fn mfcc_frames_in(audio: &[f32], block: usize, cancel: &dyn Fn() -> bool) -> Option<(usize, Vec<f32>, Vec<f32>)> {
    use crate::mel::{HOP, N_FFT};
    let frames = audio.len() / HOP;
    let mut energy = vec![0f32; frames];
    let mut mfcc = vec![0f32; frames * N_MFCC];
    let dct: Vec<f32> = (0..N_MFCC * N_MELS)
        .map(|i| {
            let (k, m) = (i / N_MELS, i % N_MELS);
            (std::f32::consts::PI * k as f32 * (m as f32 + 0.5) / N_MELS as f32).cos()
        })
        .collect();
    let block = block.max(1);
    // a frame's window reaches N_FFT / 2 either side of its centre
    let margin = (N_FFT / 2).div_ceil(HOP) * HOP;
    let mut f0 = 0;
    while f0 < frames {
        if cancel() {
            return None;
        }
        let f1 = (f0 + block).min(frames);
        let a = if f0 == 0 { 0 } else { (f0 * HOP).saturating_sub(margin) / HOP * HOP };
        let b = if f1 == frames { audio.len() } else { (f1 * HOP + margin).min(audio.len()) };
        let (local, mel) = crate::mel::mel_energies(audio.get(a..b).unwrap_or(&[]), N_MELS);
        let first = f0 - a / HOP;
        let mut lm = [0f32; N_MELS];
        for f in f0..f1 {
            let lf = first + (f - f0);
            if lf >= local {
                break;
            }
            let mut e = 0f32;
            for m in 0..N_MELS {
                let v = mel[m * local + lf];
                e += v;
                lm[m] = (v + 1e-10).ln();
            }
            energy[f] = 10.0 * (e + 1e-10).log10();
            for k in 0..N_MFCC {
                mfcc[f * N_MFCC + k] = (0..N_MELS).map(|m| dct[k * N_MELS + m] * lm[m]).sum();
            }
        }
        f0 = f1;
    }
    Some((frames, energy, mfcc))
}

fn rms_distance(a: &[f32], b: &[f32]) -> f32 {
    (a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f32>() / a.len().max(1) as f32).sqrt()
}

/// Average-linkage clusters over a triangular distance matrix (Lance-Williams updates, which give
/// exactly the mean of the pairwise distances), with a nearest-neighbour cache per cluster: the
/// merges cost about `n²` distance reads in all, not `n³`.
struct Clusters {
    n: usize,
    /// Distance of active clusters `i < j` at `tri(i, j)`.
    d: Vec<f32>,
    size: Vec<usize>,
    members: Vec<Vec<usize>>,
    active: Vec<bool>,
    /// Nearest other active cluster of each active cluster and its distance.
    nn: Vec<(f32, usize)>,
    live: usize,
}

impl Clusters {
    fn tri(&self, i: usize, j: usize) -> usize {
        let (i, j) = if i < j { (i, j) } else { (j, i) };
        i * self.n - i * (i + 1) / 2 + (j - i - 1)
    }
    fn dist(&self, i: usize, j: usize) -> f32 {
        self.d.get(self.tri(i, j)).copied().unwrap_or(f32::INFINITY)
    }
    fn nearest(&self, k: usize) -> (f32, usize) {
        let mut best = (f32::INFINITY, usize::MAX);
        for o in 0..self.n {
            if o != k && self.active[o] {
                let d = self.dist(k, o);
                if d < best.0 {
                    best = (d, o);
                }
            }
        }
        best
    }
    fn new(vecs: &[Vec<f32>], cancel: &dyn Fn() -> bool) -> Option<Self> {
        let n = vecs.len();
        let mut c = Clusters {
            n,
            d: vec![0f32; n * n.saturating_sub(1) / 2],
            size: vec![1; n],
            members: (0..n).map(|i| vec![i]).collect(),
            active: vec![true; n],
            nn: vec![(f32::INFINITY, usize::MAX); n],
            live: n,
        };
        for i in 0..n {
            if i.is_multiple_of(128) && cancel() {
                return None;
            }
            for j in i + 1..n {
                let t = c.tri(i, j);
                c.d[t] = rms_distance(&vecs[i], &vecs[j]);
            }
        }
        for k in 0..n {
            if k.is_multiple_of(128) && cancel() {
                return None;
            }
            c.nn[k] = c.nearest(k);
        }
        Some(c)
    }
    /// The closest pair `(distance, i, j)` with `i < j`; the first of equals in index order.
    fn closest(&self) -> Option<(f32, usize, usize)> {
        let mut best: Option<(f32, usize, usize)> = None;
        for k in 0..self.n {
            let o = self.nn[k].1;
            if self.active[k] && o != usize::MAX {
                // smallest distance, then the lexicographically first pair: the textbook scan order
                let cand = (self.nn[k].0, k.min(o), k.max(o));
                if best.is_none_or(|b| cand.0 < b.0 || (cand.0 == b.0 && (cand.1, cand.2) < (b.1, b.2))) {
                    best = Some(cand);
                }
            }
        }
        best
    }
    /// Merge cluster `j` into `i`.
    fn merge(&mut self, i: usize, j: usize) {
        let (si, sj) = (self.size[i] as f32, self.size[j] as f32);
        for k in 0..self.n {
            if !self.active[k] || k == i || k == j {
                continue;
            }
            let t = self.tri(k, i);
            self.d[t] = (si * self.dist(k, i) + sj * self.dist(k, j)) / (si + sj);
        }
        let moved = std::mem::take(&mut self.members[j]);
        self.members[i].extend(moved);
        self.size[i] += self.size[j];
        self.active[j] = false;
        self.live -= 1;
        self.nn[i] = self.nearest(i);
        for k in 0..self.n {
            if !self.active[k] || k == i {
                continue;
            }
            if self.nn[k].1 == i || self.nn[k].1 == j {
                self.nn[k] = self.nearest(k);
            } else {
                let d = self.dist(k, i);
                // on an exact tie the lower index stays the nearest, as a fresh scan would pick it
                if d < self.nn[k].0 || (d == self.nn[k].0 && i < self.nn[k].1) {
                    self.nn[k] = (d, i);
                }
            }
        }
    }
}

/// Label the speakers of `t` from `audio` (mono 16 kHz, the transcript's time base). Replaces
/// `t.speakers` and every word's speaker; returns the number of speakers found.
pub fn diarize(audio: &[f32], t: &mut Transcript, p: &Params) -> usize {
    diarize_with(audio, t, p, &|| false).unwrap_or(0)
}

/// Longest time any chunk parameter may have (an hour): hostile values are brought into range.
const MAX_PARAM: Tick = Tick(3600 * filmcraft_time::TICKS_PER_SECOND);

fn sane(p: &Params) -> Params {
    let t = |v: Tick| Tick(v.0.clamp(0, MAX_PARAM.0));
    let d = Params::default();
    Params {
        chunk_gap: t(p.chunk_gap),
        min_chunk: t(p.min_chunk),
        max_chunk: t(p.max_chunk),
        threshold: if p.threshold.is_finite() { p.threshold } else { d.threshold },
        max_speakers: p.max_speakers.clamp(1, 64),
    }
}

/// The chunks that will be clustered and whether they had to be grouped more coarsely than
/// `p` says because there were more than [`MAX_CHUNKS`] (a very long recording). Chunk parameters
/// are brought into range first (negative to zero, at most an hour); neighbouring chunks are then
/// joined in pairs until they fit, in a bounded number of passes.
pub fn plan(t: &Transcript, p: &Params) -> (Vec<std::ops::Range<usize>>, bool) {
    let p = sane(p);
    let mut ch = chunks(t, &p);
    let mut coarse = false;
    // more than can be clustered: join neighbours in pairs (each pass halves them, so a handful
    // of passes is enough for any transcript; raising the minimum chunk length instead would join
    // everything into one chunk)
    for _ in 0..40 {
        if ch.len() <= MAX_CHUNKS {
            break;
        }
        ch = ch.chunks(2).filter_map(|c| Some(c.first()?.start..c.last()?.end)).collect();
        coarse = true;
    }
    (ch, coarse)
}

/// What [`diarize_report`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub speakers: usize,
    /// The recording had more speech segments than can be clustered, so they were grouped
    /// coarsely (see [`plan`]); labels may be less exact.
    pub coarse: bool,
}

/// [`diarize`] that asks `cancel` between blocks of audio and between merges of clusters and
/// returns `None` (leaving `t` as it was) as soon as it says yes.
pub fn diarize_with(audio: &[f32], t: &mut Transcript, p: &Params, cancel: &dyn Fn() -> bool) -> Option<usize> {
    diarize_report(audio, t, p, cancel).map(|o| o.speakers)
}

/// [`diarize_with`] that also says whether the speech had to be grouped coarsely.
pub fn diarize_report(audio: &[f32], t: &mut Transcript, p: &Params, cancel: &dyn Fn() -> bool) -> Option<Outcome> {
    if cancel() {
        return None;
    }
    if t.words.is_empty() {
        return Some(Outcome { speakers: 0, coarse: false });
    }
    let p = sane(p);
    let (ch, coarse) = plan(t, &p);
    let done = |speakers: usize| Some(Outcome { speakers, coarse });
    let one = |t: &mut Transcript| {
        t.speakers = vec![Speaker { name: "Speaker 1".into() }];
        for w in &mut t.words {
            w.speaker = Some(0);
        }
        done(1)
    };
    if ch.len() < 2 {
        return if cancel() { None } else { one(t) };
    }
    let (frames, energy, mfcc) = mfcc_frames(audio, cancel)?;
    // chunk vectors: mean and std of MFCC 1..N over voiced frames
    let dim = 2 * (N_MFCC - 1);
    let mut vecs: Vec<Vec<f32>> = Vec::new();
    let mut weight: Vec<f32> = Vec::new();
    for c in &ch {
        let a = ((t.words[c.start].start.0 / FRAME_TICKS).max(0) as usize).min(frames);
        let b = ((t.words[c.end - 1].end.0 / FRAME_TICKS).max(0) as usize).min(frames).max(a);
        let peak = energy[a..b].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let voiced: Vec<usize> = (a..b).filter(|&f| energy[f] > peak - 30.0).collect();
        let mut v = vec![0f32; dim];
        if !voiced.is_empty() {
            let n = voiced.len() as f32;
            for k in 1..N_MFCC {
                let mean = voiced.iter().map(|&f| mfcc[f * N_MFCC + k]).sum::<f32>() / n;
                let var = voiced.iter().map(|&f| (mfcc[f * N_MFCC + k] - mean).powi(2)).sum::<f32>() / n;
                v[k - 1] = mean;
                v[N_MFCC - 1 + k - 1] = var.sqrt();
            }
        }
        vecs.push(v);
        weight.push(voiced.len() as f32);
    }
    // scale each dimension by its spread over all voiced frames of the file (an absolute yardstick:
    // one voice stays close to itself however many chunks there are)
    let all: Vec<usize> = {
        let peak = energy.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        (0..frames).filter(|&f| energy[f] > peak - 40.0).collect()
    };
    let nf = all.len().max(1) as f32;
    for k in 1..N_MFCC {
        let mean = all.iter().map(|&f| mfcc[f * N_MFCC + k]).sum::<f32>() / nf;
        let sd = (all.iter().map(|&f| (mfcc[f * N_MFCC + k] - mean).powi(2)).sum::<f32>() / nf).sqrt().max(1e-6);
        for v in &mut vecs {
            v[k - 1] = (v[k - 1] - mean) / sd;
            v[N_MFCC - 1 + k - 1] /= sd;
        }
    }
    if cancel() {
        return None;
    }
    // average-linkage agglomerative clustering
    let n = vecs.len();
    let mut cl = Clusters::new(&vecs, cancel)?;
    let max_speakers = p.max_speakers.max(1);
    let mut steps = 0usize;
    while cl.live > 1 {
        steps += 1;
        if steps.is_multiple_of(64) && cancel() {
            return None;
        }
        let Some((d, i, j)) = cl.closest() else { break };
        if d > p.threshold && cl.live <= max_speakers {
            break;
        }
        cl.merge(i, j);
    }
    // fold clusters with < 5 % of the voiced frames into their nearest neighbour
    let total: f32 = weight.iter().sum::<f32>().max(1.0);
    while cl.live > 1 {
        let small = (0..n).find(|&c| cl.active[c] && cl.members[c].iter().map(|&i| weight[i]).sum::<f32>() < 0.05 * total);
        let Some(si) = small else { break };
        let near = (0..n).filter(|&c| cl.active[c] && c != si).min_by(|&a, &b| cl.dist(si, a).total_cmp(&cl.dist(si, b))).unwrap_or(0);
        // the merged cluster keeps the nearest one's place in the order, as in the textbook version
        cl.merge(near, si);
    }
    if cl.live == 1 {
        return if cancel() { None } else { one(t) };
    }
    // number speakers by first appearance
    let mut label = vec![0usize; n];
    for c in (0..n).filter(|&c| cl.active[c]) {
        for &i in &cl.members[c] {
            label[i] = c;
        }
    }
    let mut order: Vec<usize> = Vec::new();
    for &l in &label {
        if !order.contains(&l) {
            order.push(l);
        }
    }
    for (k, c) in ch.iter().enumerate() {
        let sp = order.iter().position(|&o| o == label[k]).unwrap_or(0) as u32;
        for w in &mut t.words[c.clone()] {
            w.speaker = Some(sp);
        }
    }
    t.speakers = (0..order.len()).map(|i| Speaker { name: format!("Speaker {}", i + 1) }).collect();
    done(order.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seconds_tick;
    use filmcraft_project::Word;

    /// A synthetic "voice": a pulse train at `f0` through two resonances (formants), with a slow
    /// syllable envelope.
    fn voice(f0: f32, formants: [(f32, f32); 2], secs: f32, seed: u32) -> Vec<f32> {
        let n = (secs * 16_000.0) as usize;
        let mut out = vec![0f32; n];
        let mut phase = 0f32;
        let mut st = [[0f32; 2]; 2];
        let mut rng = seed;
        for (i, o) in out.iter_mut().enumerate() {
            let t = i as f32 / 16_000.0;
            let jitter = 1.0 + 0.03 * (2.0 * std::f32::consts::PI * 5.0 * t).sin();
            phase += f0 * jitter / 16_000.0;
            let mut x = if phase >= 1.0 {
                phase -= 1.0;
                1.0
            } else {
                0.0
            };
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            x += ((rng >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02;
            let mut y = 0f32;
            for (k, (fc, bw)) in formants.iter().enumerate() {
                let r = (-std::f32::consts::PI * bw / 16_000.0).exp();
                let a1 = 2.0 * r * (2.0 * std::f32::consts::PI * fc / 16_000.0).cos();
                let a2 = -r * r;
                let v = x + a1 * st[k][0] + a2 * st[k][1];
                st[k][1] = st[k][0];
                st[k][0] = v;
                y += v;
            }
            let env = (std::f32::consts::PI * (t * 4.0).fract()).sin().max(0.0);
            *o = y * env * 0.05;
        }
        out
    }

    #[test]
    fn two_synthetic_voices_are_told_apart() {
        let a = |s| voice(110.0, [(600.0, 90.0), (1100.0, 120.0)], 2.0, s);
        let b = |s| voice(230.0, [(400.0, 80.0), (2600.0, 150.0)], 2.0, s);
        let silence = vec![0f32; 8000];
        let turns = [true, false, true, true, false, false, true, false];
        let mut audio = Vec::new();
        let mut words = Vec::new();
        for (k, &is_a) in turns.iter().enumerate() {
            let t0 = audio.len() as f64 / 16_000.0;
            audio.extend(if is_a { a(k as u32) } else { b(k as u32) });
            // four 0.5 s "words" per turn
            for w in 0..4 {
                let s = t0 + w as f64 * 0.5;
                words.push(Word::new(format!("w{k}.{w}"), seconds_tick(s), seconds_tick(s + 0.45)));
            }
            audio.extend(&silence);
        }
        let mut t = Transcript { words, ..Default::default() };
        let n = diarize(&audio, &mut t, &Params::default());
        assert_eq!(n, 2);
        for (k, &is_a) in turns.iter().enumerate() {
            let want = if is_a { 0 } else { 1 };
            for w in &t.words[k * 4..k * 4 + 4] {
                assert_eq!(w.speaker, Some(want), "turn {k}");
            }
        }
        assert_eq!(t.speakers[1].name, "Speaker 2");
        t.check().unwrap();
    }

    #[test]
    fn one_voice_is_one_speaker() {
        let audio = voice(140.0, [(500.0, 90.0), (1500.0, 120.0)], 12.0, 1);
        let words = (0..20).map(|i| Word::new("x", seconds_tick(i as f64 * 0.6), seconds_tick(i as f64 * 0.6 + 0.4))).collect();
        let mut t = Transcript { words, ..Default::default() };
        assert_eq!(diarize(&audio, &mut t, &Params::default()), 1);
        assert!(t.words.iter().all(|w| w.speaker == Some(0)));
    }

    #[test]
    fn chunks_split_at_pauses_and_join_short_ones() {
        let w = |a: f64, b: f64| Word::new("x", seconds_tick(a), seconds_tick(b));
        let t = Transcript { words: vec![w(0.0, 0.5), w(0.5, 1.0), w(2.0, 2.2), w(3.0, 3.6), w(3.6, 4.4)], ..Default::default() };
        assert_eq!(chunks(&t, &Params::default()), vec![0..3, 3..5]);
    }

    /// A deterministic pseudo-random signal.
    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut r = seed;
        (0..n)
            .map(|_| {
                r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((r >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.4
            })
            .collect()
    }

    #[test]
    fn blocked_mfcc_frames_equal_the_whole_file_frames_coefficient_by_coefficient() {
        // short inputs, lengths that are not whole frames, and blocks of 1, 7, 100 frames and one
        // block for everything: every energy and every coefficient identical
        for len in [0usize, 1, 159, 160, 161, 399, 400, 401, 1600, 16_000 + 77, 3 * 16_000 + 3] {
            let audio = noise(len, 11 + len as u32);
            let whole = mfcc_frames_in(&audio, usize::MAX / 2, &|| false).unwrap();
            for block in [1usize, 2, 3, 7, 100, 1000] {
                let blocked = mfcc_frames_in(&audio, block, &|| false).unwrap();
                assert_eq!(blocked.0, whole.0, "frames, len {len} block {block}");
                assert_eq!(blocked.1, whole.1, "energy, len {len} block {block}");
                assert_eq!(blocked.2, whole.2, "mfcc, len {len} block {block}");
            }
        }
        // and the default block size over a file of several blocks agrees with a single block
        let audio = noise(70 * 16_000 + 123, 7);
        assert_eq!(mfcc_frames(&audio, &|| false).unwrap(), mfcc_frames_in(&audio, usize::MAX / 2, &|| false).unwrap());
    }

    #[test]
    fn cancelling_stops_the_work_and_leaves_the_transcript_alone() {
        let a = |s| voice(110.0, [(600.0, 90.0), (1100.0, 120.0)], 2.0, s);
        let b = |s| voice(230.0, [(400.0, 80.0), (2600.0, 150.0)], 2.0, s);
        let mut audio = Vec::new();
        let mut words = Vec::new();
        for k in 0..6 {
            let t0 = audio.len() as f64 / 16_000.0;
            audio.extend(if k % 2 == 0 { a(k) } else { b(k) });
            words.push(Word::new(format!("w{k}"), seconds_tick(t0), seconds_tick(t0 + 1.8)));
            audio.extend(vec![0f32; 8000]);
        }
        let base = Transcript { words, ..Default::default() };
        // cancel at every check in turn: always None, never a change
        for after in 0..40usize {
            let calls = std::cell::Cell::new(0usize);
            let mut t = base.clone();
            let r = diarize_with(&audio, &mut t, &Params::default(), &|| {
                calls.set(calls.get() + 1);
                calls.get() > after
            });
            if r.is_none() {
                assert_eq!(t, base, "cancelled after {after} checks");
            } else {
                assert_eq!(r, Some(2));
                break;
            }
        }
        let mut t = base.clone();
        assert!(diarize_with(&audio, &mut t, &Params::default(), &|| true).is_none());
        assert_eq!(t, base);
    }

    /// The textbook average-linkage merge order, to compare the cached version with.
    fn naive_merges(vecs: &[Vec<f32>]) -> Vec<(usize, usize)> {
        let n = vecs.len();
        let mut clusters: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
        let mut out = Vec::new();
        while clusters.len() > 1 {
            let mut best = (f32::INFINITY, 0, 0);
            for i in 0..clusters.len() {
                for j in i + 1..clusters.len() {
                    let mut s = 0f32;
                    for &a in &clusters[i] {
                        for &b in &clusters[j] {
                            s += rms_distance(&vecs[a], &vecs[b]);
                        }
                    }
                    let d = s / (clusters[i].len() * clusters[j].len()) as f32;
                    if d < best.0 {
                        best = (d, i, j);
                    }
                }
            }
            out.push((clusters[best.1][0], clusters[best.2][0]));
            let b = clusters.remove(best.2);
            clusters[best.1].extend(b);
        }
        out
    }

    /// Voice-like audio and words for `n` chunks: alternating two timbres, a second apart.
    fn many_chunks(n: usize) -> (Vec<f32>, Transcript) {
        let mut audio = Vec::new();
        let mut words = Vec::new();
        for k in 0..n {
            let t0 = audio.len() as f64 / 16_000.0;
            audio.extend(noise(12_800, 100 + (k % 7) as u32).iter().map(|v| v * (1.0 + (k % 2) as f32)));
            words.push(Word::new(format!("w{k}"), seconds_tick(t0), seconds_tick(t0 + 0.8)));
            audio.extend(vec![0f32; 8000]);
        }
        (audio, Transcript { words, ..Default::default() })
    }

    #[test]
    fn cancelling_works_on_the_single_chunk_path_and_inside_the_merge_loop() {
        // one chunk: a cancel is honoured, nothing changes
        let words = vec![Word::new("a", seconds_tick(0.0), seconds_tick(1.0))];
        let base = Transcript { words, ..Default::default() };
        let mut t = base.clone();
        assert!(diarize_with(&noise(16_000, 1), &mut t, &Params::default(), &|| true).is_none());
        assert_eq!(t, base);
        let mut t = base.clone();
        assert_eq!(diarize_with(&noise(16_000, 1), &mut t, &Params::default(), &|| false), Some(1));

        // 200 chunks: well over 64 merges. Count the checks of a full run, then cancel at each of
        // the last ones (they are inside the merge loop and after it): always None and unchanged
        let (audio, base) = many_chunks(200);
        let calls = std::cell::Cell::new(0usize);
        let mut t = base.clone();
        diarize_with(&audio, &mut t, &Params::default(), &|| {
            calls.set(calls.get() + 1);
            false
        })
        .unwrap();
        let total = calls.get();
        assert!(total >= 8, "checks: {total}");
        for at in [total - 1, total - 2, total - 3] {
            let n = std::cell::Cell::new(0usize);
            let mut t = base.clone();
            let r = diarize_with(&audio, &mut t, &Params::default(), &|| {
                n.set(n.get() + 1);
                n.get() > at
            });
            assert!(r.is_none(), "cancelled at check {at} of {total}");
            assert_eq!(t, base);
        }
    }

    #[test]
    fn exact_ties_merge_in_the_textbook_order() {
        // many equal vectors and equal distances: grid points, duplicates, symmetric triples
        let sets: Vec<Vec<Vec<f32>>> = vec![
            (0..40).map(|i| vec![(i % 4) as f32, (i % 3) as f32]).collect(),
            (0..30).map(|i| vec![(i / 3) as f32]).collect(),
            vec![vec![0.0]; 12],
            (0..25).map(|i| vec![(i % 5) as f32, (i / 5) as f32]).collect(),
        ];
        for vecs in sets {
            let mut cl = Clusters::new(&vecs, &|| false).unwrap();
            let mut got = Vec::new();
            while cl.live > 1 {
                let (_, i, j) = cl.closest().unwrap();
                got.push((cl.members[i][0], cl.members[j][0]));
                cl.merge(i, j);
            }
            assert_eq!(got, naive_merges(&vecs), "{} vectors", vecs.len());
        }
    }

    #[test]
    fn cached_clustering_merges_in_the_same_order_as_the_textbook_version() {
        let vecs: Vec<Vec<f32>> = (0..60).map(|i| noise(8, 100 + i).iter().map(|v| v * (1.0 + (i % 3) as f32)).collect()).collect();
        let mut cl = Clusters::new(&vecs, &|| false).unwrap();
        let mut got = Vec::new();
        while cl.live > 1 {
            let (_, i, j) = cl.closest().unwrap();
            got.push((cl.members[i][0], cl.members[j][0]));
            cl.merge(i, j);
        }
        let want = naive_merges(&vecs);
        assert_eq!(got.len(), want.len());
        // the same sequence of merges up to equal-distance ties (none for random data)
        assert_eq!(got, want);
    }

    /// `n` words of 0.2 s with 0.1 s between them (one chunk each with these parameters).
    fn dense_words(n: usize) -> Vec<Word> {
        (0..n).map(|i| Word::new("x", seconds_tick(i as f64 * 0.3), seconds_tick(i as f64 * 0.3 + 0.2))).collect()
    }

    fn dense_params() -> Params {
        Params { chunk_gap: seconds_tick(0.05), min_chunk: seconds_tick(0.1), max_chunk: seconds_tick(0.4), ..Params::default() }
    }

    #[test]
    fn more_chunks_than_the_limit_are_joined_and_still_labelled() {
        let n = MAX_CHUNKS + 1500;
        let t = Transcript { words: dense_words(n), ..Default::default() };
        let p = dense_params();
        assert_eq!(chunks(&t, &p).len(), n, "one chunk per word before joining");
        let (ch, coarse) = plan(&t, &p);
        assert!(coarse && ch.len() <= MAX_CHUNKS && ch.len() > 1, "{}", ch.len());
        // joined chunks tile the words
        assert_eq!(ch.first().unwrap().start, 0);
        assert_eq!(ch.last().unwrap().end, n);
        assert!(ch.windows(2).all(|w| w[0].end == w[1].start));
        // and a real run labels every word
        let audio = noise((n as f64 * 0.3) as usize * 16_000 + 16_000, 3);
        let mut t = t;
        let out = diarize_report(&audio, &mut t, &p, &|| false).unwrap();
        assert!(out.coarse && out.speakers >= 1);
        assert!(t.words.iter().all(|w| w.speaker.is_some()));
        // under the limit nothing is grouped coarsely
        let few = Transcript { words: dense_words(50), ..Default::default() };
        assert!(!plan(&few, &p).1);
    }

    #[test]
    fn hostile_parameters_end_and_are_brought_into_range() {
        let n = MAX_CHUNKS + 100;
        let words = dense_words(n);
        let audio = noise((n as f64 * 0.3) as usize * 16_000 + 16_000, 9);
        let zero = Tick(0);
        let hostile = [
            Params { chunk_gap: zero, min_chunk: zero, max_chunk: zero, threshold: 0.4, max_speakers: 0 },
            Params { chunk_gap: Tick(-5), min_chunk: Tick(i64::MIN), max_chunk: Tick(i64::MIN), threshold: f32::NAN, max_speakers: usize::MAX },
            Params { chunk_gap: Tick(i64::MAX), min_chunk: Tick(i64::MAX), max_chunk: Tick(i64::MAX), threshold: f32::INFINITY, max_speakers: 3 },
            Params { chunk_gap: seconds_tick(0.05), min_chunk: seconds_tick(0.1), max_chunk: zero, threshold: -1.0, max_speakers: 2 },
        ];
        for p in hostile {
            let t = Transcript { words: words.clone(), ..Default::default() };
            let (ch, _) = plan(&t, &p);
            assert!(ch.len() <= MAX_CHUNKS, "{p:?}: {}", ch.len());
            let mut t = t;
            let out = diarize_report(&audio, &mut t, &p, &|| false).expect("ends");
            assert!(out.speakers >= 1);
        }
    }

    /// Cost of a long file (run by hand: `cargo test -p filmcraft-speech --lib measure_long -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn measure_long() {
        let hours: f64 = std::env::var("DIARIZE_HOURS").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
        let secs = (hours * 3600.0) as usize;
        let audio = noise(secs * 16_000, 5);
        // a 0.8 s "word" every DIARIZE_PERIOD seconds (default 1.8: about 4000 chunks in 2 hours,
        // the clustering ceiling; 1.0 gives one chunk per second)
        let period: f64 = std::env::var("DIARIZE_PERIOD").ok().and_then(|v| v.parse().ok()).unwrap_or(1.8);
        let words: Vec<Word> =
            (0..(secs as f64 / period) as usize).map(|i| Word::new("x", seconds_tick(i as f64 * period), seconds_tick(i as f64 * period + 0.8))).collect();
        let mut t = Transcript { words, ..Default::default() };
        let t0 = std::time::Instant::now();
        let n = diarize_with(&audio, &mut t, &Params::default(), &|| false).unwrap();
        eprintln!("{hours} h: {} words, {n} speakers, {:.1} s", t.words.len(), t0.elapsed().as_secs_f64());
    }
}
