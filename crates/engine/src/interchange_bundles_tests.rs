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
