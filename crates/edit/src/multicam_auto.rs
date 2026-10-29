//! Auto-switching a multi-camera clip by speaker: the cuts that show whoever is talking.
//!
//! Input is the clip's speech as words on the outer timeline, each with the angle of its speaker
//! (`None` for a word without a speaker label: it only keeps cuts out of itself). Consecutive
//! words of one angle form a turn; a cut to a turn's angle is made `lead` before its first word,
//! pulled later when that would land inside another word. Optionally a wide angle covers long
//! silences. Overlapping speech gets no wide shot of its own: no cut may land inside a word, so a
//! speaker who starts over another word is shown once that word has ended. Shots shorter than `min_shot` are then merged into the shot before
//! them (the first into the one after), so a "yeah" does not flash the other camera. The result
//! is deterministic and feeds [`crate::multicam::record`].

use filmcraft_time::{Tick, TimeRange};

use crate::multicam::Cut;

/// One word of the clip's speech on the outer timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechWord {
    pub start: Tick,
    pub end: Tick,
    /// The angle of the word's speaker; `None` when the word has no speaker.
    pub angle: Option<u32>,
}

/// Auto-switch settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoParams {
    /// The wide-shot angle, used for long silences (when `wide_after` is set).
    pub wide: Option<u32>,
    /// Shortest shot kept; shorter ones are merged into a neighbour.
    pub min_shot: Tick,
    /// A silence between turns at least this long cuts to `wide`.
    pub wide_after: Option<Tick>,
    /// Cut this long before a speaker's first word.
    pub lead: Tick,
}

/// Words sorted by start, with the running maximum of their ends (to find the word a time is in).
struct Words<'a> {
    w: &'a [SpeechWord],
    max_end: Vec<Tick>,
}

impl<'a> Words<'a> {
    fn new(w: &'a [SpeechWord]) -> Self {
        let mut m = Tick::MIN;
        let max_end = w
            .iter()
            .map(|x| {
                m = m.max(x.end);
                m
            })
            .collect();
        Words { w, max_end }
    }

    /// `t`, or, when it falls inside a word (after its start, before its end), the end of the
    /// words that hold it; repeated until it is in none.
    fn outside_words(&self, mut t: Tick) -> Tick {
        for _ in 0..=self.w.len() {
            let n = self.w.partition_point(|x| x.start < t);
            match n.checked_sub(1).and_then(|i| self.max_end.get(i)) {
                Some(&e) if e > t => t = e,
                _ => break,
            }
        }
        t
    }
}

/// A turn: consecutive labelled words of one angle.
struct Turn {
    start: Tick,
    end: Tick,
    angle: u32,
}

fn turns(words: &[SpeechWord]) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for w in words {
        let Some(angle) = w.angle else { continue };
        match out.last_mut() {
            Some(t) if t.angle == angle => t.end = t.end.max(w.end),
            _ => out.push(Turn { start: w.start, end: w.end.max(w.start), angle }),
        }
    }
    out
}

/// The cuts for `words` inside `range`. `words` must be sorted by start. The first cut is at the
/// range start; no cut lies inside a word except that one. Empty when no word has a speaker.
pub fn auto_switch(words: &[SpeechWord], range: TimeRange, p: &AutoParams) -> Vec<Cut> {
    let (lo, hi) = (range.start, range.end());
    let all = Words::new(words);
    let turns = turns(words);
    let Some(first) = turns.first() else { return Vec::new() };
    // (wanted start, angle) in order; made monotonic below
    let mut events: Vec<(Tick, u32)> = Vec::new();
    let lead_cut = |t: &Turn| all.outside_words((t.start - p.lead).max(lo));
    let wide = p.wide.zip(p.wide_after);
    match wide {
        Some((w, after)) if first.start - lo >= after => {
            events.push((lo, w));
            events.push((lead_cut(first), first.angle));
        }
        _ => events.push((lo, first.angle)),
    }
    for pair in turns.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let silence = b.start - a.end;
        match wide {
            Some((w, after)) if silence >= after => {
                events.push((all.outside_words(a.end), w));
                events.push((lead_cut(b), b.angle));
            }
            _ if a.angle != b.angle => events.push((lead_cut(b), b.angle)),
            _ => {}
        }
    }
    // monotonic starts inside the range: a later event at or before the previous start replaces it
    let mut segs: Vec<(Tick, u32)> = Vec::new();
    for (t, angle) in events {
        if t >= hi {
            continue;
        }
        match segs.last_mut() {
            Some(l) if t <= l.0 => l.1 = angle,
            _ => segs.push((t, angle)),
        }
    }
    coalesce(&mut segs);
    // merge shots shorter than min_shot into the shot before (the first one into the next)
    while segs.len() > 1 {
        let short = (0..segs.len()).find(|&i| {
            let end = segs.get(i + 1).map_or(hi, |n| n.0);
            end - segs[i].0 < p.min_shot
        });
        let Some(i) = short else { break };
        if i == 0 {
            let start = segs[0].0;
            segs.remove(0);
            segs[0].0 = start;
        } else {
            segs.remove(i);
        }
        coalesce(&mut segs);
    }
    segs.into_iter().map(|(time, angle)| Cut { time, angle }).collect()
}

/// Neighbouring shots of one angle are one shot.
fn coalesce(segs: &mut Vec<(Tick, u32)>) {
    segs.dedup_by(|b, a| a.1 == b.1);
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = filmcraft_time::TICKS_PER_SECOND;
    fn t(sec: f64) -> Tick {
        Tick((sec * S as f64).round() as i64)
    }
    fn w(a: f64, b: f64, angle: u32) -> SpeechWord {
        SpeechWord { start: t(a), end: t(b), angle: Some(angle) }
    }
    fn params() -> AutoParams {
        AutoParams { wide: None, min_shot: t(2.0), wide_after: None, lead: t(0.1) }
    }
    fn range(a: f64, b: f64) -> TimeRange {
        TimeRange::from_bounds(t(a), t(b))
    }
    fn times(c: &[Cut]) -> Vec<(i64, u32)> {
        c.iter().map(|c| (c.time.0 * 1000 / S, c.angle)).collect()
    }

    #[test]
    fn two_speakers_alternating_cut_a_lead_before_each_turn() {
        let words = [w(0.0, 1.0, 0), w(1.2, 3.0, 0), w(4.0, 6.0, 1), w(7.0, 9.0, 0)];
        let cuts = auto_switch(&words, range(0.0, 10.0), &params());
        assert_eq!(times(&cuts), vec![(0, 0), (3900, 1), (6900, 0)]);
    }

    #[test]
    fn short_turns_merge_into_the_shot_before() {
        // B says one short word inside A's talk: stays on A
        let words = [w(0.0, 3.0, 0), w(3.5, 3.9, 1), w(4.5, 8.0, 0)];
        let cuts = auto_switch(&words, range(0.0, 9.0), &params());
        assert_eq!(times(&cuts), vec![(0, 0)]);
        // a short first shot merges forward
        let words = [w(0.0, 0.5, 1), w(0.7, 6.0, 0)];
        let cuts = auto_switch(&words, range(0.0, 7.0), &params());
        assert_eq!(times(&cuts), vec![(0, 0)]);
        // a short last shot merges back
        let words = [w(0.0, 6.0, 0), w(6.5, 7.0, 1)];
        let cuts = auto_switch(&words, range(0.0, 7.2), &params());
        assert_eq!(times(&cuts), vec![(0, 0)]);
    }

    #[test]
    fn lead_is_zero_or_clamped_and_never_inside_a_word() {
        // the lead would land inside A's last word (ends 4.05): the cut waits for it
        let words = [w(0.0, 2.0, 0), w(3.0, 4.05, 0), w(4.1, 7.0, 1)];
        let cuts = auto_switch(&words, range(0.0, 8.0), &params());
        assert_eq!(times(&cuts), vec![(0, 0), (4050, 1)]);
        let p = AutoParams { lead: Tick::ZERO, ..params() };
        let cuts = auto_switch(&words, range(0.0, 8.0), &p);
        assert_eq!(times(&cuts), vec![(0, 0), (4100, 1)]);
        // overlapping words: the cut moves past the word it would split
        let words = [w(0.0, 5.0, 0), w(3.0, 7.0, 1)];
        let cuts = auto_switch(&words, range(0.0, 9.0), &params());
        for c in cuts.iter().skip(1) {
            assert!(!words.iter().any(|x| x.start < c.time && c.time < x.end), "{c:?}");
        }
    }

    #[test]
    fn wide_covers_long_silences() {
        let p = AutoParams { wide: Some(2), wide_after: Some(t(3.0)), ..params() };
        // A, a 4 s silence, B
        let words = [w(0.0, 3.0, 0), w(7.0, 10.0, 1)];
        let cuts = auto_switch(&words, range(0.0, 11.0), &p);
        assert_eq!(times(&cuts), vec![(0, 0), (3000, 2), (6900, 1)]);
        // a short pause stays on the speaker
        let words = [w(0.0, 3.0, 0), w(4.0, 8.0, 0)];
        assert_eq!(times(&auto_switch(&words, range(0.0, 9.0), &p)), vec![(0, 0)]);
        // a long silence before the first word starts on the wide shot
        let words = [w(5.0, 8.0, 0)];
        assert_eq!(times(&auto_switch(&words, range(0.0, 9.0), &p)), vec![(0, 2), (4900, 0)]);
    }

    #[test]
    fn unlabelled_words_and_empty_input() {
        let un = SpeechWord { start: t(1.0), end: t(2.0), angle: None };
        assert!(auto_switch(&[un], range(0.0, 5.0), &params()).is_empty());
        assert!(auto_switch(&[], range(0.0, 5.0), &params()).is_empty());
        // an unlabelled word protects itself from a cut
        let words = [w(0.0, 3.0, 0), SpeechWord { start: t(3.5), end: t(5.5), angle: None }, w(5.0, 9.0, 1)];
        let cuts = auto_switch(&words, range(0.0, 10.0), &params());
        assert!(cuts.iter().skip(1).all(|c| !words.iter().any(|x| x.start < c.time && c.time < x.end)), "{cuts:?}");
    }

    #[test]
    fn the_range_bounds_the_cuts_and_the_result_is_deterministic() {
        let words = [w(0.0, 3.0, 0), w(4.0, 8.0, 1), w(9.0, 12.0, 0)];
        let a = auto_switch(&words, range(2.0, 8.5), &params());
        assert_eq!(a[0].time, t(2.0));
        assert!(a.iter().all(|c| c.time >= t(2.0) && c.time < t(8.5)));
        assert_eq!(a, auto_switch(&words, range(2.0, 8.5), &params()));
    }
}
