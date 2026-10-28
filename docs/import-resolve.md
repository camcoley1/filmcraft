# Importing from DaVinci Resolve

FilmCraft has no Resolve-specific importer. Resolve exports two interchange formats FilmCraft reads natively, and either carries the edit (clips, cuts, speed changes, transitions it can name, markers).

## OpenTimelineIO (recommended)

1. In Resolve, open the timeline and choose File > Export > Timeline (OTIO) (named "Export AAF/XML/EDL/OTIO" in some versions), format OpenTimelineIO (`.otio`).
2. In FilmCraft choose File > Import and pick the `.otio` file.

## FCPXML

1. In Resolve choose File > Export > Timeline, then FCPXML 1.10 (1.9 to 1.11 are read).
2. In FilmCraft import the `.fcpxml`. A `.fcpxmld` bundle from Final Cut or Resolve imports the same way: choose the bundle itself (its `Info.fcpxml` is the document, relative media paths resolve against the bundle).

## What to expect

- Media is linked by the paths in the document. Files that moved show as offline; use Link Media.
- Colour grades, Fusion effects, Fairlight settings and Resolve-only effects are not carried by either format; they appear in the import report with counts.
- Compound clips and nested timelines come across as nested sequences where the format describes them.

Other formats FilmCraft reads: Final Cut libraries (`.fcpbundle`) are not readable directly (the events are a Core Data store, not XML): in Final Cut choose File > Export XML, then import the `.fcpxmld`. CapCut drafts (`draft_content.json`, or the draft folder) import tracks, clips with their linked audio, speed, volume, reverse and text as a caption track. File > Import Folder or Bundle picks a bundle or draft folder where the file dialog cannot. Paths inside a bundle or draft folder may not climb or link out of it; absolute media paths are the file's own.

`.mogrt` motion graphics templates wait for EffectCraft's `.aep` reader.
