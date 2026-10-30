# filmcraft-hevc

Clean-room, pure-Rust (no `unsafe`) H.265 / HEVC decoder, implemented from the public ITU-T
Rec. H.265 (ISO/IEC 23008-2) specification. It is an L0 crate: it depends only on
`filmcraft-bitstream`, `thiserror`, and optionally `rayon` (default feature `threads`), and builds
for `wasm32-unknown-unknown`.

All syntax tables (the CABAC `initValue` tables arranged per `initType` following Table 9-4, default
scaling lists, `intraPredAngle` / `invAngle`, the chroma QP table, the deblocking beta'/tC' table,
the luma / chroma interpolation filter taps and the 32x32 DCT matrix) were extracted from the
specification text by [`tools/extract_tables.py`](tools/extract_tables.py) (it only reads the text
of the ITU-T PDF, see the script header); the output is `src/spec_tables.rs`. Scan orders are
derived by the algorithms of 6.5.3-6.5.5. The 4:2:2 chroma intra mode mapping (Table 8-3, in the
corrected form of later editions of the specification) is entered by hand in `src/tables.rs` and
verified by the JCT-VC `ADJUST_IPRED_ANGLE_A_RExt_Mitsubishi` conformance stream. No code was taken
or ported from FFmpeg, libde265, x265 or the HM reference software; ffmpeg / libx265 / VideoToolbox are only used as external test oracles
and fixture generators.

## Status

| Area | Status |
|---|---|
| NAL / Annex-B / `hvcC` length-prefixed input | done |
| VPS, SPS (VUI, HRD skipped, scaling lists, st/lt RPS, PCM), PPS (tiles, WPP, deblocking control, scaling lists, list modification) | done |
| Slice segment header (all fields: st/lt RPS, list modification, pred weight table, entry points, header extension) | done |
| POC, RPS marking, missing-reference generation, RefPicList construction, DPB output / bumping (C.5.2) | done |
| CABAC: all Main / Main 10 syntax elements, WPP context sync, dependent slice context restore, tiles | done |
| Coding quadtree, CU (skip / intra / inter, AMP, PCM, transquant bypass), PU, transform tree | done |
| Intra: 35 modes, reference substitution, [1 2 1] filtering, strong intra smoothing, constrained intra | done |
| Inter: merge (spatial, temporal, combined bi-pred, zero), AMVP, TMVP, 8-tap / 4-tap interpolation, default and explicit weighted prediction | done |
| Scaling (flat and scaling lists), DCT 4..32, DST 4x4, transform skip, sign data hiding | done |
| Deblocking (bS, slice / tile boundary rules, PCM / bypass exemption), SAO (band / edge, boundary rules) | done |
| Frame-level multithreading (CTB-row granularity dependencies) | done |
| Draft mode (non-reference pictures skip deblocking / SAO; reduced-resolution playback only) | done |
| Tiles / WPP | parsed and decoded sequentially within a picture (parallelism is across pictures) |
| 4:2:2 (Main 4:2:2 10 / 12 and Intra), 12-bit 4:2:0 (Main 12): geometry, 4:2:2 intra mode mapping, two chroma blocks (and cbfs) per TU, QpC = Min(qPi, 51), taller chroma MC, chroma deblocking grid and SAO; CU chroma QP offsets (`chroma_qp_offset_list`) | done (bit-exact on the x265 4:2:2 fixtures and the JCT-VC Main_422_10_A/B, GENERAL_*b_420/422, IPCM_A and ADJUST_IPRED_ANGLE streams) |
| Other range-extension tools: transform skip rotation / context / above 4x4, implicit / explicit RDPCM, persistent Rice adaptation, intra smoothing disabled, high-precision weighted prediction offsets, SAO offset scaling, extended precision, CABAC bypass alignment, cross-component prediction | not yet (`Error::Unsupported`: parsed, then refused because no 4:2:0 / 4:2:2 reference stream exercises them) |
| Tiles together with WPP (allowed outside Main / Main 10) | not yet (`Error::Unsupported`: the one reference stream, `WPP_AND_TILE_10Bit422Test_HIGH_TP_444_10BIT_RExt_Apple_2`, decodes differently after the first substream although the CABAC parse stays in sync: every substream ends exactly at its signalled entry point; open as a known gap) |
| 4:4:4 / monochrome, more than 12 bits, unequal luma / chroma bit depths, tiles in 4:2:2 streams, CU chroma QP offset lists with several entries or with dependent slice segments | not yet (`Error::Unsupported`: no reference stream covers them) |
| SCC, multilayer (SHVC / MV-HEVC), 3D extensions | not supported (layers > 0 are ignored) |

## API

```rust
use filmcraft_hevc::{Decoder, Plane};

let mut dec = Decoder::new(); // or Decoder::with_threads(n), Decoder::from_hvcc(&hvcc)?
for (pts, access_unit) in access_units {
    for pic in dec.decode(access_unit, pts)? {
        // pic.width / pic.height (cropped), pic.bit_depth, pic.chroma_format_idc (1 = 4:2:0, 2 = 4:2:2),
        // pic.y / pic.u / pic.v: Plane::U8(Vec<u8>) for 8-bit, Plane::U16(Vec<u16>) above,
        // pic.y_stride / pic.uv_stride, pic.pts, pic.poc, pic.key (IRAP), pic.color (VUI), pic.sar
    }
}
for pic in dec.flush() { /* ... */ }
if let Some(err) = dec.take_error() { /* error reported by a decoding thread */ }
// dec.set_draft(true): draft mode (playback at reduced resolution only)
```

`decode` expects whole access units (a buffer with several complete access units — e.g. an entire
Annex-B file — also works). RASL pictures following a CRA that starts the stream are skipped, as
ffmpeg does. `Decoder::stats()` returns counters of what was decoded (slice types, IDR/CRA, tiles,
WPP, weighted prediction, TMVP, SAO, block kinds...).

### Architecture

Same design as `filmcraft-h264`: the calling thread parses parameter sets and slice headers and does
POC / RPS / DPB bookkeeping; the CTU layer of each picture runs as a job (rayon pool). A job
reconstructs into private planes, deblocks CTB row r once row r+1 is decoded, applies SAO to row r
once row r+1 is deblocked, and publishes final rows (samples + 16x16 compressed motion for TMVP)
into the shared `Frame` (`OnceLock` per CTB row). Motion compensation of later pictures blocks per
row on exactly the reference rows it reads. Samples are stored as `u8` for 8-bit pictures (half
the memory traffic in prediction reads, in-loop filters and output) and as `u16` above 8 bits: the
slice decoder and the filters are generic over the sample type (`picture::Sample`), monomorphised
once per type.

Draft mode (`Decoder::set_draft(true)`; FilmCraft enables it with Settings ▸ Playback ▸ Draft
decoding while the Program monitor plays at 1/2 or 1/4): sub-layer non-reference pictures
(TRAIL_N, RASL_N, ...) of the highest temporal sub-layer — no later picture predicts from them,
and TMVP reads only reference pictures — skip deblocking and SAO and are flagged
`Picture::draft`; every other picture is unchanged. libx265's non-reference B pictures are such
pictures.

Modules: `params` (VPS/SPS/PPS/tile layout), `slice` (NAL + slice header), `dpb` (POC, RPS, lists,
bumping), `cabac`, `slicedec` (CTU syntax + reconstruction), `mvpred` (merge / AMVP / TMVP),
`intra`, `inter`, `transform`, `filter` (deblocking, SAO, row publication).

## Tests

`cargo test -p filmcraft-hevc` (fixtures are generated on first use into `target/fixtures/hevc/`
with ffmpeg; tests print a message and skip when ffmpeg is absent).

- Unit tests: CABAC engine round trip against an encoder model and context initialisation, scan
  orders, DCT matrix properties, QpC, intra predictors, transform DC / bounded-region shortcuts,
  MV scaling.
- Synthetic streams (`src/synth_tests.rs`): a small CABAC encoder writes streams with what libx265
  never produces — tiles (uniform and explicit spacing, loop filter across tiles on / off), PCM
  (with and without in-loop filtering), multiple slices, dependent slice segments (starting inside
  and at the start of a tile), long-term reference pictures, reference list modification. Expected
  output is known exactly where filters leave PCM samples alone; every stream is also compared with
  ffmpeg.
- Robustness: hvcC input with pts round trip; randomly corrupted, truncated and garbage input never
  panics or deadlocks (single- and multi-threaded), including 4:2:2 / 12-bit streams and bit flips
  confined to the parameter sets (hostile chroma formats, bit depths, sizes).
- Range-extension conformance (`rext_conformance_streams`): with `FILMCRAFT_HEVC_REXT_DIR` pointing
  at the JCT-VC RExt bitstreams (not in the repository), every listed 4:2:0 / 4:2:2 stream must be
  present and decode to the MD5 of the suite's own reconstruction (the MD5s are in the test), and
  the ones using unsupported tools (4:4:4, RDPCM, persistent Rice, tiles + WPP) are refused with
  `Error::Unsupported`. Skipped without the variable.
- An SPS re-sent between the slices of a picture: identical content decodes as before, other
  content is refused (`sps_change_within_a_picture_is_refused`).
- Hostile parameter sets: every SPS `ue(v)` field at `u32::MAX`, one past each spec maximum and
  conformance window offsets whose sum wraps are refused (`max_value_sps_fields_are_refused`); a
  picture larger than the memory budget is refused before allocation
  (`oversized_picture_is_refused_before_allocation`, `memory_budget_*`).
- Conformance (`tests/conformance.rs`): every fixture is decoded with 1 thread, 3 threads and every
  core and compared sample-exactly with `ffmpeg -f rawvideo -pix_fmt yuv420p / yuv422p / ...10le / ...12le`; on mismatch
  the first differing frame, plane, sample, CTB and 8x8 block are reported. Output pts/POC order is
  checked too, and that no picture is flagged draft without draft mode. `compare_detects_mismatch`
  guards the harness itself. Draft mode (`draft_mode_changes_only_flagged_non_reference_pictures`):
  on B-pyramid / weighted fixtures, single- and frame-threaded, every unflagged picture is
  bit-exact and only flagged pictures differ.

### Fixture matrix (all bit-exact, single- and multi-threaded)

libx265 unless noted (x265 enables WPP by default, so most streams carry entry points).

| fixture | source / size | configuration |
|---|---|---|
| intra_main / intra_main10 | testsrc2+noise 352x288 | keyint=1, 8 / 10-bit |
| intra_nosao_nodbk | testsrc2+noise 352x288 | intra, no SAO, no deblocking |
| ultrafast / veryfast / medium / slow | 352x288 | presets (slow: rect + AMP) |
| medium_main10 / slow_main10 | 352x288 | Main 10 presets |
| p_only | 352x288 | bframes=0 |
| bframes | 352x288, 30 frames | bframes=8, b-pyramid, ref=4 |
| no_sao / no_deblock / deblock_m3_3 | 352x288 | loop filter variants |
| no_wpp | 352x288 | WPP off |
| rect_amp / min_cu16 | mandelbrot / testsrc2 | rect + AMP, min CU 16 |
| tskip | 352x288 | transform skip |
| weightp / weightp_main10 | fade-in | weightp + weightb |
| crf0 / crf51 / qp1_main10 | | quantizer extremes |
| lossless / cu_lossless | 176x144 | cu_transquant_bypass |
| slices4 | 352x288 | 4 slices per picture |
| scaling_list | 352x288 | default scaling lists |
| ctu16 / ctu32_tu4 | 352x288 | CTB 16, CTB 32 with deep TU trees, max TU 16 |
| no_signhide / constrained_intra / no_tmvp / no_strong_intra / max_merge1 | 352x288 | tool toggles |
| open_gop | 352x288, 40 frames | CRA + RASL / RADL, keyint 12 |
| keyint5_idr | 352x288 | closed GOP, IDR every 5 |
| odd_1918x1078 | 1918x1078 | conformance-window cropping |
| hd_1080p | 1920x1080 | preset fast |
| uhd_4k_main10 | 3840x2160 | Main 10 ultrafast |
| vt_main / vt_main10 | 640x360 | Apple VideoToolbox (hevc_videotoolbox), macOS only |
| synthetic (7 streams) | 96x64 | tiles, PCM, slices, dependent segments, LTR, list modification |
| intra_422_10 / intra_422_12 / intra_422_nofilt | 352x288 | Main 4:2:2 10 / 12 intra (no SAO / deblocking) |
| medium_422_8 / medium_422_10 / medium_422_12 | 352x288 | 4:2:2 8 / 10 / 12-bit, preset medium |
| bframes_422_10 / slices4_422_10 / rect_amp_422_10 | 352x288 | 4:2:2: B-pyramid, 4 slices, rect + AMP |
| weightp_422_10 / tskip_422_10 / chroma_qp_422_10 | 352x288 | 4:2:2: weighted prediction, transform skip, cb / cr QP offsets |
| scaling_list_422_10 / lossless_422_10 / crf0_422_10 | 352x288, 176x144 | 4:2:2: scaling lists, lossless, CRF 0 |
| odd_422_10 | 1918x1078 | 4:2:2 conformance-window cropping |
| medium_main12 | 352x288 | Main 12 (4:2:0) |

`cargo test -p filmcraft-hevc --test conformance coverage_report -- --ignored --nocapture` prints the
decoder statistics per fixture.

## Performance

`cargo test --release -p filmcraft-hevc --test perf -- --ignored --nocapture` (bit-exactness is
verified first). Apple M4 Pro (14 cores), measured while the machine was heavily shared (load
average 50-100), so these are lower bounds:

| stream | 1 thread | 14 threads |
|---|---|---|
| 1080p Main, x265 medium CRF 24, 60 frames | ~75 fps | ~225 fps |
| 2160p Main 10, x265 fast CRF 26, 20 frames | ~20 fps | ~95 fps |

Main 4:2:2 10 (M8e.1, `perf_1080p_422_10`), measured on the same machine at load average ~58
(8 threads), with the 4:2:0 Main stream re-measured alongside for scale:

| stream | 1 thread | 8 threads |
|---|---|---|
| 1080p Main 4:2:2 10, x265 medium CRF 22, 30 frames | 16.9 fps | 68.5 fps |
| 1080p Main, x265 medium CRF 24, 60 frames (same run) | 26.8 fps | 89.0 fps |

No intrinsics: the hot loops are written to auto-vectorise. M4.8
profiling (`cargo xtask bench`, macOS `sample`) found the inverse transform at over half of the
decoder's CPU time: it evaluated the full matrix product with strided table reads. It now sums
contiguous basis rows scaled by the non-zero coefficients only, with the block size a compile-time
constant so the loops vectorise (exact integer sums, so bit-exact; `transform::tests` checks it
against the direct product on random sparse and dense blocks). SAO edge offset has a check-free
loop for samples whose neighbours are inside the CTB. Together: ~60 % less CPU per frame
(1080p 78 → 33 ms, 2160p 320 → 129 ms of CPU per frame through the media stack, alternating A/B), see
[docs/performance.md](../../docs/performance.md).
`cargo run --release -p filmcraft-hevc --example hevcdec -- in.hevc out.yuv` decodes a file
(`HEVC_THREADS=n`, `HEVC_LOOPS=k`, `HEVC_DRAFT=1`).

M4.10 (2026-10-03), CPU cycles per frame counted by the kernel (`/usr/bin/time -l`, all threads
summed; mean of two interleaved base / after rounds at load average 190-320) on the
`cargo xtask bench` decode clips (x265 preset fast, CRF 22, testsrc2 + grain), bit-exact:

| stream | threads | Mcycles / frame before | after | after, draft mode |
|---|---|---|---|---|
| 1080p (120 frames) | 1 | 90.9 | **67.3** (−26 %) | 61.2 |
| 1080p | 14 | 100.2 | **75.1** (−25 %) | 67.3 |
| 2160p (72 frames) | 1 | 362.6 | **262.3** (−28 %) | 232.5 |
| 2160p | 14 | 398.3 | **284.8** (−28 %) | 260.1 |

What changed: motion compensation zeroed two 10 KB windows per prediction block per component
and list (11 % of the time in `memset`) and filtered with dynamic-width scalar sums; it now keeps
reusable scratch windows and runs taps outer / outputs inner over fixed block widths. SAO
looked its offset up per sample in a table (20 % of the time); the edge / band offset is now
selected per lane from the neighbour-sign sum / band index, so the runs vectorise. Uni / bi
output clips with min / max instead of `clamp` (whose bound assert kept it scalar). Runs of
bypass bins (signs, Rice suffixes, SAO / intra fields) decode with one division, and
context-coded bins are branch-free. Single-threaded 4K now splits into CABAC residual parsing
~28 %, reference window copies ~11 %, interpolation ~11 %, deblocking + SAO + row publication
~15 %, inverse transform ~8 %.

## Known gaps

- Range extension tools that no 4:2:0 / 4:2:2 reference stream covers (implicit / explicit RDPCM,
  cross-component prediction, extended precision, persistent Rice adaptation, bypass alignment)
  are rejected with `Error::Unsupported`, as are 4:4:4 / 4:0:0 and tiles together with WPP.
- Memory budget: a picture needing more than 512 MiB (shared frame, reconstruction state and block
  metadata), or more than 8 GiB (1 GiB on 32-bit targets) for every picture that can be alive at
  once, is refused with `Error::Unsupported("frame too large for software decode: N MB")` before
  anything is allocated. 8K 4:2:2 12-bit fits with 16 worker threads.
- Tiles and WPP substreams are decoded sequentially (entry points are parsed but the decoder finds
  substream starts itself); parallelism is at the picture level.
- Error concealment is minimal: missing references are replaced by grey pictures, undecodable
  slices leave grey / partial content.
- No fixture covers the `pic_output_flag = 0` path or decoding-order-only CRA handling after EOS
  beyond the synthetic streams; libx265 never emits PCM, tiles or long-term references (covered by
  the synthetic streams only).
