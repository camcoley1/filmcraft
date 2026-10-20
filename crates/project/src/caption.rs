//! Caption tracks.
//!
//! Premiere keeps captions in their own track area above the video tracks. A [`CaptionTrack`] has a
//! format (Subtitle, CEA-608, CEA-708, Teletext), a track style (font, size, colour, background box,
//! position) and a list of [`Caption`] blocks with exact in/out times in sequence ticks. Captions on
//! one track never overlap (like track items); [`CaptionTrack::check`] verifies it.
//!
//! Caption text is kept as written (lines separated by `\n`, inline `<i>`/`<b>`/`<u>` tags and
//! WebVTT cue settings preserved) so files round-trip; renderers strip markup for display.

use filmcraft_time::{Tick, TimeRange};
use serde::{Deserialize, Serialize};

use crate::{ClipId, TrackId};

/// Caption stream format of a track (Premiere's "Caption Format" choices).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionFormat {
    /// Open/sidecar subtitles (SRT, WebVTT).
    #[default]
    Subtitle,
    /// CEA-608 (line 21) closed captions.
    Cea608,
    /// CEA-708 (DTVCC) closed captions.
    Cea708,
    /// EBU Teletext subtitles.
    Teletext,
}

impl CaptionFormat {
    pub const ALL: [CaptionFormat; 4] = [CaptionFormat::Subtitle, CaptionFormat::Cea608, CaptionFormat::Cea708, CaptionFormat::Teletext];
    pub fn label(self) -> &'static str {
        match self {
            CaptionFormat::Subtitle => "Subtitle",
            CaptionFormat::Cea608 => "CEA-608",
            CaptionFormat::Cea708 => "CEA-708",
            CaptionFormat::Teletext => "Teletext",
        }
    }
    pub fn from_name(s: &str) -> Option<CaptionFormat> {
        let n = s.to_ascii_lowercase().replace(['-', ' ', '_'], "");
        Self::ALL.iter().copied().find(|f| f.label().to_ascii_lowercase().replace(['-', ' '], "") == n)
    }
    /// Characters per line limit used when wrapping (608 is fixed at 32 columns).
    pub fn max_columns(self) -> Option<usize> {
        match self {
            CaptionFormat::Cea608 => Some(32),
            CaptionFormat::Teletext => Some(40),
            CaptionFormat::Cea708 => Some(42),
            CaptionFormat::Subtitle => None,
        }
    }
}

/// Horizontal text alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionAlign {
    Left,
    #[default]
    Center,
    Right,
}

/// Vertical placement of the caption block in the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionAnchor {
    Top,
    Middle,
    #[default]
    Bottom,
}

/// Track-level caption style (Premiere's caption track style / Essential Graphics text settings).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptionStyle {
    /// Font family name (bundled, craft-fonts or system); unknown or empty names fall back to Inter.
    pub font: String,
    /// Font size in pixels for a 1080-line frame (scaled with the frame height).
    pub size: f32,
    /// Text colour, sRGB + alpha.
    pub color: [u8; 4],
    /// Draw a background box behind each line.
    pub background: bool,
    /// Background box colour, sRGB + alpha.
    pub background_color: [u8; 4],
    pub align: CaptionAlign,
    pub anchor: CaptionAnchor,
    /// Distance from the anchored frame edge, as a fraction of the frame height.
    pub margin: f32,
    /// Line height as a multiple of the font size.
    pub line_spacing: f32,
    /// Text outline width in pixels at 1080 lines (0 = none).
    pub outline: f32,
    pub outline_color: [u8; 4],
}

impl Default for CaptionStyle {
    fn default() -> Self {
        Self {
            font: "Inter".into(),
            size: 54.0,
            color: [255, 255, 255, 255],
            background: true,
            background_color: [0, 0, 0, 191],
            align: CaptionAlign::Center,
            anchor: CaptionAnchor::Bottom,
            margin: 0.08,
            line_spacing: 1.25,
            outline: 0.0,
            outline_color: [0, 0, 0, 255],
        }
    }
}

/// One caption block on a caption track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Caption {
    pub id: ClipId,
    /// Timeline start (sequence ticks).
    pub start: Tick,
    pub duration: Tick,
    /// Caption text; lines separated by `\n`.
    pub text: String,
    /// Speaker name (WebVTT `<v Name>` voice span).
    #[serde(default)]
    pub speaker: Option<String>,
    /// WebVTT cue identifier, kept for round trips.
    #[serde(default)]
    pub cue_id: Option<String>,
    /// WebVTT cue settings (`line:90% align:start`…), kept verbatim.
    #[serde(default)]
    pub settings: String,
    /// Per-word timing for animated burn-in (karaoke, word pop, boxed word), in sequence ticks.
    /// Empty for captions typed by hand or imported from files; filled by Create Captions from
    /// the transcript. The words always spell the caption's text (see [`Caption::words_match_text`]);
    /// edits keep them inside the caption (see the `filmcraft-edit` caption functions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<WordTiming>,
}

/// One timed word of a caption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WordTiming {
    /// Sequence time the word is spoken (same time base as the caption).
    pub start: Tick,
    pub duration: Tick,
    /// The word as shown (one whitespace-free token of the caption text).
    pub text: String,
}

impl WordTiming {
    pub fn new(start: Tick, duration: Tick, text: impl Into<String>) -> Self {
        Self { start, duration, text: text.into() }
    }
    /// End time; a negative duration (corrupt file) counts as zero.
    pub fn end(&self) -> Tick {
        self.start + self.duration.max(Tick::ZERO)
    }
}

/// The whitespace-separated words of caption text as drawn (markup removed). Burn-in wraps lines
/// only at spaces, so these are also the words of the laid-out lines, in order.
pub fn caption_words(text: &str) -> Vec<String> {
    plain_text(text).split_whitespace().map(str::to_string).collect()
}

impl Caption {
    pub fn end(&self) -> Tick {
        self.start + self.duration
    }
    pub fn range(&self) -> TimeRange {
        TimeRange::new(self.start, self.duration)
    }
    /// The caption's text with markup removed and entities decoded (what is drawn).
    pub fn plain_lines(&self) -> Vec<String> {
        plain_text(&self.text).lines().map(str::to_string).collect()
    }
    /// True when `words` is non-empty and spells the text word for word (renderers animate only
    /// then; anything else draws the caption statically).
    pub fn words_match_text(&self) -> bool {
        !self.words.is_empty() && {
            let tokens = caption_words(&self.text);
            tokens.len() == self.words.len() && tokens.iter().zip(&self.words).all(|(a, w)| a == w.text.trim())
        }
    }
    /// Clear `words` when they no longer spell the text (after a text edit or a merge).
    pub fn reconcile_words(&mut self) {
        if !self.words.is_empty() && !self.words_match_text() {
            self.words.clear();
        }
    }
    /// Move the caption by `d`; its words move with it.
    pub fn shift(&mut self, d: Tick) {
        self.start += d;
        for w in &mut self.words {
            w.start += d;
        }
    }
    /// Clip every word into `[start, end]`. Words are never dropped (the text still shows them):
    /// a word left outside the caption becomes zero-length at the nearest edge.
    pub fn clamp_words(&mut self) {
        let (a, b) = (self.start, self.end().max(self.start));
        for w in &mut self.words {
            let s = w.start.clamp(a, b);
            let e = w.end().clamp(a, b).max(s);
            w.start = s;
            w.duration = e - s;
        }
    }
    /// Split the caption's words at `t` for a split edit. When the words spell the text and fall
    /// on both sides of `t`, returns the right half's text and words and leaves the left half's
    /// in `self` (the text is cut at that word boundary, line breaks kept). Otherwise returns `None`
    /// and changes nothing: both halves then keep the whole text and every word, clamped into
    /// each half by the caller.
    pub fn split_words_at(&mut self, t: Tick) -> Option<(String, Vec<WordTiming>)> {
        if !self.words_match_text() {
            return None;
        }
        // words are in text order; a corrupt, unsorted list is not split
        let k = self.words.iter().position(|w| w.start >= t)?;
        if k == 0 || self.words.iter().skip(k).any(|w| w.start < t) {
            return None;
        }
        let plain = plain_text(&self.text);
        let cut = token_offset(&plain, k)?;
        let (left, right) = (plain.get(..cut)?.trim_end().to_string(), plain.get(cut..)?.trim_start().to_string());
        let right_words = self.words.split_off(k);
        self.text = left;
        Some((right, right_words))
    }
}

/// Byte offset of the start of the `k`-th whitespace-separated token of `s`.
fn token_offset(s: &str, k: usize) -> Option<usize> {
    let mut n = 0;
    let mut in_word = false;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            in_word = false;
        } else if !in_word {
            in_word = true;
            if n == k {
                return Some(i);
            }
            n += 1;
        }
    }
    None
}

/// A caption track (shown in the caption area above the video tracks).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionTrack {
    pub id: TrackId,
    pub name: String,
    pub format: CaptionFormat,
    /// BCP-47 language tag (`en`, `fr-CA`…).
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub style: CaptionStyle,
    /// Track output (eye): shown in the Program monitor and burned in on export.
    pub enabled: bool,
    pub locked: bool,
    /// Follows inserts / extracts on other tracks.
    #[serde(default = "yes")]
    pub sync_lock: bool,
    /// Captions sorted by start, non-overlapping.
    pub captions: Vec<Caption>,
    /// WebVTT header blocks (STYLE / REGION / NOTE) kept for round trips.
    #[serde(default)]
    pub vtt_blocks: Vec<String>,
}

fn yes() -> bool {
    true
}

impl CaptionTrack {
    pub fn new(id: TrackId, name: String, format: CaptionFormat) -> Self {
        Self {
            id,
            name,
            format,
            language: "en".into(),
            style: CaptionStyle::default(),
            enabled: true,
            locked: false,
            sync_lock: true,
            captions: Vec::new(),
            vtt_blocks: Vec::new(),
        }
    }
    pub fn end(&self) -> Tick {
        self.captions.iter().map(Caption::end).max().unwrap_or(Tick::ZERO)
    }
    /// Caption covering `t`.
    pub fn caption_at(&self, t: Tick) -> Option<&Caption> {
        let idx = self.captions.partition_point(|c| c.start <= t);
        idx.checked_sub(1).map(|i| &self.captions[i]).filter(|c| t < c.end())
    }
    pub fn caption(&self, id: ClipId) -> Option<&Caption> {
        self.captions.iter().find(|c| c.id == id)
    }
    pub fn caption_mut(&mut self, id: ClipId) -> Option<&mut Caption> {
        self.captions.iter_mut().find(|c| c.id == id)
    }
    pub fn sort(&mut self) {
        self.captions.sort_by_key(|c| c.start);
    }
    /// Invariant check: captions sorted, positive duration, no overlaps.
    pub fn check(&self) -> Result<(), String> {
        self.check_bounds()?;
        for c in &self.captions {
            if c.duration.0 <= 0 {
                return Err(format!("{}: caption {:?} has non-positive duration", self.name, c.id));
            }
        }
        for w in self.captions.windows(2) {
            if w[0].end() > w[1].start {
                return Err(format!("{}: captions {:?} and {:?} overlap", self.name, w[0].id, w[1].id));
            }
        }
        Ok(())
    }
    /// Only the time bounds that keep caption arithmetic from overflowing (see `Track::check_bounds`).
    pub fn check_bounds(&self) -> Result<(), String> {
        for c in &self.captions {
            if !crate::bounded_time_range(c.start, c.duration) {
                return Err(format!("{}: caption {:?} is outside supported time bounds", self.name, c.id));
            }
        }
        Ok(())
    }
}

/// Strip `<…>` tags and decode the common HTML/WebVTT entities.
pub fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // a tag runs to the next '>' (if none, keep the '<' literally)
                let rest: String = chars.clone().collect();
                if let Some(end) = rest.find('>') {
                    for _ in 0..rest[..=end].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push('<');
                }
            }
            '{' if chars.peek() == Some(&'\\') => {
                // SSA-style override blocks some SRT files carry: {\an8}
                let rest: String = chars.clone().collect();
                if let Some(end) = rest.find('}') {
                    for _ in 0..rest[..=end].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push('{');
                }
            }
            '&' => {
                let rest: String = chars.clone().take(8).collect();
                let ents = [
                    ("amp;", '&'),
                    ("lt;", '<'),
                    ("gt;", '>'),
                    ("nbsp;", '\u{a0}'),
                    ("quot;", '"'),
                    ("apos;", '\''),
                    ("lrm;", '\u{200e}'),
                    ("rlm;", '\u{200f}'),
                ];
                if let Some((name, ch)) = ents.iter().find(|(n, _)| rest.starts_with(n)) {
                    for _ in 0..name.len() {
                        chars.next();
                    }
                    out.push(*ch);
                } else {
                    out.push('&');
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain() {
        assert_eq!(plain_text("<i>Hello</i> &amp; <b>bye</b>"), "Hello & bye");
        assert_eq!(plain_text("{\\an8}Top"), "Top");
        assert_eq!(plain_text("a < b"), "a < b");
        assert_eq!(plain_text("<v Bob>Hi</v>"), "Hi");
    }

    #[test]
    fn caption_at() {
        let mut t = CaptionTrack::new(TrackId(1), "Subtitle".into(), CaptionFormat::Subtitle);
        for (i, s) in [0i64, 100, 300].iter().enumerate() {
            t.captions.push(Caption {
                id: ClipId(i as u64),
                start: Tick(*s),
                duration: Tick(50),
                text: format!("c{i}"),
                speaker: None,
                cue_id: None,
                settings: String::new(),
                words: Vec::new(),
            });
        }
        assert_eq!(t.caption_at(Tick(120)).unwrap().text, "c1");
        assert!(t.caption_at(Tick(160)).is_none());
        assert!(t.check().is_ok());
        assert_eq!(CaptionFormat::from_name("cea608"), Some(CaptionFormat::Cea608));
        assert_eq!(CaptionFormat::from_name("CEA-708"), Some(CaptionFormat::Cea708));
    }

    fn timed_caption() -> Caption {
        Caption {
            id: ClipId(1),
            start: Tick(0),
            duration: Tick(100),
            text: "Hello <i>big</i>\nworld".into(),
            speaker: None,
            cue_id: None,
            settings: String::new(),
            words: vec![WordTiming::new(Tick(0), Tick(20), "Hello"), WordTiming::new(Tick(30), Tick(20), "big"), WordTiming::new(Tick(60), Tick(30), "world")],
        }
    }

    #[test]
    fn captions_without_words_load_and_save_as_before() {
        // a caption saved before word timing existed
        let old = r#"{"id":1,"start":0,"duration":100,"text":"Hi","speaker":null,"cue_id":null,"settings":""}"#;
        let c: Caption = serde_json::from_str(old).unwrap();
        assert!(c.words.is_empty());
        // and it saves byte for byte as before (no `words` key)
        assert_eq!(serde_json::to_string(&c).unwrap(), old);
        // a style saved before animations existed loads with the defaults for the new fields
        let st: CaptionStyle = serde_json::from_str(r#"{"font":"Inter","size":40.0}"#).unwrap();
        assert_eq!(st, CaptionStyle { size: 40.0, ..Default::default() });
    }

    #[test]
    fn words_round_trip_and_match_the_text() {
        let c = timed_caption();
        let back: Caption = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert!(c.words_match_text());
        assert_eq!(caption_words(&c.text), ["Hello", "big", "world"]);
        let mut d = c.clone();
        d.text = "Hello big".into();
        assert!(!d.words_match_text());
        d.reconcile_words();
        assert!(d.words.is_empty());
        let mut e = c.clone();
        e.text = "Hello\nbig   world".into();
        e.reconcile_words();
        assert_eq!(e.words.len(), 3);
    }

    #[test]
    fn split_words_cuts_text_at_the_word_boundary() {
        let mut c = timed_caption();
        let (right, words) = c.split_words_at(Tick(55)).unwrap();
        assert_eq!((c.text.as_str(), right.as_str()), ("Hello big", "world"));
        assert_eq!(words.len(), 1);
        assert_eq!(c.words.len(), 2);
        // no word on one side, or no words at all: nothing to split
        let mut c = timed_caption();
        assert!(c.split_words_at(Tick(0)).is_none());
        assert!(c.split_words_at(Tick(95)).is_none());
        assert_eq!(c, timed_caption());
        c.words.swap(0, 2);
        assert!(c.split_words_at(Tick(55)).is_none(), "words that do not spell the text are not split");
    }

    #[test]
    fn clamp_and_shift_never_overflow() {
        let mut c = timed_caption();
        c.words.push(WordTiming::new(Tick(i64::MAX), Tick(i64::MAX), "x"));
        c.words.push(WordTiming::new(Tick(i64::MIN), Tick(-7), "y"));
        let r = std::panic::catch_unwind(move || {
            c.shift(Tick(i64::MAX));
            c.shift(Tick(i64::MIN));
            c.clamp_words();
            c.words.iter().all(|w| w.duration >= Tick::ZERO && w.start >= c.start && w.end() <= c.end())
        });
        assert_eq!(r.ok(), Some(true));
    }

    #[test]
    fn corrupt_caption_times_do_not_overflow_the_overlap_check() {
        let mut track = CaptionTrack::new(TrackId(1), "Subtitles".into(), CaptionFormat::Subtitle);
        for (id, start) in [(1, i64::MAX), (2, 10)] {
            track.captions.push(Caption {
                id: ClipId(id),
                start: Tick(start),
                duration: Tick(50),
                text: String::new(),
                speaker: None,
                cue_id: None,
                settings: String::new(),
                words: Vec::new(),
            });
        }
        let result = std::panic::catch_unwind(|| track.check());
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }
}
