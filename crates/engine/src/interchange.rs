//! Interchange documents (CMX 3600 EDL, FCP7 XML, FCPXML, OTIO, AAF, OMF; Premiere Pro projects
//! import only) ↔ the session's project.

use serde_json::{Value, json};

use filmcraft_interchange::{ExportOptions, Format, ImportOptions};
use filmcraft_project::{ItemId, ItemKind, MediaRef};

use crate::{EngineError, Result, Session};

/// The interchange format of a file, if it is one (by content, with the extension as a hint).
pub fn detect(path: &str, bytes: &[u8]) -> Option<Format> {
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if !matches!(ext.as_deref(), Some("edl" | "xml" | "fcpxml" | "otio" | "aaf" | "omf" | "omfi" | "prproj" | "json")) {
        return None;
    }
    filmcraft_interchange::detect(bytes, ext.as_deref())
}

fn report_json(r: &filmcraft_interchange::Report) -> Vec<String> {
    r.entries.iter().map(|e| if e.count > 1 { format!("{} (×{})", e.message, e.count) } else { e.message.clone() }).collect()
}

fn is_file_media(item: &filmcraft_project::ProjectItem) -> bool {
    matches!(&item.kind, ItemKind::Media(m) if matches!(m.media, MediaRef::File { .. }))
}

/// Import a document: merge its bins, media and sequences into the project (one undo step), then
/// link each media file that exists on disk. Returns the new sequences.
pub fn import(s: &mut Session, path: &str, bytes: &[u8], format: Format) -> Result<Value> {
    let p = std::path::Path::new(path);
    // a CapCut draft is named after its folder, not `draft_content`
    let name = if format == Format::CapCut { p.parent().and_then(|d| d.file_name()) } else { p.file_stem() };
    // a CapCut draft's own files stay inside its folder; a bare relative name is made absolute
    // first (an empty folder would contain nothing)
    let abs = if format == Format::CapCut { canonical(s, path).and_then(|r| r.ok()) } else { None };
    let dir = abs.as_deref().map(std::path::Path::new).unwrap_or(p).parent().map(|d| d.to_string_lossy().to_string());
    let contain = if format == Format::CapCut { dir.clone().filter(|d| !d.is_empty()) } else { None };
    import_doc(s, path, dir, name.map(|n| n.to_string_lossy().to_string()), contain, bytes, format)
}

/// Most entries read from one folder of a document directory.
const DIR_MAX_ENTRIES: usize = 20_000;
/// Largest document read out of a bundle (an `Info.fcpxml`).
const DOC_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// The real location of `path`, when the host has links. `Err` when it cannot be resolved.
fn canonical(s: &Session, path: &str) -> Option<std::io::Result<String>> {
    s.services.canonical_path(path)
}

/// Whether `path` is `root` or lies under it once links are resolved. Hosts without links (`None`)
/// have nothing to escape through.
fn contained(s: &Session, root: &str, path: &str) -> bool {
    match (canonical(s, root), canonical(s, path)) {
        (Some(Ok(r)), Some(Ok(p))) => std::path::Path::new(&p).starts_with(&r),
        (None, _) | (_, None) => true,
        // the nearest part that exists decides: a missing file behind a link out is an escape
        (Some(Ok(r)), Some(Err(e))) if e.kind() == std::io::ErrorKind::NotFound => std::path::Path::new(path)
            .ancestors()
            .skip(1)
            .find_map(|a| canonical(s, &a.to_string_lossy()).and_then(|c| c.ok()))
            .is_some_and(|a| std::path::Path::new(&a).starts_with(&r)),
        _ => false,
    }
}

/// Read a document that is part of `root`: it must resolve inside it and be of a sane size.
fn read_owned(s: &Session, root: &str, file: &str, max: u64) -> Result<Vec<u8>> {
    if !contained(s, root, file) {
        return Err(EngineError::Other(format!("{file}: resolves outside {root}")));
    }
    if s.services.file_size(file).is_ok_and(|n| n > max) {
        return Err(EngineError::Other(format!("{file}: larger than {} MiB", max / (1024 * 1024))));
    }
    s.services.read_file(file).map_err(|e| EngineError::Other(format!("{file}: {e}")))
}

fn dir_name(path: &str) -> Option<String> {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string())
}

/// Directories the importer opens as one document: Final Cut `.fcpxmld` bundles and `.fcpbundle`
/// libraries, and CapCut draft folders. `None`: not one of those (an ordinary path).
pub fn import_directory(s: &mut Session, path: &str) -> Option<Result<Value>> {
    let trimmed = Some(path.trim_end_matches(['/', '\\'])).filter(|t| !t.is_empty()).unwrap_or(path);
    let ext = std::path::Path::new(trimmed).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let package = matches!(ext.as_deref(), Some("fcpxmld" | "fcpbundle"));
    // packages are listed (capped); a draft folder is recognised by its draft file directly
    let names: Vec<String> = if package {
        match s.services.list_entries_capped(trimmed, DIR_MAX_ENTRIES)? {
            Ok((v, _)) => v.into_iter().map(|e| e.name).collect(),
            Err(e) => return Some(Err(EngineError::Other(format!("{path}: {e}")))),
        }
    } else {
        Vec::new()
    };
    match ext.as_deref() {
        Some("fcpxmld") => Some(import_fcpxmld(s, trimmed, &names, std::path::Path::new(trimmed).file_stem().map(|n| n.to_string_lossy().to_string()))),
        Some("fcpbundle") => Some(import_fcpbundle(s, trimmed)),
        _ => {
            // draft_content.json, else draft_info.json: the first that is a readable draft
            let present: Vec<&str> =
                filmcraft_interchange::capcut::DRAFT_FILES.into_iter().filter(|f| s.services.file_size(&format!("{trimmed}/{f}")).is_ok()).collect();
            if present.is_empty() {
                return None;
            }
            let mut last = None;
            for draft in &present {
                let file = format!("{trimmed}/{draft}");
                let bytes = match read_owned(s, trimmed, &file, filmcraft_interchange::capcut::MAX_BYTES as u64) {
                    Ok(b) => b,
                    Err(e) => {
                        last = Some(e);
                        continue;
                    }
                };
                if filmcraft_interchange::capcut::sniff(&bytes) {
                    return Some(import_doc(
                        s,
                        &file,
                        Some(trimmed.to_string()),
                        dir_name(trimmed).or_else(|| Some("CapCut Draft".into())),
                        Some(trimmed.to_string()),
                        &bytes,
                        Format::CapCut,
                    ));
                }
            }
            Some(Err(last.unwrap_or_else(|| {
                EngineError::Other(format!(
                    "{trimmed}: {} is not a readable CapCut draft (newer CapCut versions encrypt drafts; export FCPXML or XML from CapCut)",
                    present.join(" and ")
                ))
            })))
        }
    }
}

/// A `.fcpxmld` bundle: `Info.fcpxml` is the document; relative media paths resolve against the
/// bundle, and bundle-owned references stay inside it.
fn import_fcpxmld(s: &mut Session, dir: &str, names: &[String], name: Option<String>) -> Result<Value> {
    let Some(doc) = names.iter().find(|n| n.eq_ignore_ascii_case("Info.fcpxml")) else {
        return Err(EngineError::Other(format!("{dir}: the bundle has no Info.fcpxml")));
    };
    let file = format!("{dir}/{doc}");
    let bytes = read_owned(s, dir, &file, DOC_MAX_BYTES)?;
    import_doc(s, &file, Some(dir.to_string()), name, Some(dir.to_string()), &bytes, Format::Fcpxml)
}

/// Most folders / documents looked at inside a library.
const LIBRARY_MAX_DEPTH: usize = 4;
const LIBRARY_MAX_DIRS: usize = 500;
const LIBRARY_MAX_DOCS: usize = 16;

/// A `.fcpbundle` library keeps its events in `CurrentVersion.fcpevent` (a Core Data store, not
/// FCPXML), which is not read. Any `.fcpxml` / `.fcpxmld` exported into the library folder is: the
/// walk stays inside the library (links resolved, each real folder once) and stops at fixed
/// limits, which it reports.
fn import_fcpbundle(s: &mut Session, dir: &str) -> Result<Value> {
    let mut docs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![(dir.to_string(), 0usize)];
    let mut truncated = false;
    let mut skipped_links = 0u32;
    while let Some((d, depth)) = stack.pop() {
        if seen.len() >= LIBRARY_MAX_DIRS || docs.len() >= LIBRARY_MAX_DOCS {
            truncated = true;
            break;
        }
        if !contained(s, dir, &d) {
            skipped_links += 1;
            continue;
        }
        // each real folder once (a link back up the tree is not followed again)
        if !seen.insert(canonical(s, &d).and_then(|r| r.ok()).unwrap_or_else(|| d.clone())) {
            continue;
        }
        let Some(Ok((mut entries, more))) = s.services.list_entries_capped(&d, DIR_MAX_ENTRIES) else { continue };
        truncated |= more;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut sub = Vec::new();
        for e in entries {
            let path = format!("{d}/{}", e.name);
            let lower = e.name.to_ascii_lowercase();
            if lower.ends_with(".fcpxmld") && e.is_dir || lower.ends_with(".fcpxml") && !e.is_dir {
                if docs.len() < LIBRARY_MAX_DOCS {
                    docs.push(path);
                } else {
                    truncated = true;
                }
            } else if e.is_dir && depth < LIBRARY_MAX_DEPTH && !e.name.starts_with('.') {
                // pending folders are bounded as well as visited ones
                if seen.len() + stack.len() + sub.len() < LIBRARY_MAX_DIRS {
                    sub.push((path, depth + 1));
                } else {
                    truncated = true;
                }
            }
        }
        // sorted order out of a stack
        stack.extend(sub.into_iter().rev());
    }
    docs.sort();
    docs.dedup();
    if docs.is_empty() {
        return Err(EngineError::Other(format!(
            "{dir}: Final Cut libraries are not readable directly: in Final Cut choose File > Export XML, then import the .fcpxmld"
        )));
    }
    s.edit_group("Import Final Cut library", |s| {
        let mut sequences = Vec::new();
        let mut report = Vec::new();
        let mut linked = 0;
        let mut offline = Vec::new();
        for d in docs {
            let r = if d.to_ascii_lowercase().ends_with(".fcpxmld") {
                match contained(s, dir, &d) {
                    true => {
                        let names = s.services.list_entries_capped(&d, DIR_MAX_ENTRIES).and_then(|r| r.ok()).map(|(v, _)| v.into_iter().map(|e| e.name).collect::<Vec<_>>()).unwrap_or_default();
                        import_fcpxmld(s, &d, &names, std::path::Path::new(&d).file_stem().map(|n| n.to_string_lossy().to_string()))
                    }
                    false => Err(EngineError::Other(format!("{d}: resolves outside the library"))),
                }
            } else {
                let p = std::path::Path::new(&d);
                read_owned(s, dir, &d, DOC_MAX_BYTES).and_then(|b| {
                    import_doc(s, &d, p.parent().map(|x| x.to_string_lossy().to_string()), p.file_stem().map(|n| n.to_string_lossy().to_string()), Some(dir.to_string()), &b, Format::Fcpxml)
                })
            };
            match r {
                Ok(v) => {
                    sequences.extend(v["sequences"].as_array().cloned().unwrap_or_default());
                    linked += v["linkedMedia"].as_u64().unwrap_or(0);
                    offline.extend(v["offlineMedia"].as_array().cloned().unwrap_or_default());
                    report.extend(v["report"].as_array().cloned().unwrap_or_default());
                }
                Err(e) => report.push(json!(format!("{e}"))),
            }
        }
        if sequences.is_empty() {
            return Err(EngineError::Other(format!("{dir}: no document in the library could be imported: {}", report.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; "))));
        }
        report.insert(0, json!("Final Cut libraries are not readable directly; only the FCPXML documents found inside were imported"));
        if truncated {
            report.push(json!(format!("the library was only partly searched (at most {LIBRARY_MAX_DOCS} documents, {LIBRARY_MAX_DIRS} folders, {LIBRARY_MAX_DEPTH} levels, {DIR_MAX_ENTRIES} entries per folder)")));
        }
        if skipped_links > 0 {
            report.push(json!(format!("{skipped_links} folders that resolve outside the library were not searched")));
        }
        Ok(json!({"format": "Final Cut Library", "sequences": sequences, "linkedMedia": linked, "offlineMedia": offline, "report": report}))
    })
}

/// `contain`: the bundle or draft folder whose own references (paths under it) must stay inside it.
fn import_doc(
    s: &mut Session,
    path: &str,
    base_dir: Option<String>,
    name: Option<String>,
    contain: Option<String>,
    bytes: &[u8],
    format: Format,
) -> Result<Value> {
    // the merge, every Link Media step and the dropping of silent audio are one undo entry; a
    // failing step rolls the project back to before the import (the opened-file cache is not
    // part of the project and keeps what was opened)
    let label = format!("Import {}", name.as_deref().filter(|n| !n.is_empty()).unwrap_or_else(|| format.name()));
    s.edit_group(&label, |s| import_doc_steps(s, path, base_dir, name, contain, bytes, format))
}

fn import_doc_steps(
    s: &mut Session,
    path: &str,
    base_dir: Option<String>,
    name: Option<String>,
    contain: Option<String>,
    bytes: &[u8],
    format: Format,
) -> Result<Value> {
    let opts = ImportOptions { base_dir, name, contain: contain.is_some(), ..Default::default() };
    let (fragment, report) = match format {
        // AAF / OMF may embed audio: write it next to the document (the media items point there)
        Format::Aaf | Format::Omf => {
            let r = if format == Format::Aaf { filmcraft_interchange::aaf::import(bytes, &opts) } else { filmcraft_interchange::omf::import(bytes, &opts) };
            let (fragment, extracted, report) = r.map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
            for m in extracted {
                if let Some(dir) = std::path::Path::new(&m.path).parent().filter(|_| !s.services.export_in_memory()) {
                    let _ = std::fs::create_dir_all(dir);
                }
                s.services.write_file(&m.path, &m.wav).map_err(|e| EngineError::Other(format!("{}: {e}", m.path)))?;
            }
            (fragment, report)
        }
        _ => filmcraft_interchange::import_with(bytes, format, &opts).map_err(|e| EngineError::Other(format!("{path}: {e}")))?,
    };
    let before: std::collections::HashSet<ItemId> = s.project.items.keys().copied().collect();
    let name = opts.name.clone().unwrap_or_else(|| format.name().to_string());
    let seqs = s.edit(&format!("Import {name}"), |proj, _| {
        let seqs = filmcraft_interchange::merge_into(proj, fragment, None);
        // A document that sets only some Motion parameters leaves the others at their "auto"
        // (NaN) defaults. Resolve them as placing a clip does, for every format. A media file's
        // picture size is only what the document says so far (an EDL says nothing: the importer
        // assumes the sequence size), so its clips' anchors wait for Link Media below.
        proj.resolve_placed_auto_points(|seq| !before.contains(&seq), |source| !is_file_media(source));
        Ok(seqs)
    })?;
    // Link media: probe each new file-backed item that exists, through the host's reader as a
    // direct import does (only the container index is read now, not the whole file: #461).
    let new_media: Vec<(ItemId, String)> = s
        .project
        .items
        .values()
        .filter(|i| !before.contains(&i.id))
        .filter_map(|i| match &i.kind {
            ItemKind::Media(m) => match &m.media {
                MediaRef::File { path } => Some((i.id, path.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect();
    // A Premiere Pro project also gives each file's path relative to the project: tried when the
    // stored path does not open (a project moved between machines).
    let relative = |s: &Session, id: ItemId| -> Option<String> {
        (format == Format::Prproj).then(|| s.project.item(id).and_then(|i| i.metadata.get(filmcraft_interchange::prproj::RELATIVE_PATH_KEY).cloned())).flatten()
    };
    let alternates: Vec<String> = new_media.iter().filter_map(|(id, _)| relative(s, *id)).collect();
    // The document's media paths are opened as stored paths of the document.
    s.media.add_stored_paths(new_media.iter().map(|(_, p)| p.clone()).chain(alternates), Some(path));
    let mut linked = 0;
    let mut offline = Vec::new();
    let mut notes = Vec::new();
    let mut escaped = 0u32;
    let mut dropped_audio = 0usize;
    for (id, stored) in new_media {
        let mut opened = None;
        for candidate in std::iter::once(stored.clone()).chain(relative(s, id)) {
            // a path under the bundle or draft folder must not reach outside it (`../`, links);
            // a path elsewhere is the file's own location
            if let Some(root) = &contain
                && std::path::Path::new(&candidate).starts_with(root)
                && !contained(s, root, &candidate)
            {
                escaped += 1;
                continue;
            }
            let file = match s.media.follow(&candidate) {
                Ok(file) => file,
                Err(why) => {
                    log::warn!("{candidate} is not opened: {why}");
                    continue;
                }
            };
            if let Ok(src) = s.media.open_file(&file, &*s.services) {
                opened = Some((candidate, file, src));
                break;
            }
        }
        let Some((mpath, file, src)) = opened else {
            offline.push(stored);
            continue;
        };
        if mpath != stored {
            notes.push(format!("{stored} was not found; linked to {mpath} (its path relative to the project)"));
        }
        let info = src.info().clone();
        let identity = crate::relink::identity_of(&*s.services, &file).ok();
        // EDL source times are absolute timecode. A Premiere Pro project's in points are
        // kept as written: the prproj-format notes (06-timing.md, "`Start`/`End` vs
        // `InPoint`/`OutPoint`") say only that they count from "an arbitrary zero point in the
        // source file's own timeline", not whether that is the file's start timecode.
        let rebase = (format == Format::Edl).then_some(info.start_timecode).flatten();
        if let Some(note) = prproj_timecode_note(format, &mpath, info.start_timecode) {
            notes.push(note);
        }
        // CapCut video segments came with a linked audio clip on the assumption the file has
        // sound; a file without any has none
        let silent = format == Format::CapCut && info.audio_streams.is_empty();
        s.edit("Link Media", |proj, _| {
            if silent {
                dropped_audio += drop_unheard_audio(proj, &before, id);
            }
            if let Some(item) = proj.items.get_mut(&id)
                && let ItemKind::Media(m) = &mut item.kind
            {
                let rate = info.video.as_ref().map(|v| v.frame_rate);
                if mpath != stored {
                    m.media = MediaRef::File { path: mpath.clone() };
                }
                m.info = info.clone();
                m.offline = false;
                m.identity = identity;
                if let (Some(tc), Some(rate)) = (rebase, rate) {
                    filmcraft_interchange::rebase_source_timecode(proj, id, tc, rate);
                }
            }
            // the picture size is now the file's own: centre the anchors of its clips in it
            proj.resolve_placed_auto_points(|seq| !before.contains(&seq), |source| source.id == id);
            Ok(())
        })?;
        s.media.insert_file(id, &mpath, src);
        linked += 1;
    }
    if escaped > 0 {
        notes.push(format!("{escaped} media paths inside {} resolve outside it and were left offline", contain.as_deref().unwrap_or("the bundle")));
    }
    if dropped_audio > 0 {
        notes.push(format!("{dropped_audio} linked audio clips were dropped: their files have no audio"));
    }
    if format == Format::CapCut && !offline.is_empty() {
        notes.push(format!("{} offline media keep the linked audio clip of their video segments until they are relinked", offline.len()));
    }
    if let Some(&first) = seqs.first() {
        s.state.active_sequence = Some(first);
        if !s.state.open_sequences.contains(&first) {
            s.state.open_sequences.push(first);
        }
    }
    Ok(json!({
        "format": format.name(),
        "sequences": seqs.iter().map(|i| i.0).collect::<Vec<_>>(),
        "linkedMedia": linked,
        "offlineMedia": offline,
        "report": report_json(&report).into_iter().chain(notes).collect::<Vec<_>>(),
    }))
}

/// Remove the linked audio clips of `item` (audio clips with a link, made for a video segment of a
/// CapCut draft) from the sequences of this import and unlink their video clips. Returns how many.
pub(crate) fn drop_unheard_audio(proj: &mut filmcraft_project::Project, before: &std::collections::HashSet<ItemId>, item: ItemId) -> usize {
    let ids: Vec<ItemId> = proj.items.keys().copied().filter(|i| !before.contains(i)).collect();
    let mut n = 0;
    for id in ids {
        let Some(seq) = proj.sequence_mut(id) else { continue };
        let mut gone = std::collections::HashSet::new();
        for t in &mut seq.audio_tracks {
            t.items.retain(|c| {
                let drop = c.item == item && c.link.is_some();
                if drop {
                    gone.insert(c.link);
                    n += 1;
                }
                !drop
            });
        }
        for t in &mut seq.video_tracks {
            for c in t.items.iter_mut().filter(|c| c.item == item && gone.contains(&c.link)) {
                c.link = None;
            }
        }
    }
    n
}

/// For a Premiere Pro project, a report line naming a linked file whose start timecode is not
/// zero: its clips' in points were kept as written, and a real project has to show whether they
/// count from that timecode.
pub(crate) fn prproj_timecode_note(format: Format, path: &str, start_timecode: Option<i64>) -> Option<String> {
    let tc = start_timecode.filter(|tc| *tc != 0)?;
    (format == Format::Prproj).then(|| {
        format!("{path} starts at timecode frame {tc}; its source in points were kept as Premiere Pro wrote them (not shifted by the timecode; unverified)")
    })
}

/// `file.exportInterchange {format: "edl"|"xml"|"fcpxml"|"otio"|"aaf"|"omf" = "xml", path, sequence?}`
pub fn export(s: &mut Session, p: &Value) -> Result<Value> {
    // FCP7 XML only when no format is asked for: a format we don't know is an error, not XML.
    // The names are the documented ones (each format's extension, in either case) and nothing
    // else, so the error can list exactly what is accepted.
    let format = match p.get("format") {
        None | Some(Value::Null) => Format::Fcp7Xml,
        Some(v) => v
            .as_str()
            .and_then(|name| Format::ALL.into_iter().filter(|f| !f.is_import_only()).find(|f| f.extension().eq_ignore_ascii_case(name)))
            .ok_or_else(|| {
                let names: Vec<&str> = Format::ALL.iter().filter(|f| !f.is_import_only()).map(|f| f.extension()).collect();
                crate::commands::bad("file.exportInterchange", format!("unknown format {v}: use one of {}", names.join(", ")))
            })?,
    };
    let path = p.get("path").and_then(Value::as_str).ok_or_else(|| EngineError::Other("need `path`".into()))?.to_string();
    let seq = p.get("sequence").and_then(Value::as_u64).map(ItemId).or(s.state.active_sequence).ok_or(EngineError::NoSequence)?;
    let opts = ExportOptions {
        relative_to: std::path::Path::new(&path).parent().map(|d| d.to_string_lossy().to_string()),
        name: s.project.item(seq).map(|i| i.name.clone()),
        ..Default::default()
    };
    let (bytes, report) = filmcraft_interchange::export(&s.project, seq, format, &opts).map_err(|e| EngineError::Other(e.to_string()))?;
    s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({"path": path, "bytes": bytes.len(), "report": report_json(&report)}))
}
