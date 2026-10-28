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
    // A1 holds the linked sound of V1's clips, A2 the draft's own audio track
    let a1 = &seq.tracks(TrackKind::Audio)[0].items;
    assert_eq!(a1.len(), 2);
    for (va, au) in v.iter().zip(a1) {
        assert_eq!(
            (va.start, va.duration, va.source_in, va.speed, va.reverse, va.enabled),
            (au.start, au.duration, au.source_in, au.speed, au.reverse, au.enabled)
        );
        assert!(va.link.is_some() && va.link == au.link);
        assert_eq!(au.item, va.item);
        assert_eq!(au.gain_db, 0.0);
    }
    assert_ne!(a1[0].link, a1[1].link);
    let a = &seq.tracks(TrackKind::Audio)[1].items;
    assert_eq!(a.len(), 1);
    assert!(a[0].link.is_none());
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
    // all three keys, none of the structure
    for doc in [
        r#"{"canvas_config":{"w":1},"tracks":[{"name":"x"}],"materials":{"foo":[1]}}"#,
        r#"{"canvas_config":{"w":1},"tracks":[],"materials":{"foo":[1]}}"#,
        r#"{"canvas_config":{},"tracks":[1,2],"materials":{"videos":[]}}"#,
        r#"{"canvas_config":[],"tracks":[{"type":"video","segments":[]}],"materials":{"videos":[]}}"#,
        r#"{"canvas_config":{},"tracks":[{"type":"video"}],"materials":{"videos":[]}}"#,
    ] {
        assert_eq!(detect(doc.as_bytes(), None), None, "{doc}");
        assert_eq!(detect(doc.as_bytes(), Some("json")), None, "{doc}");
    }
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
    ] {
        assert!(import_with(&bytes, Format::CapCut, &opts()).is_err());
    }
    // target ranges: negative, overflowing, zero; every one is skipped with a warning
    for (start, dur) in [(-5, 100), (0, -1), (0, 0), (i64::MAX, 1), (0, i64::MAX), (i64::MAX / 2, i64::MAX / 2), (0, i64::MIN)] {
        let bytes = mutate(|v| v["tracks"][0]["segments"][0]["target_timerange"] = serde_json::json!({"start": start, "duration": dur}));
        let (im, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
        let seq = im.project.sequence(im.sequences[0]).unwrap();
        assert_eq!(seq.tracks(TrackKind::Video)[0].items.len(), 1, "{start} {dur}");
        assert!(report.mentions("time range"));
    }
    // source ranges: invalid ones skip the segment, they are never rewritten to something else
    for src in [
        serde_json::json!({"start": i64::MAX, "duration": 1_000_000}),
        serde_json::json!({"start": -1, "duration": 1_000_000}),
        serde_json::json!({"start": 0, "duration": 0}),
        serde_json::json!({"start": 0, "duration": -4}),
        serde_json::json!({"start": 0}),
        serde_json::json!({"duration": 5}),
        serde_json::json!({"start": i64::MAX / 2, "duration": i64::MAX / 2}),
    ] {
        let bytes = mutate(|v| {
            v["tracks"][0]["segments"][0]["source_timerange"] = src.clone();
            v["tracks"][0]["segments"][0].as_object_mut().unwrap().remove("speed");
        });
        let (im, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
        let seq = im.project.sequence(im.sequences[0]).unwrap();
        assert_eq!(seq.tracks(TrackKind::Video)[0].items.len(), 1, "{src}");
        assert_eq!(seq.tracks(TrackKind::Audio)[0].items.len(), 1, "{src}");
        assert!(report.mentions("source range"), "{src}");
    }
    // a source range past the supported time range once the speed is applied
    let bytes = mutate(|v| {
        v["tracks"][0]["segments"][0]["speed"] = serde_json::json!(1000.0);
        v["tracks"][0]["segments"][0]["source_timerange"] = serde_json::json!({"start": 5_000_000_000_000_000i64, "duration": 1000});
    });
    let (im, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
    assert_eq!(im.project.sequence(im.sequences[0]).unwrap().tracks(TrackKind::Video)[0].items.len(), 1);
    assert!(report.mentions("source range"));
    // absurd speeds and fps, bad canvas
    let bytes = mutate(|v| {
        v["tracks"][0]["segments"][0]["speed"] = serde_json::json!(1e30);
        v["fps"] = serde_json::json!(-3.0);
        v["canvas_config"]["width"] = serde_json::json!(0);
    });
    let (im, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
    // the absurd speed is reported and its segment skipped, not changed to 100%
    assert_eq!(im.project.sequence(im.sequences[0]).unwrap().tracks(TrackKind::Video)[0].items.len(), 1);
    assert!(report.mentions("frame rate") && report.mentions("speed outside"));
    // missing materials
    let bytes = mutate(|v| v["materials"] = serde_json::json!({"videos": [], "audios": [], "texts": []}));
    let (_, report) = import_with(&bytes, Format::CapCut, &opts()).unwrap();
    assert!(report.mentions("not in the draft"));
    // no tracks (and a draft whose every segment is invalid still imports its sequence)
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

fn items(seq: &filmcraft_project::Sequence, kind: TrackKind) -> Vec<&filmcraft_project::TrackItem> {
    seq.tracks(kind).iter().flat_map(|t| t.items.iter()).collect()
}

fn one(f: impl FnOnce(&mut serde_json::Value)) -> (filmcraft_interchange::Imported, Report) {
    import_with(&mutate(f), Format::CapCut, &opts()).unwrap()
}

use filmcraft_interchange::Report;

#[test]
fn video_volume_photos_and_mutes_on_the_linked_audio() {
    let (im, _) = one(|v| {
        v["tracks"][0]["segments"][0]["volume"] = serde_json::json!(0.5);
        v["tracks"][0]["segments"][1]["volume"] = serde_json::json!(0.0);
        v["materials"]["videos"][0]["type"] = serde_json::json!("video");
    });
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    let a = &seq.tracks(TrackKind::Audio)[0].items;
    assert!((a[0].gain_db + 6.0206).abs() < 1e-3);
    assert!(!a[1].enabled && a[1].gain_db == 0.0, "volume 0 is muted, not dropped");
    // a photo has no sound
    let (im, _) = one(|v| v["materials"]["videos"][0]["type"] = serde_json::json!("photo"));
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    assert!(seq.tracks(TrackKind::Audio)[0].items.is_empty());
    assert!(items(seq, TrackKind::Video).iter().all(|c| c.link.is_none()));
}

#[test]
fn unsorted_segments_stay_on_their_track() {
    let (im, report) = one(|v| {
        let segs = serde_json::json!([
            {"material_id": "v1", "target_timerange": {"start": 8_000_000, "duration": 1_000_000}},
            {"material_id": "v1", "target_timerange": {"start": 0, "duration": 1_000_000}},
            {"material_id": "v1", "target_timerange": {"start": 4_000_000, "duration": 1_000_000}}
        ]);
        v["tracks"][0]["segments"] = segs;
        v["tracks"][2]["segments"] = serde_json::json!([
            {"material_id": "x2", "target_timerange": {"start": 5_000_000, "duration": 1_000_000}},
            {"material_id": "x1", "target_timerange": {"start": 0, "duration": 1_000_000}}
        ]);
    });
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    assert_eq!(seq.tracks(TrackKind::Video).iter().filter(|t| !t.items.is_empty()).count(), 1);
    assert_eq!(seq.tracks(TrackKind::Video)[0].items.len(), 3);
    assert_eq!(seq.caption_tracks[0].captions.len(), 2);
    assert!(!report.mentions("overlapping"));
}

#[test]
fn reverse_flag_and_implied_speed() {
    let (im, report) = one(|v| {
        let s = &mut v["tracks"][0]["segments"][0];
        s["reverse"] = serde_json::json!(true);
        // no speed field: the ranges imply 2x
        s.as_object_mut().unwrap().remove("speed");
        s["source_timerange"] = serde_json::json!({"start": 0, "duration": 4_000_000});
        // a contradicting speed: the durations win
        let s2 = &mut v["tracks"][0]["segments"][1];
        s2["speed"] = serde_json::json!(5.0);
    });
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    let v = &seq.tracks(TrackKind::Video)[0].items;
    assert!(v[0].reverse && v[0].speed == 2.0);
    assert_eq!(v[1].speed, 2.0);
    let a = &seq.tracks(TrackKind::Audio)[0].items;
    assert!(a[0].reverse && a[0].speed == 2.0);
    assert!(report.mentions("contradicts"));
    assert!(report.mentions("reversed segments"));
}

#[test]
fn material_paths_stay_inside_the_draft_folder() {
    let (im, report) = one(|v| {
        v["materials"]["videos"][0]["path"] = serde_json::json!("##_draftpath_placeholder_X_##/../../etc/passwd");
        v["materials"]["audios"][0]["path"] = serde_json::json!("sub/../../x.mp3");
    });
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
    assert!(paths.iter().all(|p| !p.contains("..") && !p.contains("passwd")), "{paths:?}");
    assert!(report.mentions("climbs out of the draft folder"));
    // an absolute path stays the file's own
    let (im, _) = one(|v| v["materials"]["audios"][0]["path"] = serde_json::json!("/Volumes/Media/../Media/x.mp3"));
    assert!(im.project.items.values().any(
        |i| matches!(&i.kind, filmcraft_project::ItemKind::Media(m) if matches!(&m.media, MediaRef::File { path } if path == "/Volumes/Media/../Media/x.mp3"))
    ));
}

#[test]
fn empty_paths_do_not_collapse_into_one_item() {
    let (im, report) = one(|v| {
        v["materials"]["videos"] = serde_json::json!([
            {"id": "v1", "path": "", "material_name": "a.mp4", "type": "video"},
            {"id": "v2", "path": " ", "material_name": "b.mp4", "type": "video"}
        ]);
        v["tracks"][0]["segments"][1]["material_id"] = serde_json::json!("v2");
    });
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    let v = &seq.tracks(TrackKind::Video)[0].items;
    assert_ne!(v[0].item, v[1].item);
    assert!(report.mentions("no file path"));
}

#[test]
fn legacy_text_markup_and_empty_text() {
    let (im, report) = one(|v| {
        v["materials"]["texts"] = serde_json::json!([
            {"id": "x1", "content": "<font id=\"x\"><size=8>[Legacy line]</size></font>"},
            {"id": "x2", "content": "{\"styles\":[]}"}
        ]);
    });
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    assert_eq!(seq.caption_tracks[0].captions.len(), 1);
    assert_eq!(seq.caption_tracks[0].captions[0].text, "Legacy line");
    assert!(report.mentions("no text"));
}

#[test]
fn noise_free_report_for_constant_speed_drafts() {
    let (_, report) = one(|v| {
        v["materials"]["speeds"] = serde_json::json!([{"id": "s", "mode": 0, "curve_speed": null}, {"id": "t", "mode": 1, "curve_speed": {"x": 1}}]);
        v["materials"]["material_animations"] = serde_json::json!([{"id": "a", "animations": []}, {"id": "b", "animations": [{}]}]);
    });
    let e = |m: &str| report.entries.iter().find(|e| e.message.starts_with(m)).map(|e| e.count);
    assert_eq!(e("speed curves"), Some(1));
    assert_eq!(e("animations"), Some(1));
}

#[test]
fn big_unread_material_arrays_cost_no_memory_or_report_time() {
    // five million minimal entries (about 10 MB) in an array nobody reads
    let zeros = vec!["0"; 5_000_000].join(",");
    let doc = format!(
        r#"{{"canvas_config":{{"width":640,"height":360}},"fps":25,"tracks":[{{"type":"video","segments":[{{"material_id":"v1","target_timerange":{{"start":0,"duration":1000}}}}]}}],"materials":{{"videos":[{{"id":"v1","path":"/a.mp4"}}],"effects":[{zeros}]}}}}"#
    );
    let t = std::time::Instant::now();
    let (_, report) = import_with(doc.as_bytes(), Format::CapCut, &opts()).unwrap();
    assert!(t.elapsed().as_secs() < 20);
    let e = report.entries.iter().find(|e| e.message == "effects were not imported").unwrap();
    assert_eq!(e.count, 5_000_000);
    assert!(detect(doc.as_bytes(), None) == Some(Format::CapCut));
}

#[test]
fn segments_beyond_the_cap_are_skipped_while_reading_and_reported() {
    let seg = r#"{"material_id":"v1","target_timerange":{"start":0,"duration":1000}}"#;
    let one_track = vec![seg; 150_000].join(",");
    let doc = format!(
        r#"{{"canvas_config":{{"width":640,"height":360}},"fps":25,"tracks":[{{"type":"video","segments":[{one_track}]}},{{"type":"video","segments":[{one_track}]}}],"materials":{{"videos":[{{"id":"v1","path":"/a.mp4"}}]}}}}"#
    );
    let (_, report) = import_with(doc.as_bytes(), Format::CapCut, &opts()).unwrap();
    assert!(report.mentions("only the first 200000 of 300000 segments"), "{report}");
}

#[test]
fn otio_with_a_json_extension_is_still_otio() {
    assert_eq!(detect(br#"{"OTIO_SCHEMA":"Timeline.1","name":"x","tracks":{}}"#, Some("json")), Some(Format::Otio));
}

#[test]
fn nested_arrays_in_unread_fields_cost_neither_memory_nor_time() {
    // 5 million numbers nested in a material entry, in an unknown field and in a text content:
    // read once through a skipping visitor, never buffered
    let nums = vec!["0"; 5_000_000].join(",");
    let doc = format!(
        r#"{{"canvas_config":{{"width":160,"height":90}},"fps":24,"tracks":[{{"type":"video","segments":[{{"material_id":"v1","target_timerange":{{"start":0,"duration":1000000}}}}]}},{{"type":"text","segments":[{{"material_id":"x","target_timerange":{{"start":0,"duration":1000}}}}]}}],"materials":{{"videos":[{{"id":"v1","path":"/a.mp4"}}],"effects":[[{nums}]],"speeds":[{{"mode":[{nums}],"extra":[{nums}]}}],"texts":[{{"id":"x","content":[{nums}]}}]}}}}"#
    );
    let t = std::time::Instant::now();
    let (im, report) = import_with(doc.as_bytes(), Format::CapCut, &opts()).unwrap();
    assert!(t.elapsed().as_secs() < 30, "{:?}", t.elapsed());
    let seq = im.project.sequence(im.sequences[0]).unwrap();
    assert_eq!(seq.tracks(TrackKind::Video)[0].items.len(), 1);
    assert!(seq.caption_tracks.is_empty());
    assert!(report.mentions("no text"));
    assert!(report.mentions("effects were not imported"));
}
