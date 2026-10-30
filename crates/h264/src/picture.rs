//! Picture buffers and per-macroblock state.

use crate::Plane;
use std::sync::{Arc, OnceLock};

/// Storage type of decoded samples: `u8` for 8-bit pictures, `u16` for higher bit depths. Values
/// are always within `0..1 << bit_depth`.
pub trait Pixel: Copy + Default + PartialEq + std::fmt::Debug + Send + Sync + 'static + crate::deblock::EdgeFilter {
    fn to_i32(self) -> i32;
    /// `v` (a sample value, already clipped to the bit depth) in the storage type.
    fn from_i32(v: i32) -> Self;
    /// `v` clipped to the sample range of bit depth `bd` (Clip1): `0..=255` for `u8`, whatever
    /// `bd`, so the 8-bit loops keep a constant bound.
    fn clip(v: i32, bd: u32) -> Self;
    /// The rows of `f` if they are stored with this type.
    fn rows(f: &Frame) -> Option<&[OnceLock<FrameRow<Self>>]>;
    fn wrap_rows(rows: Box<[OnceLock<FrameRow<Self>>]>) -> Rows;
    /// An output plane of these samples.
    fn plane(v: Vec<Self>) -> Plane;
    /// An empty output buffer with room for `n` samples.
    fn alloc(n: usize) -> Vec<Self>;
}

impl Pixel for u8 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v as u8
    }
    #[inline(always)]
    fn clip(v: i32, _bd: u32) -> Self {
        v.clamp(0, 255) as u8
    }
    #[inline(always)]
    fn rows(f: &Frame) -> Option<&[OnceLock<FrameRow<u8>>]> {
        match &f.rows {
            Rows::U8(r) => Some(r),
            Rows::U16(_) => None,
        }
    }
    fn wrap_rows(rows: Box<[OnceLock<FrameRow<u8>>]>) -> Rows {
        Rows::U8(rows)
    }
    fn plane(v: Vec<u8>) -> Plane {
        Plane::U8(v)
    }
    fn alloc(n: usize) -> Vec<u8> {
        plane_allocator()(n)
    }
}

impl Pixel for u16 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v as u16
    }
    #[inline(always)]
    fn clip(v: i32, bd: u32) -> Self {
        v.clamp(0, (1 << bd) - 1) as u16
    }
    #[inline(always)]
    fn rows(f: &Frame) -> Option<&[OnceLock<FrameRow<u16>>]> {
        match &f.rows {
            Rows::U16(r) => Some(r),
            Rows::U8(_) => None,
        }
    }
    fn wrap_rows(rows: Box<[OnceLock<FrameRow<u16>>]>) -> Rows {
        Rows::U16(rows)
    }
    fn plane(v: Vec<u16>) -> Plane {
        Plane::U16(v)
    }
    fn alloc(n: usize) -> Vec<u16> {
        PLANE_ALLOCATOR_U16.get().copied().unwrap_or(Vec::with_capacity)(n)
    }
}

/// Sample format of a picture: bit depths and chroma subsampling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub bit_depth_y: u32,
    pub bit_depth_c: u32,
    /// chroma_format_idc: 1 (4:2:0) or 2 (4:2:2).
    pub chroma_format_idc: u32,
}

impl Format {
    pub const YUV420_8: Format = Format { bit_depth_y: 8, bit_depth_c: 8, chroma_format_idc: 1 };

    /// Whether pictures of this format are stored as `u8` (both bit depths 8), else as `u16`.
    pub fn stores_u8(&self) -> bool {
        self.bit_depth_y <= 8 && self.bit_depth_c <= 8
    }
    /// Chroma lines per macroblock (MbHeightC): 8 for 4:2:0, 16 for 4:2:2.
    pub fn mb_height_c(&self) -> usize {
        if self.chroma_format_idc == 2 { 16 } else { 8 }
    }
}

/// Planar sample buffers (MB-aligned dimensions, stride == width).
#[derive(Clone)]
pub struct Planes<P> {
    pub y: Vec<P>,
    pub cb: Vec<P>,
    pub cr: Vec<P>,
    pub width: usize,
    pub height: usize,
    pub cwidth: usize,
    pub cheight: usize,
}

/// `vec![v; n]` that returns `None` instead of aborting when the allocation fails.
pub(crate) fn try_vec<T: Clone>(n: usize, v: T) -> Option<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).ok()?;
    out.resize(n, v);
    Some(out)
}

impl<P: Pixel> Planes<P> {
    /// Black luma, mid-grey chroma.
    pub fn new(width: usize, height: usize, fmt: Format) -> Self {
        let (cw, ch) = (width / 2, height * fmt.mb_height_c() / 16);
        let gc = P::from_i32(1 << (fmt.bit_depth_c.clamp(1, 16) - 1));
        Planes { y: vec![P::default(); width * height], cb: vec![gc; cw * ch], cr: vec![gc; cw * ch], width, height, cwidth: cw, cheight: ch }
    }
    /// [`Planes::new`], or `None` when the memory is not available.
    pub fn try_new(width: usize, height: usize, fmt: Format) -> Option<Self> {
        let (cw, ch) = (width / 2, height * fmt.mb_height_c() / 16);
        let gc = P::from_i32(1 << (fmt.bit_depth_c.clamp(1, 16) - 1));
        let (y, cb, cr) = (try_vec(width * height, P::default())?, try_vec(cw * ch, gc)?, try_vec(cw * ch, gc)?);
        Some(Planes { y, cb, cr, width, height, cwidth: cw, cheight: ch })
    }
    /// Mid-grey planes.
    pub fn gray(width: usize, height: usize, fmt: Format) -> Self {
        let mut p = Self::new(width, height, fmt);
        p.y.fill(P::from_i32(1 << (fmt.bit_depth_y.clamp(1, 16) - 1)));
        p
    }
}

/// Macroblock coding category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MbKind {
    #[default]
    None,
    I4x4,
    I8x8,
    I16x16,
    IPcm,
    PSkip,
    BSkip,
    BDirect16x16,
    /// Any other P or B inter macroblock.
    Inter,
}

impl MbKind {
    #[inline]
    pub fn is_intra(self) -> bool {
        matches!(self, MbKind::I4x4 | MbKind::I8x8 | MbKind::I16x16 | MbKind::IPcm)
    }
}

/// Per-macroblock state kept for the whole picture (neighbour derivations, deblocking, co-located data).
#[derive(Clone, Copy)]
pub struct MbState {
    /// Slice number within the picture; `u32::MAX` = not (yet) decoded.
    pub slice_num: u32,
    pub kind: MbKind,
    pub transform_8x8: bool,
    /// coded_block_pattern: bits 0..3 luma, bits 4..5 chroma.
    pub cbp: u8,
    pub qp: i8,
    /// QPc for Cb / Cr (used by deblocking).
    pub qpc: [i8; 2],
    pub intra_chroma_mode: u8,
    /// Intra4x4PredMode / Intra8x8PredMode per 4x4 block (raster order).
    pub intra_modes: [u8; 16],
    /// CAVLC: TotalCoeff per luma 4x4 block (raster). CABAC: coded_block_flag.
    pub nnz: [u8; 16],
    /// Same for chroma AC blocks [Cb, Cr].
    /// Raster 2 x (MbHeightC / 4) chroma AC blocks: index row * 2 + column.
    pub nnz_c: [[u8; 8]; 2],
    /// CABAC coded_block_flag of DC blocks: bit0 luma (I16x16), bit1 Cb, bit2 Cr.
    pub cbf_dc: u8,
    /// Luma 4x4 blocks (raster bit) with non-zero coefficients, for deblocking bS = 2.
    pub nz_mask: u16,
    pub ref_idx: [[i8; 4]; 2],
    /// Motion vectors per 4x4 block (raster order).
    pub mv: [[[i16; 2]; 16]; 2],
    /// |mvd| per 4x4 block (raster), saturated, for CABAC context selection.
    pub mvd: [[[u8; 2]; 16]; 2],
    /// Bit per 8x8 block predicted in direct mode.
    pub direct8x8: u8,
}

impl MbState {
    #[inline]
    pub fn kind_is_skip(&self) -> bool {
        matches!(self.kind, MbKind::PSkip | MbKind::BSkip)
    }
}

impl Default for MbState {
    fn default() -> Self {
        MbState {
            slice_num: u32::MAX,
            kind: MbKind::None,
            transform_8x8: false,
            cbp: 0,
            qp: 0,
            qpc: [0; 2],
            intra_chroma_mode: 0,
            intra_modes: [2; 16],
            nnz: [0; 16],
            nnz_c: [[0; 8]; 2],
            cbf_dc: 0,
            nz_mask: 0,
            ref_idx: [[-1; 4]; 2],
            mv: [[[0; 2]; 16]; 2],
            mvd: [[[0; 2]; 16]; 2],
            direct8x8: 0,
        }
    }
}

/// Motion data of one published macroblock row, needed as co-located information for direct
/// prediction.
pub struct RowMotion {
    /// Per 4x4 block: index mb_x * 16 + raster.
    pub mv: [Box<[[i16; 2]]>; 2],
    /// Per 8x8 block: index mb_x * 4 + b8.
    pub ref_idx: [Box<[i8]>; 2],
    /// Unique id of the referenced picture per 8x8 block (u32::MAX = none).
    pub ref_id: [Box<[u32]>; 2],
    pub intra: Box<[bool]>,
}

/// One published macroblock row of a decoded frame: final (deblocked) samples plus its motion.
pub struct FrameRow<P> {
    /// 16 luma lines.
    pub y: Box<[P]>,
    /// MbHeightC lines per chroma component.
    pub cb: Box<[P]>,
    pub cr: Box<[P]>,
    pub motion: RowMotion,
}

/// The macroblock rows of a frame, in its storage type.
pub enum Rows {
    U8(Box<[OnceLock<FrameRow<u8>>]>),
    U16(Box<[OnceLock<FrameRow<u16>>]>),
}

/// A decoded (or in-progress) frame as stored in the DPB and referenced by later pictures.
///
/// Rows are published in order by the thread decoding the picture; readers block (per row) until the
/// data they need is available, which lets several pictures decode concurrently.
pub struct Frame {
    pub id: u32,
    pub poc: i32,
    pub width: usize,
    pub height: usize,
    pub cwidth: usize,
    pub cheight: usize,
    pub mb_w: usize,
    pub fmt: Format,
    rows: Rows,
}

/// The value of a published row (waiting for it with the `threads` feature).
#[inline(always)]
fn wait<T>(cell: &OnceLock<T>) -> Option<&T> {
    #[cfg(feature = "threads")]
    {
        Some(cell.wait())
    }
    #[cfg(not(feature = "threads"))]
    {
        cell.get()
    }
}

impl Frame {
    pub fn new(id: u32, poc: i32, mb_w: usize, mb_h: usize, fmt: Format) -> Self {
        fn cells<P: Pixel>(n: usize) -> Rows {
            P::wrap_rows((0..n).map(|_| OnceLock::new()).collect())
        }
        let rows = if fmt.stores_u8() { cells::<u8>(mb_h) } else { cells::<u16>(mb_h) };
        Frame { id, poc, width: mb_w * 16, height: mb_h * 16, cwidth: mb_w * 8, cheight: mb_h * fmt.mb_height_c(), mb_w, fmt, rows }
    }

    pub fn mb_h(&self) -> usize {
        match &self.rows {
            Rows::U8(r) => r.len(),
            Rows::U16(r) => r.len(),
        }
    }

    /// Chroma lines per macroblock row.
    #[inline]
    pub fn mb_height_c(&self) -> usize {
        self.fmt.mb_height_c()
    }

    /// Publish MB row `r` (ignored if already published). A row of the other sample type (never
    /// built by the decoder) is replaced by a grey one, so that no reader waits for it forever.
    pub fn publish<P: Pixel>(&self, r: usize, row: FrameRow<P>) {
        match P::rows(self) {
            Some(rows) => {
                if let Some(cell) = rows.get(r) {
                    let _ = cell.set(row);
                }
            }
            None => {
                debug_assert!(false, "row of the wrong sample type");
                self.publish_gray();
            }
        }
    }

    pub fn is_published(&self, r: usize) -> bool {
        match &self.rows {
            Rows::U8(rows) => rows.get(r).is_some_and(|c| c.get().is_some()),
            Rows::U16(rows) => rows.get(r).is_some_and(|c| c.get().is_some()),
        }
    }

    /// MB row `r` in sample type `P`, waiting until it has been published (None: no such row, or
    /// the frame stores the other sample type).
    #[inline]
    pub fn row<P: Pixel>(&self, r: usize) -> Option<&FrameRow<P>> {
        wait(P::rows(self)?.get(r)?)
    }

    /// The motion of MB row `r`, waiting until it has been published.
    #[inline]
    pub fn motion(&self, r: usize) -> Option<&RowMotion> {
        match &self.rows {
            Rows::U8(rows) => wait(rows.get(r)?).map(|row| &row.motion),
            Rows::U16(rows) => wait(rows.get(r)?).map(|row| &row.motion),
        }
    }

    /// Wait until the whole frame is available.
    pub fn wait_complete(&self) {
        for r in 0..self.mb_h() {
            let _ = self.motion(r);
        }
    }

    /// Build a published row from a contiguous plane set (rows of MB row `r`) and motion data.
    pub fn make_row<P: Pixel>(planes: &Planes<P>, r: usize, mbs: &[MbState], ref_ids: &dyn Fn(&MbState, usize, i8) -> u32) -> FrameRow<P> {
        let w = planes.width;
        let cw = planes.cwidth;
        let mb_w = w / 16;
        let mb_h = (planes.height / 16).max(1);
        let ch = planes.cheight / mb_h;
        let lines = |p: &[P], n: usize, s: usize| -> Box<[P]> { p.get(r * n * s..(r + 1) * n * s).map(Box::from).unwrap_or_default() };
        let y = lines(&planes.y, 16, w);
        let cb = lines(&planes.cb, ch, cw);
        let cr = lines(&planes.cr, ch, cw);
        let row_mbs = mbs.get(r * mb_w..(r + 1) * mb_w).unwrap_or_default();
        let mut mv = [vec![[0i16; 2]; mb_w * 16], vec![[0i16; 2]; mb_w * 16]];
        let mut ref_idx = [vec![-1i8; mb_w * 4], vec![-1i8; mb_w * 4]];
        let mut ref_id = [vec![u32::MAX; mb_w * 4], vec![u32::MAX; mb_w * 4]];
        let mut intra = vec![true; mb_w];
        for (x, st) in row_mbs.iter().enumerate() {
            if st.slice_num == u32::MAX {
                continue;
            }
            intra[x] = st.kind.is_intra();
            for l in 0..2 {
                mv[l][x * 16..x * 16 + 16].copy_from_slice(&st.mv[l]);
                for b in 0..4 {
                    let ri = st.ref_idx[l][b];
                    ref_idx[l][x * 4 + b] = ri;
                    if ri >= 0 {
                        ref_id[l][x * 4 + b] = ref_ids(st, l, ri);
                    }
                }
            }
        }
        let [mv0, mv1] = mv;
        let [ri0, ri1] = ref_idx;
        let [id0, id1] = ref_id;
        FrameRow {
            y,
            cb,
            cr,
            motion: RowMotion { mv: [mv0.into(), mv1.into()], ref_idx: [ri0.into(), ri1.into()], ref_id: [id0.into(), id1.into()], intra: intra.into() },
        }
    }

    /// A frame with every row published from `planes` and no motion (intra), e.g. "non-existing"
    /// frames inferred for frame_num gaps.
    pub fn from_planes<P: Pixel>(id: u32, poc: i32, planes: &Planes<P>, fmt: Format) -> Self {
        let (mb_w, mb_h) = (planes.width / 16, planes.height / 16);
        let f = Frame::new(id, poc, mb_w, mb_h, fmt);
        let blank = vec![MbState::default(); mb_w * mb_h];
        for r in 0..mb_h {
            f.publish(r, Frame::make_row(planes, r, &blank, &|_, _, _| u32::MAX));
        }
        f
    }

    /// A copy of the samples of this frame (waits for completion) with id `id`, POC `poc` and no
    /// motion: the concealment picture for a frame_num gap.
    pub fn copy_samples(&self, id: u32, poc: i32) -> Frame {
        fn copy<P: Pixel>(f: &Frame, id: u32, poc: i32) -> Frame {
            let mut p = Planes::<P>::gray(f.width, f.height, f.fmt);
            let ch = f.mb_height_c();
            for r in 0..f.mb_h() {
                let Some(row) = f.row::<P>(r) else { continue };
                let put = |dst: &mut [P], src: &[P], o: usize| {
                    if let Some(d) = dst.get_mut(o..o + src.len()) {
                        d.copy_from_slice(src);
                    }
                };
                put(&mut p.y, &row.y, r * 16 * f.width);
                put(&mut p.cb, &row.cb, r * ch * f.cwidth);
                put(&mut p.cr, &row.cr, r * ch * f.cwidth);
            }
            Frame::from_planes(id, poc, &p, f.fmt)
        }
        match &self.rows {
            Rows::U8(_) => copy::<u8>(self, id, poc),
            Rows::U16(_) => copy::<u16>(self, id, poc),
        }
    }

    /// Publish every row not published yet as mid-grey without motion (a picture whose decoding
    /// failed: readers never block on it).
    pub fn publish_gray(&self) {
        fn fill<P: Pixel>(f: &Frame) {
            let planes = Planes::<P>::gray(f.width, f.height, f.fmt);
            let blank = vec![MbState::default(); f.mb_w * f.mb_h()];
            for r in 0..f.mb_h() {
                if !f.is_published(r) {
                    f.publish(r, Frame::make_row(&planes, r, &blank, &|_, _, _| u32::MAX));
                }
            }
        }
        match &self.rows {
            Rows::U8(_) => fill::<u8>(self),
            Rows::U16(_) => fill::<u16>(self),
        }
    }

    /// A mid-grey frame of `mb_w` x `mb_h` macroblocks.
    pub fn gray(id: u32, poc: i32, mb_w: usize, mb_h: usize, fmt: Format) -> Frame {
        let f = Frame::new(id, poc, mb_w, mb_h, fmt);
        f.publish_gray();
        f
    }

    /// Copy the w x h luma window at (x0, y0) (clamped to the picture) into `out` (stride `os`).
    #[inline]
    pub fn luma_window<P: Pixel>(&self, x0: i32, y0: i32, w: usize, h: usize, out: &mut [P], os: usize) {
        let (width, height) = (self.width, self.height);
        for r in 0..h {
            let yy = (y0 + r as i32).clamp(0, height as i32 - 1) as usize;
            let Some(row) = self.row::<P>(yy >> 4) else { return };
            let Some(line) = row.y.get((yy & 15) * width..(yy & 15) * width + width) else { return };
            let Some(out) = out.get_mut(r * os..) else { return };
            copy_line::<P, 24>(line, x0, out, w);
        }
    }

    /// Copy the w x h window of chroma component `c` at (x0, y0) (clamped) into `out`.
    #[inline]
    pub fn chroma_window<P: Pixel>(&self, c: usize, x0: i32, y0: i32, w: usize, h: usize, out: &mut [P], os: usize) {
        let (cw, ch) = (self.cwidth, self.cheight);
        let (sh, mask) = (self.mb_height_c().trailing_zeros(), self.mb_height_c() - 1);
        for r in 0..h {
            let yy = (y0 + r as i32).clamp(0, ch as i32 - 1) as usize;
            let Some(row) = self.row::<P>(yy >> sh) else { return };
            let plane = if c == 0 { &row.cb } else { &row.cr };
            let Some(line) = plane.get((yy & mask) * cw..(yy & mask) * cw + cw) else { return };
            let Some(out) = out.get_mut(r * os..) else { return };
            copy_line::<P, 16>(line, x0, out, w);
        }
    }

    /// Crop the frame into planar output buffers (waits for completion): `u8` planes for 8-bit
    /// frames, `u16` ones above.
    pub fn cropped_planes(&self, crop: (usize, usize, usize, usize)) -> (Plane, Plane, Plane) {
        fn planes<P: Pixel>(f: &Frame, crop: (usize, usize, usize, usize)) -> (Plane, Plane, Plane) {
            let (y, u, v) = f.copy_cropped::<P>(crop);
            (P::plane(y), P::plane(u), P::plane(v))
        }
        match &self.rows {
            Rows::U8(_) => planes::<u8>(self, crop),
            Rows::U16(_) => planes::<u16>(self, crop),
        }
    }

    /// Crop and copy the frame into planar buffers (waits for completion).
    fn copy_cropped<P: Pixel>(&self, crop: (usize, usize, usize, usize)) -> (Vec<P>, Vec<P>, Vec<P>) {
        let (cx, cy, cw, ch) = crop;
        let mut y = P::alloc(cw * ch);
        for r in cy..cy + ch {
            let Some(row) = self.row::<P>(r >> 4) else { break };
            if let Some(s) = row.y.get((r & 15) * self.width + cx..(r & 15) * self.width + cx + cw) {
                y.extend_from_slice(s);
            }
        }
        let mbh = self.mb_height_c();
        let sy = if mbh == 16 { 1 } else { 2 };
        let (ccx, ccy, ccw, cch) = (cx / 2, cy / sy, cw.div_ceil(2), ch.div_ceil(sy));
        let mut u = P::alloc(ccw * cch);
        let mut v = P::alloc(ccw * cch);
        for r in ccy..ccy + cch {
            let Some(row) = self.row::<P>(r / mbh) else { break };
            let o = (r % mbh) * self.cwidth + ccx;
            if let (Some(a), Some(b)) = (row.cb.get(o..o + ccw), row.cr.get(o..o + ccw)) {
                u.extend_from_slice(a);
                v.extend_from_slice(b);
            }
        }
        (y, u, v)
    }
}

static PLANE_ALLOCATOR: std::sync::OnceLock<fn(usize) -> Vec<u8>> = std::sync::OnceLock::new();

/// Where 8-bit output planes come from: an empty buffer with room for the given number of
/// samples. The host sets it once to recycle the planes of pictures it is done with; the first
/// call wins.
pub fn set_plane_allocator(alloc: fn(usize) -> Vec<u8>) {
    let _ = PLANE_ALLOCATOR.set(alloc);
}

fn plane_allocator() -> fn(usize) -> Vec<u8> {
    PLANE_ALLOCATOR.get().copied().unwrap_or(Vec::with_capacity)
}

static PLANE_ALLOCATOR_U16: std::sync::OnceLock<fn(usize) -> Vec<u16>> = std::sync::OnceLock::new();

/// [`set_plane_allocator`] for the 16-bit planes of pictures deeper than 8 bits.
pub fn set_plane_allocator_u16(alloc: fn(usize) -> Vec<u16>) {
    let _ = PLANE_ALLOCATOR_U16.set(alloc);
}

/// Copy `w` samples of `line` starting at x0 into `out`; when possible a fixed-size block of `N`
/// samples is copied instead (cheaper than a variable-length copy; `out` must then have room for `N`).
#[inline(always)]
fn copy_line<P: Pixel, const N: usize>(line: &[P], x0: i32, out: &mut [P], w: usize) {
    if w <= N
        && x0 >= 0
        && let Some(src) = line.get(x0 as usize..).and_then(|l| l.first_chunk::<N>())
        && let Some(dst) = out.first_chunk_mut::<N>()
    {
        *dst = *src;
    } else if let Some(out) = out.get_mut(..w) {
        copy_clamped(line, x0, out);
    }
}

/// Copy `out.len()` samples of `line` starting at x0, replicating edge samples outside the line.
#[inline(always)]
fn copy_clamped<P: Pixel>(line: &[P], x0: i32, out: &mut [P]) {
    let w = out.len();
    if x0 >= 0 && x0 as usize + w <= line.len() {
        out.copy_from_slice(&line[x0 as usize..x0 as usize + w]);
    } else {
        let max = line.len() as i32 - 1;
        for (c, o) in out.iter_mut().enumerate() {
            *o = line.get((x0 + c as i32).clamp(0, max.max(0)) as usize).copied().unwrap_or_default();
        }
    }
}

pub type FrameRef = Arc<Frame>;

/// An entry of RefPicList0/1 for the current slice.
#[derive(Clone)]
pub struct RefPic {
    pub frame: FrameRef,
    pub long_term: bool,
}

impl RefPic {
    pub fn poc(&self) -> i32 {
        self.frame.poc
    }
    pub fn id(&self) -> u32 {
        self.frame.id
    }
}
