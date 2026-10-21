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

/// Per-word caption animation for burn-in. It needs word timings ([`Caption::words`]); captions
/// without them draw statically whatever the animation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum CaptionAnimation {
    /// Static captions (as before animations existed).
    #[default]
    None,
    /// Karaoke: words at or before the playhead take `highlight_color`, the rest the text colour.
    KaraokeHighlight { highlight_color: [u8; 4] },
    /// The word being spoken is drawn `scale` times larger around its centre and baseline.
    WordPop { scale: f32 },
    /// A filled box of `box_color` behind the word being spoken, `pad` pixels (at 1080 lines)
    /// wider than the word on each side.
    Boxed { box_color: [u8; 4], pad: f32 },
}

impl CaptionAnimation {
    pub const DEFAULT_HIGHLIGHT: [u8; 4] = [255, 214, 10, 255];
    pub const DEFAULT_POP_SCALE: f32 = 1.25;
    pub const DEFAULT_BOX: [u8; 4] = [229, 57, 53, 255];
    pub const DEFAULT_BOX_PAD: f32 = 10.0;
    /// Every preset with its default parameters, in menu order.
    pub fn presets() -> [CaptionAnimation; 4] {
        [
            CaptionAnimation::None,
            CaptionAnimation::KaraokeHighlight { highlight_color: Self::DEFAULT_HIGHLIGHT },
            CaptionAnimation::WordPop { scale: Self::DEFAULT_POP_SCALE },
            CaptionAnimation::Boxed { box_color: Self::DEFAULT_BOX, pad: Self::DEFAULT_BOX_PAD },
        ]
    }
    pub fn is_none(&self) -> bool {
        matches!(self, CaptionAnimation::None)
    }
    /// The command / control-channel name (`captions.setStyle` `animation`).
    pub fn name(self) -> &'static str {
        match self {
            CaptionAnimation::None => "none",
            CaptionAnimation::KaraokeHighlight { .. } => "karaoke",
            CaptionAnimation::WordPop { .. } => "pop",
            CaptionAnimation::Boxed { .. } => "boxed",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            CaptionAnimation::None => "Static",
            CaptionAnimation::KaraokeHighlight { .. } => "Karaoke",
            CaptionAnimation::WordPop { .. } => "Word pop",
            CaptionAnimation::Boxed { .. } => "Boxed word",
        }
    }
    /// The preset called `name` with default parameters.
    pub fn from_name(name: &str) -> Option<CaptionAnimation> {
        Self::presets().into_iter().find(|a| a.name().eq_ignore_ascii_case(name.trim()))
    }
}

/// An unknown or malformed animation (a newer or hand-edited project) loads as static captions
/// instead of failing the whole project.
fn lenient_animation<'de, D: serde::Deserializer<'de>>(d: D) -> Result<CaptionAnimation, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

/// Margin (fraction of the frame height) that keeps a caption anchored at `anchor` clear of the
/// buttons and text that short-video apps lay over a 9:16 picture. A guide value for the
/// `captions.setStyle` `safeArea` preset, written into the ordinary `anchor` and `margin` fields.
/// `None` for the middle anchor, which ignores the margin (the preset leaves it unchanged).
pub fn vertical_safe_margin(anchor: CaptionAnchor) -> Option<f32> {
    match anchor {
        CaptionAnchor::Top => Some(0.12),
        // centred captions ignore the margin, so the middle preset leaves it as it is
        CaptionAnchor::Middle => None,
        CaptionAnchor::Bottom => Some(0.22),
    }
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
    /// Per-word animation for burn-in (static by default).
    #[serde(deserialize_with = "lenient_animation", skip_serializing_if = "CaptionAnimation::is_none")]
    pub animation: CaptionAnimation,
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
            animation: CaptionAnimation::None,
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
    /// in `self`. The raw text is cut at that word boundary with its markup kept: line breaks,
    /// entities, a leading `{\an8}`-style override on both halves, and inline tags open across
    /// the cut closed on the left and reopened on the right. Otherwise returns `None`
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
        let (left, right) = split_raw_at_word(&self.text, k)?;
        // both halves must still spell their words, or the split falls back to the whole text
        let (lw, rw) = self.words.split_at(k);
        let spells = |text: &str, ws: &[WordTiming]| {
            let tokens = caption_words(text);
            tokens.len() == ws.len() && tokens.iter().zip(ws).all(|(a, w)| a == w.text.trim())
        };
        if !spells(&left, lw) || !spells(&right, rw) {
            return None;
        }
        let right_words = self.words.split_off(k);
        self.text = left;
        Some((right, right_words))
    }
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

/// One piece of raw caption text.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Piece<'a> {
    /// A character that is drawn (an entity decodes to one).
    Char(char),
    /// An inline `<...>` tag; the text between the angle brackets.
    Tag(&'a str),
    /// An SSA override block (`{\an8}`) or a carriage return: markup that is not drawn.
    Hidden,
}

const ENTITIES: [(&str, char); 8] =
    [("amp;", '&'), ("lt;", '<'), ("gt;", '>'), ("nbsp;", '\u{a0}'), ("quot;", '"'), ("apos;", '\''), ("lrm;", '\u{200e}'), ("rlm;", '\u{200f}')];

/// Raw caption text as pieces with their byte ranges, by the rules of [`plain_text`]: a `<` up to
/// the next `>` is a tag (with no `>` after it, a literal `<`), `{\` up to the next `}` is an
/// override block, the common entities decode to one character, `\r` is dropped. Streams without
/// allocating, linear in the text length (once no `>` or `}` follows, none follows any later
/// position either).
struct Pieces<'a> {
    s: &'a str,
    i: usize,
    no_gt: bool,
    no_brace: bool,
}

fn pieces(s: &str) -> Pieces<'_> {
    Pieces { s, i: 0, no_gt: false, no_brace: false }
}

impl<'a> Iterator for Pieces<'a> {
    type Item = (std::ops::Range<usize>, Piece<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.s.get(self.i..)?;
        let c = rest.chars().next()?;
        let n = c.len_utf8();
        let (len, piece) = match c {
            '<' if !self.no_gt => match rest.find('>') {
                Some(e) => (e + 1, Piece::Tag(rest.get(1..e).unwrap_or(""))),
                None => {
                    self.no_gt = true;
                    (n, Piece::Char('<'))
                }
            },
            '{' if !self.no_brace && rest.get(1..).is_some_and(|r| r.starts_with('\\')) => match rest.find('}') {
                Some(e) => (e + 1, Piece::Hidden),
                None => {
                    self.no_brace = true;
                    (n, Piece::Char('{'))
                }
            },
            '&' => match rest.get(1..).and_then(|r| ENTITIES.iter().find(|(name, _)| r.starts_with(name))) {
                Some((name, ch)) => (1 + name.len(), Piece::Char(*ch)),
                None => (n, Piece::Char('&')),
            },
            '\r' => (n, Piece::Hidden),
            _ => (n, Piece::Char(c)),
        };
        let at = self.i;
        self.i += len;
        Some((at..self.i, piece))
    }
}

/// Strip `<…>` tags and decode the common HTML/WebVTT entities.
pub fn plain_text(s: &str) -> String {
    pieces(s)
        .filter_map(|(_, p)| match p {
            Piece::Char(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// Deepest inline tag nesting a word split handles; deeper markup keeps the whole-text fallback.
const MAX_TAG_DEPTH: usize = 64;

/// Cut raw caption text before its `k`-th word (counted in the drawn text, as [`caption_words`]
/// does), keeping the markup on both halves: a leading override block (`{\an8}`) starts both, and
/// inline tags open across the cut are closed on the left and reopened on the right. `None` when
/// the cut cannot be placed or the markup is not strictly nested (crossed, unclosed or unmatched
/// tags, an empty or self-closing tag inside another, nesting deeper than [`MAX_TAG_DEPTH`]).
fn split_raw_at_word(raw: &str, k: usize) -> Option<(String, String)> {
    // pass 1: the cut is the end of the whitespace before the k-th word
    let mut words = 0usize;
    let mut in_word = false;
    let mut last_space_end = None;
    let mut cut = None;
    let mut prefix = None;
    for (range, p) in pieces(raw) {
        match p {
            Piece::Char(c) if c.is_whitespace() => {
                in_word = false;
                last_space_end = Some(range.end);
            }
            Piece::Char(_) if !in_word => {
                in_word = true;
                if words == k {
                    cut = last_space_end;
                    break;
                }
                words += 1;
            }
            // a leading override block ({\an8}: position) applies to both halves
            Piece::Hidden if words == 0 && prefix.is_none() && raw.get(range.clone()).is_some_and(|b| b.starts_with('{')) => {
                prefix = Some(range);
            }
            _ => {}
        }
    }
    let cut = cut?;
    // pass 2: the whole text must be strictly nested; the tags open at the cut are snapshotted once
    let mut stack: Vec<(&str, &str)> = Vec::new();
    let mut at_cut: Option<Vec<(&str, &str)>> = None;
    for (range, p) in pieces(raw) {
        if at_cut.is_none() && range.start >= cut {
            at_cut = Some(stack.clone());
        }
        let Piece::Tag(tag) = p else { continue };
        let t = tag.trim();
        if let Some(closing) = t.strip_prefix('/') {
            // a closer must close the innermost open tag
            let (name, _) = stack.pop()?;
            if closing.trim() != name {
                return None;
            }
        } else if t.starts_with(|c: char| c.is_ascii_digit()) {
            // a WebVTT timestamp tag (<00:00:01.000>) opens nothing
        } else if t.is_empty() || t.ends_with('/') {
            if !stack.is_empty() {
                return None;
            }
        } else {
            let name = t.split(|c: char| c.is_whitespace() || c == '.').next().unwrap_or("");
            if name.is_empty() || stack.len() >= MAX_TAG_DEPTH {
                return None;
            }
            stack.push((name, t));
        }
    }
    let open = at_cut?;
    if !stack.is_empty() {
        return None;
    }
    let mut left = raw.get(..cut)?.trim_end().to_string();
    for (name, _) in open.iter().rev() {
        left.push_str("</");
        left.push_str(name);
        left.push('>');
    }
    let mut right = String::new();
    if let Some(range) = prefix {
        right.push_str(raw.get(range)?);
    }
    for (_, tag) in &open {
        right.push('<');
        right.push_str(tag);
        right.push('>');
    }
    right.push_str(raw.get(cut..)?.trim_start());
    Some((left, right))
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
        // a static style saves without an `animation` key
        assert!(!serde_json::to_string(&CaptionStyle::default()).unwrap().contains("animation"));
        // a style saved before animations existed loads with the defaults for the new fields
        let st: CaptionStyle = serde_json::from_str(r#"{"font":"Inter","size":40.0}"#).unwrap();
        assert_eq!(st, CaptionStyle { size: 40.0, ..Default::default() });
    }

    #[test]
    fn animation_presets_round_trip_and_bad_values_load_static() {
        for a in CaptionAnimation::presets() {
            let st = CaptionStyle { animation: a, ..Default::default() };
            let back: CaptionStyle = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
            assert_eq!(back, st);
            assert_eq!(CaptionAnimation::from_name(a.name()), Some(a));
        }
        assert_eq!(CaptionAnimation::from_name("Karaoke "), Some(CaptionAnimation::presets()[1]));
        assert_eq!(CaptionAnimation::from_name("sparkle"), None);
        for bad in [r#""Sparkle""#, r#"{"WordPop":{"scale":"big"}}"#, "42", "null", r#"{"Boxed":{}}"#] {
            let st: CaptionStyle = serde_json::from_str(&format!(r#"{{"size":30.0,"animation":{bad}}}"#)).unwrap();
            assert_eq!(st, CaptionStyle { size: 30.0, ..Default::default() }, "{bad}");
        }
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
        assert_eq!((c.text.as_str(), right.as_str()), ("Hello <i>big</i>", "world"), "markup and line break boundaries kept");
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

    /// A caption over 0..100 with `text`, one 10-tick word every 20 ticks.
    fn spelled(text: &str) -> Caption {
        let words = caption_words(text).into_iter().enumerate().map(|(i, w)| WordTiming::new(Tick(i as i64 * 20), Tick(10), w)).collect();
        Caption { text: text.into(), words, ..timed_caption() }
    }

    /// Split `text` before its `k`-th word: (left text, right text).
    fn split_text(text: &str, k: usize) -> Option<(String, String)> {
        let mut c = spelled(text);
        let (right, words) = c.split_words_at(Tick(k as i64 * 20))?;
        assert_eq!((c.words.len(), words.len()), (k, caption_words(text).len() - k));
        Some((c.text, right))
    }

    #[test]
    fn split_keeps_markup_on_both_halves() {
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        // an inline tag across the cut is closed and reopened
        assert_eq!(split_text("<i>hello world</i>", 1), pair("<i>hello</i>", "<i>world</i>"));
        assert_eq!(split_text("<b><i>one two</i> three</b>", 1), pair("<b><i>one</i></b>", "<b><i>two</i> three</b>"));
        assert_eq!(split_text("<v Bob>Hi there</v>", 1), pair("<v Bob>Hi</v>", "<v Bob>there</v>"));
        // tags closed before the cut need nothing
        assert_eq!(split_text("<i>hello</i> world", 1), pair("<i>hello</i>", "world"));
        // a leading override block positions both halves
        assert_eq!(split_text("{\\an8}a b", 1), pair("{\\an8}a", "{\\an8}b"));
        // entities stay encoded
        assert_eq!(split_text("a &amp; b", 2), pair("a &amp;", "b"));
        assert_eq!(split_text("x &lt;b&gt; y", 1), pair("x", "&lt;b&gt; y"));
        // plain text and line breaks behave as before
        assert_eq!(split_text("one two\nthree four", 2), pair("one two", "three four"));
        assert_eq!(split_text("one two\nthree four", 3), pair("one two\nthree", "four"));
        // a tag inside a word is part of that word
        assert_eq!(split_text("hel<b>lo</b> world", 1), pair("hel<b>lo</b>", "world"));
        // every result still spells its words
        for (text, k) in [("<i>hello world</i>", 1), ("{\\an8}a b", 1), ("a &amp; b", 2), ("<u>x\ny</u> z", 1)] {
            let (l, r) = split_text(text, k).unwrap();
            let all = caption_words(text);
            assert_eq!((caption_words(&l), caption_words(&r)), (all[..k].to_vec(), all[k..].to_vec()), "{text}");
        }
    }

    #[test]
    fn malformed_markup_keeps_the_whole_text() {
        for text in [
            "<b><i>one two</b></i>", // crossed
            "<i>one two",            // unclosed
            "one</i> two",           // unmatched closer
            "<i>one <> two</i>",     // empty tag inside another
            "<i>one <br/> two</i>",  // self-closing tag inside another
            "<i>one</b> two</i>",    // wrong closer
        ] {
            let mut c = spelled(text);
            let before = c.clone();
            assert_eq!(c.split_words_at(Tick(20)), None, "{text}");
            assert_eq!(c, before, "{text}: nothing changes before every check has passed");
        }
        // markup that is fine at the top level still splits
        assert_eq!(split_text("one <br/> two", 1), Some(("one <br/>".into(), "two".into())));
        assert_eq!(split_text("<00:00:01.000>one two", 1), Some(("<00:00:01.000>one".into(), "two".into())));
    }

    #[test]
    fn deep_nesting_is_capped_and_shallow_nesting_reopens_exactly() {
        // 10,000 nested tags: over the depth cap, so the split declines, quickly
        let n = 10_000;
        let text = format!("{}a{}b{}", "<i>".repeat(n), " ".repeat(n), "</i>".repeat(n));
        let r = std::panic::catch_unwind(|| {
            let started = std::time::Instant::now();
            let mut c = spelled(&text);
            let before = c.clone();
            let split = c.split_words_at(Tick(20));
            (split.is_none(), c == before, started.elapsed())
        });
        let (declined, unchanged, took) = r.unwrap();
        assert!(declined && unchanged);
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        // 60 nested classed spans split with every opening tag reopened as written
        let opens: String = (0..60).map(|i| format!("<c.t{i}>")).collect();
        let closes = "</c>".repeat(60);
        let (l, r) = split_text(&format!("{opens}a b{closes}"), 1).unwrap();
        assert_eq!(l, format!("{opens}a{closes}"));
        assert_eq!(r, format!("{opens}b{closes}"));
        // and 65 is one too deep
        let opens: String = (0..65).map(|i| format!("<c.t{i}>")).collect();
        assert_eq!(split_text(&format!("{opens}a b{}", "</c>".repeat(65)), 1), None);
    }

    #[test]
    fn plain_text_walker_is_linear_on_unclosed_markup() {
        // many `<` and `{\` with nothing closing them: each is a literal character
        let s = "<{\\".repeat(50_000);
        let t = std::time::Instant::now();
        assert_eq!(plain_text(&s).chars().count(), 150_000);
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
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
