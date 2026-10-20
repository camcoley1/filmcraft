//! Caption edits.
//!
//! Captions live on caption tracks ([`CaptionTrack`]); like track items they never overlap. Every
//! function validates first and returns an error leaving the sequence untouched, or applies the
//! whole edit. Locked caption tracks refuse edits. Sync-locked caption tracks follow insert and
//! extract edits on the media tracks (see [`crate::insert`] / [`crate::extract`]).
//!
//! Word timings ([`Caption::words`], sequence ticks) follow every edit deterministically:
//! - moves (move, ripple shifts, insert and extract shifts) move the words with the caption;
//! - trims keep the words where they are in sequence time and clamp them into the new range
//!   (a word left outside becomes zero-length at that edge; words are never dropped, because the
//!   text still shows them);
//! - Set In/Out (`set_caption_times`) is a move when both edges change by the same amount (the
//!   words move with it); otherwise it is a trim (words keep their sequence times, then clamp);
//! - a split divides the word list at the split time: the words starting before it stay left,
//!   the rest go right, and the text is cut at that word boundary. When the words do not fall
//!   on both sides (or there are none), both halves keep the text and all words, clamped;
//! - a merge concatenates the word lists; a text edit keeps the words only while they still
//!   spell the text word for word ([`Caption::reconcile_words`]), otherwise they are cleared.

use filmcraft_project::{Caption, CaptionTrack, ClipId, Sequence, TrackId};
use filmcraft_time::{Tick, TimeRange};

use crate::{Edge, EditCtx, EditError, Result};

/// Default duration of a new caption (Premiere uses about three seconds).
pub const DEFAULT_SECONDS: i64 = 3;

fn track_mut(seq: &mut Sequence, id: TrackId) -> Result<&mut CaptionTrack> {
    let t = seq.caption_track_mut(id).ok_or(EditError::NoTrack(id))?;
    if t.locked {
        return Err(EditError::Locked);
    }
    Ok(t)
}

fn owner(seq: &Sequence, id: ClipId) -> Result<TrackId> {
    seq.find_caption(id).map(|(t, _)| t).ok_or(EditError::NoItem(id))
}

/// Previous end / next start around caption `id` on its track.
fn neighbours(t: &CaptionTrack, id: ClipId) -> (Tick, Tick) {
    let i = t.captions.iter().position(|c| c.id == id).unwrap_or(0);
    let prev = if i > 0 { t.captions[i - 1].end() } else { Tick::ZERO };
    let next = t.captions.get(i + 1).map(|c| c.start).unwrap_or(Tick::MAX);
    (prev, next)
}

/// Add a caption at `start`. The duration is cut short at the next caption; fails if a caption
/// already covers `start` or less than one frame would remain.
pub fn add_caption(seq: &mut Sequence, track: TrackId, start: Tick, duration: Tick, text: &str, ctx: &mut EditCtx) -> Result<ClipId> {
    let start = start.max(Tick::ZERO);
    let t = track_mut(seq, track)?;
    if t.caption_at(start).is_some() {
        return Err(EditError::Other("a caption already covers this time".into()));
    }
    let next = t.captions.iter().map(|c| c.start).filter(|s| *s > start).min().unwrap_or(Tick::MAX);
    let dur = duration.min(next - start);
    if dur < ctx.min_duration {
        return Err(EditError::TooShort);
    }
    let id = ClipId(ctx.alloc());
    t.captions.push(Caption { id, start, duration: dur, text: text.to_string(), speaker: None, cue_id: None, settings: String::new(), words: Vec::new() });
    t.sort();
    Ok(id)
}

/// Split caption `id` at `t` (strictly inside it); both halves keep the text. Returns the new
/// (right) caption.
pub fn split_caption(seq: &mut Sequence, id: ClipId, t: Tick, ctx: &mut EditCtx) -> Result<ClipId> {
    let tid = owner(seq, id)?;
    let min = ctx.min_duration;
    let new_id = ClipId(ctx.alloc());
    let tr = track_mut(seq, tid)?;
    let c = tr.caption_mut(id).ok_or(EditError::NoItem(id))?;
    if t - c.start < min || c.end() - t < min {
        return Err(EditError::TooShort);
    }
    let right = cut(c, t, new_id);
    tr.captions.push(right);
    tr.sort();
    Ok(new_id)
}

/// Cut `c` at `at` (strictly inside it): `c` keeps the left part, the returned caption (id `id`)
/// is the right part. Words follow the split rule in the module docs.
fn cut(c: &mut Caption, at: Tick, id: ClipId) -> Caption {
    let mut right = c.clone();
    right.id = id;
    right.start = at;
    right.duration = c.end() - at;
    right.cue_id = None;
    c.duration = at - c.start;
    if let Some((text, words)) = c.split_words_at(at) {
        right.text = text;
        right.words = words;
    }
    c.clamp_words();
    right.clamp_words();
    right
}

/// Split every caption strictly containing `t` on unlocked caption tracks (all when `tracks` is
/// empty). Returns the new captions.
pub fn split_captions_at(seq: &mut Sequence, tracks: &[TrackId], t: Tick, ctx: &mut EditCtx) -> Vec<ClipId> {
    let ids: Vec<ClipId> = seq
        .caption_tracks
        .iter()
        .filter(|tr| !tr.locked && (tracks.is_empty() || tracks.contains(&tr.id)))
        .filter_map(|tr| tr.caption_at(t).filter(|c| c.start < t).map(|c| c.id))
        .collect();
    ids.into_iter().filter_map(|id| split_caption(seq, id, t, ctx).ok()).collect()
}

/// Merge captions on one track into the first: it spans from the first start to the last end and
/// joins the texts with line breaks. The captions must be consecutive (none in between).
pub fn merge_captions(seq: &mut Sequence, ids: &[ClipId]) -> Result<ClipId> {
    if ids.len() < 2 {
        return Err(EditError::Nothing);
    }
    let tid = owner(seq, ids[0])?;
    for id in ids {
        if owner(seq, *id)? != tid {
            return Err(EditError::Other("captions to merge must be on one track".into()));
        }
    }
    let tr = track_mut(seq, tid)?;
    let mut idx: Vec<usize> = ids.iter().filter_map(|id| tr.captions.iter().position(|c| c.id == *id)).collect();
    idx.sort_unstable();
    idx.dedup();
    if idx.windows(2).any(|w| w[1] != w[0] + 1) {
        return Err(EditError::Other("captions to merge must be next to each other".into()));
    }
    let (Some(&first), Some(&last)) = (idx.first(), idx.last()) else {
        return Err(EditError::Nothing);
    };
    let end = tr.captions[last].end();
    let text: Vec<String> = idx.iter().map(|&i| tr.captions[i].text.trim().to_string()).filter(|t| !t.is_empty()).collect();
    let words: Vec<_> = idx.iter().flat_map(|&i| tr.captions[i].words.clone()).collect();
    let keep = tr.captions[first].id;
    let c = &mut tr.captions[first];
    c.duration = end - c.start;
    c.text = text.join("\n");
    c.words = words;
    c.reconcile_words();
    c.clamp_words();
    tr.captions.drain(first + 1..=last);
    Ok(keep)
}

/// Change a caption's text and/or speaker.
pub fn set_caption(seq: &mut Sequence, id: ClipId, text: Option<&str>, speaker: Option<Option<&str>>) -> Result<()> {
    let tid = owner(seq, id)?;
    let c = track_mut(seq, tid)?.caption_mut(id).ok_or(EditError::NoItem(id))?;
    if let Some(t) = text {
        c.text = t.replace("\r\n", "\n");
        c.reconcile_words();
    }
    if let Some(s) = speaker {
        c.speaker = s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    }
    Ok(())
}

/// Set a caption's in and out points directly (Captions panel timecode fields).
pub fn set_caption_times(seq: &mut Sequence, id: ClipId, start: Tick, end: Tick, ctx: &EditCtx) -> Result<()> {
    let tid = owner(seq, id)?;
    let tr = track_mut(seq, tid)?;
    if end - start < ctx.min_duration || start < Tick::ZERO {
        return Err(EditError::TooShort);
    }
    if tr.captions.iter().any(|c| c.id != id && c.start < end && start < c.end()) {
        return Err(EditError::Other("captions would overlap".into()));
    }
    let c = tr.caption_mut(id).ok_or(EditError::NoItem(id))?;
    if end - start == c.duration {
        // both edges moved by the same amount: a move
        c.shift(start - c.start);
    } else {
        // a trim of one or both edges: words stay with the audio
        c.start = start;
        c.duration = end - start;
    }
    c.clamp_words();
    tr.sort();
    Ok(())
}

/// Trim a caption edge by `delta`, limited by its neighbours and a one-frame minimum. Returns the
/// delta applied.
pub fn trim_caption(seq: &mut Sequence, id: ClipId, edge: Edge, delta: Tick, ctx: &EditCtx) -> Result<Tick> {
    let tid = owner(seq, id)?;
    let tr = track_mut(seq, tid)?;
    let (prev, next) = neighbours(tr, id);
    let c = tr.caption_mut(id).ok_or(EditError::NoItem(id))?;
    let d = match edge {
        Edge::In => delta.clamp(prev - c.start, c.duration - ctx.min_duration),
        Edge::Out => delta.clamp(ctx.min_duration - c.duration, next - c.end()),
    };
    if d == Tick::ZERO {
        return Err(EditError::Nothing);
    }
    match edge {
        Edge::In => {
            c.start += d;
            c.duration -= d;
        }
        Edge::Out => c.duration += d,
    }
    c.clamp_words();
    Ok(d)
}

/// Move captions by `delta`, clamped so they stay at or after zero and do not overlap captions
/// that are not moving. Returns the delta applied.
pub fn move_captions(seq: &mut Sequence, ids: &[ClipId], delta: Tick) -> Result<Tick> {
    let mut lo = Tick::MIN;
    let mut hi = Tick::MAX;
    let mut tracks = Vec::new();
    for id in ids {
        let tid = owner(seq, *id)?;
        let tr = seq.caption_track(tid).ok_or(EditError::NoTrack(tid))?;
        if tr.locked {
            return Err(EditError::Locked);
        }
        let c = tr.caption(*id).ok_or(EditError::NoItem(*id))?;
        lo = lo.max(-c.start);
        // nearest non-moving neighbours on each side
        for o in tr.captions.iter().filter(|o| !ids.contains(&o.id)) {
            if o.end() <= c.start {
                lo = lo.max(o.end() - c.start);
            } else if o.start >= c.end() {
                hi = hi.min(o.start - c.end());
            } else {
                return Err(EditError::Other("captions overlap".into()));
            }
        }
        if !tracks.contains(&tid) {
            tracks.push(tid);
        }
    }
    // a gap between moving and non-moving captions is jumped over only when it fits entirely
    let d = delta.clamp(lo.min(Tick::ZERO), hi.max(Tick::ZERO));
    if d == Tick::ZERO {
        return Err(EditError::Nothing);
    }
    for tid in tracks {
        let Some(tr) = seq.caption_track_mut(tid) else { continue };
        for c in tr.captions.iter_mut().filter(|c| ids.contains(&c.id)) {
            c.shift(d);
        }
        tr.sort();
    }
    Ok(d)
}

/// Delete captions. With `ripple`, later captions on the same track move left to close each gap.
pub fn delete_captions(seq: &mut Sequence, ids: &[ClipId], ripple: bool) -> Result<usize> {
    let mut n = 0;
    for id in ids {
        let tid = owner(seq, *id)?;
        if seq.caption_track(tid).is_some_and(|t| t.locked) {
            return Err(EditError::Locked);
        }
    }
    for tr in seq.caption_tracks.iter_mut().filter(|t| !t.locked) {
        let mut removed: Vec<TimeRange> = tr.captions.iter().filter(|c| ids.contains(&c.id)).map(Caption::range).collect();
        if removed.is_empty() {
            continue;
        }
        n += removed.len();
        tr.captions.retain(|c| !ids.contains(&c.id));
        if ripple {
            removed.sort_by_key(|r| std::cmp::Reverse(r.start));
            for r in removed {
                shift_from(tr, r.end(), -r.duration);
            }
        }
    }
    Ok(n)
}

/// Shift every caption starting at or after `at` by `delta`.
pub fn shift_from(tr: &mut CaptionTrack, at: Tick, delta: Tick) {
    for c in tr.captions.iter_mut().filter(|c| c.start >= at) {
        c.shift(delta);
    }
}

/// Open a gap of `dur` at `at` (insert edit): a caption straddling `at` is split.
pub fn insert_gap(tr: &mut CaptionTrack, at: Tick, dur: Tick, ctx: &mut EditCtx) {
    if let Some(id) = tr.caption_at(at).filter(|c| c.start < at).map(|c| c.id) {
        let new_id = ClipId(ctx.alloc());
        if let Some(right) = tr.caption_mut(id).map(|l| cut(l, at, new_id)) {
            tr.captions.push(right);
            tr.sort();
        }
    }
    shift_from(tr, at, dur);
}

/// Remove `range` from a caption track (captions inside go, captions across it are cut) and close
/// the gap (extract).
pub fn extract_range(tr: &mut CaptionTrack, range: TimeRange) {
    let (a, b) = (range.start, range.end());
    let mut out = Vec::with_capacity(tr.captions.len());
    for mut c in tr.captions.drain(..) {
        let (s, e) = (c.start, c.end());
        if e <= a {
            out.push(c);
        } else if s >= b {
            c.shift(-range.duration);
            out.push(c);
        } else {
            // overlaps the range: keep what lies outside it, joined across the closed gap
            let left = (a - s).max(Tick::ZERO);
            let right = (e - b).max(Tick::ZERO);
            if left + right > Tick::ZERO {
                c.start = s.min(a);
                c.duration = left + right;
                // words map through the extract: inside the range they collapse onto the cut
                let close = |x: Tick| {
                    if x <= a {
                        x
                    } else if x >= b {
                        x - range.duration
                    } else {
                        a
                    }
                };
                for w in &mut c.words {
                    let (ws, we) = (close(w.start), close(w.end()));
                    w.start = ws;
                    w.duration = (we - ws).max(Tick::ZERO);
                }
                c.clamp_words();
                out.push(c);
            }
        }
    }
    tr.captions = out;
    tr.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::CaptionFormat;

    fn seq_with(captions: &[(i64, i64, &str)]) -> (Sequence, TrackId) {
        let mut p = filmcraft_project::Project::new("t");
        let sid = p.new_sequence("s", Default::default(), 1, 1, None);
        let mut seq = p.sequence(sid).unwrap().clone();
        let tid = TrackId(900);
        let mut tr = CaptionTrack::new(tid, "Subtitle".into(), CaptionFormat::Subtitle);
        for (i, (s, d, t)) in captions.iter().enumerate() {
            tr.captions.push(Caption {
                id: ClipId(1000 + i as u64),
                start: Tick(*s),
                duration: Tick(*d),
                text: t.to_string(),
                speaker: None,
                cue_id: None,
                settings: String::new(),
                words: Vec::new(),
            });
        }
        seq.caption_tracks.push(tr);
        (seq, tid)
    }

    fn ctx(next: &mut u64) -> EditCtx<'_> {
        static NONE: fn(filmcraft_project::ItemId) -> Option<Tick> = |_| None;
        EditCtx { next_id: next, media_duration: &NONE, media_start: &|_| Tick::ZERO, min_duration: Tick(10) }
    }

    fn spans(seq: &Sequence, tid: TrackId) -> Vec<(i64, i64, String)> {
        seq.caption_track(tid).unwrap().captions.iter().map(|c| (c.start.0, c.end().0, c.text.clone())).collect()
    }

    #[test]
    fn add_clamps_to_next() {
        let (mut seq, tid) = seq_with(&[(100, 50, "b")]);
        let mut n = 1;
        let id = add_caption(&mut seq, tid, Tick(40), Tick(1000), "a", &mut ctx(&mut n)).unwrap();
        assert_eq!(seq.find_caption(id).unwrap().1.end(), Tick(100));
        assert!(add_caption(&mut seq, tid, Tick(120), Tick(10), "x", &mut ctx(&mut n)).is_err());
        assert!(add_caption(&mut seq, tid, Tick(95), Tick(10), "x", &mut ctx(&mut n)).is_err(), "would be shorter than min");
        assert!(seq.check().is_ok());
    }

    #[test]
    fn split_and_merge() {
        let (mut seq, tid) = seq_with(&[(0, 100, "hello"), (100, 50, "world")]);
        let mut n = 1;
        let r = split_caption(&mut seq, ClipId(1000), Tick(40), &mut ctx(&mut n)).unwrap();
        assert_eq!(spans(&seq, tid), vec![(0, 40, "hello".into()), (40, 100, "hello".into()), (100, 150, "world".into())]);
        assert!(split_caption(&mut seq, r, Tick(45), &mut ctx(&mut n)).is_err());
        let m = merge_captions(&mut seq, &[r, ClipId(1001)]).unwrap();
        assert_eq!(m, r);
        assert_eq!(spans(&seq, tid), vec![(0, 40, "hello".into()), (40, 150, "hello\nworld".into())]);
        // not adjacent
        let (mut seq, _) = seq_with(&[(0, 10, "a"), (20, 10, "b"), (40, 10, "c")]);
        assert!(merge_captions(&mut seq, &[ClipId(1000), ClipId(1002)]).is_err());
    }

    #[test]
    fn trim_and_move_clamp() {
        let (mut seq, tid) = seq_with(&[(0, 100, "a"), (150, 50, "b"), (300, 50, "c")]);
        let n = &mut 1;
        assert_eq!(trim_caption(&mut seq, ClipId(1001), Edge::In, Tick(-500), &ctx(n)).unwrap(), Tick(-50));
        assert_eq!(trim_caption(&mut seq, ClipId(1001), Edge::Out, Tick(500), &ctx(n)).unwrap(), Tick(100));
        assert_eq!(spans(&seq, tid)[1], (100, 300, "b".into()));
        assert_eq!(trim_caption(&mut seq, ClipId(1001), Edge::Out, Tick(-1000), &ctx(n)).unwrap(), Tick(-190));
        // move c left: stops at b's end (110)
        assert_eq!(move_captions(&mut seq, &[ClipId(1002)], Tick(-1000)).unwrap(), Tick(-190));
        assert_eq!(spans(&seq, tid)[2], (110, 160, "c".into()));
        // a cannot move right (b is flush against it); b and c move together freely
        assert_eq!(move_captions(&mut seq, &[ClipId(1000)], Tick(5)), Err(EditError::Nothing));
        assert_eq!(move_captions(&mut seq, &[ClipId(1001), ClipId(1002)], Tick(5)).unwrap(), Tick(5));
        assert_eq!(spans(&seq, tid)[1..], [(105, 115, "b".into()), (115, 165, "c".into())]);
        assert!(seq.check().is_ok());
    }

    #[test]
    fn delete_ripple_and_sync() {
        let (mut seq, tid) = seq_with(&[(0, 100, "a"), (100, 50, "b"), (200, 50, "c")]);
        delete_captions(&mut seq, &[ClipId(1001)], true).unwrap();
        assert_eq!(spans(&seq, tid), vec![(0, 100, "a".into()), (150, 200, "c".into())]);
        let mut n = 1;
        let tr = seq.caption_track_mut(tid).unwrap();
        insert_gap(tr, Tick(50), Tick(30), &mut ctx(&mut n));
        assert_eq!(spans(&seq, tid), vec![(0, 50, "a".into()), (80, 130, "a".into()), (180, 230, "c".into())]);
        let tr = seq.caption_track_mut(tid).unwrap();
        extract_range(tr, TimeRange::new(Tick(40), Tick(150)));
        assert_eq!(spans(&seq, tid), vec![(0, 40, "a".into()), (40, 80, "c".into())]);
    }

    use filmcraft_project::WordTiming;

    /// A caption 0..100 reading "one two\nthree four" with words at 0, 20, 50 and 70 (15 long).
    fn timed() -> (Sequence, TrackId) {
        let (mut seq, tid) = seq_with(&[(0, 100, "one two\nthree four"), (200, 50, "later")]);
        let c = seq.caption_track_mut(tid).unwrap().caption_mut(ClipId(1000)).unwrap();
        c.words = [(0, "one"), (20, "two"), (50, "three"), (70, "four")].iter().map(|(s, t)| WordTiming::new(Tick(*s), Tick(15), *t)).collect();
        assert!(c.words_match_text());
        (seq, tid)
    }

    fn words(seq: &Sequence, id: ClipId) -> Vec<(i64, i64, String)> {
        seq.find_caption(id).unwrap().1.words.iter().map(|w| (w.start.0, w.end().0, w.text.clone())).collect()
    }

    fn w(s: i64, e: i64, t: &str) -> (i64, i64, String) {
        (s, e, t.to_string())
    }

    #[test]
    fn split_divides_text_and_words_at_the_word_boundary() {
        let (mut seq, tid) = timed();
        let mut n = 1;
        let r = split_caption(&mut seq, ClipId(1000), Tick(60), &mut ctx(&mut n)).unwrap();
        assert_eq!(spans(&seq, tid)[..2], [(0, 60, "one two\nthree".into()), (60, 100, "four".into())]);
        assert_eq!(words(&seq, ClipId(1000)), [w(0, 15, "one"), w(20, 35, "two"), w(50, 60, "three")], "three is clipped at the cut");
        assert_eq!(words(&seq, r), [w(70, 85, "four")]);
        assert!(seq.find_caption(ClipId(1000)).unwrap().1.words_match_text());
        assert!(seq.find_caption(r).unwrap().1.words_match_text());
        // splitting between lines drops the line break from both halves
        let (mut seq, tid) = timed();
        split_caption(&mut seq, ClipId(1000), Tick(40), &mut ctx(&mut n)).unwrap();
        assert_eq!(spans(&seq, tid)[..2], [(0, 40, "one two".into()), (40, 100, "three four".into())]);
    }

    #[test]
    fn split_outside_the_words_keeps_text_and_clamps() {
        let (mut seq, tid) = timed();
        let mut n = 1;
        let r = split_caption(&mut seq, ClipId(1000), Tick(90), &mut ctx(&mut n)).unwrap();
        // every word starts before 90: both halves keep the whole text and all four words
        assert_eq!(spans(&seq, tid)[..2], [(0, 90, "one two\nthree four".into()), (90, 100, "one two\nthree four".into())]);
        assert_eq!(words(&seq, ClipId(1000)), [w(0, 15, "one"), w(20, 35, "two"), w(50, 65, "three"), w(70, 85, "four")]);
        assert_eq!(words(&seq, r), [w(90, 90, "one"), w(90, 90, "two"), w(90, 90, "three"), w(90, 90, "four")]);
        assert!(seq.find_caption(r).unwrap().1.words_match_text());
        // a caption without words splits exactly as before
        let (mut seq, tid) = seq_with(&[(0, 100, "plain text")]);
        split_caption(&mut seq, ClipId(1000), Tick(50), &mut ctx(&mut n)).unwrap();
        assert_eq!(spans(&seq, tid), [(0, 50, "plain text".into()), (50, 100, "plain text".into())]);
        assert!(seq.caption_track(tid).unwrap().captions.iter().all(|c| c.words.is_empty()));
    }

    #[test]
    fn merge_concatenates_words_or_clears_them() {
        let (mut seq, _) = timed();
        let mut n = 1;
        let r = split_caption(&mut seq, ClipId(1000), Tick(40), &mut ctx(&mut n)).unwrap();
        let m = merge_captions(&mut seq, &[ClipId(1000), r]).unwrap();
        let c = seq.find_caption(m).unwrap().1;
        assert_eq!(c.text, "one two\nthree four");
        assert_eq!(words(&seq, m), [w(0, 15, "one"), w(20, 35, "two"), w(50, 65, "three"), w(70, 85, "four")]);
        // merging with a caption that has no words clears them (they no longer spell the text)
        let (mut seq, tid) = timed();
        seq.caption_track_mut(tid).unwrap().caption_mut(ClipId(1001)).unwrap().start = Tick(100);
        let m = merge_captions(&mut seq, &[ClipId(1000), ClipId(1001)]).unwrap();
        assert_eq!(spans(&seq, tid), [(0, 150, "one two\nthree four\nlater".into())]);
        assert!(seq.find_caption(m).unwrap().1.words.is_empty());
    }

    #[test]
    fn text_edits_keep_words_only_while_they_match() {
        let (mut seq, _) = timed();
        // re-breaking lines or adding markup keeps the words
        set_caption(&mut seq, ClipId(1000), Some("one <i>two</i> three\r\nfour"), None).unwrap();
        assert_eq!(words(&seq, ClipId(1000)).len(), 4);
        // a speaker change alone keeps them
        set_caption(&mut seq, ClipId(1000), None, Some(Some("Ann"))).unwrap();
        assert_eq!(words(&seq, ClipId(1000)).len(), 4);
        // a changed word clears them
        set_caption(&mut seq, ClipId(1000), Some("one two three five"), None).unwrap();
        assert!(words(&seq, ClipId(1000)).is_empty());
    }

    #[test]
    fn set_times_moves_words_only_on_a_pure_move() {
        let (mut seq, _) = timed();
        let n = &mut 1;
        // same length, later: a pure move, the words move with it
        set_caption_times(&mut seq, ClipId(1000), Tick(1000), Tick(1100), &ctx(n)).unwrap();
        assert_eq!(words(&seq, ClipId(1000)), [w(1000, 1015, "one"), w(1020, 1035, "two"), w(1050, 1065, "three"), w(1070, 1085, "four")]);
        // only the in point changes: like Trim In, the words keep their times and clamp
        set_caption_times(&mut seq, ClipId(1000), Tick(1025), Tick(1100), &ctx(n)).unwrap();
        assert_eq!(words(&seq, ClipId(1000)), [w(1025, 1025, "one"), w(1025, 1035, "two"), w(1050, 1065, "three"), w(1070, 1085, "four")]);
        // both edges change by different amounts: also a trim
        set_caption_times(&mut seq, ClipId(1000), Tick(1030), Tick(1060), &ctx(n)).unwrap();
        assert_eq!(words(&seq, ClipId(1000)), [w(1030, 1030, "one"), w(1030, 1035, "two"), w(1050, 1060, "three"), w(1060, 1060, "four")]);
        // the same as trimming the same edges
        let (mut by_trim, _) = timed();
        trim_caption(&mut by_trim, ClipId(1000), Edge::In, Tick(25), &ctx(n)).unwrap();
        let (mut by_times, _) = timed();
        set_caption_times(&mut by_times, ClipId(1000), Tick(25), Tick(100), &ctx(n)).unwrap();
        assert_eq!(words(&by_trim, ClipId(1000)), words(&by_times, ClipId(1000)));
    }

    #[test]
    fn trims_clamp_and_moves_shift_words() {
        let (mut seq, _) = timed();
        let n = &mut 1;
        trim_caption(&mut seq, ClipId(1000), Edge::In, Tick(25), &ctx(n)).unwrap();
        assert_eq!(words(&seq, ClipId(1000)), [w(25, 25, "one"), w(25, 35, "two"), w(50, 65, "three"), w(70, 85, "four")]);
        trim_caption(&mut seq, ClipId(1000), Edge::Out, Tick(-20), &ctx(n)).unwrap();
        assert_eq!(words(&seq, ClipId(1000)), [w(25, 25, "one"), w(25, 35, "two"), w(50, 65, "three"), w(70, 80, "four")]);
        // trimming back out does not grow clamped words again (deterministic, no memory)
        trim_caption(&mut seq, ClipId(1000), Edge::In, Tick(-25), &ctx(n)).unwrap();
        assert_eq!(words(&seq, ClipId(1000))[0], w(25, 25, "one"));
        move_captions(&mut seq, &[ClipId(1000)], Tick(10)).unwrap();
        assert_eq!(words(&seq, ClipId(1000)), [w(35, 35, "one"), w(35, 45, "two"), w(60, 75, "three"), w(80, 90, "four")]);
    }

    #[test]
    fn ripple_insert_and_extract_carry_words() {
        let (mut seq, tid) = timed();
        let mut n = 1;
        let tr = seq.caption_track_mut(tid).unwrap();
        shift_from(tr, Tick(0), Tick(5));
        assert_eq!(words(&seq, ClipId(1000))[0], w(5, 20, "one"));
        // insert at 45 splits at the word boundary and pushes the right half 30 later
        let tr = seq.caption_track_mut(tid).unwrap();
        insert_gap(tr, Tick(45), Tick(30), &mut ctx(&mut n));
        assert_eq!(spans(&seq, tid)[..2], [(5, 45, "one two".into()), (75, 135, "three four".into())]);
        let right = seq.caption_track(tid).unwrap().captions[1].id;
        assert_eq!(words(&seq, right), [w(85, 100, "three"), w(105, 120, "four")]);
        // extracting through a caption collapses the removed words onto the cut
        let (mut seq, tid) = timed();
        let tr = seq.caption_track_mut(tid).unwrap();
        extract_range(tr, TimeRange::new(Tick(18), Tick(40)));
        assert_eq!(spans(&seq, tid)[0], (0, 60, "one two\nthree four".into()));
        assert_eq!(words(&seq, ClipId(1000)), [w(0, 15, "one"), w(18, 18, "two"), w(18, 25, "three"), w(30, 45, "four")]);
        assert_eq!(spans(&seq, tid)[1], (160, 210, "later".into()));
    }

    #[test]
    fn hostile_word_lists_do_not_panic() {
        let (mut seq, tid) = timed();
        let c = seq.caption_track_mut(tid).unwrap().caption_mut(ClipId(1000)).unwrap();
        c.words = vec![
            WordTiming::new(Tick(i64::MAX), Tick(i64::MAX), "one"),
            WordTiming::new(Tick(i64::MIN), Tick(-5), "two"),
            WordTiming::new(Tick(50), Tick(i64::MIN), "three"),
            WordTiming::new(Tick(10), Tick(5), "four"),
        ];
        let mut n = 1;
        let r = std::panic::catch_unwind(move || {
            let r = split_caption(&mut seq, ClipId(1000), Tick(30), &mut ctx(&mut n));
            let _ = move_captions(&mut seq, &[ClipId(1000)], Tick(i64::MAX));
            let tr = seq.caption_track_mut(tid).unwrap();
            extract_range(tr, TimeRange::new(Tick(0), Tick(i64::MAX / 8)));
            r.is_ok()
        });
        assert!(r.is_ok());
    }

    #[test]
    fn locked_refuses() {
        let (mut seq, tid) = seq_with(&[(0, 100, "a")]);
        seq.caption_track_mut(tid).unwrap().locked = true;
        assert_eq!(set_caption(&mut seq, ClipId(1000), Some("x"), None), Err(EditError::Locked));
        assert_eq!(delete_captions(&mut seq, &[ClipId(1000)], false), Err(EditError::Locked));
    }
}
