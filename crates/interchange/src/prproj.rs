//! Adobe Premiere Pro projects (`.prproj`), import only.
//!
//! Source: the MIT-licensed `prproj-format` research notes (version 1.1, 2026-09-18), which describe
//! Project versions 40, 42, 43 and 45 as observed in saved files. No Adobe specification, binary or
//! source was consulted.
//!
//! The file is UTF-8 XML, plain or gzip-compressed, with a `<PremiereData>` root. Objects are flat
//! siblings under the root that point at each other with `ObjectRef` (to an `ObjectID`) and
//! `ObjectURef` (to an `ObjectUID`); only direct children of the root are indexed, because the same
//! numbers recur deep inside UI preference data. Time is in ticks at 254 016 000 000 per second,
//! which is exactly [`Tick`], so timeline and source times map without rounding.
//!
//! Read: sequences (name, frame rate, frame size, audio sample rate), video and audio tracks (name,
//! lock, sync lock, mute / output), clip items (timeline start and end, source in point, constant
//! speed, a negative speed as reverse), media files (path, title, stream rate and size), generated
//! media (Black Video, Color Matte, Bars and Tone, Transparent Video, counting leader), nested
//! sequences at any depth, sequence and clip markers (`DVAMarker`), linked video and audio, and the
//! Project panel bin tree.
//!
//! Reported, not imported (a warning per occurrence, merged with a count): transitions, effects
//! (including the fixed Motion, Opacity and Volume settings and their keyframes), speed ramps (time
//! remapping), captions, adjustment layers, titles and graphics, and clip sources of any other
//! kind (multicam). Source in points are stored as Premiere wrote them; the notes do not say whether
//! they count from a file's start timecode, so the engine reports files that have one.
//!
//! Hostile input: the decompressed document is capped at [`MAX_DECOMPRESSED`] bytes and
//! [`MAX_NODES`] XML nodes, a truncated gzip stream or invalid UTF-8 is an error, a reference to an
//! object that is not in the file is a report entry, and nested sequences and bins are walked with
//! seen-sets and depth limits.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Read;

use filmcraft_media::{Generator, MediaKind};
use filmcraft_project::{BinId, ClipId, ItemId, Label, Marker, MarkerId, MarkerKind, Sequence, TrackItem, TrackKind, checked_par, validate_frame_size};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use roxmltree::{Node, NodeId};

use crate::common::{Builder, MediaSpec, empty_sequence, file_name, kind_from_ext, resolve_path, settings_for};
use crate::xml::{child, child_bool, child_text, children, elements};
use crate::{Error, Format, ImportOptions, Imported, Report, Result};

/// Project item metadata key holding a media file's path relative to the project (resolved against
/// the project's folder) when it differs from the stored path: the engine tries it when the stored
/// path does not open.
pub const RELATIVE_PATH_KEY: &str = "Premiere Relative Path";

/// Largest decompressed document read (512 MiB; 128 MiB on the web, where memory is tighter).
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_DECOMPRESSED: u64 = 512 * 1024 * 1024;
#[cfg(target_arch = "wasm32")]
pub const MAX_DECOMPRESSED: u64 = 128 * 1024 * 1024;
/// Largest number of XML nodes parsed into the document tree (8 million; 2 million on the web).
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_NODES: u32 = 8_000_000;
#[cfg(target_arch = "wasm32")]
pub const MAX_NODES: u32 = 2_000_000;
/// Deepest XML element nesting parsed (the parser's tokenizer recurses once per level).
const MAX_XML_DEPTH: usize = 1_000;
/// Deepest chain of nested sequences followed.
const MAX_NEST_DEPTH: usize = 64;
/// Deepest bin tree followed.
const MAX_BIN_DEPTH: usize = 64;
/// Most tracks of one kind a sequence gets (overlapping clips move to new tracks).
const MAX_TRACKS: usize = 256;
/// Most clip items one import creates (a damaged file can reference one clip many times).
const MAX_ITEMS: usize = 200_000;
/// Most markers one import creates.
const MAX_MARKERS: usize = 200_000;
/// Distinct report entries before the rest are summarised in one line.
const MAX_REPORT_ENTRIES: usize = 500;
/// Largest tick value accepted: the project's own bound ([`Tick::MAX`]), so whatever is imported
/// also saves and loads again. Anything larger is a damaged number.
const MAX_TICK: i64 = Tick::MAX.0;
/// How much of a document detection looks at.
const SNIFF_BYTES: u64 = 64 * 1024;
/// Project versions the format notes describe.
const KNOWN_VERSIONS: std::ops::RangeInclusive<i64> = 40..=45;

fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

fn perr(message: impl Into<String>) -> Error {
    Error::parse(Format::Prproj, message)
}

/// Whether `bytes` (plain or gzip) is XML whose root element is `<PremiereData>`. Gzip magic alone
/// is not enough: the decompressed start of the document has to say so.
pub(crate) fn sniff(bytes: &[u8]) -> bool {
    if is_gzip(bytes) {
        let mut head = Vec::new();
        // A prefix is enough to see the root; a damaged tail is the importer's error to report.
        let _ = flate2::read::MultiGzDecoder::new(bytes).take(SNIFF_BYTES).read_to_end(&mut head);
        root_is_premiere(&head)
    } else {
        root_is_premiere(bytes.get(..SNIFF_BYTES as usize).unwrap_or(bytes))
    }
}

/// Skip the XML declaration, comments and a doctype, then check the first element's name.
fn root_is_premiere(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    let mut s = text.trim_start_matches('\u{feff}').trim_start();
    loop {
        let rest = if let Some(r) = s.strip_prefix("<?") {
            r.find("?>").and_then(|i| r.get(i + 2..))
        } else if let Some(r) = s.strip_prefix("<!--") {
            r.find("-->").and_then(|i| r.get(i + 3..))
        } else if let Some(r) = s.strip_prefix("<!") {
            r.find('>').and_then(|i| r.get(i + 1..))
        } else {
            break;
        };
        let Some(rest) = rest else { return false };
        s = rest.trim_start();
    }
    s.strip_prefix("<PremiereData").and_then(|r| r.chars().next()).is_some_and(|c| c.is_whitespace() || c == '>' || c == '/')
}

/// The document text: gunzipped if needed (at most `max` bytes), UTF-8 (a BOM is allowed).
fn decode(bytes: &[u8], max: u64) -> Result<Cow<'_, str>> {
    let data: Cow<'_, [u8]> = if is_gzip(bytes) {
        let mut out = Vec::new();
        // every member of a multi-member stream, not just the first
        flate2::read::MultiGzDecoder::new(bytes)
            .take(max.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| perr(format!("the compressed project is damaged or truncated ({e})")))?;
        Cow::Owned(out)
    } else {
        Cow::Borrowed(bytes)
    };
    if data.len() as u64 > max {
        return Err(perr(format!("the project is larger than {} MiB of XML", max / (1024 * 1024))));
    }
    match data {
        Cow::Borrowed(b) => {
            let b = b.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(b);
            std::str::from_utf8(b).map(Cow::Borrowed).map_err(|e| perr(format!("the project is not UTF-8 text ({e})")))
        }
        Cow::Owned(mut v) => {
            if v.starts_with(&[0xef, 0xbb, 0xbf]) {
                v.drain(..3);
            }
            String::from_utf8(v).map(Cow::Owned).map_err(|e| perr(format!("the project is not UTF-8 text ({})", e.utf8_error())))
        }
    }
}

/// Refuse documents whose elements nest deeper than `max` before the XML parser sees them: its
/// tokenizer recurses once per element level (the tree itself is built iteratively), so a very
/// deep document overflows the stack. Counts the way the tokenizer recurses: an open tag goes one level down, a close tag one up,
/// and comments, CDATA, declarations and quoted attribute values are skipped.
fn check_depth(b: &[u8], max: usize) -> Result<()> {
    let find = |from: usize, pat: &[u8]| b.get(from..).and_then(|r| r.windows(pat.len()).position(|w| w == pat)).map(|p| from + p + pat.len());
    let mut depth = 0usize;
    let mut i = 0usize;
    while let Some(off) = b.get(i..).and_then(|r| r.iter().position(|&c| c == b'<')) {
        let lt = i + off;
        let rest = b.get(lt..).unwrap_or(&[]);
        let next = if rest.starts_with(b"<!--") {
            find(lt + 4, b"-->")
        } else if rest.starts_with(b"<![CDATA[") {
            find(lt + 9, b"]]>")
        } else if rest.starts_with(b"<?") {
            find(lt + 2, b"?>")
        } else if rest.starts_with(b"<!") {
            find(lt + 2, b">")
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            find(lt + 2, b">")
        } else {
            // an open tag: find its end outside quoted attribute values
            let mut j = lt + 1;
            let mut quote = None;
            let mut end = None;
            while let Some(&c) = b.get(j) {
                match (quote, c) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, b'"' | b'\'') => quote = Some(c),
                    (None, b'>') => {
                        end = Some(j);
                        break;
                    }
                    _ => {}
                }
                j += 1;
            }
            let Some(end) = end else { break };
            if b.get(end.wrapping_sub(1)) != Some(&b'/') {
                depth += 1;
                if depth > max {
                    return Err(perr(format!("XML elements are nested more than {max} deep")));
                }
            }
            Some(end + 1)
        };
        match next {
            Some(n) => i = n,
            // unterminated markup: the parser reports it
            None => break,
        }
    }
    Ok(())
}

/// Import a `.prproj` document (plain or gzip).
pub(crate) fn import(bytes: &[u8], opts: &ImportOptions) -> Result<(Imported, Report)> {
    import_limited(bytes, opts, Limits { nodes: MAX_NODES, items: MAX_ITEMS, markers: MAX_MARKERS })
}

/// Work budget of one import.
#[derive(Clone, Copy)]
struct Limits {
    nodes: u32,
    items: usize,
    markers: usize,
}

fn import_limited(bytes: &[u8], opts: &ImportOptions, limits: Limits) -> Result<(Imported, Report)> {
    let text = decode(bytes, MAX_DECOMPRESSED)?;
    check_depth(text.as_bytes(), MAX_XML_DEPTH)?;
    let po = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: limits.nodes, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(&text, po).map_err(|e| perr(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "PremiereData" {
        return Err(perr("the root element is not <PremiereData>: not a Premiere Pro project"));
    }
    let mut report = Report::default();
    let project = elements(root)
        .find(|n| n.has_tag_name("Project") && n.attribute("ObjectRef").is_none() && n.attribute("ObjectURef").is_none())
        .ok_or_else(|| perr("the document has no <Project> object"))?;
    match project.attribute("Version").and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(v) if KNOWN_VERSIONS.contains(&v) => {}
        Some(v) => report.warn(format!("Premiere Pro project version {v} is outside the versions this importer was written for (40 to 45)")),
        None => report.warn("the project does not say which Premiere Pro version wrote it"),
    }
    let name = opts.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("Premiere Pro Project");
    let mut imp = Imp {
        by_id: HashMap::new(),
        by_uid: HashMap::new(),
        b: Builder::new(name),
        base_dir: opts.base_dir.as_deref(),
        report: &mut report,
        seq_items: HashMap::new(),
        in_progress: HashSet::new(),
        media_items: HashMap::new(),
        master_items: HashMap::new(),
        clip_refs: HashMap::new(),
        nested: HashSet::new(),
        marker_lists: HashMap::new(),
        limits,
        items_made: 0,
        markers_made: 0,
        warned: HashSet::new(),
        track_effects: false,
    };
    imp.index(root);
    let sequences: Vec<Node> = elements(root).filter(|n| n.has_tag_name("Sequence")).collect();
    let mut order = Vec::new();
    for s in &sequences {
        if let Some(id) = imp.sequence(*s, 0) {
            order.push(id);
        }
    }
    imp.bins(project);
    imp.links(root);
    let mut top: Vec<ItemId> = order.iter().copied().filter(|id| !imp.nested.contains(id)).collect();
    if top.is_empty() {
        // Every sequence is inside another (a damaged, cyclic project): list them all.
        top = order;
    }
    top.dedup();
    let Imp { mut b, .. } = imp;
    if b.p.items.is_empty() {
        return Err(Error::Empty);
    }
    b.top = top;
    Ok((b.finish(), report))
}

/// One sequence's picture and sound settings, for the clips read into it.
#[derive(Clone, Copy)]
struct Frame {
    width: u32,
    height: u32,
    rate: FrameRate,
}

struct Imp<'a, 'i, 'r> {
    by_id: HashMap<&'a str, Node<'a, 'i>>,
    by_uid: HashMap<&'a str, Node<'a, 'i>>,
    b: Builder,
    base_dir: Option<&'r str>,
    report: &'r mut Report,
    seq_items: HashMap<NodeId, ItemId>,
    in_progress: HashSet<NodeId>,
    media_items: HashMap<NodeId, ItemId>,
    master_items: HashMap<NodeId, Option<ItemId>>,
    /// `ObjectID` of a clip track item → where its clip went (for `Link`).
    clip_refs: HashMap<&'a str, (ItemId, ClipId)>,
    /// Sequences used as clips inside other sequences.
    nested: HashSet<ItemId>,
    /// Parsed marker lists, so a list referenced many times is read once.
    marker_lists: HashMap<NodeId, Vec<Marker>>,
    limits: Limits,
    items_made: usize,
    markers_made: usize,
    /// Messages already in the report (bounds the report's own linear merge).
    warned: HashSet<String>,
    /// Track-level effects were seen (reported once).
    track_effects: bool,
}

/// Parse a tick value; `None` when absent, unparsable or out of range.
fn tick_value(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok().filter(|v| v.checked_abs().is_some_and(|a| a <= MAX_TICK))
}

/// A rate from ticks per frame (`VideoTrackGroup` / `VideoStream` `<FrameRate>`), exact.
fn rate_from_ticks(ticks_per_frame: i64) -> Option<FrameRate> {
    // between 1 and 1000 frames per second
    if !(TICKS_PER_SECOND / 1000..=TICKS_PER_SECOND).contains(&ticks_per_frame) {
        return None;
    }
    // A sequence's rate must reduce to 32-bit terms (`SequenceSettings::validate`).
    let r = FrameRate::new(TICKS_PER_SECOND, ticks_per_frame);
    (r.num <= i64::from(u32::MAX) && r.den <= i64::from(u32::MAX)).then_some(r)
}

/// A sample rate from ticks per sample (`AudioTrackGroup` `<FrameRate>`).
fn sample_rate_from_ticks(ticks_per_sample: i64) -> Option<u32> {
    if ticks_per_sample <= 0 {
        return None;
    }
    let sr = if TICKS_PER_SECOND % ticks_per_sample == 0 {
        TICKS_PER_SECOND / ticks_per_sample
    } else {
        (TICKS_PER_SECOND as f64 / ticks_per_sample as f64).round() as i64
    };
    u32::try_from(sr).ok().filter(|r| (1_000..=384_000).contains(r))
}

/// `num,den` (`<PixelAspectRatio>`).
fn parse_par(s: &str) -> Option<(u32, u32)> {
    let (n, d) = s.split_once(',')?;
    Some((n.trim().parse().ok()?, d.trim().parse().ok()?))
}

/// `left,top,right,bottom` → (width, height).
fn frame_rect(s: &str) -> Option<(u32, u32)> {
    let v: Vec<i64> = s.split(',').map(|p| p.trim().parse::<i64>().ok()).collect::<Option<_>>()?;
    let [l, t, r, b] = v.as_slice() else { return None };
    let w = u32::try_from(r.checked_sub(*l)?).ok()?;
    let h = u32::try_from(b.checked_sub(*t)?).ok()?;
    validate_frame_size(w, h).is_ok().then_some((w, h))
}

/// Children named `tag` of `list`, ordered by their `Index` attribute (document order otherwise).
fn indexed<'a, 'i>(list: Option<Node<'a, 'i>>, tag: &'static str) -> Vec<Node<'a, 'i>> {
    let Some(l) = list else { return Vec::new() };
    let mut v: Vec<(i64, usize, Node)> =
        children(l, tag).enumerate().map(|(i, n)| (n.attribute("Index").and_then(|s| s.trim().parse().ok()).unwrap_or(i as i64), i, n)).collect();
    v.sort_by_key(|(k, i, _)| (*k, *i));
    v.into_iter().map(|(_, _, n)| n).collect()
}

fn is_ref(n: Node) -> bool {
    n.attribute("ObjectRef").is_some() || n.attribute("ObjectURef").is_some()
}

fn non_empty(s: &str) -> bool {
    !s.is_empty()
}

/// Whether a `<FilePath>` is an internal id (generated media) rather than a path.
fn is_internal_id(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())
}

/// Ticks inside a `DVAMarker` time object: `{"ticks": 123}` or `{"ticks": "123"}`.
fn json_ticks(v: Option<&serde_json::Value>) -> Option<i64> {
    let t = v?.get("ticks")?;
    let n = t.as_i64().or_else(|| t.as_str().and_then(tick_value))?;
    n.checked_abs().is_some_and(|a| a <= MAX_TICK).then_some(n)
}

/// Whether `c` overlaps none of `items` (sorted by start, not overlapping each other).
fn fits(items: &[TrackItem], c: &TrackItem) -> bool {
    let i = items.partition_point(|o| o.start < c.end());
    i == 0 || items.get(i - 1).is_none_or(|o| o.end() <= c.start)
}

/// Insert `c` into `items`, keeping them sorted by start.
fn insert_sorted(items: &mut Vec<TrackItem>, c: TrackItem) {
    let i = items.partition_point(|o| o.start <= c.start);
    items.insert(i, c);
}

impl<'a, 'i, 'r> Imp<'a, 'i, 'r> {
    fn warn(&mut self, message: impl Into<String>) {
        let m = message.into();
        if self.warned.len() >= MAX_REPORT_ENTRIES && !self.warned.contains(&m) {
            self.report.warn("more problems were found than this report lists");
        } else {
            self.warned.insert(m.clone());
            self.report.warn(m);
        }
    }

    /// Index the top-level objects (direct children of the root) by `ObjectID` and `ObjectUID`.
    fn index(&mut self, root: Node<'a, 'i>) {
        let mut duplicates = 0usize;
        for n in elements(root) {
            if let Some(id) = n.attribute("ObjectID")
                && self.by_id.insert(id, n).is_some()
            {
                duplicates += 1;
            }
            if let Some(uid) = n.attribute("ObjectUID")
                && self.by_uid.insert(uid, n).is_some()
            {
                duplicates += 1;
            }
        }
        for _ in 0..duplicates.min(MAX_REPORT_ENTRIES) {
            self.warn("two objects in the project share an id; the later one was used");
        }
    }

    /// The object a reference element points at. A reference to an object that is not in the
    /// file is reported (once per kind of reference, with a count).
    fn follow(&mut self, r: Option<Node<'a, 'i>>, what: &str) -> Option<Node<'a, 'i>> {
        let r = r?;
        let hit = if let Some(id) = r.attribute("ObjectRef") {
            self.by_id.get(id).copied()
        } else if let Some(uid) = r.attribute("ObjectURef") {
            self.by_uid.get(uid).copied()
        } else {
            self.warn(format!("{what} has no object reference; it was skipped"));
            return None;
        };
        if hit.is_none() {
            self.warn(format!("{what} refers to an object that is not in the project file; it was skipped"));
        }
        hit
    }

    // -----------------------------------------------------------------------------------------
    // Sequences and tracks
    // -----------------------------------------------------------------------------------------

    fn sequence(&mut self, n: Node<'a, 'i>, depth: usize) -> Option<ItemId> {
        let name = child_text(n, "Name").filter(|s| non_empty(s)).unwrap_or("Sequence").to_string();
        // Before the cache: a sequence being read is already cached, and using it would make a cycle.
        if self.in_progress.contains(&n.id()) {
            self.warn(format!("sequence \"{name}\" is nested inside itself; that clip was left out"));
            return None;
        }
        if let Some(id) = self.seq_items.get(&n.id()) {
            return Some(*id);
        }
        if depth > MAX_NEST_DEPTH {
            self.warn(format!("sequences are nested more than {MAX_NEST_DEPTH} deep; the deeper ones were left out"));
            return None;
        }
        self.in_progress.insert(n.id());
        let mut groups: HashMap<&str, Node<'a, 'i>> = HashMap::new();
        let list = child(n, "TrackGroups");
        for g in indexed(list, "TrackGroup") {
            if let Some(obj) = self.follow(child(g, "Second"), "sequence track group") {
                groups.entry(obj.tag_name().name()).or_insert(obj);
            }
        }
        let video = groups.get("VideoTrackGroup").copied();
        let audio = groups.get("AudioTrackGroup").copied();
        let data = groups.get("DataTrackGroup").copied();
        let vtg = video.and_then(|v| child(v, "TrackGroup"));
        let rate = match vtg.and_then(|t| child_text(t, "FrameRate")).and_then(tick_value) {
            Some(t) => rate_from_ticks(t).unwrap_or_else(|| {
                self.warn(format!("sequence \"{name}\" has an unusable frame rate; {} was used", FrameRate::default().label()));
                FrameRate::default()
            }),
            None => {
                self.warn(format!("sequence \"{name}\" has no frame rate; {} was used", FrameRate::default().label()));
                FrameRate::default()
            }
        };
        let (width, height) = match video.and_then(|v| child_text(v, "FrameRect")) {
            Some(r) => frame_rect(r).unwrap_or_else(|| {
                self.warn(format!("sequence \"{name}\" has an unusable frame size; 1920x1080 was used"));
                (1920, 1080)
            }),
            None => (1920, 1080),
        };
        let mut settings = settings_for(rate, width, height, false);
        if let Some(t) = audio.and_then(|a| child(a, "TrackGroup")).and_then(|t| child_text(t, "FrameRate")) {
            match tick_value(t).and_then(sample_rate_from_ticks) {
                Some(sr) => settings.sample_rate = sr,
                None => self.warn(format!("sequence \"{name}\" has an unusable audio sample rate; 48000 Hz was used")),
            }
        }
        if let Some(par) = video.and_then(|v| child_text(v, "PixelAspectRatio")) {
            match parse_par(par).and_then(checked_par) {
                Some(p) => settings.par = p,
                None => self.warn(format!("sequence \"{name}\" has an unusable pixel aspect ratio; square pixels were used")),
            }
        }
        // What the project itself accepts on load: anything else would import, then fail to reopen.
        if let Err(e) = settings.validate() {
            self.warn(format!("sequence \"{name}\" settings are out of range ({e}); the defaults were used"));
            settings = settings_for(FrameRate::default(), 1920, 1080, false);
        }
        let id = self.b.reserve_sequence(&name, settings.clone(), None);
        self.seq_items.insert(n.id(), id);
        let mut seq = empty_sequence(settings);
        seq.markers = self.markers_of(n);
        let frame = Frame { width: seq.settings.width, height: seq.settings.height, rate: seq.settings.frame_rate };
        for (kind, group) in [(TrackKind::Video, video), (TrackKind::Audio, audio)] {
            let tracks = indexed(group.and_then(|g| child(g, "TrackGroup")).and_then(|t| child(t, "Tracks")), "Track");
            let mut ti = 0usize;
            let mut clips = Vec::new();
            for t in tracks {
                let Some(tn) = self.follow(Some(t), "track list entry") else { continue };
                if ti >= MAX_TRACKS {
                    self.warn(format!("a sequence has more than {MAX_TRACKS} tracks of one kind; the rest were left out"));
                    break;
                }
                self.b.ensure_tracks(&mut seq, kind, ti + 1);
                self.track(&mut seq, kind, ti, tn, frame, depth, &mut clips);
                ti += 1;
            }
            self.place_clips(&mut seq, id, kind, clips);
        }
        if let Some(d) = data {
            for t in indexed(child(d, "TrackGroup").and_then(|t| child(t, "Tracks")), "Track") {
                let Some(tn) = self.follow(Some(t), "track list entry") else { continue };
                let items = child(tn, "ClipTrack").and_then(|c| child(c, "ClipItems")).and_then(|c| child(c, "TrackItems"));
                for _ in indexed(items, "TrackItem") {
                    self.warn("caption clips were not imported");
                }
            }
        }
        if seq.video_tracks.is_empty() {
            self.b.ensure_tracks(&mut seq, TrackKind::Video, 1);
        }
        if seq.audio_tracks.is_empty() {
            self.b.ensure_tracks(&mut seq, TrackKind::Audio, 1);
        }
        self.b.put_sequence(id, seq);
        self.in_progress.remove(&n.id());
        Some(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn track(
        &mut self,
        seq: &mut Sequence,
        kind: TrackKind,
        ti: usize,
        tn: Node<'a, 'i>,
        frame: Frame,
        depth: usize,
        clips: &mut Vec<(usize, Option<&'a str>, TrackItem)>,
    ) {
        let ct = child(tn, "ClipTrack");
        if let (Some(meta), Some(t)) = (ct.and_then(|c| child(c, "Track")), seq.tracks_mut(kind).get_mut(ti)) {
            t.locked = child_bool(meta, "IsLocked").unwrap_or(false);
            t.sync_lock = child_bool(meta, "IsSyncLocked").unwrap_or(true);
            let muted = child_bool(meta, "IsMuted").unwrap_or(false);
            match kind {
                TrackKind::Video => t.enabled = !muted,
                TrackKind::Audio => t.muted = muted,
            }
            if let Some(name) = child_text(meta, "Name").filter(|s| non_empty(s)) {
                t.name = name.to_string();
            }
        }
        // Track volume, pan and track effects hang off the track object; none are imported.
        if !self.track_effects
            && tn
                .descendants()
                .any(|d| matches!(d.tag_name().name(), "Panner" | "Components" | "ComponentOwner" | "ComponentChain") && (is_ref(d) || d.has_children()))
        {
            self.track_effects = true;
            self.warn("track-level effects (track volume, pan and track effects) were not imported");
        }
        // Clips are placed once every track of this kind is read (`place_clips`).
        let refs = indexed(ct.and_then(|c| child(c, "ClipItems")).and_then(|c| child(c, "TrackItems")), "TrackItem");
        for r in refs {
            let Some(it) = self.follow(Some(r), "track item") else { continue };
            match it.tag_name().name() {
                "VideoClipTrackItem" | "AudioClipTrackItem" => {
                    if let Some(c) = self.clip_item(it, kind, frame, depth) {
                        clips.push((ti, it.attribute("ObjectID"), c));
                    }
                }
                other => self.warn(format!("{other} track items are not supported and were skipped")),
            }
        }
        for r in indexed(ct.and_then(|c| child(c, "TransitionItems")).and_then(|c| child(c, "TrackItems")), "TrackItem") {
            let Some(tr) = self.follow(Some(r), "transition") else { continue };
            let inner = child(tr, "TransitionTrackItem");
            let name = inner
                .and_then(|t| child_text(t, "DisplayName"))
                .filter(|s| non_empty(s))
                .or_else(|| inner.and_then(|t| child_text(t, "MatchName")))
                .unwrap_or("?");
            self.warn(format!("transition \"{name}\" was not imported"));
        }
    }

    /// Put the clips of every track of `kind` on the sequence: each on its own track first (in
    /// time order); a clip that overlaps one already there goes to the lowest new track above all
    /// the native ones where it overlaps nothing.
    fn place_clips(&mut self, seq: &mut Sequence, seq_id: ItemId, kind: TrackKind, mut clips: Vec<(usize, Option<&'a str>, TrackItem)>) {
        clips.sort_by_key(|(ti, _, c)| (*ti, c.start));
        let mut overflow = Vec::new();
        for (ti, obj, c) in clips {
            match seq.tracks_mut(kind).get_mut(ti) {
                Some(t) if fits(&t.items, &c) => {
                    if let Some(o) = obj {
                        self.clip_refs.insert(o, (seq_id, c.id));
                    }
                    insert_sorted(&mut t.items, c);
                }
                _ => overflow.push((ti, obj, c)),
            }
        }
        // Overflow goes on new tracks above every native one (unlocked, as Track::new makes them),
        // so it never lands among another Premiere track's clips or inherits its lock.
        let native = seq.tracks(kind).len();
        for (_, obj, c) in overflow {
            let mut target = None;
            for i in native..MAX_TRACKS {
                match seq.tracks(kind).get(i) {
                    Some(t) if !fits(&t.items, &c) => continue,
                    _ => {
                        target = Some(i);
                        break;
                    }
                }
            }
            let Some(i) = target else {
                self.warn(format!("overlapping clip items; one was left out (more than {MAX_TRACKS} tracks would be needed)"));
                continue;
            };
            self.warn("overlapping clip items on one track; the later one was moved to a new track above the others");
            self.b.ensure_tracks(seq, kind, i + 1);
            if let Some(t) = seq.tracks_mut(kind).get_mut(i) {
                if let Some(o) = obj {
                    self.clip_refs.insert(o, (seq_id, c.id));
                }
                insert_sorted(&mut t.items, c);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Clips
    // -----------------------------------------------------------------------------------------

    /// The `VideoClip` / `AudioClip` of a master clip for a track of `kind` (else its first).
    fn master_clip_of_kind(&mut self, master: Node<'a, 'i>, kind: TrackKind) -> Option<Node<'a, 'i>> {
        let want = if kind == TrackKind::Video { "VideoClip" } else { "AudioClip" };
        let mut first = None;
        for r in indexed(child(master, "Clips"), "Clip") {
            let Some(c) = self.follow(Some(r), "master clip entry") else { continue };
            if c.has_tag_name(want) {
                return Some(c);
            }
            first.get_or_insert(c);
        }
        first
    }

    fn clip_item(&mut self, it: Node<'a, 'i>, kind: TrackKind, frame: Frame, depth: usize) -> Option<TrackItem> {
        if self.items_made >= self.limits.items {
            if self.items_made == self.limits.items {
                self.items_made += 1;
                self.warn(format!("the project has more than {} clip items; the rest were left out", self.limits.items));
            }
            return None;
        }
        let Some(cti) = child(it, "ClipTrackItem") else {
            self.warn("a track item without clip data was skipped");
            return None;
        };
        let tri = child(cti, "TrackItem");
        // Project version 45 leaves <Start> out when it is zero.
        let start = match tri.and_then(|t| child_text(t, "Start")) {
            None => 0,
            Some(s) => match tick_value(s) {
                Some(v) => v,
                None => {
                    self.warn("a clip item with an unreadable start time was skipped");
                    return None;
                }
            },
        };
        let Some(end) = tri.and_then(|t| child_text(t, "End")).and_then(tick_value) else {
            self.warn("a clip item without a readable end time was skipped");
            return None;
        };
        let duration = end.checked_sub(start).unwrap_or(0);
        if start < 0 || duration <= 0 {
            self.warn("a clip item that ends before it starts was skipped");
            return None;
        }
        let sub = self.follow(child(cti, "SubClip"), "clip item");
        let master = sub.and_then(|s| child(s, "MasterClip"));
        let master = self.follow(master, "sub clip");
        let name = sub
            .and_then(|s| child_text(s, "Name"))
            .filter(|s| non_empty(s))
            .or_else(|| master.and_then(|m| child_text(m, "Name")).filter(|s| non_empty(s)))
            .unwrap_or("Clip")
            .to_string();
        if master.and_then(|m| child_bool(m, "AdjustmentLayer")) == Some(true) {
            self.warn("adjustment layers were not imported");
            return None;
        }
        let clip_el = match sub.and_then(|s| child(s, "Clip")) {
            Some(r) => self.follow(Some(r), "sub clip"),
            None => match master {
                Some(m) => self.master_clip_of_kind(m, kind),
                None => None,
            },
        };
        let c = clip_el.and_then(|c| child(c, "Clip"));
        // An absent in point is the start of the source; one that is there but unreadable is reported.
        let in_point = match c.and_then(|c| child_text(c, "InPoint")) {
            None => 0,
            Some(t) => tick_value(t).unwrap_or_else(|| {
                self.warn("a source in point could not be read; the start of the source was used");
                0
            }),
        };
        let out_point = match c.and_then(|c| child_text(c, "OutPoint")) {
            None => None,
            Some(t) => {
                let v = tick_value(t);
                if v.is_none() {
                    self.warn("a source out point could not be read and was ignored");
                }
                v
            }
        };
        if out_point.is_some_and(|o| o < in_point) {
            self.warn("a clip's source out point is before its in point; the in point was kept");
        }
        let mut speed = 1.0;
        let mut reverse = false;
        if let Some(s) = c.and_then(|c| child_text(c, "PlaybackSpeed")) {
            match s.trim().parse::<f64>() {
                Ok(v) if v.is_finite() && v.abs() >= 1e-4 && v.abs() <= 1e4 => {
                    speed = v.abs();
                    reverse = v < 0.0;
                }
                _ => self.warn("a clip speed could not be read; 100% was used"),
            }
        }
        if reverse {
            self.warn("a negative clip speed was read as reverse playback (unverified against Premiere Pro files)");
        }
        if c.is_some_and(|c| child(c, "Reverse").is_some()) {
            self.warn("a clip <Reverse> setting was not imported (its meaning is unverified)");
        }
        if c.is_some_and(|c| child(c, "TimeRemapping").is_some()) {
            self.warn("speed ramps (time remapping) were not imported; the clip plays at a constant speed");
        }
        let src = c.and_then(|c| child(c, "Source"));
        let src = self.follow(src, "clip");
        let item = match src.map(|s| (s, s.tag_name().name())) {
            Some((s, "VideoMediaSource" | "AudioMediaSource")) => {
                let media = child(s, "MediaSource").and_then(|m| child(m, "Media"));
                match self.follow(media, "media source") {
                    Some(m) => {
                        if child_bool(m, "AdjustmentLayer") == Some(true) {
                            self.warn("adjustment layers were not imported");
                            return None;
                        }
                        self.media(m, kind, &name, frame)
                    }
                    None => self.offline(&name, kind, frame),
                }
            }
            Some((s, "VideoSequenceSource" | "AudioSequenceSource")) => {
                let target = child(s, "SequenceSource").and_then(|q| child(q, "Sequence"));
                let target = self.follow(target, "nested sequence clip")?;
                if !target.has_tag_name("Sequence") {
                    self.warn("a nested sequence clip points at something that is not a sequence; it was skipped");
                    return None;
                }
                let id = self.sequence(target, depth + 1)?;
                self.nested.insert(id);
                id
            }
            Some((_, other)) => {
                let what = if other.to_ascii_lowercase().contains("multicam") { "multicam" } else { other };
                self.warn(format!("clip sources of type {what} are not supported; those clips were imported offline"));
                self.offline(&name, kind, frame)
            }
            None => {
                self.warn("a clip without source media was imported offline");
                self.offline(&name, kind, frame)
            }
        };
        let mut ti = self.b.clip(item, kind, &name, Tick(start), Tick(duration), Tick(in_point));
        self.items_made += 1;
        ti.speed = speed;
        ti.reverse = reverse;
        ti.markers = self.markers_of(cti);
        self.report_effects(cti);
        Some(ti)
    }

    /// Effects on a clip are not imported: name each one in the report.
    fn report_effects(&mut self, cti: Node<'a, 'i>) {
        let chain = child(cti, "ComponentOwner").and_then(|c| child(c, "Components"));
        let Some(chain) = self.follow(chain, "clip effect list") else { return };
        let comps = indexed(child(chain, "ComponentChain").and_then(|c| child(c, "Components")), "Component");
        for r in comps {
            let Some(comp) = self.follow(Some(r), "effect") else { continue };
            let inner = child(comp, "Component");
            let display = inner
                .and_then(|i| child_text(i, "DisplayName"))
                .filter(|s| non_empty(s))
                .or_else(|| child_text(comp, "MatchName"))
                .filter(|s| non_empty(s))
                .unwrap_or("unnamed effect")
                .to_string();
            let intrinsic = inner.and_then(|i| child_bool(i, "Intrinsic")).unwrap_or(false);
            let mut animated = false;
            for p in indexed(inner.and_then(|i| child(i, "Params")), "Param") {
                if let Some(param) = self.follow(Some(p), "effect parameter")
                    && child_bool(param, "IsTimeVarying") == Some(true)
                {
                    animated = true;
                }
            }
            match (intrinsic, animated) {
                (true, false) => self.warn(format!("{display} settings were not imported (defaults used)")),
                (true, true) => self.warn(format!("{display} keyframes were not imported (defaults used)")),
                (false, false) => self.warn(format!("effect \"{display}\" was not imported")),
                (false, true) => self.warn(format!("effect \"{display}\" and its keyframes were not imported")),
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Media
    // -----------------------------------------------------------------------------------------

    fn offline(&mut self, name: &str, kind: TrackKind, frame: Frame) -> ItemId {
        let spec = MediaSpec {
            video: (kind == TrackKind::Video).then_some((frame.width, frame.height, frame.rate)),
            audio: (kind == TrackKind::Audio).then_some((48_000, 2)),
            ..Default::default()
        };
        let path = resolve_path(name, self.base_dir);
        let id = self.b.file_media(&format!("offline:{name}"), name, &path, &spec, None);
        if let Some(m) = self.b.p.item_mut(id).and_then(|i| i.as_media_mut()) {
            m.offline = true;
        }
        id
    }

    fn media(&mut self, m: Node<'a, 'i>, kind: TrackKind, fallback_name: &str, frame: Frame) -> ItemId {
        if let Some(id) = self.media_items.get(&m.id()) {
            return *id;
        }
        let title = child_text(m, "Title").filter(|s| non_empty(s));
        let file_path = child_text(m, "FilePath").filter(|s| non_empty(s));
        let id = if file_path.is_some_and(is_internal_id) || (file_path.is_none() && child_bool(m, "Infinite") == Some(true)) {
            self.generated(m, title.unwrap_or(fallback_name), frame)
        } else {
            self.file(m, file_path, title, kind, fallback_name)
        };
        self.media_items.insert(m.id(), id);
        id
    }

    /// Premiere's own generated media (Black Video, Color Matte, …), whose `<FilePath>` is an id.
    fn generated(&mut self, m: Node<'a, 'i>, title: &str, frame: Frame) -> ItemId {
        let lower = title.to_ascii_lowercase();
        let g = match lower.as_str() {
            "black video" => Generator::BlackVideo,
            "transparent video" => Generator::TransparentVideo,
            "color matte" | "colour matte" => {
                self.warn("the colour of a Color Matte was not read; black was used");
                Generator::ColorMatte { color: [0.0, 0.0, 0.0, 1.0] }
            }
            "universal counting leader" | "counting leader" => Generator::CountingLeader,
            n if n.starts_with("bars and tone") || n.starts_with("hd bars and tone") => Generator::BarsAndTone,
            _ => {
                self.warn("titles, graphics and other generated clips were not imported; Transparent Video was used in their place");
                Generator::TransparentVideo
            }
        };
        let spec = MediaSpec { video: Some((frame.width, frame.height, frame.rate)), ..Default::default() };
        let key = format!("prgen:{}:{}", m.attribute("ObjectUID").or(m.attribute("ObjectID")).unwrap_or(""), m.id().get());
        self.b.generator_media(&key, title, g, &spec, None)
    }

    fn file(&mut self, m: Node<'a, 'i>, file_path: Option<&str>, title: Option<&str>, kind: TrackKind, fallback_name: &str) -> ItemId {
        let path = file_path
            .or_else(|| child_text(m, "ActualMediaFilePath").filter(|s| non_empty(s)))
            .or_else(|| child_text(m, "RelativePath").filter(|s| non_empty(s)));
        let name = title.or(path.map(file_name)).filter(|s| non_empty(s)).unwrap_or(fallback_name).to_string();
        let mut spec = MediaSpec::default();
        // Stream values go through the same bounds as sequence settings; unusable ones are said.
        let mut unusable = false;
        if let Some(vs) = self.follow(child(m, "VideoStream"), "media") {
            let rate = match child_text(vs, "FrameRate") {
                None => FrameRate::default(),
                Some(t) => tick_value(t).and_then(rate_from_ticks).unwrap_or_else(|| {
                    unusable = true;
                    FrameRate::default()
                }),
            };
            let (w, h) = match child_text(vs, "FrameRect") {
                None => (1920, 1080),
                Some(r) => frame_rect(r).unwrap_or_else(|| {
                    unusable = true;
                    (1920, 1080)
                }),
            };
            spec.video = Some((w, h, rate));
            spec.duration = child_text(vs, "Duration").and_then(tick_value).filter(|d| *d > 0).map(Tick);
        }
        if let Some(a) = self.follow(child(m, "AudioStream"), "media") {
            let sr = match child_text(a, "FrameRate") {
                None => 48_000,
                Some(t) => tick_value(t).and_then(sample_rate_from_ticks).unwrap_or_else(|| {
                    unusable = true;
                    48_000
                }),
            };
            spec.audio = Some((sr, 2));
            if spec.duration.is_none() {
                spec.duration = child_text(a, "Duration").and_then(tick_value).filter(|d| *d > 0).map(Tick);
            }
        }
        if unusable {
            self.warn("a media file's frame rate, frame size or sample rate is out of range; defaults were used until the file is linked");
        }
        if spec.video.is_none() && spec.audio.is_none() {
            // No stream description: guess from the extension (and the track the clip is on).
            match kind_from_ext(path.unwrap_or(&name)) {
                MediaKind::AudioOnly => spec.audio = Some((48_000, 2)),
                k => {
                    spec.video = Some((1920, 1080, FrameRate::default()));
                    spec.kind = Some(k);
                    if k == MediaKind::Movie || kind == TrackKind::Audio {
                        spec.audio = Some((48_000, 2));
                    }
                }
            }
        }
        match path {
            Some(p) => {
                let p = resolve_path(p, self.base_dir);
                let id = self.b.file_media(&format!("file:{p}"), &name, &p, &spec, None);
                // The path relative to the project, for relinking when the absolute one is missing
                // (a project moved between machines).
                if let Some(rel) = child_text(m, "RelativePath").filter(|s| non_empty(s)).map(|r| resolve_path(r, self.base_dir)).filter(|r| *r != p)
                    && let Some(item) = self.b.p.item_mut(id)
                {
                    item.metadata.entry(RELATIVE_PATH_KEY.to_string()).or_insert(rel);
                }
                id
            }
            None => {
                self.warn("a media file without a path was imported offline");
                let p = resolve_path(&name, self.base_dir);
                let key = format!("prmedia:{}", m.id().get());
                let id = self.b.file_media(&key, &name, &p, &spec, None);
                if let Some(mc) = self.b.p.item_mut(id).and_then(|i| i.as_media_mut()) {
                    mc.offline = true;
                }
                id
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Markers, links, bins
    // -----------------------------------------------------------------------------------------

    /// Markers of an object: every `<Markers>` reference inside its element, each a list of
    /// `<Marker>` objects carrying a `DVAMarker` JSON blob.
    fn markers_of(&mut self, owner: Node<'a, 'i>) -> Vec<Marker> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let refs: Vec<Node<'a, 'i>> = owner.descendants().filter(|d| d.has_tag_name("Markers") && is_ref(*d)).collect();
        for r in refs {
            let Some(list) = self.follow(Some(r), "marker list") else { continue };
            if !seen.insert(list.id()) {
                continue;
            }
            if !self.marker_lists.contains_key(&list.id()) {
                let parsed = self.parse_marker_list(list);
                self.marker_lists.insert(list.id(), parsed);
            }
            let Some(cached) = self.marker_lists.get(&list.id()) else { continue };
            let room = self.limits.markers.saturating_sub(self.markers_made);
            let take = cached.len().min(room);
            let mut copied: Vec<Marker> = cached.iter().take(take).cloned().collect();
            for m in &mut copied {
                m.id = MarkerId(self.b.alloc());
            }
            self.markers_made = self.markers_made.saturating_add(copied.len());
            out.extend(copied);
            if take < self.marker_lists.get(&list.id()).map_or(0, Vec::len) && self.markers_made == self.limits.markers {
                self.markers_made += 1;
                self.warn(format!("the project has more than {} markers; the rest were left out", self.limits.markers));
            }
        }
        out.sort_by_key(|m| m.start);
        out
    }

    /// The markers of one `Markers` list object, read once (ids are given when copied out).
    fn parse_marker_list(&mut self, list: Node<'a, 'i>) -> Vec<Marker> {
        let mut out = Vec::new();
        let entries: Vec<Node<'a, 'i>> = list.descendants().filter(|d| d.has_tag_name("Marker")).collect();
        for e in entries {
            if out.len() >= self.limits.markers {
                break;
            }
            let mk = if is_ref(e) { self.follow(Some(e), "marker") } else { Some(e) };
            let Some(mk) = mk else { continue };
            let Some(json) = child_text(mk, "DVAMarker") else {
                self.warn("a marker without marker data (<DVAMarker>) was skipped");
                continue;
            };
            match marker_from_json(json) {
                Some(mut m) => {
                    if m.start < Tick::ZERO {
                        self.warn("a marker before the start was moved to the start");
                        m.start = Tick::ZERO;
                    }
                    out.push(m);
                }
                None => self.warn("a marker could not be read and was skipped"),
            }
        }
        out
    }
}

/// A marker from a `DVAMarker` JSON blob (id 0: the caller gives it one).
fn marker_from_json(json: &str) -> Option<Marker> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let m = v.get("DVAMarker").unwrap_or(&v);
    let start = json_ticks(m.get("mStartTime"))?;
    let duration = json_ticks(m.get("mDuration")).unwrap_or(0).max(0);
    let text = |k: &str| m.get(k).and_then(|s| s.as_str()).unwrap_or("").to_string();
    let kind = match m.get("mType").and_then(|t| t.as_str()).unwrap_or("") {
        "Chapter" => MarkerKind::Chapter,
        "Segmentation" => MarkerKind::Segmentation,
        "WebLink" => MarkerKind::WebLink,
        "FlashCuePoint" | "FlashCue" => MarkerKind::FlashCue,
        _ => MarkerKind::Comment,
    };
    Some(Marker { id: MarkerId(0), start: Tick(start), duration: Tick(duration), name: text("mName"), comment: text("mComment"), kind, color: Label::Green })
}

impl<'a, 'i, 'r> Imp<'a, 'i, 'r> {
    /// `Link` objects tie a video clip item to its audio: give their clips one link id.
    fn links(&mut self, root: Node<'a, 'i>) {
        // where each clip is: (sequence, video?, track, index), built once
        let mut at: HashMap<ClipId, (ItemId, bool, usize, usize)> = HashMap::new();
        let seqs: HashSet<ItemId> = self.clip_refs.values().map(|(s, _)| *s).collect();
        for seq in seqs {
            let Some(q) = self.b.p.sequence(seq) else { continue };
            for (video, tracks) in [(true, &q.video_tracks), (false, &q.audio_tracks)] {
                for (ti, t) in tracks.iter().enumerate() {
                    for (ci, c) in t.items.iter().enumerate() {
                        at.insert(c.id, (seq, video, ti, ci));
                    }
                }
            }
        }
        for l in elements(root).filter(|n| n.has_tag_name("Link")) {
            let members: Vec<(ItemId, ClipId)> = l
                .descendants()
                .filter(|d| d.has_tag_name("TrackItem"))
                .filter_map(|d| d.attribute("ObjectRef"))
                .filter_map(|r| self.clip_refs.get(r).copied())
                .collect();
            if members.len() < 2 {
                continue;
            }
            let link = self.b.link_id();
            for (_, clip) in members {
                let Some(&(seq, video, ti, ci)) = at.get(&clip) else { continue };
                let Some(s) = self.b.p.sequence_mut(seq) else { continue };
                let tracks = if video { &mut s.video_tracks } else { &mut s.audio_tracks };
                if let Some(c) = tracks.get_mut(ti).and_then(|t| t.items.get_mut(ci)) {
                    c.link = Some(link);
                }
            }
        }
    }

    /// The project item a master clip stands for: its media file or sequence.
    fn master_item(&mut self, mc: Node<'a, 'i>) -> Option<ItemId> {
        if let Some(id) = self.master_items.get(&mc.id()) {
            return *id;
        }
        // Mark it first: a damaged file could lead back here.
        self.master_items.insert(mc.id(), None);
        let name = child_text(mc, "Name").filter(|s| non_empty(s)).unwrap_or("Clip").to_string();
        let frame = Frame { width: 1920, height: 1080, rate: FrameRate::default() };
        let mut found = None;
        for r in indexed(child(mc, "Clips"), "Clip") {
            let Some(c) = self.follow(Some(r), "master clip entry") else { continue };
            let kind = if c.has_tag_name("AudioClip") { TrackKind::Audio } else { TrackKind::Video };
            let src = child(c, "Clip").and_then(|i| child(i, "Source"));
            let Some(src) = self.follow(src, "clip") else { continue };
            found = match src.tag_name().name() {
                "VideoMediaSource" | "AudioMediaSource" => {
                    let media = child(src, "MediaSource").and_then(|m| child(m, "Media"));
                    self.follow(media, "media source").map(|m| self.media(m, kind, &name, frame))
                }
                "VideoSequenceSource" | "AudioSequenceSource" => {
                    let s = child(src, "SequenceSource").and_then(|q| child(q, "Sequence"));
                    self.follow(s, "nested sequence clip").filter(|s| s.has_tag_name("Sequence")).and_then(|s| self.sequence(s, 0))
                }
                other => {
                    self.warn(format!("Project panel clips with a {other} source are not supported and were not imported"));
                    None
                }
            };
            if found.is_some() {
                break;
            }
        }
        self.master_items.insert(mc.id(), found);
        found
    }

    /// The Project panel tree, walked with an explicit stack. Bins deeper than
    /// [`MAX_BIN_DEPTH`] are flattened: their contents go into the deepest bin kept.
    fn bins(&mut self, project: Node<'a, 'i>) {
        let Some(root_item) = self.follow(child(project, "RootProjectItem"), "project") else { return };
        let mut seen = HashSet::new();
        seen.insert(root_item.id());
        // (container, its bin, its depth); children are pushed in reverse to keep document order
        let mut stack: Vec<(Node<'a, 'i>, Option<BinId>, usize)> = vec![(root_item, None, 0)];
        let mut flattened = false;
        while let Some((container, bin, depth)) = stack.pop() {
            let items = child(container, "ProjectItemContainer").and_then(|c| child(c, "Items"));
            let mut sub_bins = Vec::new();
            for r in indexed(items, "Item") {
                let Some(n) = self.follow(Some(r), "bin") else { continue };
                if !seen.insert(n.id()) {
                    self.warn("a bin lists an item twice or contains itself; the repeat was skipped");
                    continue;
                }
                match n.tag_name().name() {
                    "BinProjectItem" => {
                        if depth >= MAX_BIN_DEPTH {
                            flattened = true;
                            sub_bins.push((n, bin, depth));
                        } else {
                            let name = child(n, "ProjectItem").and_then(|p| child_text(p, "Name")).filter(|s| non_empty(s)).unwrap_or("Bin");
                            let b = self.b.bin(name, bin);
                            sub_bins.push((n, Some(b), depth + 1));
                        }
                    }
                    "ClipProjectItem" => {
                        let Some(mc) = self.follow(child(n, "MasterClip"), "project panel clip") else { continue };
                        let Some(id) = self.master_item(mc) else { continue };
                        // An item goes to the first bin that lists it.
                        if let Some(b) = bin
                            && self.b.p.root.parent_of(id) == Some(self.b.p.root.id)
                        {
                            self.b.p.move_to_bin(&[id], Some(b));
                        }
                    }
                    other => self.warn(format!("{other} entries in the Project panel were not imported")),
                }
            }
            stack.extend(sub_bins.into_iter().rev());
        }
        if flattened {
            self.warn(format!("bins nested more than {MAX_BIN_DEPTH} deep were flattened into the deepest bin kept"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
  <Project ObjectRef="1"/>
  <Project ObjectID="1" ClassID="62ad66dd-0dcd-42da-a660-6d8fbde94876" Version="43"/>
  <Sequence ObjectUID="s"><Name>S</Name></Sequence>
</PremiereData>"#;

    #[test]
    fn root_detection_skips_prolog_comments_and_bom() {
        assert!(root_is_premiere(b"<?xml version=\"1.0\"?>\n<!-- c --><PremiereData Version=\"3\">"));
        assert!(root_is_premiere("\u{feff}  <PremiereData>".as_bytes()));
        assert!(!root_is_premiere(b"<?xml version=\"1.0\"?><PremiereDataX>"));
        assert!(!root_is_premiere(b"<?xml version=\"1.0\"?><xmeml version=\"4\">"));
        assert!(!root_is_premiere(b"<?xml unterminated"));
        assert!(!root_is_premiere(b""));
    }

    #[test]
    fn size_cap_is_an_error_plain_and_gzip() {
        use std::io::Write;
        let big = format!("<?xml version=\"1.0\"?><PremiereData>{}</PremiereData>", " ".repeat(10_000));
        assert!(decode(big.as_bytes(), 1 << 20).is_ok());
        assert!(matches!(decode(big.as_bytes(), 1000), Err(Error::Parse { .. })));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(big.as_bytes()).unwrap();
        let gz = gz.finish().unwrap();
        assert!(decode(&gz, 1 << 20).is_ok());
        let e = decode(&gz, 1000).unwrap_err();
        assert!(e.to_string().contains("larger than"), "{e}");
    }

    #[test]
    fn depth_scan_counts_like_the_parser() {
        let nest =
            |n: usize| format!("<?xml version=\"1.0\"?><!-- <a><a> --><r a=\"<x>\" b='/>'>{}<e/><![CDATA[<a><a>]]>{}</r>", "<a>".repeat(n), "</a>".repeat(n));
        assert!(check_depth(nest(8).as_bytes(), 9).is_ok());
        assert!(check_depth(nest(9).as_bytes(), 9).is_err());
        assert!(check_depth(b"<a><b/><b/><b/></a>", 1).is_ok());
        assert!(check_depth(b"</a></a><a><a>", 2).is_ok());
        assert!(check_depth(b"<a", 0).is_ok());
        assert!(check_depth(b"", 0).is_ok());
    }

    /// One clip item referenced 30 times, with a marker list of six (one unreadable) that every
    /// reference shares: the item and marker budgets stop the copies, each warns once, and the
    /// list is parsed once (the unreadable marker is reported once, not per reference).
    #[test]
    fn item_and_marker_budgets_bound_repeated_references() {
        let refs: String = (0..30).map(|i| format!(r#"<TrackItem Index="{i}" ObjectRef="ti"/>"#)).collect();
        let marks: String =
            (0..5).map(|i| format!(r#"<Marker ObjectID="mk{i}"><DVAMarker>{{"DVAMarker":{{"mStartTime":{{"ticks":{i}}}}}}}</DVAMarker></Marker>"#)).collect();
        let doc = format!(
            r#"<?xml version="1.0"?><PremiereData Version="3"><Project ObjectID="1" Version="43"/>
<Sequence ObjectUID="s"><TrackGroups><TrackGroup Index="0"><Second ObjectRef="g"/></TrackGroup></TrackGroups><Name>S</Name></Sequence>
<VideoTrackGroup ObjectID="g"><TrackGroup><Tracks><Track Index="0" ObjectURef="t"/></Tracks><FrameRate>10160640000</FrameRate></TrackGroup></VideoTrackGroup>
<VideoClipTrack ObjectUID="t"><ClipTrack><ClipItems><TrackItems>{refs}</TrackItems></ClipItems></ClipTrack></VideoClipTrack>
<VideoClipTrackItem ObjectID="ti"><ClipTrackItem><TrackItem><End>254016000000</End></TrackItem><Owner><Markers ObjectRef="ml"/></Owner></ClipTrackItem></VideoClipTrackItem>
{marks}<Marker ObjectID="bad"><DVAMarker>not json</DVAMarker></Marker>
<Markers ObjectID="ml"><L><Marker ObjectRef="mk0"/><Marker ObjectRef="mk1"/><Marker ObjectRef="mk2"/><Marker ObjectRef="mk3"/><Marker ObjectRef="mk4"/><Marker ObjectRef="bad"/></L></Markers>
</PremiereData>"#
        );
        let limits = Limits { nodes: MAX_NODES, items: 10, markers: 25 };
        let (im, report) = import_limited(doc.as_bytes(), &ImportOptions::default(), limits).unwrap();
        let seq = im.project.items.values().find_map(|i| i.as_sequence()).unwrap();
        let clips: Vec<&TrackItem> = seq.video_tracks.iter().flat_map(|t| &t.items).collect();
        assert_eq!(clips.len(), 10);
        let markers: usize = clips.iter().map(|c| c.markers.len()).sum();
        assert_eq!(markers, 25);
        let count = |needle: &str| report.entries.iter().filter(|e| e.message.contains(needle)).map(|e| e.count).sum::<u32>();
        assert_eq!(count("more than 10 clip items"), 1, "{report}");
        assert_eq!(count("more than 25 markers"), 1, "{report}");
        assert_eq!(count("marker could not be read"), 1, "the shared list is parsed once: {report}");
        assert_eq!(count("not in the project file"), 0, "{report}");
        // every marker copy has its own id
        let mut ids: Vec<u64> = clips.iter().flat_map(|c| c.markers.iter().map(|m| m.id.0)).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 25);
    }

    #[test]
    fn node_cap_is_an_error() {
        assert!(import_limited(SMALL.as_bytes(), &ImportOptions::default(), Limits { nodes: MAX_NODES, items: MAX_ITEMS, markers: MAX_MARKERS }).is_ok());
        let e = import_limited(SMALL.as_bytes(), &ImportOptions::default(), Limits { nodes: 5, items: MAX_ITEMS, markers: MAX_MARKERS }).unwrap_err();
        assert!(matches!(e, Error::Parse { .. }), "{e:?}");
    }

    #[test]
    fn rates_from_ticks_are_exact() {
        assert_eq!(rate_from_ticks(10_160_640_000), Some(FrameRate::FPS_25));
        assert_eq!(rate_from_ticks(10_594_584_000), Some(FrameRate::FPS_23_976));
        assert_eq!(rate_from_ticks(8_475_667_200), Some(FrameRate::FPS_29_97));
        assert_eq!(rate_from_ticks(0), None);
        assert_eq!(rate_from_ticks(-5), None);
        assert_eq!(rate_from_ticks(i64::MAX), None);
        assert_eq!(sample_rate_from_ticks(5_292_000), Some(48_000));
        assert_eq!(sample_rate_from_ticks(5_760_000), Some(44_100));
        assert_eq!(sample_rate_from_ticks(0), None);
        assert_eq!(frame_rect("0,0,1080,1920"), Some((1080, 1920)));
        assert_eq!(frame_rect("0,0,-1,5"), None);
        assert_eq!(frame_rect("9223372036854775807,0,-9223372036854775808,1"), None);
        assert_eq!(frame_rect("1,2,3"), None);
        assert_eq!(tick_value("9223372036854775807"), None);
        assert_eq!(tick_value("-9223372036854775808"), None);
        assert_eq!(tick_value(" 42 "), Some(42));
    }
}
