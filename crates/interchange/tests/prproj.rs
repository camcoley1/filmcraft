//! Premiere Pro project (`.prproj`) import: synthetic projects shaped like the files the
//! `prproj-format` research notes describe (no real Premiere Pro project is committed), in plain
//! and gzip form, plus hostile mutations that must give an error or a report, never a panic.

use std::io::Write;

use filmcraft_interchange::{Error, Format, ImportOptions, Imported, Report, detect, export, import_with};
use filmcraft_media::Generator;
use filmcraft_project::{ItemKind, MarkerKind, MediaRef, Sequence, TrackItem};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};

const SEC: i64 = TICKS_PER_SECOND;

/// A small edit: sequence "Main" (25 fps, 1080x1920, 48 kHz) with two clips on V1 (the second at
/// 200 %), a nested sequence "Nest" (23.976 fps, a Black Video clip) on a locked V2, a linked audio
/// clip on A1, a muted A2, a caption track, a transition, effects, sequence and clip markers, bins,
/// and an `ObjectID` reused deep inside UI preference data.
const FULL: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<PremiereData Version="3">
	<Project ObjectRef="1"/>
	<Project ObjectID="1" ClassID="62ad66dd-0dcd-42da-a660-6d8fbde94876" Version="43">
		<RootProjectItem ObjectURef="root"/>
		<NextID>2000</NextID>
	</Project>
	<ProjectPanelPrefs ObjectID="77">
		<Deep><TimecodeColumn ObjectID="400" Version="1"><Column.Name>Subclip End</Column.Name></TimecodeColumn></Deep>
	</ProjectPanelPrefs>
	<RootProjectItem ObjectUID="root" Version="1">
		<ProjectItemContainer Version="1">
			<Items Version="1">
				<Item Index="0" ObjectURef="bin-footage"/>
				<Item Index="1" ObjectURef="cpi-main"/>
				<Item Index="2" ObjectURef="cpi-nest"/>
			</Items>
		</ProjectItemContainer>
	</RootProjectItem>
	<BinProjectItem ObjectUID="bin-footage" Version="1">
		<ProjectItem Version="1"><Name>Footage</Name></ProjectItem>
		<ProjectItemContainer Version="1">
			<Items Version="1">
				<Item Index="0" ObjectURef="cpi-a"/>
				<Item Index="1" ObjectURef="cpi-c"/>
				<Item Index="2" ObjectURef="cpi-unused"/>
			</Items>
		</ProjectItemContainer>
	</BinProjectItem>
	<ClipProjectItem ObjectUID="cpi-a"><ProjectItem Version="1"><Name>intro_clip.mp4</Name></ProjectItem><MasterClip ObjectURef="mc-a"/></ClipProjectItem>
	<ClipProjectItem ObjectUID="cpi-c"><ProjectItem Version="1"><Name>narration.wav</Name></ProjectItem><MasterClip ObjectURef="mc-c"/></ClipProjectItem>
	<ClipProjectItem ObjectUID="cpi-unused"><ProjectItem Version="1"><Name>unused.mov</Name></ProjectItem><MasterClip ObjectURef="mc-unused"/></ClipProjectItem>
	<ClipProjectItem ObjectUID="cpi-main"><ProjectItem Version="1"><Name>Main</Name></ProjectItem><MasterClip ObjectURef="mc-main"/></ClipProjectItem>
	<ClipProjectItem ObjectUID="cpi-nest"><ProjectItem Version="1"><Name>Nest</Name></ProjectItem><MasterClip ObjectURef="mc-nest"/></ClipProjectItem>

	<Sequence ObjectUID="seq-main" ClassID="6a15d903-8739-11d5-af2d-9b7855ad8974" Version="11">
		<TrackGroups Version="1">
			<TrackGroup Version="1" Index="0"><First>228cda18-3625-4d2d-951e-348879e4ed93</First><Second ObjectRef="100"/></TrackGroup>
			<TrackGroup Version="1" Index="1"><First>80b8e3d5-6dca-4195-aefb-cb5f407ab009</First><Second ObjectRef="200"/></TrackGroup>
			<TrackGroup Version="1" Index="2"><First>d8143ffe-eec4-4d2a-a909-d5f7bf094dc5</First><Second ObjectRef="300"/></TrackGroup>
		</TrackGroups>
		<MarkerOwner Version="1"><Markers ObjectRef="900"/></MarkerOwner>
		<Name>Main</Name>
		<ID>1</ID>
	</Sequence>
	<VideoTrackGroup ObjectID="100" Version="13">
		<TrackGroup Version="1">
			<Tracks Version="1">
				<Track Index="1" ObjectURef="vt2"/>
				<Track Index="0" ObjectURef="vt1"/>
			</Tracks>
			<FrameRate>10160640000</FrameRate>
		</TrackGroup>
		<FrameRect>0,0,1080,1920</FrameRect>
	</VideoTrackGroup>
	<AudioTrackGroup ObjectID="200" Version="6">
		<TrackGroup Version="1">
			<Tracks Version="1">
				<Track Index="0" ObjectURef="at1"/>
				<Track Index="1" ObjectURef="at2"/>
			</Tracks>
			<FrameRate>5292000</FrameRate>
		</TrackGroup>
	</AudioTrackGroup>
	<DataTrackGroup ObjectID="300" Version="1">
		<TrackGroup Version="1">
			<Tracks Version="1"><Track Index="0" ObjectURef="dt1"/></Tracks>
			<FrameRate>10160640000</FrameRate>
		</TrackGroup>
	</DataTrackGroup>
	<VideoClipTrack ObjectUID="vt1" Version="1">
		<ClipTrack Version="2">
			<Track Version="3"><IsLocked>false</IsLocked><Index>0</Index><IsMuted>false</IsMuted><IsSyncLocked>true</IsSyncLocked><ID>1</ID></Track>
			<ClipItems Version="3">
				<TrackItems Version="1">
					<TrackItem Index="0" ObjectRef="400"/>
					<TrackItem Index="1" ObjectRef="401"/>
				</TrackItems>
			</ClipItems>
			<TransitionItems Version="3">
				<TrackItems Version="1"><TrackItem Index="0" ObjectRef="450"/></TrackItems>
			</TransitionItems>
		</ClipTrack>
	</VideoClipTrack>
	<VideoClipTrack ObjectUID="vt2" Version="1">
		<ClipTrack Version="2">
			<Track Version="3"><IsLocked>true</IsLocked><Index>1</Index><IsMuted>false</IsMuted><ID>2</ID></Track>
			<ClipItems Version="3"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="402"/></TrackItems></ClipItems>
		</ClipTrack>
	</VideoClipTrack>
	<AudioClipTrack ObjectUID="at1" Version="11">
		<ClipTrack Version="2">
			<Track Version="3"><IsLocked>false</IsLocked><Index>0</Index><IsMuted>false</IsMuted><ID>3</ID></Track>
			<ClipItems Version="3"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="500"/></TrackItems></ClipItems>
		</ClipTrack>
	</AudioClipTrack>
	<AudioClipTrack ObjectUID="at2" Version="11">
		<ClipTrack Version="2">
			<Track Version="3"><IsLocked>false</IsLocked><Index>1</Index><IsMuted>true</IsMuted><ID>4</ID></Track>
			<ClipItems Version="3"><MediaType>80b8e3d5-6dca-4195-aefb-cb5f407ab009</MediaType><Index>0</Index></ClipItems>
		</ClipTrack>
	</AudioClipTrack>
	<CaptionDataClipTrack ObjectUID="dt1" Version="1">
		<ClipTrack Version="2">
			<ClipItems Version="3"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="700"/></TrackItems></ClipItems>
		</ClipTrack>
	</CaptionDataClipTrack>

	<VideoClipTrackItem ObjectID="400" ClassID="368b0406-29e3-4923-9fcd-094fbf9a1089" Version="6">
		<ClipTrackItem Version="8">
			<ComponentOwner Version="1"><Components ObjectRef="800"/></ComponentOwner>
			<TrackItem Version="3"><Start>0</Start><End>254016000000</End></TrackItem>
			<SubClip ObjectRef="600"/>
			<MarkerOwner Version="1"><Markers ObjectRef="901"/></MarkerOwner>
		</ClipTrackItem>
		<FrameRect>0,0,1080,1920</FrameRect>
	</VideoClipTrackItem>
	<VideoClipTrackItem ObjectID="401" Version="6">
		<ClipTrackItem Version="8">
			<TrackItem Version="3"><Start>254016000000</Start><End>508032000000</End></TrackItem>
			<SubClip ObjectRef="601"/>
		</ClipTrackItem>
	</VideoClipTrackItem>
	<VideoClipTrackItem ObjectID="402" Version="6">
		<ClipTrackItem Version="8">
			<TrackItem Version="3"><Start>0</Start><End>508032000000</End></TrackItem>
			<SubClip ObjectRef="602"/>
		</ClipTrackItem>
	</VideoClipTrackItem>
	<AudioClipTrackItem ObjectID="500" ClassID="064ec682-9ba6-11d5-af2d-9ca32c7d6164" Version="11">
		<ClipTrackItem Version="8">
			<TrackItem Version="3"><Start>0</Start><End>254016000000</End></TrackItem>
			<SubClip ObjectRef="603"/>
		</ClipTrackItem>
	</AudioClipTrackItem>
	<VideoTransitionTrackItem ObjectID="450" Version="5">
		<TransitionTrackItem Version="3">
			<TrackItem Version="3"><Start>228614400000</Start><End>279417600000</End></TrackItem>
			<DisplayName>Cross Dissolve</DisplayName>
			<MatchName>AE.ADBE Cross Dissolve New</MatchName>
		</TransitionTrackItem>
	</VideoTransitionTrackItem>
	<CaptionDataClipTrackItem ObjectID="700" Version="3">
		<DataClipTrackItem Version="1"><ClipTrackItem Version="8"><TrackItem Version="3"><Start>0</Start><End>254016000000</End></TrackItem></ClipTrackItem></DataClipTrackItem>
	</CaptionDataClipTrackItem>

	<SubClip ObjectID="600" Version="5"><Clip ObjectRef="610"/><MasterClip ObjectURef="mc-a"/><Name>intro_clip.mp4</Name></SubClip>
	<SubClip ObjectID="601" Version="5"><Clip ObjectRef="611"/><MasterClip ObjectURef="mc-b"/><Name>fast b-roll</Name></SubClip>
	<SubClip ObjectID="602" Version="5"><Clip ObjectRef="612"/><MasterClip ObjectURef="mc-nest"/><Name>Nest</Name></SubClip>
	<SubClip ObjectID="603" Version="5"><Clip ObjectRef="613"/><MasterClip ObjectURef="mc-c"/><Name>narration.wav</Name></SubClip>
	<SubClip ObjectID="640" Version="5"><Clip ObjectRef="641"/><MasterClip ObjectURef="mc-black"/><Name>Black Video</Name></SubClip>

	<MasterClip ObjectUID="mc-a" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="610"/></Clips><Name>intro_clip.mp4</Name></MasterClip>
	<MasterClip ObjectUID="mc-b" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="611"/></Clips><Name>b-roll.mov</Name></MasterClip>
	<MasterClip ObjectUID="mc-c" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="613"/></Clips><Name>narration.wav</Name></MasterClip>
	<MasterClip ObjectUID="mc-unused" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="614"/></Clips><Name>unused.mov</Name></MasterClip>
	<MasterClip ObjectUID="mc-nest" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="612"/></Clips><Name>Nest</Name></MasterClip>
	<MasterClip ObjectUID="mc-main" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="615"/></Clips><Name>Main</Name></MasterClip>
	<MasterClip ObjectUID="mc-black" Version="11"><Clips Version="1"><Clip Index="0" ObjectRef="641"/></Clips><Name>Black Video</Name></MasterClip>

	<VideoClip ObjectID="610" Version="11"><Clip Version="18"><Source ObjectRef="620"/><InPoint>25401600000</InPoint><OutPoint>279417600000</OutPoint></Clip></VideoClip>
	<VideoClip ObjectID="611" Version="11"><Clip Version="18"><Source ObjectRef="621"/><PlaybackSpeed>2</PlaybackSpeed><InPoint>0</InPoint><OutPoint>508032000000</OutPoint></Clip></VideoClip>
	<VideoClip ObjectID="612" Version="11"><Clip Version="18"><Source ObjectRef="622"/><InPoint>0</InPoint><OutPoint>508032000000</OutPoint></Clip></VideoClip>
	<AudioClip ObjectID="613" Version="11"><Clip Version="18"><Source ObjectRef="623"/><InPoint>0</InPoint><OutPoint>254016000000</OutPoint></Clip></AudioClip>
	<VideoClip ObjectID="614" Version="11"><Clip Version="18"><Source ObjectRef="624"/></Clip></VideoClip>
	<VideoClip ObjectID="615" Version="11"><Clip Version="18"><Source ObjectRef="625"/></Clip></VideoClip>
	<VideoClip ObjectID="641" Version="11"><Clip Version="18"><Source ObjectRef="642"/><InPoint>0</InPoint><OutPoint>254016000000</OutPoint></Clip></VideoClip>

	<VideoMediaSource ObjectID="620" Version="2"><MediaSource Version="4"><Media ObjectURef="media-a"/></MediaSource></VideoMediaSource>
	<VideoMediaSource ObjectID="621" Version="2"><MediaSource Version="4"><Media ObjectURef="media-b"/></MediaSource></VideoMediaSource>
	<VideoSequenceSource ObjectID="622" Version="1"><SequenceSource Version="4"><Sequence ObjectURef="seq-nest"/></SequenceSource></VideoSequenceSource>
	<AudioMediaSource ObjectID="623" Version="2"><MediaSource Version="4"><Media ObjectURef="media-c"/></MediaSource></AudioMediaSource>
	<VideoMediaSource ObjectID="624" Version="2"><MediaSource Version="4"><Media ObjectURef="media-unused"/></MediaSource></VideoMediaSource>
	<VideoSequenceSource ObjectID="625" Version="1"><SequenceSource Version="4"><Sequence ObjectURef="seq-main"/></SequenceSource></VideoSequenceSource>
	<VideoMediaSource ObjectID="642" Version="2"><MediaSource Version="4"><Media ObjectURef="media-black"/></MediaSource></VideoMediaSource>

	<Media ObjectUID="media-a" Version="28">
		<VideoStream ObjectRef="630"/>
		<RelativePath>.\IN\intro_clip.mp4</RelativePath>
		<FilePath>C:\Projects\Media\intro_clip.mp4</FilePath>
		<Title>intro_clip.mp4</Title>
	</Media>
	<Media ObjectUID="media-b" Version="28"><RelativePath>.\IN\b-roll.mov</RelativePath><Title>b-roll.mov</Title></Media>
	<Media ObjectUID="media-c" Version="28"><AudioStream ObjectRef="631"/><FilePath>/Users/me/Audio/narration.wav</FilePath><Title>narration.wav</Title></Media>
	<Media ObjectUID="media-unused" Version="28"><FilePath>/Volumes/Shoot/unused.mov</FilePath><Title>unused.mov</Title></Media>
	<Media ObjectUID="media-black" Version="28"><FilePath>1112293707</FilePath><Title>Black Video</Title><Infinite>true</Infinite></Media>
	<VideoStream ObjectID="630" Version="19"><FrameRate>10160640000</FrameRate><Duration>2540160000000</Duration><FrameRect>0,0,1920,1080</FrameRect></VideoStream>
	<AudioStream ObjectID="631" Version="9"><FrameRate>5292000</FrameRate><Duration>2540160000000</Duration></AudioStream>

	<Sequence ObjectUID="seq-nest" Version="11">
		<TrackGroups Version="1">
			<TrackGroup Version="1" Index="0"><Second ObjectRef="101"/></TrackGroup>
			<TrackGroup Version="1" Index="1"><Second ObjectRef="201"/></TrackGroup>
		</TrackGroups>
		<Name>Nest</Name>
	</Sequence>
	<VideoTrackGroup ObjectID="101" Version="13">
		<TrackGroup Version="1"><Tracks Version="1"><Track Index="0" ObjectURef="vt-n1"/></Tracks><FrameRate>10594584000</FrameRate></TrackGroup>
		<FrameRect>0,0,1920,1080</FrameRect>
	</VideoTrackGroup>
	<AudioTrackGroup ObjectID="201" Version="6"><TrackGroup Version="1"><FrameRate>5760000</FrameRate></TrackGroup></AudioTrackGroup>
	<VideoClipTrack ObjectUID="vt-n1" Version="1">
		<ClipTrack Version="2">
			<Track Version="3"><Index>0</Index></Track>
			<ClipItems Version="3"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="410"/></TrackItems></ClipItems>
		</ClipTrack>
	</VideoClipTrack>
	<VideoClipTrackItem ObjectID="410" Version="6">
		<ClipTrackItem Version="8"><TrackItem Version="3"><Start>0</Start><End>254016000000</End></TrackItem><SubClip ObjectRef="640"/></ClipTrackItem>
	</VideoClipTrackItem>

	<VideoComponentChain ObjectID="800" Version="3">
		<ComponentChain Version="3"><Components Version="1"><Component Index="0" ObjectRef="810"/><Component Index="1" ObjectRef="811"/></Components></ComponentChain>
	</VideoComponentChain>
	<VideoFilterComponent ObjectID="810" Version="9">
		<Component Version="6"><Params Version="1"><Param Index="0" ObjectRef="820"/></Params><DisplayName>Motion</DisplayName><Intrinsic>true</Intrinsic></Component>
		<MatchName>AE.ADBE Motion</MatchName>
	</VideoFilterComponent>
	<VideoComponentParam ObjectID="820" Version="9"><IsTimeVarying>true</IsTimeVarying><Name>Scale</Name></VideoComponentParam>
	<VideoFilterComponent ObjectID="811" Version="9">
		<Component Version="6"><DisplayName>Lumetri Color</DisplayName></Component>
		<MatchName>AE.ADBE Lumetri</MatchName>
	</VideoFilterComponent>

	<Markers ObjectID="900" Version="1">
		<MarkerList><Marker Index="0" ObjectRef="911"/><Marker Index="1" ObjectRef="910"/></MarkerList>
	</Markers>
	<Marker ObjectID="910" Version="3"><DVAMarker>{"DVAMarker":{"mMarkerID":"m1","mStartTime":{"ticks":127008000000},"mName":"Beat","mComment":"drop","mType":"Comment"}}</DVAMarker></Marker>
	<Marker ObjectID="911" Version="3"><DVAMarker>{"DVAMarker":{"mMarkerID":"m2","mStartTime":{"ticks":"381024000000"},"mDuration":{"ticks":25401600000},"mName":"Act 2","mType":"Chapter"}}</DVAMarker></Marker>
	<Markers ObjectID="901" Version="1"><MarkerList><Marker Index="0" ObjectRef="912"/></MarkerList></Markers>
	<Marker ObjectID="912" Version="3"><DVAMarker>{"DVAMarker":{"mStartTime":{"ticks":50803200000},"mType":"Comment"}}</DVAMarker></Marker>

	<Link ObjectID="950" Version="1">
		<TrackItemGroup Version="1"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="400"/><TrackItem Index="1" ObjectRef="500"/></TrackItems></TrackItemGroup>
	</Link>
</PremiereData>
"#;

/// The fixture from the research notes' `conftest.py`, unchanged: it has no `Project` object.
const CONFTEST_MINIMAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
  <Sequence ObjectUID="seq-uid-001" ClassID="6a15d903-8739-11d5-af2d-9b7855ad8974" Version="11">
    <TrackGroups Version="1">
      <TrackGroup Version="1" Index="0"><First>228cda18-3625-4d2d-951e-348879e4ed93</First><Second ObjectRef="100"/></TrackGroup>
      <TrackGroup Version="1" Index="1"><First>80b8e3d5-6dca-4195-aefb-cb5f407ab009</First><Second ObjectRef="200"/></TrackGroup>
      <TrackGroup Version="1" Index="2"><First>d8143ffe-eec4-4d2a-a909-d5f7bf094dc5</First><Second ObjectRef="300"/></TrackGroup>
    </TrackGroups>
    <Name>Test Sequence</Name>
    <ID>1</ID>
  </Sequence>
  <VideoTrackGroup ObjectID="100" ClassID="9e9abf7a-0918-49c2-91ae-991b5dde77bb" Version="13">
    <TrackGroup Version="1">
      <Tracks Version="1"><Track Index="0" ObjectURef="vtrack-uid-001"/></Tracks>
      <FrameRate>10160640000</FrameRate>
      <NextTrackID>2</NextTrackID>
    </TrackGroup>
    <FrameRect>0,0,1080,1920</FrameRect>
  </VideoTrackGroup>
  <AudioTrackGroup ObjectID="200" ClassID="9b9238b9-53a8-4cc3-b03f-b36246d052e6" Version="6">
    <TrackGroup Version="1">
      <Tracks Version="1"><Track Index="0" ObjectURef="atrack-uid-001"/></Tracks>
      <FrameRate>5292000</FrameRate>
      <NextTrackID>2</NextTrackID>
    </TrackGroup>
  </AudioTrackGroup>
  <DataTrackGroup ObjectID="300" ClassID="b714b71d-6838-48dd-9b77-db19088ced7e" Version="1">
    <TrackGroup Version="1"><FrameRate>10160640000</FrameRate><NextTrackID>1</NextTrackID></TrackGroup>
  </DataTrackGroup>
  <VideoClipTrack ObjectUID="vtrack-uid-001" ClassID="f68dcd81-8805-11d5-af2d-9bfa89d4ddd4" Version="11">
    <ClipTrack Version="2">
      <Track Version="3"><Index>0</Index></Track>
      <ClipItems Version="3"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="400"/><TrackItem Index="1" ObjectRef="401"/></TrackItems></ClipItems>
    </ClipTrack>
  </VideoClipTrack>
  <AudioClipTrack ObjectUID="atrack-uid-001" ClassID="f68dcd81-8805-11d5-af2d-9bfa89d4ddd5" Version="11">
    <ClipTrack Version="2">
      <Track Version="3"><Index>0</Index></Track>
      <ClipItems Version="3"><TrackItems Version="1"><TrackItem Index="0" ObjectRef="500"/></TrackItems></ClipItems>
    </ClipTrack>
  </AudioClipTrack>
  <VideoClipTrackItem ObjectID="400" ClassID="368b0406-29e3-4923-9fcd-094fbf9a1089" Version="6">
    <ClipTrackItem Version="8"><TrackItem Version="3"><Start>0</Start><End>254016000000</End></TrackItem><SubClip ObjectRef="600"/></ClipTrackItem>
  </VideoClipTrackItem>
  <VideoClipTrackItem ObjectID="401" ClassID="368b0406-29e3-4923-9fcd-094fbf9a1089" Version="6">
    <ClipTrackItem Version="8"><TrackItem Version="3"><Start>254016000000</Start><End>508032000000</End></TrackItem><SubClip ObjectRef="601"/></ClipTrackItem>
  </VideoClipTrackItem>
  <AudioClipTrackItem ObjectID="500" ClassID="064ec682-9ba6-11d5-af2d-9ca32c7d6164" Version="11">
    <ClipTrackItem Version="8"><TrackItem Version="3"><Start>0</Start><End>508032000000</End></TrackItem><SubClip ObjectRef="602"/></ClipTrackItem>
  </AudioClipTrackItem>
  <SubClip ObjectID="600" ClassID="e0c58dc9-dbdd-4166-aef7-5db7e3f22e84" Version="5"><MasterClip ObjectURef="mc-uid-001"/><Name>intro_clip.mp4</Name></SubClip>
  <SubClip ObjectID="601" ClassID="e0c58dc9-dbdd-4166-aef7-5db7e3f22e84" Version="5"><MasterClip ObjectURef="mc-uid-002"/><Name>background.png</Name></SubClip>
  <SubClip ObjectID="602" ClassID="e0c58dc9-dbdd-4166-aef7-5db7e3f22e84" Version="5"><MasterClip ObjectURef="mc-uid-003"/><Name>narration.wav</Name></SubClip>
  <MasterClip ObjectUID="mc-uid-001" ClassID="fb11c33a-b0a9-4465-aa94-b6d5db2628cf" Version="11"><Name>intro_clip.mp4</Name></MasterClip>
  <MasterClip ObjectUID="mc-uid-002" ClassID="fb11c33a-b0a9-4465-aa94-b6d5db2628cf" Version="11"><Name>background.png</Name></MasterClip>
  <MasterClip ObjectUID="mc-uid-003" ClassID="fb11c33a-b0a9-4465-aa94-b6d5db2628cf" Version="11"><Name>narration.wav</Name></MasterClip>
  <Media ObjectUID="media-uid-001" ClassID="7a5c103e-f3ac-4391-b6b4-7cc3d2f9a7ff" Version="28">
    <FilePath>C:\Projects\Media\intro_clip.mp4</FilePath><Title>intro_clip.mp4</Title>
  </Media>
  <BinProjectItem ObjectUID="bin-uid-001" ClassID="dbfd6653-24da-480e-a35e-ba45e9504e4b" Version="11">
    <ProjectItem Version="1"><Name>Media Bin</Name></ProjectItem>
  </BinProjectItem>
  <VideoComponentParam ObjectID="9001" ClassID="test" Version="1"><Name>Opacity</Name></VideoComponentParam>
  <VideoFilterComponent ObjectID="9002" ClassID="test" Version="1"><Name>Motion</Name></VideoFilterComponent>
</PremiereData>
"#;

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

fn opts() -> ImportOptions {
    ImportOptions { base_dir: Some("/proj".into()), name: Some("edit".into()), ..Default::default() }
}

fn import(bytes: &[u8]) -> filmcraft_interchange::Result<(Imported, Report)> {
    import_with(bytes, Format::Prproj, &opts())
}

/// The whole import, field by field. `PartialEq` can't compare imports: the intrinsic Motion
/// effect's "auto" parameters are NaN, which never equals itself.
fn dump(r: &(Imported, Report)) -> String {
    format!("{r:#?}")
}

fn seq_named<'a>(im: &'a Imported, name: &str) -> (filmcraft_project::ItemId, &'a Sequence) {
    im.project.items.values().find(|i| i.name == name).and_then(|i| i.as_sequence().map(|s| (i.id, s))).unwrap_or_else(|| panic!("no sequence {name}"))
}

fn media_path(im: &Imported, c: &TrackItem) -> Option<String> {
    match &im.project.item(c.item)?.kind {
        ItemKind::Media(m) => match &m.media {
            MediaRef::File { path } => Some(path.clone()),
            MediaRef::Generator(_) => None,
        },
        _ => None,
    }
}

#[test]
fn full_project_maps_sequences_tracks_clips_markers_and_bins() {
    let (im, report) = import(FULL.as_bytes()).unwrap();
    let (main_id, main) = seq_named(&im, "Main");
    let (nest_id, nest) = seq_named(&im, "Nest");
    assert_eq!(im.sequences, vec![main_id], "the nested sequence is not a top-level one");
    assert_eq!(im.project.name, "edit");

    assert_eq!(main.settings.frame_rate, FrameRate::FPS_25);
    assert_eq!((main.settings.width, main.settings.height), (1080, 1920));
    assert_eq!(main.settings.sample_rate, 48_000);
    assert_eq!(nest.settings.frame_rate, FrameRate::FPS_23_976);
    assert_eq!(nest.settings.sample_rate, 44_100);

    // tracks in Index order (the list gives V2 first), lock, mute
    assert_eq!(main.video_tracks.len(), 2);
    assert_eq!(main.audio_tracks.len(), 2);
    assert!(!main.video_tracks[0].locked && main.video_tracks[1].locked);
    assert!(main.video_tracks[0].enabled);
    assert!(!main.audio_tracks[0].muted && main.audio_tracks[1].muted);

    // V1: exact ticks, speed
    let v1 = &main.video_tracks[0].items;
    assert_eq!(v1.len(), 2);
    assert_eq!((v1[0].start, v1[0].duration, v1[0].source_in), (Tick(0), Tick(SEC), Tick(SEC / 10)));
    assert_eq!(v1[0].name, "intro_clip.mp4");
    assert_eq!(media_path(&im, &v1[0]).as_deref(), Some(r"C:\Projects\Media\intro_clip.mp4"));
    assert_eq!((v1[1].start, v1[1].duration, v1[1].speed, v1[1].reverse), (Tick(SEC), Tick(SEC), 2.0, false));
    assert_eq!(v1[1].name, "fast b-roll");
    assert_eq!(v1[1].source_out(), Tick(2 * SEC), "2x speed consumes twice the timeline length of source");
    assert_eq!(media_path(&im, &v1[1]).as_deref(), Some("/proj/IN/b-roll.mov"), "a relative path resolves against the document directory");
    // clip marker
    assert_eq!(v1[0].markers.len(), 1);
    assert_eq!(v1[0].markers[0].start, Tick(SEC / 5));

    // V2: the nested sequence, and inside it a Black Video generator
    let v2 = &main.video_tracks[1].items;
    assert_eq!(v2.len(), 1);
    assert_eq!(v2[0].item, nest_id);
    assert_eq!(v2[0].duration, Tick(2 * SEC));
    let black = &nest.video_tracks[0].items[0];
    match &im.project.item(black.item).unwrap().kind {
        ItemKind::Media(m) => assert!(matches!(m.media, MediaRef::Generator(Generator::BlackVideo))),
        k => panic!("{k:?}"),
    }

    // A1 linked to the first V1 clip
    let a1 = &main.audio_tracks[0].items;
    assert_eq!(a1.len(), 1);
    assert_eq!(media_path(&im, &a1[0]).as_deref(), Some("/Users/me/Audio/narration.wav"));
    assert!(a1[0].link.is_some());
    assert_eq!(a1[0].link, v1[0].link);
    assert_eq!(v1[1].link, None);

    // sequence markers, in time order, with name, comment, duration and kind
    assert_eq!(main.markers.len(), 2);
    assert_eq!((main.markers[0].start, main.markers[0].name.as_str(), main.markers[0].comment.as_str()), (Tick(SEC / 2), "Beat", "drop"));
    assert_eq!(main.markers[1].start, Tick(3 * SEC / 2));
    assert_eq!(main.markers[1].kind, MarkerKind::Chapter);
    assert_eq!(main.markers[1].duration, Tick(SEC / 10));

    // bins: the Footage bin holds the two used files and the unused one
    let footage = im.project.root.children.iter().find_map(|c| match c {
        filmcraft_project::BinEntry::Bin(b) if b.name == "Footage" => Some(b),
        _ => None,
    });
    let footage = footage.expect("Footage bin");
    let names: Vec<&str> = footage
        .children
        .iter()
        .filter_map(|c| match c {
            filmcraft_project::BinEntry::Item(i) => im.project.item(*i).map(|i| i.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["intro_clip.mp4", "narration.wav", "unused.mov"]);

    // what was not imported is named, with counts, and nothing dangled
    assert!(report.mentions("Cross Dissolve"), "{report}");
    assert!(report.mentions("Lumetri Color"), "{report}");
    assert!(report.mentions("Motion keyframes"), "{report}");
    assert!(report.mentions("caption"), "{report}");
    assert!(!report.mentions("not in the project file"), "{report}");
    assert!(report.warnings().all(|w| w.count >= 1));
}

#[test]
fn plain_and_gzip_give_identical_imports() {
    let plain = dump(&import(FULL.as_bytes()).unwrap());
    let gz = dump(&import(&gzip(FULL.as_bytes())).unwrap());
    assert_eq!(plain, gz);
    // a UTF-8 BOM changes nothing either
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(FULL.as_bytes());
    assert_eq!(dump(&import(&bom).unwrap()), plain);
    assert_eq!(dump(&import(&gzip(&bom)).unwrap()), plain);
}

/// Project version 45 drops `<Start>` when it is zero; the edit must come out the same.
#[test]
fn version_45_shape_without_zero_starts_matches_version_43() {
    let v45 =
        FULL.replace("<Start>0</Start>", "").replace(r#"Version="43""#, r#"Version="45""#).replace(r#"<TrackItem Version="3">"#, r#"<TrackItem Version="4">"#);
    assert!(!v45.contains("<Start>0</Start>"));
    let a = import(FULL.as_bytes()).unwrap();
    let b = import(v45.as_bytes()).unwrap();
    assert_eq!(dump(&a), dump(&b));
    let (_, main) = seq_named(&b.0, "Main");
    assert_eq!(main.video_tracks[0].items[0].start, Tick(0));
}

#[test]
fn detection_needs_a_premiere_data_root_not_just_gzip() {
    assert_eq!(Format::from_extension("prproj"), Some(Format::Prproj));
    assert_eq!(Format::from_extension(".PRPROJ"), Some(Format::Prproj));
    assert_eq!(Format::Prproj.extension(), "prproj");
    assert!(Format::ALL.contains(&Format::Prproj));
    assert_eq!(detect(FULL.as_bytes(), Some("prproj")), Some(Format::Prproj));
    assert_eq!(detect(FULL.as_bytes(), None), Some(Format::Prproj));
    assert_eq!(detect(&gzip(FULL.as_bytes()), Some("prproj")), Some(Format::Prproj));
    assert_eq!(detect(&gzip(FULL.as_bytes()), None), Some(Format::Prproj));
    // gzip of something else is not a Premiere project
    let xmeml = r#"<?xml version="1.0"?><xmeml version="4"><sequence/></xmeml>"#;
    assert_ne!(detect(&gzip(xmeml.as_bytes()), Some("prproj")), Some(Format::Prproj));
    assert_eq!(detect(&gzip(b"hello"), Some("prproj")), None);
    assert_eq!(detect(xmeml.as_bytes(), Some("prproj")), Some(Format::Fcp7Xml));
    assert_eq!(detect(br#"<?xml version="1.0"?><Other/>"#, Some("prproj")), None);
    assert_eq!(detect(&[0x1f, 0x8b], Some("prproj")), None);
    // an element merely named like the root further in does not count
    assert_eq!(detect(br#"<?xml version="1.0"?><x><PremiereData/></x>"#, None), None);
}

#[test]
fn damaged_compression_and_encoding_are_errors() {
    let gz = gzip(FULL.as_bytes());
    for cut in [gz.len() / 2, gz.len() - 1, gz.len() - 4, gz.len() - 8, 10, 3] {
        let e = import(&gz[..cut]).unwrap_err();
        assert!(matches!(e, Error::Parse { .. }), "cut at {cut}: {e:?}");
    }
    // invalid UTF-8, plain and compressed
    let mut bad = FULL.as_bytes().to_vec();
    let at = FULL.find("<Name>Main").unwrap() + 6;
    bad[at] = 0xff;
    assert!(matches!(import(&bad), Err(Error::Parse { .. })));
    assert!(matches!(import(&gzip(&bad)), Err(Error::Parse { .. })));
    // not XML, not Premiere XML, nothing at all
    for junk in [&b""[..], b"\x1f\x8b", b"PremiereData", br#"<?xml version="1.0"?><xmeml version="4"/>"#, b"<PremiereData"] {
        assert!(import(junk).is_err(), "{:?}", String::from_utf8_lossy(junk));
    }
    // a DTD (entity expansion) is refused outright
    let dtd = FULL.replacen("<PremiereData", "<!DOCTYPE PremiereData [<!ENTITY a \"aaaa\">]>\n<PremiereData", 1);
    assert!(import(dtd.as_bytes()).is_err());
}

#[test]
fn a_project_without_a_project_object_is_an_error() {
    // the research notes' own synthetic fixture lacks it
    let e = import(CONFTEST_MINIMAL.as_bytes()).unwrap_err();
    assert!(e.to_string().contains("Project"), "{e}");
    assert!(import(&gzip(CONFTEST_MINIMAL.as_bytes())).is_err());
    // the pointer alone is not the object
    let pointer_only = CONFTEST_MINIMAL.replacen("<PremiereData Version=\"3\">", "<PremiereData Version=\"3\"><Project ObjectRef=\"1\"/>", 1);
    assert!(import(pointer_only.as_bytes()).is_err());
}

#[test]
fn the_conftest_fixture_with_a_project_imports_offline_clips() {
    let with_project = CONFTEST_MINIMAL.replacen(
        "<PremiereData Version=\"3\">",
        "<PremiereData Version=\"3\"><Project ObjectRef=\"1\"/><Project ObjectID=\"1\" Version=\"42\"/>",
        1,
    );
    for bytes in [with_project.as_bytes().to_vec(), gzip(with_project.as_bytes())] {
        let (im, report) = import(&bytes).unwrap();
        let (id, seq) = seq_named(&im, "Test Sequence");
        assert_eq!(im.sequences, vec![id]);
        assert_eq!(seq.settings.frame_rate, FrameRate::FPS_25);
        assert_eq!(seq.video_tracks[0].items.len(), 2);
        assert_eq!(seq.audio_tracks[0].items.len(), 1);
        assert_eq!(seq.video_tracks[0].items[1].start, Tick(SEC));
        // its master clips name no source: offline, and the report says so with a count
        let e = report.entries.iter().find(|e| e.message.contains("without source media")).expect("reported");
        assert_eq!(e.count, 3, "{report}");
        let m = im.project.item(seq.video_tracks[0].items[0].item).and_then(|i| i.as_media()).unwrap();
        assert!(m.offline);
    }
}

/// Sequence A uses B, B uses A (and C uses itself): no recursion without a bound, a report entry,
/// and both sequences still come in.
#[test]
fn cyclic_nested_sequences_are_cut_and_reported() {
    let seq = |uid: &str, name: &str, vg: &str, track: &str, item: &str| {
        format!(
            r#"<Sequence ObjectUID="{uid}"><TrackGroups><TrackGroup Index="0"><Second ObjectRef="{vg}"/></TrackGroup></TrackGroups><Name>{name}</Name></Sequence>
<VideoTrackGroup ObjectID="{vg}"><TrackGroup><Tracks><Track Index="0" ObjectURef="{track}"/></Tracks><FrameRate>10160640000</FrameRate></TrackGroup><FrameRect>0,0,1920,1080</FrameRect></VideoTrackGroup>
<VideoClipTrack ObjectUID="{track}"><ClipTrack><ClipItems><TrackItems><TrackItem Index="0" ObjectRef="{item}"/></TrackItems></ClipItems></ClipTrack></VideoClipTrack>"#
        )
    };
    let nest_clip = |item: &str, sub: &str, clip: &str, src: &str, target: &str| {
        format!(
            r#"<VideoClipTrackItem ObjectID="{item}"><ClipTrackItem><TrackItem><Start>0</Start><End>254016000000</End></TrackItem><SubClip ObjectRef="{sub}"/></ClipTrackItem></VideoClipTrackItem>
<SubClip ObjectID="{sub}"><Clip ObjectRef="{clip}"/><Name>nest</Name></SubClip>
<VideoClip ObjectID="{clip}"><Clip><Source ObjectRef="{src}"/></Clip></VideoClip>
<VideoSequenceSource ObjectID="{src}"><SequenceSource><Sequence ObjectURef="{target}"/></SequenceSource></VideoSequenceSource>"#
        )
    };
    let doc = format!(
        r#"<?xml version="1.0"?><PremiereData Version="3"><Project ObjectID="1" Version="43"/>{}{}{}{}{}{}</PremiereData>"#,
        seq("A", "A", "10", "ta", "20"),
        seq("B", "B", "11", "tb", "21"),
        seq("C", "C", "12", "tc", "22"),
        nest_clip("20", "30", "40", "50", "B"),
        nest_clip("21", "31", "41", "51", "A"),
        nest_clip("22", "32", "42", "52", "C"),
    );
    for bytes in [doc.as_bytes().to_vec(), gzip(doc.as_bytes())] {
        let (im, report) = import(&bytes).unwrap();
        assert!(report.mentions("nested inside itself"), "{report}");
        assert_eq!(im.project.items.values().filter(|i| i.as_sequence().is_some()).count(), 3);
        assert!(!im.sequences.is_empty(), "a project whose sequences all nest each other still lists them");
        let (_, a) = seq_named(&im, "A");
        let (b_id, _) = seq_named(&im, "B");
        assert_eq!(a.video_tracks[0].items[0].item, b_id, "A uses B");
    }
}

#[test]
fn a_very_deep_chain_of_nests_stops_at_the_depth_limit() {
    let mut doc = String::from(r#"<?xml version="1.0"?><PremiereData Version="3"><Project ObjectID="1" Version="43"/>"#);
    let n = 200;
    for i in 0..n {
        let next = i + 1;
        doc.push_str(&format!(
            r#"<Sequence ObjectUID="s{i}"><TrackGroups><TrackGroup Index="0"><Second ObjectRef="g{i}"/></TrackGroup></TrackGroups><Name>S{i}</Name></Sequence>
<VideoTrackGroup ObjectID="g{i}"><TrackGroup><Tracks><Track Index="0" ObjectURef="t{i}"/></Tracks><FrameRate>10160640000</FrameRate></TrackGroup></VideoTrackGroup>
<VideoClipTrack ObjectUID="t{i}"><ClipTrack><ClipItems><TrackItems><TrackItem ObjectRef="i{i}"/></TrackItems></ClipItems></ClipTrack></VideoClipTrack>
<VideoClipTrackItem ObjectID="i{i}"><ClipTrackItem><TrackItem><End>254016000000</End></TrackItem><SubClip ObjectRef="sc{i}"/></ClipTrackItem></VideoClipTrackItem>
<SubClip ObjectID="sc{i}"><Clip ObjectRef="c{i}"/></SubClip>
<VideoClip ObjectID="c{i}"><Clip><Source ObjectRef="src{i}"/></Clip></VideoClip>
<VideoSequenceSource ObjectID="src{i}"><SequenceSource><Sequence ObjectURef="s{next}"/></SequenceSource></VideoSequenceSource>"#
        ));
    }
    doc.push_str("</PremiereData>");
    let (im, report) = import(doc.as_bytes()).unwrap();
    assert!(report.mentions("nested more than"), "{report}");
    assert!(report.mentions("not in the project file"), "the last nest points nowhere: {report}");
    assert!(im.project.items.values().filter(|i| i.as_sequence().is_some()).count() >= 64);
}

#[test]
fn dangling_references_are_reported_not_fatal() {
    let doc = FULL
        .replace(r#"<SubClip ObjectRef="601"/>"#, r#"<SubClip ObjectRef="99999"/>"#)
        .replace(r#"<Media ObjectURef="media-c"/>"#, r#"<Media ObjectURef="nowhere"/>"#)
        .replace(r#"<Track Index="1" ObjectURef="vt2"/>"#, r#"<Track Index="1" ObjectURef="missing-track"/>"#)
        .replace(r#"<Markers ObjectRef="900"/>"#, r#"<Markers ObjectRef="0"/>"#)
        .replace(r#"<RootProjectItem ObjectURef="root"/>"#, r#"<RootProjectItem ObjectURef="no-root"/>"#);
    let (im, report) = import(doc.as_bytes()).unwrap();
    let dangling: u32 = report.entries.iter().filter(|e| e.message.contains("not in the project file")).map(|e| e.count).sum();
    assert!(dangling >= 5, "{report}");
    let (_, main) = seq_named(&im, "Main");
    assert_eq!(main.video_tracks.len(), 1, "the missing V2 is left out");
    assert!(main.markers.is_empty());
    // the clip whose sub clip is missing still comes in, offline
    let v1 = &main.video_tracks[0].items;
    assert_eq!(v1.len(), 2);
    assert!(im.project.item(v1[1].item).and_then(|i| i.as_media()).is_some_and(|m| m.offline));
}

/// Out-of-range numbers everywhere: errors and report entries, never a panic or overflow.
#[test]
fn hostile_numbers_are_rejected_or_defaulted() {
    let cases: &[(&str, &str)] = &[
        ("<End>254016000000</End>", "<End>9223372036854775807</End>"),
        ("<End>254016000000</End>", "<End>-5</End>"),
        ("<Start>254016000000</Start>", "<Start>-9223372036854775808</Start>"),
        ("<Start>254016000000</Start>", "<Start>x</Start>"),
        ("<FrameRate>10160640000</FrameRate>", "<FrameRate>0</FrameRate>"),
        ("<FrameRate>10160640000</FrameRate>", "<FrameRate>-1</FrameRate>"),
        ("<FrameRate>10160640000</FrameRate>", "<FrameRate>1</FrameRate>"),
        ("<FrameRate>5292000</FrameRate>", "<FrameRate>0</FrameRate>"),
        ("<FrameRect>0,0,1080,1920</FrameRect>", "<FrameRect>-9223372036854775808,0,9223372036854775807,1</FrameRect>"),
        ("<FrameRect>0,0,1080,1920</FrameRect>", "<FrameRect>0,0,0,0</FrameRect>"),
        ("<PlaybackSpeed>2</PlaybackSpeed>", "<PlaybackSpeed>NaN</PlaybackSpeed>"),
        ("<PlaybackSpeed>2</PlaybackSpeed>", "<PlaybackSpeed>0</PlaybackSpeed>"),
        ("<PlaybackSpeed>2</PlaybackSpeed>", "<PlaybackSpeed>1e300</PlaybackSpeed>"),
        ("<PlaybackSpeed>2</PlaybackSpeed>", "<PlaybackSpeed>-2</PlaybackSpeed>"),
        ("<InPoint>25401600000</InPoint>", "<InPoint>-9223372036854775808</InPoint>"),
        (r#"{"ticks":127008000000}"#, r#"{"ticks":9.9e300}"#),
        (r#"{"ticks":127008000000}"#, r#"{"ticks":-9223372036854775808}"#),
        (r#""mType":"Comment"}}"#, r#""mType":"Comment"}"#),
        (r#"<Track Index="0" ObjectURef="vt1"/>"#, r#"<Track Index="-9223372036854775808" ObjectURef="vt1"/>"#),
        (r#"Version="43""#, r#"Version="99999999999999999999""#),
    ];
    for (from, to) in cases {
        assert!(FULL.contains(from), "{from}");
        let doc = FULL.replace(from, to);
        let r = std::panic::catch_unwind(|| import(doc.as_bytes()));
        assert!(r.is_ok(), "panicked on {to}");
    }
    let doc = FULL.replace("<PlaybackSpeed>2</PlaybackSpeed>", "<PlaybackSpeed>-2</PlaybackSpeed>");
    let (im, report) = import(doc.as_bytes()).unwrap();
    let (_, main) = seq_named(&im, "Main");
    assert!(main.video_tracks[0].items[1].reverse);
    assert!(report.mentions("reverse"));
    let doc = FULL.replace("<End>254016000000</End>", "<End>-5</End>");
    let (_, report) = import(doc.as_bytes()).unwrap();
    assert!(report.mentions("ends before it starts"), "{report}");
}

#[test]
fn overlapping_clips_move_to_a_new_track() {
    let doc = FULL.replace("<Start>254016000000</Start><End>508032000000</End>", "<Start>127008000000</Start><End>508032000000</End>");
    let (im, report) = import(doc.as_bytes()).unwrap();
    assert!(report.mentions("overlapping"), "{report}");
    let (_, main) = seq_named(&im, "Main");
    assert_eq!(main.video_tracks[0].items.len(), 1);
    assert_eq!(main.video_tracks[1].items.len(), 2, "moved up next to the nest (they do not overlap there)");
}

/// Truncation at every point and random byte flips, plain and compressed: never a panic.
#[test]
fn mutations_never_panic() {
    let plain = FULL.as_bytes();
    let gz = gzip(plain);
    for doc in [plain, &gz[..]] {
        let step = (doc.len() / 300).max(1);
        for cut in (0..doc.len()).step_by(step) {
            let r = std::panic::catch_unwind(|| {
                let _ = detect(&doc[..cut], Some("prproj"));
                let _ = import(&doc[..cut]);
            });
            assert!(r.is_ok(), "panic at truncation {cut}");
        }
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        for round in 0..300 {
            let mut m = doc.to_vec();
            for _ in 0..4 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                let at = (seed >> 33) as usize % m.len();
                m[at] ^= 1 << ((seed >> 29) & 7);
            }
            let r = std::panic::catch_unwind(|| {
                let _ = detect(&m, Some("prproj"));
                let _ = import(&m);
            });
            assert!(r.is_ok(), "panic in mutation round {round}");
        }
    }
    // structural mutations: digits swapped into every number
    for (i, _) in FULL.match_indices(|c: char| c.is_ascii_digit()).step_by(7) {
        let mut s = FULL.to_string();
        s.replace_range(i..i + 1, "9");
        let r = std::panic::catch_unwind(|| import(s.as_bytes()));
        assert!(r.is_ok(), "panic with a digit changed at {i}");
    }
}

#[test]
fn deep_xml_nesting_does_not_overflow_the_stack() {
    let depth = 100_000;
    let doc = format!(
        r#"<?xml version="1.0"?><PremiereData Version="3"><Project ObjectID="1" Version="43"/><Prefs ObjectID="2">{}{}</Prefs></PremiereData>"#,
        "<a>".repeat(depth),
        "</a>".repeat(depth)
    );
    let e = import(doc.as_bytes()).unwrap_err();
    assert!(e.to_string().contains("nested more than"), "{e}");
    assert!(import(&gzip(doc.as_bytes())).is_err());
    // ordinary depths still parse
    let shallow = FULL.replace("<Deep>", &format!("<Deep>{}", "<a>".repeat(500))).replace("</Deep>", &format!("{}</Deep>", "</a>".repeat(500)));
    assert!(import(shallow.as_bytes()).is_ok());
}

#[test]
fn a_project_without_sequences_or_media_is_empty() {
    let doc = r#"<?xml version="1.0"?><PremiereData Version="3"><Project ObjectRef="1"/><Project ObjectID="1" Version="43"/></PremiereData>"#;
    assert!(matches!(import(doc.as_bytes()), Err(Error::Empty)));
}

#[test]
fn unknown_versions_are_reported() {
    let doc = FULL.replace(r#"Version="43""#, r#"Version="39""#);
    let (_, report) = import(doc.as_bytes()).unwrap();
    assert!(report.mentions("version 39"), "{report}");
    let (_, report) = import(FULL.as_bytes()).unwrap();
    assert!(!report.mentions("version"), "{report}");
}

#[test]
fn premiere_projects_are_import_only() {
    let (im, _) = import(FULL.as_bytes()).unwrap();
    let seq = im.sequences[0];
    let e = export(&im.project, seq, Format::Prproj, &Default::default()).unwrap_err();
    assert!(e.to_string().contains("imported but not exported"), "{e}");
    assert!(Format::Prproj.is_import_only());
    assert!(Format::ALL.iter().filter(|f| **f != Format::Prproj).all(|f| !f.is_import_only()));
}
