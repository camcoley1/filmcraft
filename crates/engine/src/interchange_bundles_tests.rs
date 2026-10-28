//! `file.import` of directory documents (`.fcpxmld`, `.fcpbundle`, CapCut draft folders) and of a
//! CapCut `draft_content.json` file.

use filmcraft_project::{ItemId, TrackKind};
use filmcraft_time::{FrameRate, Tick};
use serde_json::json;

use crate::Session;

const DRAFT: &str = include_str!("../../interchange/tests/fixtures/capcut_draft_content.json");
const INFO: &str = include_str!("../../interchange/tests/fixtures/info.fcpxml");

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fc-m7-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn capcut_draft_file_imports_with_canvas_times_and_captions() {
    let d = dir("capcut-file");
    let path = d.join("draft_content.json");
    std::fs::write(&path, DRAFT).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [path.to_string_lossy()]})).unwrap();
    let seq = ItemId(r["sequences"][0].as_u64().unwrap_or_else(|| panic!("no sequence in {r}")));
    assert_eq!(s.state.active_sequence, Some(seq));
    let q = s.active_sequence().unwrap();
    q.check().unwrap();
    assert_eq!((q.settings.width, q.settings.height, q.settings.frame_rate), (1080, 1920, FrameRate::FPS_30));
    let v = &q.tracks(TrackKind::Video)[0].items;
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].duration, Tick(2_000_000 * 254_016));
    assert_eq!(q.caption_tracks.len(), 1);
    assert_eq!(q.caption_tracks[0].captions.len(), 2);
    assert_eq!(r["documents"][0]["format"], "CapCut Draft");
    assert!(r["documents"][0]["report"].as_array().unwrap().iter().any(|e| e.as_str().unwrap_or("").contains("effects")), "{r}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn capcut_draft_folder_imports() {
    let d = dir("capcut-folder");
    let folder = d.join("My Draft");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("draft_content.json"), DRAFT).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [folder.to_string_lossy()]})).unwrap();
    let seq = ItemId(r["sequences"][0].as_u64().unwrap());
    assert_eq!(s.project.item(seq).map(|i| i.name.as_str()), Some("My Draft"));
    // the draft's own media placeholder resolved into the folder
    assert!(s.project.items.values().any(|i| matches!(&i.kind, filmcraft_project::ItemKind::Media(m) if matches!(&m.media, filmcraft_project::MediaRef::File { path } if path.starts_with(&*folder.to_string_lossy())))));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn encrypted_or_broken_draft_is_an_error() {
    let d = dir("capcut-bad");
    let path = d.join("draft_content.json");
    std::fs::write(&path, b"\x8f\x01 not json").unwrap();
    let mut s = Session::default();
    assert!(s.execute("file.import", json!({"paths": [path.to_string_lossy()]})).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fcpxmld_bundle_imports_and_resolves_media_against_the_bundle() {
    let d = dir("fcpxmld");
    let bundle = d.join("Cut.fcpxmld");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("Info.fcpxml"), INFO).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [bundle.to_string_lossy()]})).unwrap();
    let seq = ItemId(r["sequences"][0].as_u64().unwrap_or_else(|| panic!("no sequence in {r}")));
    assert_eq!(s.state.active_sequence, Some(seq));
    assert_eq!((s.active_sequence().unwrap().video_tracks[0].items.len()), 1);
    let want = format!("{}/media/boats.mov", bundle.to_string_lossy());
    assert!(
        s.project.items.values().any(
            |i| matches!(&i.kind, filmcraft_project::ItemKind::Media(m) if matches!(&m.media, filmcraft_project::MediaRef::File { path } if *path == want))
        ),
        "{r}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fcpxmld_without_info_is_an_error() {
    let d = dir("fcpxmld-empty");
    let bundle = d.join("Empty.fcpxmld");
    std::fs::create_dir_all(&bundle).unwrap();
    let mut s = Session::default();
    let e = s.execute("file.import", json!({"paths": [bundle.to_string_lossy()]})).unwrap_err().to_string();
    assert!(e.contains("Info.fcpxml"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fcpbundle_without_exported_xml_says_how_to_export() {
    let d = dir("fcpbundle");
    let lib = d.join("Lib.fcpbundle");
    std::fs::create_dir_all(lib.join("Event/Original Media")).unwrap();
    std::fs::write(lib.join("Event/CurrentVersion.fcpevent"), b"SQLite format 3\0").unwrap();
    let mut s = Session::default();
    let e = s.execute("file.import", json!({"paths": [lib.to_string_lossy()]})).unwrap_err().to_string();
    assert!(e.contains("File > Export XML"), "{e}");
    // an exported bundle inside the library is imported
    let bundle = lib.join("Exports/Cut.fcpxmld");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("Info.fcpxml"), INFO).unwrap();
    let r = s.execute("file.import", json!({"paths": [lib.to_string_lossy()]})).unwrap();
    assert_eq!(r["sequences"].as_array().map(Vec::len), Some(1), "{r}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn media_browser_lists_bundles_as_project_files() {
    use crate::media_browser::{kind_of, passes};
    assert_eq!(kind_of("Cut.fcpxmld"), Some("project"));
    assert_eq!(kind_of("Lib.fcpbundle"), Some("project"));
    assert_eq!(kind_of("draft_content.json"), Some("project"));
    assert_eq!(kind_of("other.json"), None);
    assert!(passes("project", "project", "Cut.fcpxmld"));
}

fn draft_for(media_path: &str) -> String {
    json!({
        "canvas_config": {"width": 160, "height": 90},
        "fps": 24.0,
        "tracks": [{"type": "video", "segments": [
            {"material_id": "v1", "target_timerange": {"start": 0, "duration": 500_000}, "source_timerange": {"start": 0, "duration": 500_000}, "speed": 1.0, "volume": 0.5}
        ]}],
        "materials": {"videos": [{"id": "v1", "path": media_path, "material_name": "m.mov", "duration": 500_000, "width": 160, "height": 90, "type": "video"}]}
    })
    .to_string()
}

#[test]
fn capcut_video_with_sound_keeps_its_linked_audio_and_offline_media_says_so() {
    use crate::media_test_util::make_movie;
    let d = dir("capcut-audio");
    let movie = d.join("m.mov");
    make_movie(&movie, filmcraft_media::DemoScene::Aurora, 160, 90, 24);
    let draft = d.join("draft_content.json");
    std::fs::write(&draft, draft_for(&movie.to_string_lossy())).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [draft.to_string_lossy()]})).unwrap();
    assert_eq!(r["documents"][0]["linkedMedia"], 1, "{r}");
    let q = s.active_sequence().unwrap();
    let (v, a) = (&q.video_tracks[0].items[0], &q.audio_tracks[0].items[0]);
    assert!(v.link.is_some() && v.link == a.link);
    assert!((a.gain_db + 6.0206).abs() < 1e-3);
    // offline media keep the audio clip, and the report says so
    std::fs::write(&draft, draft_for("/nonexistent/filmcraft-m7/m.mov")).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [draft.to_string_lossy()]})).unwrap();
    assert_eq!(s.active_sequence().unwrap().audio_tracks[0].items.len(), 1);
    assert!(r["documents"][0]["report"].as_array().unwrap().iter().any(|e| e.as_str().unwrap_or("").contains("keep the linked audio")), "{r}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn files_without_sound_lose_the_assumed_audio_clip() {
    let d = dir("capcut-silent");
    let draft = d.join("draft_content.json");
    std::fs::write(&draft, draft_for("/nonexistent/m.mov")).unwrap();
    let mut s = Session::default();
    s.execute("file.import", json!({"paths": [draft.to_string_lossy()]})).unwrap();
    let before = std::collections::HashSet::new();
    let item = s.active_sequence().unwrap().video_tracks[0].items[0].item;
    let mut proj = (*s.project).clone();
    assert_eq!(crate::interchange::drop_unheard_audio(&mut proj, &before, item), 1);
    let seq = proj.sequence(s.state.active_sequence.unwrap()).unwrap();
    assert!(seq.audio_tracks[0].items.is_empty());
    assert!(seq.video_tracks[0].items[0].link.is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn draft_folder_tries_the_second_file_and_is_named_after_the_folder() {
    let d = dir("capcut-two");
    let folder = d.join("Trip.v2");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("draft_content.json"), b"\x8f\x01 encrypted").unwrap();
    std::fs::write(folder.join("draft_info.json"), DRAFT).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [folder.to_string_lossy()]})).unwrap();
    let seq = ItemId(r["sequences"][0].as_u64().unwrap());
    assert_eq!(s.project.item(seq).map(|i| i.name.as_str()), Some("Trip.v2"));
    // both unreadable: one error naming both
    std::fs::write(folder.join("draft_info.json"), b"{}").unwrap();
    let e = s.execute("file.import", json!({"paths": [folder.to_string_lossy()]})).unwrap_err().to_string();
    assert!(e.contains("draft_content.json") && e.contains("draft_info.json"), "{e}");
    // a draft file imported directly is named after its folder
    std::fs::write(folder.join("draft_content.json"), DRAFT).unwrap();
    let r = s.execute("file.import", json!({"paths": [folder.join("draft_content.json").to_string_lossy()]})).unwrap();
    assert_eq!(s.project.item(ItemId(r["sequences"][0].as_u64().unwrap())).map(|i| i.name.as_str()), Some("Trip.v2"));
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn info_fcpxml_symlinked_out_of_the_bundle_is_refused() {
        let d = dir("link-info");
        let outside = d.join("outside.fcpxml");
        std::fs::write(&outside, INFO).unwrap();
        let bundle = d.join("Cut.fcpxmld");
        std::fs::create_dir_all(&bundle).unwrap();
        symlink(&outside, bundle.join("Info.fcpxml")).unwrap();
        let mut s = Session::default();
        let e = s.execute("file.import", json!({"paths": [bundle.to_string_lossy()]})).unwrap_err().to_string();
        assert!(e.contains("outside"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bundle_media_may_not_climb_or_link_out_but_absolute_media_is_the_files_own() {
        use crate::media_test_util::make_movie;
        let d = dir("link-media");
        let movie = d.join("secret.mov");
        make_movie(&movie, filmcraft_media::DemoScene::Aurora, 160, 90, 12);
        let bundle = d.join("Cut.fcpxmld");
        std::fs::create_dir_all(bundle.join("media")).unwrap();
        symlink(&movie, bundle.join("media/linked.mov")).unwrap();
        let doc = |src: &str| INFO.replace("media/boats.mov", src);
        let run = |src: &str| {
            std::fs::write(bundle.join("Info.fcpxml"), doc(src)).unwrap();
            let mut s = Session::default();
            let r = s.execute("file.import", json!({"paths": [bundle.to_string_lossy()]})).unwrap();
            r["documents"][0].clone()
        };
        let climbing = run("../secret.mov");
        assert_eq!(climbing["linkedMedia"], 0, "{climbing}");
        assert!(climbing["report"].as_array().unwrap().iter().any(|e| e.as_str().unwrap_or("").contains("resolve outside")), "{climbing}");
        let linked = run("media/linked.mov");
        assert_eq!(linked["linkedMedia"], 0, "{linked}");
        // the file's own absolute location is allowed
        let absolute = run(&movie.to_string_lossy());
        assert_eq!(absolute["linkedMedia"], 1, "{absolute}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn library_walk_does_not_follow_links_out_or_loop() {
        let d = dir("link-lib");
        let outside = d.join("elsewhere");
        std::fs::create_dir_all(outside.join("Out.fcpxmld")).unwrap();
        std::fs::write(outside.join("Out.fcpxmld/Info.fcpxml"), INFO).unwrap();
        let lib = d.join("Lib.fcpbundle");
        std::fs::create_dir_all(lib.join("Event")).unwrap();
        symlink(&outside, lib.join("Event/escape")).unwrap();
        symlink(&lib, lib.join("Event/loop")).unwrap();
        let mut s = Session::default();
        let e = s.execute("file.import", json!({"paths": [lib.to_string_lossy()]})).unwrap_err().to_string();
        assert!(e.contains("File > Export XML"), "{e}");
        // a real document beside the links is imported once, with a note about the links
        std::fs::create_dir_all(lib.join("Event/Exports/Cut.fcpxmld")).unwrap();
        std::fs::write(lib.join("Event/Exports/Cut.fcpxmld/Info.fcpxml"), INFO).unwrap();
        let r = s.execute("file.import", json!({"paths": [lib.to_string_lossy()]})).unwrap();
        assert_eq!(r["sequences"].as_array().map(Vec::len), Some(1), "{r}");
        assert!(r["documents"][0]["report"].as_array().unwrap().iter().any(|e| e.as_str().unwrap_or("").contains("outside the library")), "{r}");
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn wide_library_directories_and_many_documents_are_capped_and_reported() {
    let d = dir("wide-lib");
    let lib = d.join("Lib.fcpbundle");
    let wide = lib.join("Wide");
    std::fs::create_dir_all(&wide).unwrap();
    for i in 0..20_100 {
        std::fs::write(wide.join(format!("f{i:05}")), b"").unwrap();
    }
    for i in 0..20 {
        let b = lib.join(format!("Exports/Cut{i:02}.fcpxmld"));
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("Info.fcpxml"), INFO).unwrap();
    }
    let mut s = Session::default();
    let before = s.history.undo.len();
    let r = s.execute("file.import", json!({"paths": [lib.to_string_lossy()]})).unwrap();
    assert_eq!(r["sequences"].as_array().map(Vec::len), Some(16), "{r}");
    assert!(r["documents"][0]["report"].as_array().unwrap().iter().any(|e| e.as_str().unwrap_or("").contains("partly searched")), "{r}");
    assert_eq!(s.history.undo.len(), before + 1, "one undo step for the whole library");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn import_folder_command_takes_a_path() {
    let d = dir("import-folder");
    let folder = d.join("Draft");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("draft_content.json"), DRAFT).unwrap();
    let mut s = Session::default();
    let r = s.execute("file.importFolder", json!({"path": folder.to_string_lossy()})).unwrap();
    assert_eq!(r["sequences"].as_array().map(Vec::len), Some(1), "{r}");
    assert!(s.execute("file.importFolder", json!({})).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bundle_info_over_the_size_cap_is_refused_before_it_is_read() {
    // sparse 300 MiB file: the size check must refuse it without reading
    let d = dir("big-info");
    let bundle = d.join("Big.fcpxmld");
    std::fs::create_dir_all(&bundle).unwrap();
    let f = std::fs::File::create(bundle.join("Info.fcpxml")).unwrap();
    f.set_len(300 * 1024 * 1024).unwrap();
    let mut s = Session::default();
    let e = s.execute("file.import", json!({"paths": [bundle.to_string_lossy()]})).unwrap_err().to_string();
    assert!(e.contains("larger than"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}
