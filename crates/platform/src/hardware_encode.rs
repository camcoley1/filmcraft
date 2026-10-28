//! The VideoToolbox H.264 and HEVC encoders behind `filmcraft_export::VideoEncoder` (macOS; safe code
//! around [`crate::videotoolbox_encode`]).
//!
//! H.264 hardware encoding is opt-in per export (`ExportSettings::hardware_encoding`, `Auto`): its output
//! depends on the machine, while the built-in encoder's is byte-identical everywhere. The factory
//! declines, so the built-in encoder is used, for everything the hardware path does not take: other
//! formats, MXF (Annex B), two-pass VBR, HDR H.264, odd picture sizes, non-square pixels, and any
//! configuration VideoToolbox cannot create a hardware session for. A hardware encoder that fails in
//! the middle of an export reports an error (the export stops); it cannot hand the stream over to
//! another encoder.
//!
//! HEVC (`Format::Hevc`) has no built-in encoder, so choosing the format is the opt-in, the same
//! configurations are declined, and a declined export fails with "encoder not available" instead of
//! falling back; [`hevc_available`] tells the format list whether this machine can do it at all.
//! HDR (PQ / HLG) sequences export as HEVC Main 10 (BT.2020, 10-bit 4:2:0, with the HDR10 static
//! metadata for PQ) where [`hevc_main10_available`] says the encoder takes 10-bit pictures; elsewhere the
//! export crate tone-maps them to SDR, as before.
//!
//! B-frames (frame reordering) are used when `ExportSettings::hardware_b_frames` asks for them: the
//! samples then carry composition offsets (`ctts`) and the track an edit list that starts the
//! presentation at the first picture.

use std::sync::{Once, OnceLock};

use filmcraft_export::{
    BitrateMode, ColorSignal, EncodedPacket, EncoderFrame, ExportError, ExportSettings, Format, H264Pass, H264Profile, HardwareEncoding, VideoEncoder,
};
use filmcraft_isobmff::{AvcConfig, HevcConfig, SampleEntry};
use filmcraft_time::FrameRate;

use crate::videotoolbox_encode::{ParamSets, VtCodec, VtColor, VtConfig, VtEncoder, VtPacket, VtProfile, VtRate};

/// The hardware encoder's configuration for an export, or why it is not used for it.
pub fn config_for(w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Result<VtConfig, String> {
    let hevc = s.format == Format::Hevc;
    if !hevc && s.hardware_encoding != HardwareEncoding::Auto {
        return Err("hardware encoding is off".into());
    }
    if s.format.is_mxf() {
        return Err("MXF carries an Annex B stream".into());
    }
    if s.bitrate_mode == BitrateMode::Vbr2Pass || !matches!(s.h264_pass, H264Pass::Single) {
        return Err("two-pass encoding".into());
    }
    if s.bitrate_mode == BitrateMode::Crf {
        return Err("CRF (constant quality)".into());
    }
    let hdr = s.signal.is_hdr();
    if hdr && !hevc {
        return Err("HDR (H.264 here is 8-bit: the built-in encoder signals it)".into());
    }
    // HEVC: BT.709 SDR (Main) or BT.2020 NCL with PQ / HLG (Main 10), as NVENC writes
    if hdr && (s.signal.primaries != 9 || s.signal.matrix != 9) {
        return Err(format!("the colour signal {:?} (HDR H.265 here is BT.2020 with PQ / HLG)", s.signal));
    }
    if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err("odd picture size".into());
    }
    if s.pixel_aspect.is_some_and(|(n, d)| n != d) {
        return Err("non-square pixels".into());
    }
    let timescale = u32::try_from(rate.num).ok().filter(|n| *n > 0).ok_or("invalid frame rate")?;
    let frame_duration = u32::try_from(rate.den).ok().filter(|d| *d > 0).ok_or("invalid frame rate")?;
    let kbps = s.bitrate_kbps.max(100);
    let max_kbps = s.max_bitrate_kbps.filter(|m| *m >= kbps).unwrap_or(kbps.saturating_mul(3) / 2);
    let fps = f64::from(timescale) / f64::from(frame_duration);
    Ok(VtConfig {
        width: w,
        height: h,
        timescale,
        frame_duration,
        keyframe_interval: s.keyframe_distance.filter(|k| *k > 0).unwrap_or_else(|| (fps * 2.0).round().max(1.0) as u32),
        profile: match (hevc, s.h264_profile) {
            (true, _) if hdr => VtProfile::HevcMain10,
            (true, _) => VtProfile::HevcMain,
            (false, H264Profile::Baseline) => VtProfile::Baseline,
            (false, H264Profile::Main) => VtProfile::Main,
            (false, H264Profile::High) => VtProfile::High,
        },
        rate: match s.bitrate_mode {
            BitrateMode::Cbr => VtRate::Cbr { kbps },
            _ => VtRate::Vbr { target_kbps: kbps, max_kbps },
        },
        b_frames: s.hardware_b_frames,
        color: vt_color(&s.signal),
    })
}

/// The stream's colour description for an export's signal: BT.709, or BT.2020 with PQ and the HDR10
/// static metadata (the values and bytes of the `mdcv` / `clli` boxes: [`ColorSignal::static_metadata`])
/// or with HLG.
fn vt_color(signal: &ColorSignal) -> VtColor {
    match (signal.is_hdr(), signal.static_metadata()) {
        (false, _) => VtColor::Bt709,
        (true, Some((md, (max_cll, max_fall)))) => {
            let mut content_light = max_cll.to_be_bytes().to_vec();
            content_light.extend_from_slice(&max_fall.to_be_bytes());
            VtColor::Bt2020Pq { mastering_display: md.to_bytes(), content_light }
        }
        (true, None) => VtColor::Bt2020Hlg,
    }
}

static HEVC_PROBE: OnceLock<bool> = OnceLock::new();

/// Whether this machine has a hardware HEVC encoder (asked once, by creating a small session, which
/// takes about 0.1 s): what makes `Format::Hevc` available. [`warm_hevc_probe`] asks it ahead of the
/// first caller.
pub fn hevc_available() -> bool {
    *HEVC_PROBE.get_or_init(|| {
        let probe = VtConfig {
            width: 640,
            height: 360,
            timescale: 30,
            frame_duration: 1,
            keyframe_interval: 30,
            profile: VtProfile::HevcMain,
            rate: VtRate::Vbr { target_kbps: 2000, max_kbps: 3000 },
            b_frames: false,
            color: VtColor::Bt709,
        };
        let result = VtEncoder::new(probe);
        if let Err(why) = &result {
            log::info!("no hardware HEVC encoder: {why}");
        }
        result.is_ok()
    })
}

static MAIN10_PROBE: OnceLock<bool> = OnceLock::new();

/// Whether this machine's hardware HEVC encoder takes 10-bit pictures (HEVC Main 10, PQ-tagged), asked
/// once by creating a small session: what lets an HDR sequence export as HDR H.265
/// (`filmcraft_export::register_hdr_probe`). False without a hardware HEVC encoder at all.
pub fn hevc_main10_available() -> bool {
    *MAIN10_PROBE.get_or_init(|| {
        if !hevc_available() {
            return false;
        }
        let probe = VtConfig {
            width: 640,
            height: 360,
            timescale: 30,
            frame_duration: 1,
            keyframe_interval: 30,
            profile: VtProfile::HevcMain10,
            rate: VtRate::Vbr { target_kbps: 2000, max_kbps: 3000 },
            b_frames: false,
            color: vt_color(&ColorSignal::PQ),
        };
        let result = VtEncoder::new(probe);
        if let Err(why) = &result {
            log::info!("no hardware HEVC Main 10 encoder: {why}");
        }
        result.is_ok()
    })
}

/// Whether [`hevc_available`] has its answer yet (diagnostics and tests).
pub fn hevc_probed() -> bool {
    HEVC_PROBE.get().is_some()
}

/// Ask [`hevc_available`] now, on a thread of its own, so that the first draw of the format list does
/// not spend its 0.1 s creating a hardware session on the UI thread. Whoever asks while the thread
/// runs waits for the same answer (no longer than asking first); if the thread cannot start, or the
/// probe panics, the first caller asks, as before. Only the first call starts a thread.
pub fn warm_hevc_probe() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new().name("filmcraft-hevc-probe".into()).spawn(|| {
            let _ = std::panic::catch_unwind(hevc_available);
            let _ = std::panic::catch_unwind(hevc_main10_available);
        });
        if let Err(why) = spawned {
            log::info!("HEVC probe not started ahead of time: {why}");
        }
    });
}

/// The encoder factory `register` puts in front of the built-in ones: a hardware encoder when the
/// export asks for one (H.264 with `hardware_encoding`; HEVC always) and the OS takes the
/// configuration, `None` (the built-in encoder, if there is one) otherwise.
pub fn videotoolbox_encoder_factory(
    format: Format,
    w: u32,
    h: u32,
    rate: FrameRate,
    s: &ExportSettings,
) -> Option<filmcraft_export::Result<Box<dyn VideoEncoder>>> {
    let name = match format {
        Format::H264 if s.hardware_encoding == HardwareEncoding::Auto => "H.264",
        Format::Hevc => "HEVC",
        _ => return None,
    };
    let config = match config_for(w, h, rate, s) {
        Ok(c) => c,
        Err(why) => {
            log::info!("hardware {name} encoding declined: {why}");
            filmcraft_export::note_hw_encode_declined();
            return None;
        }
    };
    match VtEncoder::new(config) {
        Ok(vt) => {
            filmcraft_export::note_hw_encode_session();
            Some(Ok(Box::new(HardwareEncoder::new(vt, s.signal))))
        }
        Err(why) => {
            log::info!("hardware {name} encoding declined: {why}");
            filmcraft_export::note_hw_encode_declined();
            None
        }
    }
}

/// The `hvcC` configuration of an HEVC stream: VideoToolbox's own record when it wrote one.
fn hevc_config(p: &ParamSets) -> Result<HevcConfig, String> {
    let record = p.hvcc.as_deref().ok_or("VideoToolbox wrote no hvcC record for the stream")?;
    let cfg = HevcConfig::parse(record).map_err(|e| format!("unreadable hvcC record: {e}"))?;
    if cfg.vps().is_empty() || cfg.sps().is_empty() || cfg.pps().is_empty() || cfg.length_size != 4 {
        return Err("the hvcC record lacks a parameter set or does not use 4-byte NAL lengths".into());
    }
    Ok(cfg)
}

/// Pictures a reordered stream may hold back at once (a presentation index at most this far after
/// its decoding position; H.264 and HEVC allow at most 16 reorder and 16 buffered pictures).
const MAX_LEAD: i64 = 32;

/// The MP4 timeline of the compressed frames, in decoding order, and the checks that every picture
/// comes out exactly once.
///
/// Frame `n` is decoded at `n × step` on the MP4 decode timeline and presented `bound` frames after
/// its presentation index, so its composition offset is `(index + bound - n) × step`, never negative;
/// the edit list starts at `bound`, where the first picture (presentation time 0) sits. `bound` is the
/// stream's declared reorder depth (its SPS), known from the first frame; once every frame is in,
/// [`Timeline::depth`] is the depth the stream really used and the muxer moves the presentation
/// back by the difference (`Mp4Writer::rebase_composition`), so a player that ignores edit lists is
/// late by the real reorder delay only. (VideoToolbox's own decode times are not used: on reordered
/// streams it reports `n × step` from 0, which puts some pictures' presentation before their decoding.)
#[derive(Debug, Default)]
struct Timeline {
    step: i64,
    reorder: bool,
    /// Declared reorder depth (frames): the composition offsets as written are relative to it.
    bound: i64,
    /// Compressed frames returned so far.
    decoded: i64,
    /// Every presentation index below this came out once.
    next: i64,
    /// Presentation indices at or above `next` that came out (ahead of a lower one still missing).
    pending: std::collections::BTreeSet<i64>,
    /// Deepest reorder seen: max over frames of (decoding position − presentation index).
    depth: i64,
}

impl Timeline {
    fn new(step: u32, reorder: bool) -> Self {
        Timeline { step: i64::from(step).max(1), reorder, ..Default::default() }
    }

    /// The composition offset of the next frame (decoding order), or why the frame does not fit.
    fn place(&mut self, pts: i64, dts: i64) -> Result<i32, String> {
        let n = self.decoded;
        let what = |why: &str| format!("VideoToolbox returned frame {n} (in decoding order) at time {pts} (decode time {dts}): {why}");
        let index = if pts >= 0 && pts % self.step == 0 { pts / self.step } else { return Err(what("not on the frame grid")) };
        if !self.reorder && (index != n || pts != dts) {
            return Err(what("out of order"));
        }
        if index < self.next || self.pending.contains(&index) {
            return Err(what("a picture that already came out"));
        }
        if n.saturating_sub(index) > self.bound {
            return Err(what("reordered deeper than the stream allows"));
        }
        if index.saturating_sub(n) > MAX_LEAD || self.pending.len() as i64 >= MAX_LEAD {
            return Err(what("too far ahead of a missing picture"));
        }
        let offset = index.checked_add(self.bound).and_then(|i| i.checked_sub(n)).and_then(|f| f.checked_mul(self.step)).and_then(|o| i32::try_from(o).ok());
        let offset = offset.ok_or_else(|| what("composition offset out of range"))?;
        self.pending.insert(index);
        while self.pending.remove(&self.next) {
            self.next += 1;
        }
        self.depth = self.depth.max(n - index);
        self.decoded += 1;
        Ok(offset)
    }

    /// After the last frame: every one of the `submitted` pictures came out exactly once.
    fn finish(&self, submitted: u64) -> Result<(), String> {
        let submitted = i64::try_from(submitted).unwrap_or(i64::MAX);
        if self.decoded != submitted || self.next != submitted || !self.pending.is_empty() {
            return Err(format!(
                "VideoToolbox returned {} of {submitted} pictures ({} in presentation order before the first missing one)",
                self.decoded, self.next
            ));
        }
        Ok(())
    }
}

/// [`VtEncoder`] as a `VideoEncoder`: RGBA (or, for Main 10, HDR R'G'B') pictures in, MP4 samples out.
pub struct HardwareEncoder {
    vt: VtEncoder,
    /// The stream's colour signal (`colr` / `mdcv` / `clli` on the sample entry for HDR).
    signal: ColorSignal,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// Main 10: the 10-bit planes of the current picture.
    y16: Vec<u16>,
    u16: Vec<u16>,
    v16: Vec<u16>,
    timeline: Timeline,
    /// Every frame is out (`flush` ran): [`VideoEncoder::media_start`] answers the measured depth.
    flushed: bool,
}

impl HardwareEncoder {
    fn new(vt: VtEncoder, signal: ColorSignal) -> Self {
        let timeline = Timeline::new(vt.config().frame_duration, vt.config().b_frames);
        HardwareEncoder { vt, signal, y: Vec::new(), u: Vec::new(), v: Vec::new(), y16: Vec::new(), u16: Vec::new(), v16: Vec::new(), timeline, flushed: false }
    }

    /// MP4 samples from compressed frames (decoding order), timed by [`Timeline`]. A dropped,
    /// duplicated or misplaced picture is an error, never a file with wrong times.
    fn samples(&mut self, packets: Vec<VtPacket>) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        let duration = self.vt.config().frame_duration;
        let mut out = Vec::with_capacity(packets.len());
        for p in packets {
            let offset = self.timeline.place(p.pts, p.dts).map_err(ExportError::Encode)?;
            out.push(EncodedPacket { data: p.data, key: p.key, duration, composition_offset: offset });
        }
        Ok(out)
    }

    /// The reorder depth of the stream (frames), from the SPS of its parameter sets: how many pictures
    /// may precede a picture in decoding order and follow it in presentation order.
    fn reorder_depth(codec: VtCodec, p: &ParamSets) -> Result<usize, String> {
        let info = match codec {
            VtCodec::H264 => filmcraft_codecs::hw::NalStreamInfo::from_avcc(&AvcConfig::new(p.sps.clone(), p.pps.clone(), 4).to_bytes()),
            VtCodec::Hevc => filmcraft_codecs::hw::NalStreamInfo::from_hvcc(p.hvcc.as_deref().unwrap_or_default()),
        };
        let reorder = info.map_err(|e| format!("cannot read the stream's reorder depth: {e}"))?.reorder;
        // H.264 allows at most 16 reference frames; anything deeper is not a stream we wrote
        if reorder > 16 {
            return Err(format!("reorder depth {reorder}"));
        }
        Ok(reorder)
    }
}

impl VideoEncoder for HardwareEncoder {
    fn sample_entry(&self) -> SampleEntry {
        let c = self.vt.config();
        let (w, h) = (c.width.min(u32::from(u16::MAX)) as u16, c.height.min(u32::from(u16::MAX)) as u16);
        match (c.profile.codec(), self.vt.parameter_sets()) {
            (VtCodec::H264, p) => {
                let (sps, pps) = p.map(|p| (p.sps, p.pps)).unwrap_or_default();
                SampleEntry::avc(AvcConfig::new(sps, pps, 4), w, h)
            }
            // `encode` refuses to go on without a usable record, so a stream that got this far has one
            (VtCodec::Hevc, p) => {
                let mut e = SampleEntry::hevc(p.as_ref().and_then(|p| hevc_config(p).ok()).unwrap_or_default(), w, h);
                // HDR: `colr` (BT.2020 + PQ / HLG) and, for PQ, `mdcv` / `clli`, as NVENC and the software H.264
                // encoder write them (nclx, whatever the container). SDR stays as it was, with the VUI only.
                if self.signal.is_hdr() {
                    self.signal.apply_to(&mut e, false);
                }
                e
            }
        }
    }

    fn timescale(&self) -> u32 {
        self.vt.config().timescale
    }

    fn encode(&mut self, f: &EncoderFrame) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        let c = self.vt.config();
        let codec = c.profile.codec();
        if (f.width, f.height) != (c.width, c.height) {
            return Err(ExportError::Encode(format!("picture is {}x{}, the encoder was created for {}x{}", f.width, f.height, c.width, c.height)));
        }
        let (w, h) = (f.width as usize, f.height as usize);
        let packets = match (c.profile.is_ten_bit(), f.hdr) {
            // Main 10 takes the encoded R'G'B' floats: 10-bit limited-range BT.2020 NCL (or the signal's matrix)
            (true, Some(rgb)) => {
                let (kr, kb) = self.signal.kr_kb();
                filmcraft_export::timed(filmcraft_export::Stage::Convert, || {
                    filmcraft_export::rgbf_to_yuv420_10(rgb, w, h, kr, kb, &mut self.y16, &mut self.u16, &mut self.v16)
                })?;
                self.vt.encode_10(f.index, &self.y16, &self.u16, &self.v16).map_err(ExportError::Encode)?
            }
            (true, None) => return Err(ExportError::Unsupported("HEVC Main 10 needs the HDR picture (EncoderFrame::hdr)".into())),
            (false, Some(_)) => return Err(ExportError::Unsupported("hardware encoding does not take HDR pictures in 8 bits".into())),
            (false, None) => {
                if f.rgba.len() < w.saturating_mul(h).saturating_mul(4) {
                    return Err(ExportError::Encode("the RGBA picture is smaller than its size".into()));
                }
                filmcraft_export::timed(filmcraft_export::Stage::Convert, || {
                    filmcraft_export::rgba_to_yuv420_8(f.rgba, w, h, &mut self.y, &mut self.u, &mut self.v)
                });
                self.vt.encode(f.index, &self.y, &self.u, &self.v).map_err(ExportError::Encode)?
            }
        };
        filmcraft_export::note_hw_encode_frame();
        // the muxer needs the parameter sets after the first group of pictures
        if f.index == 0 {
            let params = self.vt.parameter_sets().ok_or_else(|| ExportError::Encode("VideoToolbox returned no parameter sets for the first frame".into()))?;
            if codec == VtCodec::Hevc {
                hevc_config(&params).map_err(ExportError::Encode)?;
            }
            if self.vt.config().b_frames {
                let depth = Self::reorder_depth(codec, &params).map_err(ExportError::Encode)?;
                self.timeline.bound = i64::try_from(depth).unwrap_or(16);
            }
        }
        self.samples(packets)
    }

    fn flush(&mut self) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        let packets = self.vt.flush().map_err(ExportError::Encode)?;
        let out = self.samples(packets)?;
        self.timeline.finish(self.vt.submitted()).map_err(ExportError::Encode)?;
        self.flushed = true;
        Ok(out)
    }

    fn media_start(&self) -> Option<i64> {
        // with B-frames presentation runs behind decoding: the edit list starts it at the first picture.
        // Before the last frame that is the declared depth (what the offsets are written against); after
        // it, the depth the stream used, which the export moves the presentation back to.
        let frames = if self.flushed { self.timeline.depth } else { self.timeline.bound };
        (frames > 0).then(|| frames.saturating_mul(self.timeline.step))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (presentation index, decoding position) of an HEVC-style group of pictures, as VideoToolbox
    /// hands them over: decoding order, two deep.
    const GOP: [i64; 10] = [0, 4, 2, 1, 3, 5, 9, 7, 6, 8];
    const STEP: u32 = 1001;

    fn run(timeline: &mut Timeline, indices: &[i64]) -> Result<Vec<i32>, String> {
        indices.iter().enumerate().map(|(n, i)| timeline.place(i * i64::from(STEP), n as i64 * i64::from(STEP))).collect()
    }

    fn reordered(bound: i64) -> Timeline {
        Timeline { bound, ..Timeline::new(STEP, true) }
    }

    #[test]
    fn a_reordered_stream_gets_non_negative_offsets_and_its_measured_depth() {
        let mut t = reordered(4);
        let offsets = run(&mut t, &GOP).unwrap();
        // offset = (index + bound - n) x step: never negative, presentation after decoding
        assert_eq!(offsets[0], 4 * 1001);
        assert!(offsets.iter().all(|o| *o >= 0));
        assert_eq!(t.depth, 2, "picture 1 is decoded third");
        t.finish(10).unwrap();
        // without B-frames: in order, offset 0
        let mut t = Timeline::new(STEP, false);
        assert_eq!(run(&mut t, &[0, 1, 2, 3]).unwrap(), vec![0; 4]);
        assert_eq!(t.depth, 0);
        t.finish(4).unwrap();
    }

    #[test]
    fn a_duplicated_picture_is_an_error() {
        let mut t = reordered(2);
        let e = run(&mut t, &[0, 2, 1, 2]).unwrap_err();
        assert!(e.contains("already came out"), "{e}");
        // also once the window has moved past it
        let mut t = reordered(2);
        assert!(run(&mut t, &[0, 2, 1, 3, 1]).unwrap_err().contains("already came out"));
        let mut t = Timeline::new(STEP, false);
        assert!(run(&mut t, &[0, 1, 1]).is_err());
    }

    #[test]
    fn a_missing_picture_is_an_error() {
        // a hole the stream never fills: found when the last frame is in
        let mut t = reordered(2);
        run(&mut t, &[0, 2, 1, 4, 3, 6, 7]).unwrap();
        let e = t.finish(8).unwrap_err();
        assert!(e.contains("7 of 8") && e.contains("5 in presentation order"), "{e}");
        // fewer frames out than went in
        let mut t = reordered(2);
        run(&mut t, &[0, 2, 1]).unwrap();
        assert!(t.finish(4).is_err());
        // a hole that keeps the window from moving: found as soon as too much is pending
        let mut t = reordered(2);
        let far: Vec<i64> = std::iter::once(0).chain(2..3 + MAX_LEAD).collect();
        assert!(run(&mut t, &far).unwrap_err().contains("too far ahead"));
        // without B-frames a skipped frame is out of order at once
        let mut t = Timeline::new(STEP, false);
        assert!(run(&mut t, &[0, 2]).unwrap_err().contains("out of order"));
    }

    #[test]
    fn a_picture_deeper_than_the_stream_declares_or_off_the_grid_is_an_error() {
        // picture 1 decoded third is two deep: a stream that declares one is lying
        let mut t = reordered(1);
        assert!(run(&mut t, &[0, 4, 2, 1]).unwrap_err().contains("deeper"));
        // a time that is not a whole number of frames, or before the start
        let mut t = reordered(2);
        assert!(t.place(500, 0).unwrap_err().contains("frame grid"));
        assert!(t.place(-1001, 0).unwrap_err().contains("frame grid"));
        // a picture far ahead of its decoding position
        let mut t = reordered(2);
        assert!(t.place((MAX_LEAD + 1) * 1001, 0).unwrap_err().contains("too far ahead"));
        // hostile times never overflow
        let mut t = reordered(16);
        let _ = t.place(i64::MAX - i64::MAX % 1001, 0);
        let mut t = Timeline { bound: i64::MAX, ..Timeline::new(STEP, true) };
        assert!(t.place(0, 0).is_err(), "offset out of range");
    }
}
