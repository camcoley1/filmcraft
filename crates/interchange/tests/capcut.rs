//! CapCut draft import: a synthetic draft shaped like the community schema, plus hostile
//! mutations that must give an error or a report, never a panic.

use filmcraft_interchange::{Error, Format, ImportOptions, detect, export, import_with};
use filmcraft_project::{MediaRef, TrackKind};
use filmcraft_time::{FrameRate, Tick};

const DRAFT: &str = include_str!("fixtures/capcut_draft_content.json");
const US: i64 = 254_016;

fn opts() -> ImportOptions {
    ImportOptions { base_dir: Some("/drafts/one".into()), name: Some("One".into()), ..Default::default() }
}

#[test]
fn imports_canvas_tracks_speed_volume_and_captions() {
    assert_eq!(detect(DRAFT.as_bytes(), Some("json")), Some(Format::CapCut));
    assert_eq!(detect(DRAFT.as_bytes(), None), Some(Format::CapCut));
    let (im, report) = import_with(DRAFT.as_bytes(), Format::CapCut, &opts()).unwrap();
    let id = im.sequences[0];
    let seq = im.project.sequence(id).unwrap();
    assert_eq!((seq.settings.width, seq.settings.height), (1080, 1920));
    assert_eq!(seq.settings.frame_rate, FrameRate::FPS_30);
    let v = &seq.tracks(TrackKind::Video)[0].items;
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].start, Tick(0));
    assert_eq!(v[0].duration, Tick(2_000_000 * US));
    assert_eq!(v[0].source_in, Tick(1_000_000 * US));
    assert_eq!(v[1].start, Tick(2_000_000 * US));
    assert_eq!(v[1].speed, 2.0);
    let a = &seq.tracks(TrackKind::Audio)[0].items;
    assert_eq!(a.len(), 1);
    assert!((a[0].gain_db + 6.0206).abs() < 1e-3);
    assert_eq!(seq.caption_tracks.len(), 1);
    let caps = &seq.caption_tracks[0].captions;
    assert_eq!(caps.len(), 2);
    assert_eq!(caps[0].text, "Hello world");
    assert_eq!(caps[0].start, Tick(500_000 * US));
    assert_eq!(caps[1].text, "Plain caption");
    // media paths: the draft placeholder resolves against the draft folder
    let paths: Vec<String> = im
        .project
        .items
        .values()
        .filter_map(|i| match &i.kind {
            filmcraft_project::ItemKind::Media(m) => match &m.media {
                MediaRef::File { path } => Some(path.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(paths.contains(&"/drafts/one/media/clip.mp4".to_string()), "{paths:?}");
    assert!(paths.contains(&"/music/song.mp3".to_string()));
    // unsupported things are counted
    assert!(report.mentions("effects were not imported"));
    assert!(report.mentions("transitions were not imported"));
    assert!(report.mentions("sticker tracks were not imported"));
    assert!(report.mentions("keyframes"));
    assert!(report.mentions("position, scale or rotation"));
    assert!(report.mentions("text styling"));
    let e = report.entries.iter().find(|e| e.message == "effects were not imported").unwrap();
    assert_eq!(e.count, 2);
}

#[test]
fn capcut_is_import_only() {
    let (im, _) = import_with(DRAFT.as_bytes(), Format::CapCut, &opts()).unwrap();
    assert!(Format::CapCut.is_import_only());
    assert!(export(&im.project, im.sequences[0], Format::CapCut, &Default::default()).is_err());
}

#[test]
fn other_json_is_not_a_draft() {
    assert_eq!(detect(br#"{"a": 1}"#, Some("json")), None);
    assert_eq!(detect(br#"{"canvas_config": {}, "tracks": []}"#, None), None);
}

fn mutate(f: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_str(DRAFT).unwrap();
    f(&mut v);
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn hostile_drafts_never_panic() {
    let truncated = &DRAFT.as_bytes()[..DRAFT.len() / 2];
    assert!(matches!(import_with(truncated, Format::CapCut, &opts()), Err(Error::Parse { .. })));
    assert!(import_with(b"\x00\x01\x02 encrypted", Format::CapCut, &opts()).is_err());
    assert!(import_with(b"", Format::CapCut, &opts()).is_err());
    // wrong types
    for bytes in [
        mutate(|v| v["tracks"] = serde_json::json!("x")),
        mutate(|v| v["tracks"][0]["segments"][0]["speed"] = serde_json::json!("fast")),
        mutate(|v| v["canvas_config"]["width"] = serde_json::json!("wide")),
        mutate(|v| v["materials"] = serde_json::json!([])),
    ] {
        assert!(import_with(&bytes, Format::CapCut, &opts()).is_err());
    }
    // ranges: negative, overflowing, zero; every one is skipped with a warning
    for (start, dur) in [(-5, 100), (0, -1), (0, 0), (i64::MAX, 1), (0, i64::MAX), (i64::MAX / 2, i64::MAX / 2), (0, i64::MIN)] {
        let bytes = mutate(|v| v["tracks"][0]["segments"][0]["target_timerange"] = serde_json::json!({"start": start, "duration": dur}));
        let (im, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
        let seq = im.project.sequence(im.sequences[0]).unwrap();
        assert_eq!(seq.tracks(TrackKind::Video)[0].items.len(), 1, "{start} {dur}");
        assert!(report.mentions("time range"));
    }
    // source start overflow is read as 0
    let bytes = mutate(|v| v["tracks"][0]["segments"][0]["source_timerange"]["start"] = serde_json::json!(i64::MAX));
    assert!(import_with(&bytes, Format::CapCut, &opts()).is_ok());
    // absurd speeds and fps, bad canvas
    let bytes = mutate(|v| {
        v["tracks"][0]["segments"][0]["speed"] = serde_json::json!(1e30);
        v["fps"] = serde_json::json!(-3.0);
        v["canvas_config"]["width"] = serde_json::json!(0);
    });
    let (im, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
    assert_eq!(im.project.sequence(im.sequences[0]).unwrap().tracks(TrackKind::Video)[0].items[0].speed, 1.0);
    assert!(report.mentions("frame rate") && report.mentions("speed"));
    // missing materials
    let bytes = mutate(|v| v["materials"] = serde_json::json!({"videos": [], "audios": [], "texts": []}));
    let (_, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
    assert!(report.mentions("not in the draft"));
    // no tracks
    assert!(matches!(import_with(&mutate(|v| v["tracks"] = serde_json::json!([])), Format::CapCut, &opts()), Err(Error::Empty)));
}

#[test]
fn huge_counts_are_capped() {
    // 250k segments on one track: only the cap is read, quickly, without panicking
    let seg = r#"{"material_id":"v1","target_timerange":{"start":0,"duration":1000}}"#;
    let segs = vec![seg; 250_000].join(",");
    let doc = format!(
        r#"{{"canvas_config":{{"width":640,"height":360}},"fps":25,"tracks":[{{"type":"video","segments":[{segs}]}}],"materials":{{"videos":[{{"id":"v1","path":"/a.mp4"}}]}}}}"#
    );
    let (_, report) = import_with(doc.as_bytes(), Format::CapCut, &opts()).unwrap();
    assert!(report.mentions("only the first"));
}
