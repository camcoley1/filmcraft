//! `ai.stems` against the fake helper: the four stems in a bin, placed under the clip, one undo
//! step; a stem of the wrong length, an over-long source, a timeout and a cancel change nothing.

use filmcraft_project::{BinEntry, ItemId, ItemKind, TrackKind};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::json;

use super::super::tests::{Fixture, finish, fixture, job_error, reason, snapshot};

/// A mono 16-bit 8 kHz WAV of `seconds` of silence.
fn wav(seconds: f64) -> Vec<u8> {
    let n = (8000.0 * seconds) as u32;
    let data = n * 2;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&8000u32.to_le_bytes());
    b.extend_from_slice(&16000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    b.resize(44 + data as usize, 0);
    b
}

/// What the last `stems` job received: (rate, channels, data bytes) of its `source.wav`.
static SOURCE: std::sync::Mutex<Option<(u32, u16, usize)>> = std::sync::Mutex::new(None);

/// The fixture's source is 2 s long.
fn stems(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    if let Ok(b) = std::fs::read(&req.inputs[0]) {
        *SOURCE.lock().unwrap() = Some((u32::from_le_bytes(b[24..28].try_into().unwrap()), u16::from_le_bytes(b[22..24].try_into().unwrap()), b.len() - 44));
    }
    ["vocals", "drums", "bass", "other"].iter().map(|s| (format!("{s}.wav"), wav(2.0))).collect()
}

/// The helper's input handed back as every stem (an identity separation): shows where the
/// decoded audio lands.
fn identity(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    let b = std::fs::read(&req.inputs[0]).unwrap_or_default();
    ["vocals", "drums", "bass", "other"].iter().map(|s| (format!("{s}.wav"), b.clone())).collect()
}

fn short_bass(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
    ["vocals", "drums", "bass", "other"].iter().map(|s| (format!("{s}.wav"), wav(if *s == "bass" { 1.0 } else { 2.0 }))).collect()
}

fn helper(name: &str, write: filmcraft_sidecar::fake::Writer) -> Fixture {
    fixture(name, Script { features: vec!["stems".into()], result: ResultMode::Custom(write), ..Default::default() })
}

fn bin_items(f: &Fixture, name: &str) -> Vec<String> {
    let bin = f.s.project.root.children.iter().find_map(|c| match c {
        BinEntry::Bin(b) if b.name == name => Some(b),
        _ => None,
    });
    bin.map(|b| {
        b.children
            .iter()
            .filter_map(|c| match c {
                BinEntry::Item(i) => f.s.project.item(*i).map(|it| it.name.clone()),
                BinEntry::Bin(_) => None,
            })
            .collect()
    })
    .unwrap_or_default()
}

#[test]
fn stems_land_in_a_bin_in_one_undo_step() {
    let mut f = helper("st-bin", stems);
    let media = f.cache.join("media");
    f.s.ai.media_root = Some(media.clone());
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.stems", json!({"wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = f.helper.started()[0].clone();
    // the helper got the source decoded by FilmCraft: 2 s of 44.1 kHz float stereo, deleted after
    assert_eq!(req.kind, "stems");
    assert!(req.inputs.len() == 1 && req.inputs[0].ends_with("source.wav"), "{:?}", req.inputs);
    assert_eq!(*SOURCE.lock().unwrap(), Some((44_100, 2, 2 * 44_100 * 2 * 4)));
    assert!(!std::path::Path::new(&req.inputs[0]).exists(), "the decoded input is deleted");
    // the stems live in the project's AI media folder, not in the cache, and the job folder is gone
    let out = std::path::PathBuf::from(r["outDir"].as_str().unwrap());
    assert!(!out.exists());
    for it in f.s.project.items.values() {
        if let Some(m) = it.as_media().filter(|_| it.name.ends_with(".wav")) {
            let filmcraft_project::MediaRef::File { path } = &m.media else { panic!("a file") };
            assert!(std::path::Path::new(path).starts_with(media.join("AI Media").join(&f.s.project.id).join("stems")), "{path}");
            assert!(std::path::Path::new(path).exists());
        }
    }
    assert_eq!(req.params["stems"], json!(["vocals", "drums", "bass", "other"]));
    assert_eq!(req.params["bandwidth"], "16k", "16 kHz separation by default");
    assert!(f.s.execute("ai.stems", json!({"bandwidth": "22k"})).is_err());
    assert_eq!(bin_items(&f, "Stems: interview"), ["vocals.wav", "drums.wav", "bass.wav", "other.wav"]);
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.s.history.undo.last().unwrap().0, "Extract Stems");
    let tracks = f.s.active_sequence().unwrap().audio_tracks.len();
    f.s.execute("edit.undo", json!({})).unwrap();
    assert!(bin_items(&f, "Stems: interview").is_empty());
    assert_eq!(f.s.active_sequence().unwrap().audio_tracks.len(), tracks, "no tracks were added without `place`");
}

#[test]
fn placed_stems_line_up_with_the_clip() {
    let mut f = helper("st-place", stems);
    let before = f.s.active_sequence().unwrap().audio_tracks.len();
    f.s.execute("ai.stems", json!({"wait": true, "place": true})).unwrap();
    let q = f.s.active_sequence().unwrap();
    let (_, clip) = q.find_item(f.clip).unwrap();
    assert_eq!(q.audio_tracks.len(), before + 4);
    for (k, stem) in ["vocals", "drums", "bass", "other"].iter().enumerate() {
        let t = &q.audio_tracks[before + k];
        assert_eq!(t.kind, TrackKind::Audio);
        let it = &t.items[0];
        assert_eq!(
            (it.start, it.duration, it.source_in, it.speed, it.reverse),
            (clip.start, clip.duration, clip.source_in, clip.speed, clip.reverse),
            "{stem}"
        );
        assert_eq!(it.name, format!("interview ({stem})"));
    }
    assert_eq!(f.s.history.undo.last().unwrap().0, "Extract Stems");
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(f.s.active_sequence().unwrap().audio_tracks.len(), before);
}

/// A failed apply changes nothing and rolls the files back: the stems the job moved to the media
/// folder are back in the job folder in the cache, and the media folder is gone.
#[test]
fn a_stem_of_the_wrong_length_changes_nothing() {
    let mut f = helper("st-len", short_bass);
    let media = f.cache.join("media");
    f.s.ai.media_root = Some(media.clone());
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.stems", json!({"wait": true, "place": true})).unwrap_err().to_string();
    assert!(e.contains("bass stem is not as long as the decoded source"), "{e}");
    assert_eq!(snapshot(&f.s), before);
    let out = std::path::PathBuf::from(&f.helper.started()[0].out_dir);
    for stem in ["vocals", "drums", "bass", "other"] {
        assert!(out.join(format!("{stem}.wav")).is_file(), "{stem} is back in the cache");
    }
    assert!(!media.join("AI Media").join(&f.s.project.id).exists(), "the media folder is removed");
}

#[test]
fn long_sources_are_refused_and_jobs_are_bounded() {
    let mut f = helper("st-long", stems);
    let item = f.item;
    f.s.edit("Long", |pr, _| {
        if let Some(ItemKind::Media(m)) = pr.item_mut(item).map(|i| &mut i.kind) {
            m.info.duration = Tick(254_016_000_000 * 1300);
        }
        Ok(())
    })
    .unwrap();
    // a short clip from a long recording is fine: only the range it plays is decoded
    assert!(f.s.is_enabled("ai.stems"));
    f.s.edit_sequence("Long clip", |q, _, _| {
        q.video_tracks[0].items[0].duration = Tick(254_016_000_000 * 1250);
        Ok(())
    })
    .unwrap();
    let e = f.s.execute("ai.stems", json!({})).unwrap_err().to_string();
    assert!(e.contains("uses 21 min of its source") && e.contains("up to 20 min"), "{e}");

    let mut g = fixture("st-timeout", Script { features: vec!["stems".into()], steps: 100_000, ..Default::default() });
    g.s.ai.limits.timeout = std::time::Duration::from_millis(50);
    let e = g.s.execute("ai.stems", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("took longer"), "{e}");
}

#[test]
fn cancel_mid_stems_changes_nothing() {
    let mut f = fixture("st-cancel", Script { features: vec!["stems".into()], steps: 100_000, result: ResultMode::Custom(stems), ..Default::default() });
    let id = f.s.execute("ai.stems", json!({})).unwrap()["job"].as_u64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    f.s.execute("jobs.cancel", json!({"job": id})).unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id).as_deref(), Some("stopped"));
    assert!(bin_items(&f, "Stems: interview").is_empty());
}

#[test]
fn stems_need_the_feature_and_audio() {
    let f = fixture("st-gate", Script::default());
    assert!(reason(&f.s, "ai.stems").contains("does not provide `stems`"));
    let mut g = helper("st-gate2", stems);
    assert!(g.s.is_enabled("ai.stems"));
    g.s.state.selection.clear();
    assert_eq!(reason(&g.s, "ai.stems"), "select a clip with audio in the timeline");
}

/// 10 s of 48 kHz audio: silence with a one-sample click at 3.0 s on the right channel only.
fn click_wav(path: &std::path::Path) {
    let rate = 48_000u32;
    let n = rate as usize * 10;
    let mut b = Vec::new();
    let data = (n * 2 * 2) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &1u16.to_le_bytes(),
        &2u16.to_le_bytes(),
        &rate.to_le_bytes(),
        &(rate * 4).to_le_bytes(),
        &4u16.to_le_bytes(),
        &16u16.to_le_bytes(),
    ] {
        b.extend_from_slice(v);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..n {
        let r: i16 = if i == 3 * rate as usize { 30_000 } else { 0 };
        b.extend_from_slice(&0i16.to_le_bytes());
        b.extend_from_slice(&r.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Where the loudest sample of a mono or stereo float WAV is (sample index), and its channels.
fn peak(path: &str) -> (usize, u16) {
    let b = std::fs::read(path).unwrap();
    let ch = u16::from_le_bytes(b[22..24].try_into().unwrap());
    let samples: Vec<f32> = b[44..].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()).abs()).collect();
    let i = samples.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
    (i / ch as usize, ch)
}

/// Trimmed, sped-up and reversed clips of a 48 kHz recording: the decoded range starts on a
/// 44.1 kHz sample, the stems carry the click within a sample of where the source has it, and the
/// placed stems show, at every timeline time, the source time the clip shows (exactly, in ticks).
#[test]
fn stems_line_up_sample_accurately_with_trimmed_sped_and_reversed_clips() {
    for (name, source_in_s, speed, reverse, channels) in
        [("plain", 1.0, 1.0, false, vec![]), ("fast", 1.5, 2.0, false, vec![1u16]), ("rev", 0.5, 1.0, true, vec![1u16])]
    {
        let mut f = fixture(&format!("st-align-{name}"), Script { features: vec!["stems".into()], result: ResultMode::Custom(identity), ..Default::default() });
        f.s.ai.media_root = Some(f.cache.join("media"));
        let wav = f.cache.join("click.wav");
        std::fs::create_dir_all(&f.cache).unwrap();
        click_wav(&wav);
        let item = ItemId(f.s.execute("file.import", json!({"paths": [wav.to_string_lossy()]})).unwrap()["items"][0].as_u64().unwrap());
        let seq = f.s.state.active_sequence.unwrap();
        let tps = TICKS_PER_SECOND as f64;
        let clip =
            f.s.edit("Place", |pr, _| {
                let rate = pr.sequence(seq).unwrap().settings.frame_rate;
                let mut it = pr
                    .make_track_item(item, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick((source_in_s * tps) as i64), Tick((4.0 * tps) as i64)), rate)
                    .unwrap();
                it.speed = speed;
                it.reverse = reverse;
                it.duration = Tick((4.0 / speed * tps) as i64);
                it.source_channels = channels.clone();
                let id = it.id;
                pr.sequence_mut(seq).unwrap().audio_tracks[0].items.push(it);
                Ok(id)
            })
            .unwrap();
        f.s.state.selection = vec![clip];
        let r = f.s.execute("ai.stems", json!({"wait": true, "place": true})).unwrap();
        assert_eq!(r["applied"], true, "{name}: {r}");
        let q = f.s.active_sequence().unwrap();
        let (_, c) = q.find_item(clip).unwrap();
        let placed: Vec<_> = q.audio_tracks.iter().flat_map(|t| &t.items).filter(|i| i.name.starts_with("click.wav (")).collect();
        assert_eq!(placed.len(), 4, "{name}");
        let stem = f.s.project.item(placed[0].item).and_then(|i| i.as_media()).unwrap();
        let filmcraft_project::MediaRef::File { path } = &stem.media else { panic!("a file") };
        let (i, ch) = peak(path);
        assert_eq!(ch, if channels.is_empty() { 2 } else { 1 }, "{name}: the clip's channels");
        // offset of the decoded range: what the placed stem's source In lost against the clip's
        let offset = c.source_in - placed[0].source_in;
        assert_eq!(offset.0 % 5_760_000, 0, "{name}: the range starts on a 44.1 kHz sample");
        let click = Tick(i as i64 * 5_760_000) + offset;
        assert!((click.0 - (3.0 * tps) as i64).abs() <= 2 * 5_760_000, "{name}: click at {} s", click.0 as f64 / tps);
        for p in &placed {
            for k in 0..40 {
                let t = c.start + Tick(c.duration.0 * k / 40);
                assert_eq!(p.source_time_at(t) + offset, c.source_time_at(t), "{name} at {t:?}");
            }
        }
    }
}

/// A linked video and audio pair is split once; the stems outlive the cache.
#[test]
fn a_linked_pair_is_split_once_and_stems_outlive_the_cache() {
    let mut f = helper("st-pair", stems);
    f.s.ai.media_root = Some(f.cache.join("media"));
    let seq = f.s.state.active_sequence.unwrap();
    let (v, item) = (f.clip, f.item);
    let a =
        f.s.edit("Link", |pr, _| {
            let rate = pr.sequence(seq).unwrap().settings.frame_rate;
            let mut it = pr.make_track_item(item, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick(254_016_000_000 * 2)), rate).unwrap();
            let q = pr.sequence_mut(seq).unwrap();
            it.link = Some(7);
            q.video_tracks[0].items[0].link = Some(7);
            let id = it.id;
            q.audio_tracks[0].items.push(it);
            Ok(id)
        })
        .unwrap();
    f.s.state.selection = vec![v, a];
    f.s.execute("ai.stems", json!({"wait": true})).unwrap();
    assert_eq!(f.helper.started().len(), 1, "one job for the pair");
    assert_eq!(bin_items(&f, "Stems: interview").len(), 4);
    // pruning the whole cache leaves the imported stems alone
    super::super::prune_cache(&f.cache.join("AI"), 0, &[], std::time::Duration::ZERO);
    for it in f.s.project.items.values().filter(|i| i.name.ends_with(".wav")) {
        let filmcraft_project::MediaRef::File { path } = &it.as_media().unwrap().media else { panic!("a file") };
        assert!(std::path::Path::new(path).exists(), "{path} survived pruning");
    }
}

/// The channel counts of the `source.wav` each [`per_channel`] job received (its own record: tests
/// run in parallel).
static CHANNELS: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

fn per_channel(req: &JobRequest) -> Vec<(String, Vec<u8>)> {
    if let Ok(b) = std::fs::read(&req.inputs[0]) {
        CHANNELS.lock().unwrap_or_else(|e| e.into_inner()).push(u16::from_le_bytes(b[22..24].try_into().unwrap()));
    }
    stems(req)
}

/// Dual mono split to two tracks (channel 1 on one clip, channel 2 on another, same range): each
/// clip gets its stems; a video clip linked to one of them adds no job.
#[test]
fn clips_playing_other_channels_of_the_same_range_each_get_stems() {
    let mut f = helper("st-dual", per_channel);
    f.s.ai.media_root = Some(f.cache.join("media"));
    let seq = f.s.state.active_sequence.unwrap();
    let (v, item) = (f.clip, f.item);
    let (a1, a2) =
        f.s.edit("Split", |pr, _| {
            let rate = pr.sequence(seq).unwrap().settings.frame_rate;
            let range = TimeRange::new(Tick::ZERO, Tick(254_016_000_000 * 2));
            let mut left = pr.make_track_item(item, TrackKind::Audio, Tick::ZERO, range, rate).unwrap();
            let mut right = pr.make_track_item(item, TrackKind::Audio, Tick(254_016_000_000 * 2), range, rate).unwrap();
            (left.source_channels, right.source_channels) = (vec![0], vec![1]);
            left.link = Some(7);
            let q = pr.sequence_mut(seq).unwrap();
            q.video_tracks[0].items[0].link = Some(7);
            let ids = (left.id, right.id);
            q.audio_tracks[0].items.push(left);
            q.audio_tracks[0].items.push(right);
            Ok(ids)
        })
        .unwrap();
    f.s.state.selection = vec![v, a1, a2];
    f.s.execute("ai.stems", json!({"wait": true})).unwrap();
    assert_eq!(f.helper.started().len(), 2, "one job per channel, none for the linked video");
    assert_eq!(f.s.project.items.values().filter(|i| i.name.ends_with(".wav")).count(), 8);
    // each job decoded one channel
    assert_eq!(*CHANNELS.lock().unwrap(), [1, 1]);
}

/// The job report points at the stems where they now live.
#[test]
fn the_job_report_points_at_the_media_folder() {
    let mut f = helper("st-report", stems);
    f.s.ai.media_root = Some(f.cache.join("media"));
    let r = f.s.execute("ai.stems", json!({"wait": true})).unwrap();
    let id = r["job"].as_u64().unwrap();
    let job = f.s.jobs.iter().find(|j| j.id == id).unwrap();
    let report = job.result.lock().unwrap().clone().unwrap().unwrap();
    assert!(std::path::Path::new(&report.path).is_dir(), "{}", report.path);
    assert!(report.path.contains("AI Media"), "{}", report.path);
    assert_eq!(report.extra_files.len(), 4);
    assert!(report.extra_files.iter().all(|p| std::path::Path::new(p).is_file() && p.starts_with(&report.path)), "{:?}", report.extra_files);
}

/// A saved project keeps its AI media next to it.
#[test]
fn a_saved_project_keeps_its_stems_beside_it() {
    let mut f = helper("st-saved", stems);
    std::fs::create_dir_all(&f.cache).unwrap();
    let proj = f.cache.join("Show.fcproj");
    f.s.execute("file.saveAs", json!({"path": proj.to_string_lossy()})).unwrap();
    f.s.state.selection = vec![f.clip];
    f.s.execute("ai.stems", json!({"wait": true})).unwrap();
    let dir = f.cache.join("Show AI Media").join("stems");
    let paths: Vec<String> =
        f.s.project
            .items
            .values()
            .filter_map(|i| match &i.as_media()?.media {
                filmcraft_project::MediaRef::File { path } if i.name.ends_with(".wav") => Some(path.clone()),
                _ => None,
            })
            .collect();
    assert_eq!(paths.len(), 4);
    assert!(paths.iter().all(|p| std::path::Path::new(p).starts_with(&dir) && std::path::Path::new(p).exists()), "{paths:?}");
}

/// The decode counts against the job's time limit; a job out of time never reaches the helper.
#[test]
fn preparing_the_input_is_inside_the_time_limit() {
    let mut f = helper("st-deadline", stems);
    f.s.ai.limits.timeout = std::time::Duration::ZERO;
    let e = f.s.execute("ai.stems", json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("took longer"), "{e}");
    assert!(f.helper.started().is_empty());
}

/// Not placed, the stems do not depend on where the clip is: moving it during the job keeps the
/// result. Placing needs the clip's sequence open; otherwise the stems are only imported.
#[test]
fn unplaced_stems_survive_a_move_and_placing_needs_the_sequence() {
    let mut f = fixture("st-loose", Script { features: vec!["stems".into()], steps: 3, result: ResultMode::Custom(stems), ..Default::default() });
    f.s.ai.media_root = Some(f.cache.join("media"));
    let id = f.s.execute("ai.stems", json!({})).unwrap()["job"].as_u64().unwrap();
    f.s.edit_sequence("Move", |q, _, _| {
        q.video_tracks[0].items[0].start = Tick(254_016_000_000);
        Ok(())
    })
    .unwrap();
    finish(&mut f.s, id);
    assert_eq!(job_error(&f.s, id), None);
    assert_eq!(bin_items(&f, "Stems: interview").len(), 4);

    let mut g = helper("st-other-seq", stems);
    g.s.ai.media_root = Some(g.cache.join("media"));
    let id = g.s.execute("ai.stems", json!({"place": true})).unwrap()["job"].as_u64().unwrap();
    g.s.execute("file.newSequence", json!({"name": "Other"})).unwrap();
    let _ = g.s.drain_events();
    finish(&mut g.s, id);
    assert_eq!(job_error(&g.s, id), None);
    assert_eq!(bin_items(&g, "Stems: interview").len(), 4);
    let toasts: Vec<String> =
        g.s.drain_events().into_iter().filter_map(|e| if let crate::Event::Toast { message, .. } = e { Some(message) } else { None }).collect();
    assert!(toasts.iter().any(|m| m.contains("open the clip's sequence to place them")), "{toasts:?}");
}

/// An audio source that decodes up to sample `decodes_to` (44.1 kHz) and fails after it.
struct Broken {
    info: filmcraft_media::MediaInfo,
    decodes_to: i64,
}

impl filmcraft_media::MediaSource for Broken {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        &self.info
    }
    fn video_frame(&self, _: filmcraft_media::FrameRequest) -> filmcraft_media::Result<std::sync::Arc<filmcraft_frame::VideoFrame>> {
        Err(filmcraft_media::MediaError::NoStream("video"))
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        if start + frames as i64 > self.decodes_to {
            return Err(filmcraft_media::MediaError::Decode("corrupt packet".into()));
        }
        Ok(filmcraft_frame::AudioBuffer { sample_rate: rate, channels: vec![vec![0.5; frames]; 2] })
    }
}

fn broken(decodes_to: i64) -> Broken {
    use filmcraft_media::MediaSource;
    let rate = filmcraft_time::FrameRate::FPS_24;
    let info =
        filmcraft_media::generators::GeneratorSource::new(filmcraft_media::Generator::Demo(filmcraft_media::DemoScene::Plasma), 8, 8, rate, rate.tick_of(240))
            .info()
            .clone();
    Broken { info, decodes_to }
}

/// A decoder that fails inside the media is an error naming the stream and the range (no silent
/// stems); a stream that only ends a little before the media is padded with silence.
#[test]
fn decode_failures_are_errors_and_only_a_stream_end_is_silence() {
    let dir = std::env::temp_dir().join(format!("filmcraft-ai-broken-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("source.wav");
    let end = 10 * 44_100;
    let e = super::decode_to_wav(&broken(4 * 44_100), 0, &[0, 1], (0, end, end), &path, &|| false, &mut |_| {}).unwrap_err();
    assert!(e.contains("decoding audio stream 0 failed at 4.000 s") && e.contains("corrupt packet"), "{e}");
    // 0.1 s short of the media: the rest is silence
    let short = end - 4_410;
    super::decode_to_wav(&broken(short), 0, &[0, 1], (0, end, end), &path, &|| false, &mut |_| {}).unwrap();
    let b = std::fs::read(&path).unwrap();
    let sample = |i: i64| f32::from_le_bytes(b[44 + i as usize * 8..48 + i as usize * 8].try_into().unwrap());
    assert_eq!((sample(0), sample(short - 1), sample(short), sample(end - 1)), (0.5, 0.5, 0.0, 0.0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Source positions at the ends of the tick range are refused with a reason, never wrapped.
#[test]
fn extreme_source_positions_are_refused() {
    for at in [i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX] {
        let mut f = helper("st-extreme", stems);
        let seq = f.s.state.active_sequence.unwrap();
        let clip = f.clip;
        f.s.edit("Hostile", |pr, _| {
            pr.sequence_mut(seq).unwrap().find_item_mut(clip).unwrap().1.source_in = Tick(at);
            Ok(())
        })
        .unwrap();
        let e = f.s.execute("ai.stems", json!({"wait": true})).unwrap_err().to_string();
        assert!(e.contains("out of range"), "{at}: {e}");
        assert!(f.helper.started().is_empty());
    }
}
