//! `ai.stems` against the fake helper: the four stems in a bin, placed under the clip, one undo
//! step; a stem of the wrong length, an over-long source, a timeout and a cancel change nothing.

use filmcraft_project::{BinEntry, ItemKind, TrackKind};
use filmcraft_sidecar::JobRequest;
use filmcraft_sidecar::fake::{ResultMode, Script};
use filmcraft_time::Tick;
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

/// The fixture's source is 2 s long.
fn stems(_: &JobRequest) -> Vec<(String, Vec<u8>)> {
    ["vocals", "drums", "bass", "other"].iter().map(|s| (format!("{s}.wav"), wav(2.0))).collect()
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
    let undo = f.s.history.undo.len();
    let r = f.s.execute("ai.stems", json!({"wait": true})).unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let req = f.helper.started()[0].clone();
    // the helper gets the source decoded by FilmCraft: 2 s of 44.1 kHz float stereo in the job folder
    assert_eq!(req.kind, "stems");
    assert!(req.inputs.len() == 1 && req.inputs[0].ends_with("source.wav"), "{:?}", req.inputs);
    let wav = std::fs::read(&req.inputs[0]).unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 44_100);
    assert_eq!(wav.len(), 44 + 2 * 44_100 * 2 * 4);
    assert_eq!(req.params["stems"], json!(["vocals", "drums", "bass", "other"]));
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

#[test]
fn a_stem_of_the_wrong_length_changes_nothing() {
    let mut f = helper("st-len", short_bass);
    super::super::ensure_project_id(&mut f.s);
    let before = snapshot(&f.s);
    let e = f.s.execute("ai.stems", json!({"wait": true, "place": true})).unwrap_err().to_string();
    assert!(e.contains("bass stem is not as long as the source"), "{e}");
    assert_eq!(snapshot(&f.s), before);
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
    let e = f.s.execute("ai.stems", json!({})).unwrap_err().to_string();
    assert!(e.contains("up to 20 min"), "{e}");

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
