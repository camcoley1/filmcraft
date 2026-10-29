//! Deterministic tests of the transcript → sequence mapping and the text-based edits, on the
//! hand-made interview transcript (`tests/fixtures/interview.transcript.json`, 10 s, two speakers,
//! a 2 s pause after "show.", fillers "Um," and "uh", phrase "you know.").

#![allow(clippy::single_range_in_vec_init)] // word-index ranges are the data under test

use super::*;
use filmcraft_project::{Label, Project, SequenceSettings, TrackItem};
use filmcraft_time::FrameRate;

const R: FrameRate = FrameRate { num: 25, den: 1 };
const MEDIA: ItemId = ItemId(1);

fn s(x: f64) -> Tick {
    Tick((x * TICKS_PER_SECOND as f64).round() as i64)
}

fn fixture() -> Transcript {
    let mut t: Transcript = serde_json::from_str(include_str!("../../tests/fixtures/interview.transcript.json")).unwrap();
    t.normalize();
    t.check().unwrap();
    t
}

fn transcripts() -> Transcripts {
    let mut m = Transcripts::new();
    m.insert(MEDIA, Arc::new(fixture()));
    m
}

fn seq() -> Sequence {
    let mut p = Project::new("t");
    let id = p.new_sequence("s", SequenceSettings { frame_rate: R, ..Default::default() }, 1, 2, None);
    p.sequence(id).unwrap().clone()
}

fn clip(id: u64, start: f64, dur: f64, src_in: f64) -> TrackItem {
    TrackItem {
        id: ClipId(id),
        item: MEDIA,
        name: "interview".into(),
        label: Label::Iris,
        start: s(start),
        duration: s(dur),
        source_in: s(src_in),
        speed: 1.0,
        reverse: false,
        enabled: true,
        link: None,
        group: None,
        effects: vec![],
        markers: vec![],
        gain_db: 0.0,
        frame_hold: None,
        scale_to_frame: false,
        essential: None,
        multicam: None,
        time_interpolation: Default::default(),
        hold_filters: false,
        field_options: None,
        source_channels: Vec::new(),
        audio_stream: 0,
        graphic: None,
    }
}

fn texts(w: &[SeqWord]) -> Vec<&str> {
    w.iter().map(|w| w.text.as_str()).collect()
}

fn media_dur(_: ItemId) -> Option<Tick> {
    Some(s(10.0))
}

fn whole() -> Sequence {
    let mut q = seq();
    q.audio_tracks[0].items.push(clip(100, 0.0, 10.0, 0.0));
    q
}

#[test]
fn words_map_through_the_clip() {
    let mut q = seq();
    // media 3.5 s – 6.5 s at timeline 2 s
    q.audio_tracks[0].items.push(clip(100, 2.0, 3.0, 3.5));
    let w = sequence_words(&q, &transcripts());
    assert_eq!(texts(&w), ["Um,", "today", "we", "talk", "about", "rivers."]);
    assert_eq!(w[0].start, s(2.2));
    assert_eq!(w[0].end, s(2.5));
    assert_eq!(w[5].end, s(4.4));
    assert!(w.iter().all(|x| x.clip == ClipId(100) && x.item == MEDIA && x.track == 0));
    assert_eq!(w[0].index, 4);
    assert_eq!(w[0].speaker.as_deref(), Some("Speaker 1"));
}

#[test]
fn a_cut_word_is_kept_by_its_midpoint_and_clamped() {
    let mut q = seq();
    // cut inside "show." (1.20–1.70, midpoint 1.45): the clip starts at media 1.4
    q.audio_tracks[0].items.push(clip(100, 0.0, 1.0, 1.4));
    let w = sequence_words(&q, &transcripts());
    assert_eq!(texts(&w), ["show."]);
    assert_eq!(w[0].start, Tick::ZERO);
    assert_eq!(w[0].end, s(0.3));
}

#[test]
fn speed_scales_word_times() {
    let mut q = seq();
    let mut c = clip(100, 0.0, 5.0, 0.0);
    c.speed = 2.0;
    q.audio_tracks[0].items.push(c);
    let w = sequence_words(&q, &transcripts());
    assert_eq!(w.len(), 20);
    assert_eq!((w[3].start, w[3].end), (s(0.6), s(0.85)));
}

#[test]
fn duplicate_tracks_read_once_and_disabled_or_reversed_clips_are_silent() {
    let mut q = whole();
    q.audio_tracks[1].items.push(clip(101, 0.0, 10.0, 0.0));
    let w = sequence_words(&q, &transcripts());
    assert_eq!(w.len(), 20);
    assert!(w.iter().all(|x| x.track == 0));
    q.audio_tracks[0].items[0].enabled = false;
    let w = sequence_words(&q, &transcripts());
    assert_eq!(w.len(), 20);
    assert!(w.iter().all(|x| x.track == 1));
    q.audio_tracks[1].items[0].reverse = true;
    assert!(sequence_words(&q, &transcripts()).is_empty());
}

#[test]
fn two_clips_of_the_same_media_repeat_words_in_edit_order() {
    let mut q = seq();
    // "rivers." twice: media 5.3–5.9 at 0 s and again at 1 s
    q.audio_tracks[0].items.push(clip(100, 0.0, 1.0, 5.2));
    q.audio_tracks[0].items.push(clip(101, 1.0, 1.0, 5.2));
    let w = sequence_words(&q, &transcripts());
    assert_eq!(texts(&w), ["rivers.", "rivers."]);
    assert_eq!(w[1].start, s(1.1));
    assert_eq!(w[1].clip, ClipId(101));
}

#[test]
fn search_word_at_and_paragraphs() {
    let w = sequence_words(&whole(), &transcripts());
    assert_eq!(search(&w, "rivers"), vec![9..10, 17..18]);
    assert_eq!(search(&w, "YOU kn"), vec![18..20]);
    assert_eq!(search(&w, "talk about"), vec![7..9]);
    assert!(search(&w, "  ").is_empty());
    assert_eq!(word_at(&w, s(4.2)), Some(5));
    assert_eq!(word_at(&w, s(2.5)), None); // in the pause
    assert_eq!(word_at(&w, s(0.1)), None);
    // pause ≥ 1.5 s after "show.", speaker change before "Thanks"
    assert_eq!(paragraphs(&w, s(1.5)), vec![0..4, 4..10, 10..20]);
}

#[test]
fn word_ranges_snap_outward() {
    let w = sequence_words(&whole(), &transcripts());
    // "Welcome to the show." 0.50–1.70 → frames 12..43 (0.48–1.72)
    let r = word_range(&w, 3, 0, R).unwrap();
    assert_eq!((r.start, r.end()), (s(0.48), s(1.72)));
    let m = media_word_range(&fixture(), 4, 9, R).unwrap();
    assert_eq!((m.start, m.end()), (s(3.68), s(5.92)));
    assert!(word_range(&w, 0, 99, R).is_none());
}

#[test]
fn extract_text_closes_the_gap() {
    let mut q = whole();
    let w = sequence_words(&q, &transcripts());
    let r = word_range(&w, 0, 3, R).unwrap();
    let mut next = 1000;
    let mut ctx = EditCtx { next_id: &mut next, media_duration: &media_dur, media_start: &|_| Tick::ZERO, min_duration: R.frame_duration() };
    let tracks: Vec<TrackId> = q.all_tracks().map(|t| t.id).collect();
    crate::extract(&mut q, &tracks, r, &mut ctx);
    q.check().unwrap();
    let w2 = sequence_words(&q, &transcripts());
    assert_eq!(w2.len(), 16);
    assert_eq!(w2[0].text, "Um,");
    assert_eq!(w2[0].start, s(3.70) - r.duration);
}

#[test]
fn pauses_are_found_and_removed_in_one_pass() {
    let mut q = whole();
    let w = sequence_words(&q, &transcripts());
    // only the 2 s pause after "show." is ≥ 1 s; 0.1 s is kept on each side
    let p = find_pauses(&w, s(1.0), s(0.1), Tick::ZERO, R);
    assert_eq!(p.len(), 1);
    assert_eq!((p[0].start, p[0].end()), (s(1.80), s(3.60)));
    // 0.4 s threshold also finds 5.90→6.40 and 7.60→8.00
    let p2 = find_pauses(&w, s(0.4), s(0.1), Tick::ZERO, R);
    assert_eq!(p2.len(), 3);
    let mut next = 1000;
    let mut ctx = EditCtx { next_id: &mut next, media_duration: &media_dur, media_start: &|_| Tick::ZERO, min_duration: R.frame_duration() };
    let removed = ripple_delete_ranges(&mut q, p2.clone(), &mut ctx);
    q.check().unwrap();
    assert_eq!(removed, p2.iter().map(|r| r.duration).fold(Tick::ZERO, |a, b| a + b));
    let w2 = sequence_words(&q, &transcripts());
    assert_eq!(texts(&w2), texts(&w));
    // the long pause is now 0.2 s
    assert_eq!(w2[4].start - w2[3].end, s(0.2));
    assert_eq!(w2.last().unwrap().end, s(10.0) - removed);
    assert!(find_pauses(&w2, s(0.4), s(0.1), Tick::ZERO, R).is_empty());
}

#[test]
fn fillers_are_found_with_phrases_and_removed() {
    let mut q = whole();
    let w = sequence_words(&q, &transcripts());
    let defaults: Vec<String> = DEFAULT_FILLERS.iter().map(|x| x.to_string()).collect();
    let hits = find_fillers(&w, &defaults);
    assert_eq!(hits, vec![4..5, 15..16]);
    let mut more = defaults.clone();
    more.push("You know".into());
    more.push("you".into());
    assert_eq!(find_fillers(&w, &more), vec![4..5, 15..16, 18..20]);
    let ranges = filler_ranges(&w, &hits, Tick::ZERO, R);
    // "Um," 3.70–4.00 → nearest frames 3.68–4.00; "uh" 8.20–8.50 → 8.20–8.48
    assert_eq!(ranges.iter().map(|r| (r.start, r.end())).collect::<Vec<_>>(), vec![(s(3.68), s(4.00)), (s(8.20), s(8.48))]);
    let mut next = 1000;
    let mut ctx = EditCtx { next_id: &mut next, media_duration: &media_dur, media_start: &|_| Tick::ZERO, min_duration: R.frame_duration() };
    ripple_delete_ranges(&mut q, ranges, &mut ctx);
    q.check().unwrap();
    let w2 = sequence_words(&q, &transcripts());
    assert_eq!(w2.len(), 18);
    assert!(find_fillers(&w2, &defaults).is_empty());
    assert_eq!(q.audio_tracks[0].items.len(), 3);
}

#[test]
fn merge_ranges_joins_overlaps() {
    let r = |a: f64, b: f64| TimeRange::from_bounds(s(a), s(b));
    let m = merge_ranges(vec![r(5.0, 6.0), r(1.0, 2.0), r(1.5, 3.0), r(3.0, 4.0)]);
    assert_eq!(m, vec![r(1.0, 4.0), r(5.0, 6.0)]);
}

fn check_blocks(b: &[CaptionBlock], rules: &CaptionRules) {
    for (i, x) in b.iter().enumerate() {
        assert!(x.end > x.start, "{x:?}");
        assert_eq!(R.snap(x.start), x.start);
        assert_eq!(R.snap(x.end), x.end);
        let lines: Vec<&str> = x.text.lines().collect();
        assert!(lines.len() <= rules.lines, "{x:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= rules.max_chars), "{x:?}");
        if i > 0 {
            assert!(x.start >= b[i - 1].end + Tick(R.frame_duration().0 * rules.gap_frames), "{:?} overlaps {:?}", b[i - 1], x);
        }
    }
}

#[test]
fn captions_follow_line_length_speaker_and_pause_rules() {
    let w = sequence_words(&whole(), &transcripts());
    let rules = CaptionRules::default();
    let b = caption_blocks(&w, &rules, R);
    check_blocks(&b, &rules);
    assert_eq!(
        b.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
        ["Welcome to the show.", "Um, today we talk about rivers.", "Thanks for having me. I uh love rivers,\nyou know."]
    );
    assert_eq!((b[0].start, b[0].end), (s(0.48), s(1.72)));
    assert_eq!(b[2].speaker.as_deref(), Some("Speaker 2"));
    assert_eq!(b[2].words, 10..20);

    // single 20-character lines, two-frame gaps, 3 s minimum
    let tight = CaptionRules { max_chars: 20, lines: 1, gap_frames: 2, min_duration: s(3.0), ..Default::default() };
    let b = caption_blocks(&w, &tight, R);
    check_blocks(&b, &tight);
    assert!(b.len() >= 5);
    // the first block is extended to 3 s (room before "Um,")
    assert_eq!(b[0].end - b[0].start, s(3.0));
    // every word is shown exactly once, in order
    let all: Vec<usize> = b.iter().flat_map(|x| x.words.clone()).collect();
    assert_eq!(all, (0..20).collect::<Vec<_>>());
}

#[test]
fn captions_never_overlap_with_dense_words() {
    // 30 one-frame words back to back
    let words: Vec<SeqWord> = (0..30)
        .map(|i| SeqWord {
            text: format!("w{i}."),
            start: R.tick_of(i),
            end: R.tick_of(i + 1),
            clip: ClipId(1),
            item: MEDIA,
            index: i as usize,
            track: 0,
            speaker: if i % 3 == 0 { Some("A".into()) } else { Some("B".into()) },
            confidence: 1.0,
        })
        .collect();
    let rules = CaptionRules { gap_frames: 1, ..Default::default() };
    let b = caption_blocks(&words, &rules, R);
    check_blocks(&b, &rules);
}

#[test]
fn created_captions_carry_the_transcript_word_timings() {
    let w = sequence_words(&whole(), &transcripts());
    let rules = CaptionRules::default();
    let b = caption_blocks(&w, &rules, R);
    let mut next = 1000;
    let mut ctx = EditCtx { next_id: &mut next, media_duration: &media_dur, media_start: &|_| Tick::ZERO, min_duration: R.frame_duration() };
    let caps = blocks_to_captions(&b, &w, &mut ctx);
    assert_eq!(caps.len(), b.len());
    // every caption is word-timed, its words spell its text and lie inside it
    for (c, blk) in caps.iter().zip(&b) {
        assert!(c.words_match_text(), "{c:?}");
        assert_eq!(c.words.len(), blk.words.len());
        assert!(c.words.iter().all(|x| x.start >= c.start && x.end() <= c.end()), "{c:?}");
        // words keep their transcript times except where frame snapping clipped the caption
        for (x, sw) in c.words.iter().zip(&w[blk.words.clone()]) {
            assert_eq!(x.text, sw.text);
            assert_eq!(x.start, sw.start.clamp(c.start, c.end()));
            assert_eq!(x.end(), sw.end.clamp(c.start, c.end()));
        }
    }
    let all: Vec<&str> = caps.iter().flat_map(|c| c.words.iter().map(|x| x.text.as_str())).collect();
    assert_eq!(all, w.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), "every word once, in order");
    // a word range that does not fit the words (stale blocks) gives no timings, never a panic
    let mut stale = b.clone();
    stale[0].words = 5..999;
    let caps = blocks_to_captions(&stale, &w, &mut ctx);
    assert!(caps[0].words.is_empty());
    assert!(caps[1].words_match_text());
}

#[test]
fn transcript_words_with_spaces_or_no_text_keep_captions_animated() {
    // a recogniser token holding two words, and an empty token
    let word = |text: &str, a: f64, b: f64| SeqWord {
        text: text.into(),
        start: s(a),
        end: s(b),
        clip: ClipId(1),
        item: MEDIA,
        index: 0,
        track: 0,
        speaker: None,
        confidence: 1.0,
    };
    let w = vec![word("I", 0.0, 0.2), word("love", 0.2, 0.5), word("New York", 0.5, 1.1), word("", 1.1, 1.2), word("pizza.", 1.2, 1.7)];
    let b = caption_blocks(&w, &CaptionRules::default(), R);
    assert_eq!(b.len(), 1, "{b:?}");
    let mut next = 1000;
    let mut ctx = EditCtx { next_id: &mut next, media_duration: &media_dur, media_start: &|_| Tick::ZERO, min_duration: R.frame_duration() };
    let caps = blocks_to_captions(&b, &w, &mut ctx);
    let c = &caps[0];
    assert!(c.words_match_text(), "{c:?}");
    let got: Vec<(&str, Tick, Tick)> = c.words.iter().map(|x| (x.text.as_str(), x.start, x.end())).collect();
    assert_eq!(got, [("I", s(0.0), s(0.2)), ("love", s(0.2), s(0.5)), ("New", s(0.5), s(0.8)), ("York", s(0.8), s(1.1)), ("pizza.", s(1.2), s(1.7))]);
}

fn word(text: &str, start: f64, end: f64) -> SeqWord {
    SeqWord { text: text.into(), start: s(start), end: s(end), clip: ClipId(1), item: MEDIA, index: 0, track: 0, speaker: None, confidence: 1.0 }
}

#[test]
fn onset_guard_clamps_hostile_values() {
    assert_eq!(onset_guard(0), Tick::ZERO);
    assert_eq!(onset_guard(40), s(0.04));
    assert_eq!(onset_guard(MAX_ONSET_GUARD_MS), s(0.5));
    assert_eq!(onset_guard(u32::MAX), s(0.5));
}

#[test]
fn a_pause_cut_stays_the_guard_away_from_both_neighbours() {
    let w = [word("a", 0.0, 1.0), word("b", 3.0, 3.5)];
    // no guard and no keep: the naive cut is the whole gap, touching the onset of "b"
    let naive = find_pauses(&w, s(1.0), Tick::ZERO, Tick::ZERO, R);
    assert_eq!((naive[0].start, naive[0].end()), (s(1.0), s(3.0)));
    // 50 ms guard: starts at/after 1.05 (frame 1.08), ends at/before 2.95 (frame 2.92)
    let g = find_pauses(&w, s(1.0), Tick::ZERO, s(0.05), R);
    assert_eq!((g[0].start, g[0].end()), (s(1.08), s(2.92)));
    assert!(g[0].start >= w[0].end + s(0.05) && g[0].end() <= w[1].start - s(0.05));
    // a larger keep wins over a smaller guard
    let k = find_pauses(&w, s(1.0), s(0.2), s(0.05), R);
    assert_eq!((k[0].start, k[0].end()), (s(1.2), s(2.8)));
    // a pause only as long as the two guards leaves nothing to cut
    assert!(find_pauses(&w, s(1.0), Tick::ZERO, s(1.0), R).is_empty());
}

#[test]
fn a_filler_cut_never_clips_the_neighbouring_words() {
    // "um" touches both neighbours, as Whisper often times it
    let w = [word("so", 0.0, 0.5), word("um", 0.5, 0.8), word("then", 0.8, 1.2)];
    let naive = filler_ranges(&w, &[1..2], Tick::ZERO, R);
    assert_eq!((naive[0].start, naive[0].end()), (s(0.52), s(0.8)));
    let g = filler_ranges(&w, &[1..2], s(0.1), R);
    assert_eq!((g[0].start, g[0].end()), (s(0.6), s(0.68)));
    assert!(g[0].start >= w[0].end + s(0.1) && g[0].end() <= w[2].start - s(0.1));
    // a filler shorter than the two guards is kept whole
    assert!(filler_ranges(&w, &[1..2], s(0.2), R).is_empty());
    // with air around it the cut keeps the filler's own bounds
    let air = [word("so", 0.0, 0.4), word("um", 1.0, 1.4), word("then", 2.0, 2.4)];
    let r = filler_ranges(&air, &[1..2], s(0.04), R);
    assert_eq!((r[0].start, r[0].end()), (s(1.0), s(1.4)));
}

#[test]
fn overlapping_unsorted_or_out_of_range_words_do_not_panic() {
    let w = [word("a", 2.0, 1.0), word("b", 0.5, 3.0), word("um", 0.0, 0.0), word("c", 1.0, 1.0), word("d", -5.0, 9e3)];
    for ms in [0, 40, 500, u32::MAX] {
        let g = onset_guard(ms);
        for keep in [Tick::ZERO, s(0.1), Tick(i64::MAX), Tick(i64::MIN)] {
            for r in find_pauses(&w, Tick::ZERO, keep, g, R) {
                assert!(r.duration > Tick::ZERO);
            }
        }
        for hits in [vec![0..0], vec![2..3], vec![0..5], vec![3..9], vec![9..10], vec![std::ops::Range { start: 4, end: 2 }]] {
            for r in filler_ranges(&w, &hits, g, R) {
                assert!(r.duration > Tick::ZERO);
            }
        }
    }
    assert!(filler_ranges(&[], &[0..1], s(0.04), R).is_empty());
    assert!(find_pauses(&[], Tick::ZERO, Tick::ZERO, s(0.04), R).is_empty());
    // a negative guard is treated as none
    assert_eq!(find_pauses(&[word("a", 0.0, 1.0), word("b", 3.0, 4.0)], s(1.0), Tick::ZERO, Tick(-5), R).len(), 1);
}

#[test]
fn back_to_back_fillers_are_one_cut() {
    let w = [word("so", 0.5, 1.0), word("um", 1.0, 1.3), word("uh", 1.3, 1.6), word("then", 1.6, 2.0)];
    let g = s(0.04);
    let hits = find_fillers(&w, &["um".to_string(), "uh".to_string()]);
    assert_eq!(hits, [1..2, 2..3]);
    let r = filler_ranges(&w, &hits, g, R);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].start >= w[0].end + g && r[0].end() <= w[3].start - g, "{r:?}");
    assert_eq!((r[0].start, r[0].end()), (s(1.04), s(1.56)));
}

#[test]
fn words_overlapping_a_cut_from_another_track_are_protected() {
    // sorted by start: A [0,5] spans B [1,2] and C [4,6]
    let w = [word("a", 0.0, 5.0), word("b", 1.0, 2.0), word("c", 4.0, 6.0), word("d", 9.0, 10.0)];
    // the gap B..C lies inside A, so only the real silence 6..9 may be cut
    let p = find_pauses(&w, s(0.5), Tick::ZERO, s(0.04), R);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].start >= s(6.04) && p[0].end() <= s(8.96), "{p:?}");
    // a filler inside A never cuts into it
    let f = [word("a", 0.0, 5.0), word("um", 1.0, 2.0), word("c", 4.0, 6.0)];
    assert!(filler_ranges(&f, &[1..2], s(0.04), R).is_empty());
    // a filler sticking out past the long word keeps clear of it
    let f = [word("a", 0.0, 1.5), word("um", 1.0, 2.0), word("c", 3.0, 4.0)];
    let r = filler_ranges(&f, &[1..2], s(0.04), R);
    assert!(r[0].start >= s(1.54) && r[0].end() <= s(2.0), "{r:?}");
}

#[test]
fn an_upward_snap_never_lands_before_the_boundary_at_fractional_rates() {
    for rate in [FrameRate::new(30000, 1001), FrameRate::new(24000, 1001), FrameRate::new(60000, 1001), FrameRate::new(37516, 1000)] {
        let (a, b) = (s(1.0) + Tick(7), s(5.0) + Tick(11));
        let w = [SeqWord { end: a, ..word("a", 0.0, 0.0) }, SeqWord { start: b, end: b + s(1.0), ..word("b", 0.0, 0.0) }];
        for keep in [Tick::ZERO, Tick(12_345), s(0.1)] {
            let r = find_pauses(&w, Tick::ZERO, keep, Tick::ZERO, rate);
            assert_eq!(r.len(), 1);
            assert!(r[0].start >= a + keep, "{:?} vs {:?}", r[0].start, a + keep);
            assert_eq!(rate.snap(r[0].start), r[0].start, "on a frame boundary");
            assert!(r[0].end() <= b - keep);
            // the filler lower bound too
            let f = [SeqWord { end: a, ..word("a", 0.0, 0.0) }, SeqWord { start: a, end: b, ..word("um", 0.0, 0.0) }];
            for fr in filler_ranges(&f, &[1..2], keep, rate) {
                assert!(fr.start >= a + keep && rate.snap(fr.start) == fr.start);
            }
        }
    }
}

#[test]
fn phrase_fillers_must_be_contiguous() {
    let you_know = ["you know".to_string()];
    let ok = [word("you", 0.0, 0.3), word("know", 0.4, 0.7)];
    assert_eq!(find_fillers(&ok, &you_know), [0..2]);
    // a long gap between the words
    let gap = [word("you", 0.0, 0.3), word("know", 1.5, 1.8)];
    assert!(find_fillers(&gap, &you_know).is_empty());
    // different clips
    let mut cross = ok.clone();
    cross[1].clip = ClipId(2);
    assert!(find_fillers(&cross, &you_know).is_empty());
    // different speakers
    let mut spk = ok.clone();
    spk[0].speaker = Some("A".into());
    spk[1].speaker = Some("B".into());
    assert!(find_fillers(&spk, &you_know).is_empty());
    // a single word is unaffected by any of this
    assert_eq!(find_fillers(&cross, &["know".to_string()]), [1..2]);
}

/// Words of a nested sequence's audio sit where the nested clip plays them, through nonzero inner
/// and outer offsets.
#[test]
fn nested_sequence_words_are_mapped_through_both_offsets() {
    let mut p = Project::new("t");
    let settings = SequenceSettings { frame_rate: R, ..Default::default() };
    let inner_id = p.new_sequence("inner", settings.clone(), 1, 1, None);
    let outer_id = p.new_sequence("outer", settings, 1, 1, None);
    // inner: the interview from its 1 s on, placed at inner 2 s, 6 s long
    p.sequence_mut(inner_id).unwrap().audio_tracks[0].items.push(clip(1, 2.0, 6.0, 1.0));
    // outer: inner from its 3 s on, placed at outer 10 s, 2.5 s long
    let mut c = clip(2, 10.0, 2.5, 3.0);
    c.item = inner_id;
    p.sequence_mut(outer_id).unwrap().audio_tracks[0].items.push(c);
    let tr = transcripts();
    let inner = p.sequence(inner_id).unwrap();
    let outer = p.sequence(outer_id).unwrap();
    assert!(sequence_words(outer, &tr).is_empty(), "the plain mapping does not see through the nest");
    let words = sequence_words_nested(outer, &tr, &|id| p.sequence(id));
    assert!(!words.is_empty());
    let direct = sequence_words(inner, &tr);
    for w in &words {
        assert!(w.start >= s(10.0) && w.end <= s(12.5), "{w:?}");
        let src = direct.iter().find(|d| d.index == w.index).expect("the word comes from the inner sequence");
        // outer = 10 + (inner - 3), clamped to the nested window [3, 5.5]
        assert_eq!(w.start, s(10.0) + (src.start.max(s(3.0)) - s(3.0)));
        assert_eq!(w.item, MEDIA);
    }
    // a sequence that contains itself ends at the depth limit
    let mut p2 = Project::new("loop");
    let id = p2.new_sequence("loop", SequenceSettings { frame_rate: R, ..Default::default() }, 1, 1, None);
    let mut c = clip(3, 0.0, 5.0, 0.0);
    c.item = id;
    p2.sequence_mut(id).unwrap().audio_tracks[0].items.push(c);
    assert!(sequence_words_nested(p2.sequence(id).unwrap(), &tr, &|i| p2.sequence(i)).is_empty());
}

/// A project with an inner sequence holding the interview, and one outer sequence.
fn nest(inner_start: f64, inner_src: f64, inner_dur: f64) -> (Project, ItemId, ItemId) {
    let mut p = Project::new("t");
    let settings = SequenceSettings { frame_rate: R, ..Default::default() };
    let inner = p.new_sequence("inner", settings.clone(), 1, 1, None);
    let outer = p.new_sequence("outer", settings, 1, 3, None);
    p.sequence_mut(inner).unwrap().audio_tracks[0].items.push(clip(1, inner_start, inner_dur, inner_src));
    (p, inner, outer)
}

fn nested_clip(id: u64, inner: ItemId, start: f64, dur: f64, src_in: f64) -> TrackItem {
    let mut c = clip(id, start, dur, src_in);
    c.item = inner;
    c
}

#[test]
fn duplicate_nested_tracks_add_no_words_and_upper_tracks_win() {
    let (mut p, inner, outer) = nest(0.0, 0.0, 10.0);
    // the same nested audio on A1 and A2: read once
    for (t, id) in [(0, 10), (1, 11)] {
        p.sequence_mut(outer).unwrap().audio_tracks[t].items.push(nested_clip(id, inner, 0.0, 10.0, 0.0));
    }
    let tr = transcripts();
    let once = sequence_words_nested(p.sequence(outer).unwrap(), &tr, &|id| p.sequence(id));
    let single = sequence_words(p.sequence(inner).unwrap(), &tr);
    assert_eq!(once.len(), single.len());
    assert!(once.iter().all(|w| w.track == 0));
    // a transcribed direct clip above a nested one claims its range: the nested words under it go
    let (mut p, inner, outer) = nest(0.0, 0.0, 10.0);
    p.sequence_mut(outer).unwrap().audio_tracks[0].items.push(clip(10, 0.0, 10.0, 0.0));
    p.sequence_mut(outer).unwrap().audio_tracks[1].items.push(nested_clip(11, inner, 0.0, 10.0, 0.0));
    let words = sequence_words_nested(p.sequence(outer).unwrap(), &tr, &|id| p.sequence(id));
    assert_eq!(words.len(), single.len());
    assert!(words.iter().all(|w| w.track == 0), "only the upper track speaks");
}

#[test]
fn nested_speed_maps_start_and_end_exactly() {
    // inner is the interview at inner 0; the nest plays inner 2 s..6 s at double speed from outer 5 s
    let (mut p, inner, outer) = nest(0.0, 0.0, 10.0);
    let mut c = nested_clip(10, inner, 5.0, 2.0, 2.0);
    c.speed = 2.0;
    p.sequence_mut(outer).unwrap().audio_tracks[0].items.push(c);
    let tr = transcripts();
    let direct = sequence_words(p.sequence(inner).unwrap(), &tr);
    let words = sequence_words_nested(p.sequence(outer).unwrap(), &tr, &|id| p.sequence(id));
    assert!(!words.is_empty());
    for w in &words {
        let src = direct.iter().find(|d| d.index == w.index).unwrap();
        let want_a = s(5.0) + Tick(((src.start.max(s(2.0)) - s(2.0)).0 as f64 / 2.0).round() as i64);
        let want_b = s(5.0) + Tick(((src.end.min(s(6.0)) - s(2.0)).0 as f64 / 2.0).round() as i64);
        assert_eq!((w.start, w.end), (want_a, want_b.max(want_a)), "word {}", w.index);
        assert!(w.start >= s(5.0) && w.end <= s(7.0));
    }
}

#[test]
fn the_word_budget_is_counted_before_words_are_collected() {
    let tr = transcripts();
    let (mut p, inner, outer) = nest(0.0, 0.0, 10.0);
    p.sequence_mut(outer).unwrap().audio_tracks[0].items.push(nested_clip(10, inner, 0.0, 10.0, 0.0));
    p.sequence_mut(outer).unwrap().audio_tracks[1].items.push(clip(11, 0.0, 10.0, 0.0));
    let all = sequence_words(p.sequence(inner).unwrap(), &tr).len();
    assert!(all > 5);
    for cap in [0, 1, 5, all - 1] {
        let direct = sequence_words_budget(p.sequence(inner).unwrap(), &tr, None, cap);
        assert_eq!(direct.len(), cap, "direct words stop at the cap");
        let find = |id: ItemId| p.sequence(id);
        let words = sequence_words_budget(p.sequence(outer).unwrap(), &tr, Some(&find), cap);
        assert!(words.len() <= cap, "nested words stop at the cap too: {} > {cap}", words.len());
    }
}

#[test]
fn fan_out_of_nested_sequences_is_bounded() {
    // four levels, 12 references to the next level in each: 12^4 reads without a bound
    let mut p = Project::new("fan");
    let settings = SequenceSettings { frame_rate: R, ..Default::default() };
    let ids: Vec<ItemId> = (0..5).map(|i| p.new_sequence(&format!("s{i}"), settings.clone(), 1, 12, None)).collect();
    p.sequence_mut(ids[4]).unwrap().audio_tracks[0].items.push(clip(1000, 0.0, 10.0, 0.0));
    for level in 0..4 {
        for t in 0..12 {
            p.sequence_mut(ids[level]).unwrap().audio_tracks[t].items.push(nested_clip(100 * level as u64 + t as u64 + 1, ids[level + 1], 0.0, 10.0, 0.0));
        }
    }
    let tr = transcripts();
    let start = std::time::Instant::now();
    let words = sequence_words_nested(p.sequence(ids[0]).unwrap(), &tr, &|id| p.sequence(id));
    assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
    assert!(!words.is_empty());
}
