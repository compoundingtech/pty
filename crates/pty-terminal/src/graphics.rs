//! Kitty graphics state: the durable, typed image state behind
//! [`crate::actor::TerminalActor`] (and the `pty` crate's `TerminalHandle`,
//! which reads it from an actor thread).
//!
//! libghostty owns the image storage; this module is the boundary that turns
//! its borrowed handles into owned, `Send` values a compositor can hold across
//! frames, and that puts the storage back on the wire for the ATTACH/PEEK
//! replay (see [`replay`]).
//!
//! What a consumer gets:
//!
//! - [`GraphicsState`]: the storage generation, every [`ImageDesc`] that has a
//!   placement, and every [`Placement`] with its resolved source crop, cell
//!   extent, and position in the window it was read for.
//! - [`ImageBytes`]: the pixels themselves, copied once, on request. Metadata
//!   is cheap enough to read per frame; bytes are not, so they are keyed by
//!   [`ImageDesc::generation`] and fetched only when that changes.
//!
//! Positions are typed rather than optional numbers ([`PlacementPosition`]):
//! libghostty resolves a cursor-positioned placement to a viewport cell, but a
//! virtual (Unicode placeholder) placement has no position of its own — it is
//! wherever its placeholder cells are. Those cells are ordinary text, each
//! naming its own image row and column, so this module decodes them from the
//! grid; that is what survives scrolling, reflow, and a windowed read.
//!
//! Kitty only: SIXEL and the iTerm2 protocol have no equivalent
//! arbitrary-pane contract and are not read here.

use libghostty_vt::alloc::{Allocator, Bytes};
use libghostty_vt::kitty::graphics as gfx;
use libghostty_vt::style::StyleColor;
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace, Terminal};

/// The Unicode placeholder character (U+10EEEE) a virtual placement is drawn
/// with.
pub const PLACEHOLDER: char = '\u{10eeee}';

/// Cell pixel metrics. Kitty graphics geometry is defined in pixels, so a
/// terminal that stores images has to know how big a cell is: a placement
/// that did not say `c=`/`r=` gets its cell extent from the image's pixel
/// size divided by this.
///
/// The metrics belong to whoever draws the cells — a font, on a host the
/// session daemon may never see — so they travel from the client (the `pty`
/// crate's `AttachOptions::graphics`, carried on ATTACH and RESIZE) rather
/// than being assumed. Zero means undeclared, and derived
/// geometry uses [`CellSize::FALLBACK`] until someone says otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellSize {
    /// Cell width in pixels; zero when undeclared.
    pub width: u32,
    /// Cell height in pixels; zero when undeclared.
    pub height: u32,
}

impl CellSize {
    /// The conventional 8x16 monospace cell, used only when nobody has
    /// declared the real one. Deterministic on purpose: two clients reading
    /// an undeclared terminal must agree, even though both are guessing.
    pub const FALLBACK: CellSize = CellSize {
        width: 8,
        height: 16,
    };

    /// Whether a real cell size was declared.
    pub fn is_declared(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// This size, or [`CellSize::FALLBACK`] when undeclared.
    pub fn or_fallback(self) -> CellSize {
        if self.is_declared() {
            self
        } else {
            CellSize::FALLBACK
        }
    }
}

/// The largest image storage this module supports, and therefore the largest
/// a replay can have to carry: 32 MiB, four 2048x2048 RGBA images.
///
/// One number bounds both on purpose. A storage limit above what a replay
/// carries would mean a terminal that accepts an image a late client can
/// never receive — a supported state that silently loses data, which is the
/// failure this module exists to prevent.
/// [`crate::actor::TerminalActor::enable_graphics`] clamps to it, so a caller
/// asking for more gets this and can see that it did
/// ([`crate::actor::TerminalActor::graphics_options`]).
pub const MAX_STORAGE_BYTES: u64 = 32 * 1024 * 1024;

/// How much graphics state a terminal may hold, and how it measures cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphicsOptions {
    /// Image storage limit in bytes, clamped to [`MAX_STORAGE_BYTES`]. Zero
    /// disables the protocol; this is the only bound on how much a child can
    /// make the terminal hold, and it is also the bound on a replay.
    pub storage_bytes: u64,
    /// Cell metrics used for every pixel/cell conversion. Zero means
    /// undeclared: nobody has told the terminal how big a cell is, and
    /// derived geometry falls back to [`CellSize::FALLBACK`].
    pub cell: CellSize,
    /// Cap on the bytes one APC command may buffer (`None` keeps
    /// libghostty's default).
    pub apc_max_bytes: Option<usize>,
}

impl GraphicsOptions {
    /// The full supported storage, a conventional 8x16 cell, libghostty's
    /// APC cap.
    pub const DEFAULT: GraphicsOptions = GraphicsOptions {
        storage_bytes: MAX_STORAGE_BYTES,
        cell: CellSize {
            width: 8,
            height: 16,
        },
        apc_max_bytes: None,
    };
}

impl Default for GraphicsOptions {
    fn default() -> Self {
        GraphicsOptions::DEFAULT
    }
}

/// Pixel format of stored image data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 24-bit RGB (kitty `f=24`).
    Rgb,
    /// 32-bit RGBA (kitty `f=32`).
    Rgba,
    /// PNG bytes (kitty `f=100`), still encoded.
    Png,
    /// 8-bit grayscale.
    Gray,
    /// 8-bit grayscale + alpha.
    GrayAlpha,
}

impl PixelFormat {
    /// The kitty `f=` value, or `None` for a format the protocol cannot
    /// express (grayscale).
    pub fn kitty_format(self) -> Option<u32> {
        match self {
            PixelFormat::Rgb => Some(24),
            PixelFormat::Rgba => Some(32),
            PixelFormat::Png => Some(100),
            PixelFormat::Gray | PixelFormat::GrayAlpha => None,
        }
    }

    /// Whether the data is raw pixels (and therefore needs `s=`/`v=` when
    /// transmitted).
    pub fn is_raw(self) -> bool {
        !matches!(self, PixelFormat::Png)
    }
}

/// Compression applied to stored image data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Stored uncompressed.
    None,
    /// zlib deflate (kitty `o=z`).
    ZlibDeflate,
}

/// One stored image, without its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDesc {
    /// Kitty image id (`i=`).
    pub id: u32,
    /// Kitty image number (`I=`), zero when the child used none.
    pub number: u32,
    /// The stamp libghostty assigned when these pixels entered the storage.
    /// A cache keyed on `(id, generation)` is never stale: the same id with
    /// new pixels gets a new stamp.
    pub generation: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub format: PixelFormat,
    /// Compression.
    pub compression: Compression,
    /// Length of the stored data in bytes.
    pub len: usize,
}

/// A stored image with its pixels copied out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBytes {
    /// What the pixels describe.
    pub desc: ImageDesc,
    /// The bytes, exactly as libghostty stores them (`desc.format`).
    pub data: Vec<u8>,
}

/// The part of the image a placement shows, in image pixels, already resolved
/// (kitty's "0 means the whole dimension") and clamped to the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRect {
    /// Left edge in image pixels.
    pub x: u32,
    /// Top edge in image pixels.
    pub y: u32,
    /// Width in image pixels.
    pub width: u32,
    /// Height in image pixels.
    pub height: u32,
}

/// Where a virtual placement's placeholder cells are in the window that was
/// read.
///
/// A placeholder cell names its own image row and column, so a window that
/// shows only part of an image still says exactly which part: `cell_row` /
/// `cell_col` are the image cell indices of the top-left visible cell, and
/// `origin_row` / `origin_col` are where the image's own cell (0, 0) would be
/// — negative when it has scrolled above the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderRect {
    /// Window row of the first placeholder row.
    pub row: u16,
    /// Window column of the first placeholder column.
    pub col: u16,
    /// Placeholder rows present in the window.
    pub rows: u16,
    /// Placeholder columns present in the window.
    pub cols: u16,
    /// Image cell row of the cell at (`row`, `col`).
    pub cell_row: u16,
    /// Image cell column of the cell at (`row`, `col`).
    pub cell_col: u16,
    /// Window row of the placement's own row 0; negative when scrolled above.
    pub origin_row: i32,
    /// Window column of the placement's own column 0.
    pub origin_col: i32,
}

/// Where a placement is, in the window it was read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementPosition {
    /// Stored, but nothing of it is in this window: scrolled out, or a
    /// virtual placement whose placeholder cells are elsewhere (or gone).
    Offscreen,
    /// A cursor-positioned placement (`a=p` without `U=1`). `row` is negative
    /// when the top of the image has scrolled above the window.
    Direct {
        /// Window column of the top-left corner.
        col: i32,
        /// Window row of the top-left corner.
        row: i32,
        /// Cells wide.
        cols: u32,
        /// Cells tall.
        rows: u32,
    },
    /// A virtual placement (`U=1`), located by its placeholder cells.
    Placeholder(PlaceholderRect),
}

/// One placement: an image, where it is, and which part of it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// The image it draws.
    pub image_id: u32,
    /// Kitty placement id (`p=`), zero when the child used none. With the
    /// image id this is the placement's identity.
    pub placement_id: u32,
    /// Whether it is a virtual (Unicode placeholder) placement.
    pub is_virtual: bool,
    /// Kitty `z=`.
    pub z: i32,
    /// The image generation this placement was resolved against.
    pub image_generation: u64,
    /// The source crop, resolved and clamped.
    pub source: SourceRect,
    /// Pixel offset inside the first cell (`X=`, `Y=`).
    pub cell_offset: (u32, u32),
    /// Rendered size in pixels, after crop and aspect ratio.
    pub pixel_size: (u32, u32),
    /// Rendered size in cells.
    pub cell_size: (u32, u32),
    /// The raw `c=` and `r=` the child asked for; zero means "natural size".
    pub requested_cells: (u32, u32),
    /// Where it is in this window.
    pub position: PlacementPosition,
}

/// Everything a compositor needs for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsState {
    /// Whether the protocol is on (a non-zero storage limit).
    pub enabled: bool,
    /// The storage generation. Unchanged means the images and the set of
    /// placements are byte-for-byte what they were; geometry may still have
    /// moved (scroll, resize), so a dirty frame still re-reads positions.
    pub generation: u64,
    /// The storage limit in bytes.
    pub storage_bytes: u64,
    /// The cell metrics the geometry was computed with, always a usable
    /// size.
    pub cell: CellSize,
    /// Whether `cell` is what a client declared, or
    /// [`CellSize::FALLBACK`] because nobody has. A consumer that draws
    /// pixels should declare its own
    /// ([`crate::actor::TerminalActor::set_cell_size`], or the `pty`
    /// crate's `TerminalHandle::set_cell_size`) rather than trust a
    /// fallback: `c=`/`r=` placements are exact either way, but a
    /// placement that left its size implicit is only as right as this.
    pub cell_declared: bool,
    /// Every image that has at least one placement, by id.
    pub images: Vec<ImageDesc>,
    /// Every placement, in libghostty's iteration order.
    pub placements: Vec<Placement>,
}

impl Default for GraphicsState {
    fn default() -> Self {
        GraphicsState {
            enabled: false,
            generation: 0,
            storage_bytes: 0,
            cell: CellSize::FALLBACK,
            cell_declared: false,
            images: Vec::new(),
            placements: Vec::new(),
        }
    }
}

impl GraphicsState {
    /// The description of `id`, if it is in this state.
    pub fn image(&self, id: u32) -> Option<&ImageDesc> {
        self.images.iter().find(|i| i.id == id)
    }

    /// The placements of `id`.
    pub fn placements_of(&self, id: u32) -> impl Iterator<Item = &Placement> {
        self.placements.iter().filter(move |p| p.image_id == id)
    }

    /// Placements with anything in the window, in draw order (lowest `z`
    /// first, ties in storage order).
    pub fn visible(&self) -> Vec<&Placement> {
        let mut v: Vec<&Placement> = self
            .placements
            .iter()
            .filter(|p| p.position != PlacementPosition::Offscreen)
            .collect();
        v.sort_by_key(|p| p.z);
        v
    }
}

// ---------------------------------------------------------------------------
// Reading the storage
// ---------------------------------------------------------------------------

/// Whether the protocol is enabled on `term`.
pub fn enabled(term: &Terminal) -> bool {
    term.kitty_image_storage_limit().unwrap_or(0) > 0
}

/// The storage generation, or 0 when the storage is empty or disabled.
///
/// Cheap: one read, no iteration. A caller that sees an unchanged generation
/// can skip [`read`]'s image work, but not its geometry work.
pub fn generation(term: &Terminal) -> u64 {
    term.kitty_graphics()
        .and_then(|g| g.generation())
        .unwrap_or(0)
}

/// Read the whole state for the window starting `scroll_offset` rows above
/// the live viewport (0 = the live viewport), the same window
/// [`crate::snapshot::snapshot`] reads.
///
/// Positions are relative to that window, so a grid and a state read with the
/// same offset line up cell for cell.
pub fn read(term: &Terminal, cell: CellSize, scroll_offset: usize) -> GraphicsState {
    let storage_bytes = term.kitty_image_storage_limit().unwrap_or(0);
    let mut state = GraphicsState {
        enabled: storage_bytes > 0,
        generation: 0,
        storage_bytes,
        cell: cell.or_fallback(),
        cell_declared: cell.is_declared(),
        images: Vec::new(),
        placements: Vec::new(),
    };
    if !state.enabled {
        return state;
    }
    let Ok(graphics) = term.kitty_graphics() else {
        return state;
    };
    state.generation = graphics.generation().unwrap_or(0);
    let Ok(mut iter) = gfx::PlacementIterator::new() else {
        return state;
    };
    let Ok(mut placements) = iter.update(&graphics) else {
        return state;
    };

    // The buffer row this window starts at, the same one
    // `crate::snapshot::snapshot` uses. Direct placements resolve in screen
    // space, so this is what turns them into window rows.
    let window_start = term
        .scrollback_rows()
        .unwrap_or(0)
        .saturating_sub(scroll_offset) as i64;

    // The placeholder scan is one pass over the window and only happens once
    // a virtual placement is actually there.
    let mut placeholders: Option<Vec<(u32, u32, PlaceholderRect)>> = None;

    while let Some(p) = placements.next() {
        let Ok(image_id) = p.image_id() else { continue };
        let Some(image) = graphics.image(image_id) else {
            continue;
        };
        let Some(desc) = describe(image_id, &image) else {
            continue;
        };
        let is_virtual = p.is_virtual().unwrap_or(false);
        let placement_id = p.placement_id().unwrap_or(0);
        let source = p
            .source_rect(&image)
            .map(|r| SourceRect {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            })
            .unwrap_or(SourceRect {
                x: 0,
                y: 0,
                width: desc.width,
                height: desc.height,
            });
        let pixel_size = p
            .pixel_size(&image, term)
            .map(|s| (s.width, s.height))
            .unwrap_or((0, 0));
        let cell_size = p
            .grid_size(&image, term)
            .map(|s| (s.cols, s.rows))
            .unwrap_or((0, 0));
        let position = if is_virtual {
            let found = placeholders.get_or_insert_with(|| scan_placeholders(term, scroll_offset));
            // A placeholder cell names a placement by its underline colour,
            // and a cell that carries none names the image's default
            // placement. So an exact match wins, and a placement falls back
            // to the cells that named no placement at all rather than
            // reporting itself absent.
            found
                .iter()
                .find(|(i, pl, _)| *i == image_id && *pl == placement_id)
                .or_else(|| found.iter().find(|(i, pl, _)| *i == image_id && *pl == 0))
                .map(|(_, _, r)| PlacementPosition::Placeholder(*r))
                .unwrap_or(PlacementPosition::Offscreen)
        } else {
            // Not `viewport_pos`: that answers only for the live viewport and
            // reports nothing for a placement above it, which would make a
            // scrolled-back window lose exactly the images it should show.
            // The placement's own rectangle resolves in screen space, so it
            // answers for any window — including a negative row for a
            // placement whose top has scrolled above this one.
            match screen_origin(&p, &image, term) {
                Some((col, screen_row)) => PlacementPosition::Direct {
                    col,
                    row: (screen_row - window_start) as i32,
                    cols: cell_size.0,
                    rows: cell_size.1,
                },
                None => PlacementPosition::Offscreen,
            }
        };
        if !state.images.iter().any(|i| i.id == desc.id) {
            state.images.push(desc);
        }
        state.placements.push(Placement {
            image_id,
            placement_id,
            is_virtual,
            z: p.z().unwrap_or(0),
            image_generation: desc.generation,
            source,
            cell_offset: (p.x_offset().unwrap_or(0), p.y_offset().unwrap_or(0)),
            pixel_size,
            cell_size,
            requested_cells: (p.columns().unwrap_or(0), p.rows().unwrap_or(0)),
            position,
        });
    }
    state
}

/// The top-left corner of a cursor-positioned placement, as a column and an
/// absolute buffer (screen-space) row.
///
/// The placement's own rectangle is the only answer that works for a window
/// other than the live viewport: `PlacementIteration::viewport_pos` is
/// defined against the viewport and reports nothing for a placement above
/// it, which is precisely the placement a scrolled-back reader wants.
fn screen_origin(
    p: &gfx::PlacementIteration<'_, '_>,
    image: &gfx::Image<'_>,
    term: &Terminal,
) -> Option<(i32, i64)> {
    let rect = p.rect(image, term).ok()?;
    let point = term
        .point_from_grid_ref(&rect.start(), PointSpace::Screen)
        .ok()??;
    Some((point.x as i32, point.y as i64))
}

/// The pixels of `id`, copied out of the storage. `None` when the image is
/// gone, so a caller that raced a delete learns it here.
pub fn image_bytes(term: &Terminal, id: u32) -> Option<ImageBytes> {
    let graphics = term.kitty_graphics().ok()?;
    let image = graphics.image(id)?;
    let desc = describe(id, &image)?;
    let data = image.data().ok()?;
    Some(ImageBytes {
        desc,
        data: data.to_vec(),
    })
}

fn describe(id: u32, image: &gfx::Image<'_>) -> Option<ImageDesc> {
    let width = image.width().ok()?;
    let height = image.height().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let format = match image.format().ok()? {
        gfx::ImageFormat::Rgb => PixelFormat::Rgb,
        gfx::ImageFormat::Rgba => PixelFormat::Rgba,
        gfx::ImageFormat::Png => PixelFormat::Png,
        gfx::ImageFormat::Gray => PixelFormat::Gray,
        gfx::ImageFormat::GrayAlpha => PixelFormat::GrayAlpha,
        _ => return None,
    };
    let compression = match image.compression().ok()? {
        gfx::Compression::ZlibDeflate => Compression::ZlibDeflate,
        _ => Compression::None,
    };
    Some(ImageDesc {
        id,
        number: image.number().unwrap_or(0),
        generation: image.generation().unwrap_or(0),
        width,
        height,
        format,
        compression,
        len: image.data().map(|d| d.len()).unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------
// Placeholder cells
// ---------------------------------------------------------------------------

/// The row/column diacritics, in index order (kitty's
/// `gen/rowcolumn-diacritics.txt`). Sorted, so a codepoint's index is a
/// binary search.
const ROWCOLUMN_DIACRITICS: [u32; 297] = [
    0x0305, 0x030d, 0x030e, 0x0310, 0x0312, 0x033d, 0x033e, 0x033f, 0x0346, 0x034a, 0x034b,
    0x034c, 0x0350, 0x0351, 0x0352, 0x0357, 0x035b, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367,
    0x0368, 0x0369, 0x036a, 0x036b, 0x036c, 0x036d, 0x036e, 0x036f, 0x0483, 0x0484, 0x0485,
    0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059c, 0x059d,
    0x059e, 0x059f, 0x05a0, 0x05a1, 0x05a8, 0x05a9, 0x05ab, 0x05ac, 0x05af, 0x05c4, 0x0610,
    0x0611, 0x0612, 0x0613, 0x0614, 0x0615, 0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065a,
    0x065b, 0x065d, 0x065e, 0x06d6, 0x06d7, 0x06d8, 0x06d9, 0x06da, 0x06db, 0x06dc, 0x06df,
    0x06e0, 0x06e1, 0x06e2, 0x06e4, 0x06e7, 0x06e8, 0x06eb, 0x06ec, 0x0730, 0x0732, 0x0733,
    0x0735, 0x0736, 0x073a, 0x073d, 0x073f, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747, 0x0749,
    0x074a, 0x07eb, 0x07ec, 0x07ed, 0x07ee, 0x07ef, 0x07f0, 0x07f1, 0x07f3, 0x0816, 0x0817,
    0x0818, 0x0819, 0x081b, 0x081c, 0x081d, 0x081e, 0x081f, 0x0820, 0x0821, 0x0822, 0x0823,
    0x0825, 0x0826, 0x0827, 0x0829, 0x082a, 0x082b, 0x082c, 0x082d, 0x0951, 0x0953, 0x0954,
    0x0f82, 0x0f83, 0x0f86, 0x0f87, 0x135d, 0x135e, 0x135f, 0x17dd, 0x193a, 0x1a17, 0x1a75,
    0x1a76, 0x1a77, 0x1a78, 0x1a79, 0x1a7a, 0x1a7b, 0x1a7c, 0x1b6b, 0x1b6d, 0x1b6e, 0x1b6f,
    0x1b70, 0x1b71, 0x1b72, 0x1b73, 0x1cd0, 0x1cd1, 0x1cd2, 0x1cda, 0x1cdb, 0x1ce0, 0x1dc0,
    0x1dc1, 0x1dc3, 0x1dc4, 0x1dc5, 0x1dc6, 0x1dc7, 0x1dc8, 0x1dc9, 0x1dcb, 0x1dcc, 0x1dd1,
    0x1dd2, 0x1dd3, 0x1dd4, 0x1dd5, 0x1dd6, 0x1dd7, 0x1dd8, 0x1dd9, 0x1dda, 0x1ddb, 0x1ddc,
    0x1ddd, 0x1dde, 0x1ddf, 0x1de0, 0x1de1, 0x1de2, 0x1de3, 0x1de4, 0x1de5, 0x1de6, 0x1dfe,
    0x20d0, 0x20d1, 0x20d4, 0x20d5, 0x20d6, 0x20d7, 0x20db, 0x20dc, 0x20e1, 0x20e7, 0x20e9,
    0x20f0, 0x2cef, 0x2cf0, 0x2cf1, 0x2de0, 0x2de1, 0x2de2, 0x2de3, 0x2de4, 0x2de5, 0x2de6,
    0x2de7, 0x2de8, 0x2de9, 0x2dea, 0x2deb, 0x2dec, 0x2ded, 0x2dee, 0x2def, 0x2df0, 0x2df1,
    0x2df2, 0x2df3, 0x2df4, 0x2df5, 0x2df6, 0x2df7, 0x2df8, 0x2df9, 0x2dfa, 0x2dfb, 0x2dfc,
    0x2dfd, 0x2dfe, 0x2dff, 0xa66f, 0xa67c, 0xa67d, 0xa6f0, 0xa6f1, 0xa8e0, 0xa8e1, 0xa8e2,
    0xa8e3, 0xa8e4, 0xa8e5, 0xa8e6, 0xa8e7, 0xa8e8, 0xa8e9, 0xa8ea, 0xa8eb, 0xa8ec, 0xa8ed,
    0xa8ee, 0xa8ef, 0xa8f0, 0xa8f1, 0xaab0, 0xaab2, 0xaab3, 0xaab7, 0xaab8, 0xaabe, 0xaabf,
    0xaac1, 0xfe20, 0xfe21, 0xfe22, 0xfe23, 0xfe24, 0xfe25, 0xfe26, 0x10a0f, 0x10a38, 0x1d185,
    0x1d186, 0x1d187, 0x1d188, 0x1d189, 0x1d1aa, 0x1d1ab, 0x1d1ac, 0x1d1ad, 0x1d242, 0x1d243,
    0x1d244,
];

/// The index this diacritic encodes, if it is one.
fn diacritic_index(c: char) -> Option<u16> {
    ROWCOLUMN_DIACRITICS
        .binary_search(&(c as u32))
        .ok()
        .map(|i| i as u16)
}

/// One accumulating placeholder region.
struct Accum {
    image_id: u32,
    placement_id: u32,
    row: u16,
    col: u16,
    last_row: u16,
    last_col: u16,
    cell_row: u16,
    cell_col: u16,
}

/// Walk the window once and collect, per (image id, placement id), the
/// placeholder cells that are in it.
///
/// The scan is bounded by the window: rows x cols cell reads, never the whole
/// scrollback. Kitty's inheritance rules live in [`placeholder_cell`].
fn scan_placeholders(term: &Terminal, scroll_offset: usize) -> Vec<(u32, u32, PlaceholderRect)> {
    let rows_n = term.rows().unwrap_or(0);
    let cols = term.cols().unwrap_or(0);
    let base_y = term.scrollback_rows().unwrap_or(0);
    let len = term.total_rows().unwrap_or(rows_n as usize);
    let start = base_y.saturating_sub(scroll_offset);
    let live = start == base_y;

    let mut acc: Vec<Accum> = Vec::new();
    for r in 0..rows_n {
        if start + r as usize >= len {
            break;
        }
        // The cell to the left, when it was a placeholder: what an omitted
        // diacritic inherits from.
        let mut prev: Option<PlaceholderCell> = None;
        for x in 0..cols {
            let point = if live {
                Point::Active(PointCoordinate { x, y: r as u32 })
            } else {
                Point::Screen(PointCoordinate {
                    x,
                    y: (start + r as usize) as u32,
                })
            };
            let Some(cell) = placeholder_cell(term, point, prev) else {
                prev = None;
                continue;
            };
            prev = Some(cell);
            match acc
                .iter_mut()
                .find(|a| a.image_id == cell.image_id && a.placement_id == cell.placement_id)
            {
                Some(a) => {
                    a.last_row = a.last_row.max(r);
                    a.last_col = a.last_col.max(x);
                    a.row = a.row.min(r);
                    if x < a.col {
                        a.col = x;
                        a.cell_col = cell.cell_col;
                    }
                    if r < a.row || (r == a.row && x == a.col) {
                        a.cell_row = cell.cell_row;
                    }
                }
                None => acc.push(Accum {
                    image_id: cell.image_id,
                    placement_id: cell.placement_id,
                    row: r,
                    col: x,
                    last_row: r,
                    last_col: x,
                    cell_row: cell.cell_row,
                    cell_col: cell.cell_col,
                }),
            }
        }
    }
    acc.into_iter()
        .map(|a| {
            (
                a.image_id,
                a.placement_id,
                PlaceholderRect {
                    row: a.row,
                    col: a.col,
                    rows: a.last_row - a.row + 1,
                    cols: a.last_col - a.col + 1,
                    cell_row: a.cell_row,
                    cell_col: a.cell_col,
                    origin_row: a.row as i32 - a.cell_row as i32,
                    origin_col: a.col as i32 - a.cell_col as i32,
                },
            )
        })
        .collect()
}

/// What one placeholder cell says. Also what the cell to its right inherits
/// when it leaves a diacritic out.
#[derive(Clone, Copy)]
struct PlaceholderCell {
    image_id: u32,
    placement_id: u32,
    cell_row: u16,
    cell_col: u16,
    /// The high byte of the image id, carried separately because a
    /// continuation cell inherits it rather than restating it.
    id_high: u16,
}

/// Decode the cell at `point` as a placeholder, or `None` when it is not one.
///
/// The image id is the cell's foreground colour — 24 bits in truecolor, or a
/// palette index in 256-colour mode, both of which kitty allows — plus an
/// optional high byte from a third diacritic. The placement id is the
/// underline colour, read the same way; no underline colour means the
/// image's default placement.
///
/// Kitty's inheritance rules apply to a cell that omits diacritics: it takes
/// the row, the column plus one, and the image-id high byte from the cell to
/// its left. That is what makes the compact form (`U+10EEEE` repeated with no
/// diacritics after the first cell) decode to a real rectangle.
fn placeholder_cell(
    term: &Terminal,
    point: Point,
    prev: Option<PlaceholderCell>,
) -> Option<PlaceholderCell> {
    let g = term.grid_ref(point).ok()?;
    let mut buf = [char::default(); 8];
    let n = g.graphemes(&mut buf).ok()?;
    let chars = &buf[..n];
    if chars.first() != Some(&PLACEHOLDER) {
        return None;
    }
    let style = g.style().ok()?;
    let base = color_id(style.fg_color)?;
    let placement_id = color_id(style.underline_color).unwrap_or(0);
    // Only a cell that names the same image and placement can be continued.
    let left = prev.filter(|p| p.image_id & 0x00ff_ffff == base && p.placement_id == placement_id);
    let mut diacritics = chars[1..].iter().filter_map(|&c| diacritic_index(c));
    let cell_row = diacritics
        .next()
        .or_else(|| left.map(|p| p.cell_row))
        .unwrap_or(0);
    let cell_col = diacritics
        .next()
        .or_else(|| left.map(|p| p.cell_col + 1))
        .unwrap_or(0);
    let id_high = diacritics
        .next()
        .or_else(|| left.map(|p| p.id_high))
        .unwrap_or(0);
    Some(PlaceholderCell {
        image_id: ((id_high as u32) << 24) | base,
        placement_id,
        cell_row,
        cell_col,
        id_high,
    })
}

/// The 24-bit id a placeholder colour names: truecolor components, or a
/// palette index (kitty's 256-colour form, which limits ids to 8 bits).
/// `None` for a default colour, which names nothing.
fn color_id(color: StyleColor) -> Option<u32> {
    match color {
        StyleColor::Rgb(c) => Some(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32),
        StyleColor::Palette(i) => Some(i.0 as u32),
        StyleColor::None => None,
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Put the storage back on the wire: one kitty transmission per image, then
/// one placement command per placement, in the protocol the child used.
///
/// This is what makes graphics survive the ATTACH/PEEK replay. libghostty's
/// VT serialization keeps the placeholder cells (ordinary text with a
/// foreground colour) but not the images or the placements, so a client that
/// only replayed the VT body would hold placeholders naming images it does
/// not have.
///
/// Everything the storage holds is emitted. The bound on a replay is the
/// bound on the state — [`MAX_STORAGE_BYTES`], which
/// [`crate::actor::TerminalActor::enable_graphics`] clamps the storage limit
/// to — so an image the terminal accepted can always be replayed. A second,
/// smaller replay cap would mean a supported image that a late client can
/// never receive, which is exactly the state this whole module exists to
/// prevent.
///
/// The block carries no cursor movement of its own beyond a save/restore
/// around a cursor-positioned placement, so it can be appended to a replay
/// payload without disturbing it.
pub fn replay(term: &Terminal, cell: CellSize) -> String {
    let cell = cell.or_fallback();
    let state = read(term, cell, 0);
    if state.placements.is_empty() {
        return String::new();
    }
    let rows = term.rows().unwrap_or(0) as i32;
    let cols = term.cols().unwrap_or(0) as i32;

    let mut ids: Vec<u32> = state.images.iter().map(|i| i.id).collect();
    ids.sort_unstable();
    let mut out = String::new();
    let mut sent: Vec<u32> = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(bytes) = image_bytes(term, id) else {
            continue;
        };
        let Some(f) = bytes.desc.format.kitty_format() else {
            // Grayscale has no kitty `f=`; nothing correct to emit.
            continue;
        };
        if bytes.data.is_empty() {
            continue;
        }
        transmit(&mut out, &bytes, f);
        sent.push(id);
    }
    for p in &state.placements {
        if !sent.contains(&p.image_id) {
            continue;
        }
        if p.is_virtual {
            // A virtual placement's command carries no position — its cells
            // do, and they are in the VT body wherever they are, including
            // in the scrollback. So where it currently shows has nothing to
            // do with whether it can be replayed.
            place_virtual(&mut out, p);
            continue;
        }
        // A placement the child put at the cursor is restored by putting the
        // cursor back, so it needs a cell in the active area. One whose top
        // has scrolled above it is still partly on screen: anchor it at row
        // 0 and advance the crop by the rows that are gone, which is what
        // the source terminal is showing.
        let PlacementPosition::Direct { col, row, .. } = p.position else {
            continue;
        };
        let clipped = (-row).max(0);
        if row >= rows || col < 0 || col >= cols || clipped >= p.cell_size.1 as i32 {
            continue;
        }
        place_direct(&mut out, p, col, row.max(0), clipped as u32, cell);
    }
    out
}

/// `ESC _G a=t,...;<base64> ESC \`, chunked at 4096 base64 bytes (the
/// protocol's limit, and what OMP emits).
fn transmit(out: &mut String, image: &ImageBytes, f: u32) {
    let mut params = format!("a=t,q=2,i={},f={}", image.desc.id, f);
    if image.desc.format.is_raw() {
        params.push_str(&format!(",s={},v={}", image.desc.width, image.desc.height));
    }
    if image.desc.compression == Compression::ZlibDeflate {
        params.push_str(",o=z");
    }
    let payload = base64(&image.data);
    let mut chunks = payload.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = chunks.peek().is_some();
        let chunk = std::str::from_utf8(chunk).unwrap_or_default();
        if first {
            out.push_str("\x1b_G");
            out.push_str(&params);
            if more {
                out.push_str(",m=1");
            }
            out.push(';');
            first = false;
        } else {
            out.push_str("\x1b_Gq=2,m=");
            out.push(if more { '1' } else { '0' });
            out.push(';');
        }
        out.push_str(chunk);
        out.push_str("\x1b\\");
    }
}

/// `ESC _G a=p,U=1,... ESC \`: the virtual placement the placeholder cells in
/// the VT body refer to.
fn place_virtual(out: &mut String, p: &Placement) {
    out.push_str("\x1b_Ga=p,U=1,q=2");
    out.push_str(&placement_params(p));
    out.push_str("\x1b\\");
}

/// A cursor-positioned placement: save the cursor, put it on the placement's
/// cell, place, restore.
///
/// `clipped_rows` is how many of its rows have scrolled above the active
/// area; the crop starts that many cells lower so what is emitted is what is
/// still on screen.
fn place_direct(out: &mut String, p: &Placement, col: i32, row: i32, clipped_rows: u32, cell: CellSize) {
    out.push_str(&format!("\x1b7\x1b[{};{}H\x1b_Ga=p,q=2", row + 1, col + 1));
    out.push_str(&placement_params(&clip_top(p, clipped_rows, cell)));
    out.push_str("\x1b\\\x1b8");
}

/// The same placement with its first `rows` cells of image cut off: the crop
/// moves down and shrinks, and the requested row count follows.
fn clip_top(p: &Placement, rows: u32, cell: CellSize) -> Placement {
    if rows == 0 {
        return *p;
    }
    let cut = (rows * cell.height.max(1)).min(p.source.height);
    let mut clipped = *p;
    clipped.source.y += cut;
    clipped.source.height -= cut;
    clipped.requested_cells.1 = p.requested_cells.1.saturating_sub(rows);
    clipped
}

/// The parameters both placement forms share.
///
/// The source rectangle is emitted in full, always: `w=`/`h=` default to 0,
/// which the protocol reads as "the whole image", so omitting them turns a
/// cropped placement into an uncropped one squeezed into the cropped
/// placement's cell box — the wrong pixels, at the right size.
fn placement_params(p: &Placement) -> String {
    let mut s = format!(",i={}", p.image_id);
    if p.placement_id != 0 {
        s.push_str(&format!(",p={}", p.placement_id));
    }
    if p.requested_cells.0 != 0 {
        s.push_str(&format!(",c={}", p.requested_cells.0));
    }
    if p.requested_cells.1 != 0 {
        s.push_str(&format!(",r={}", p.requested_cells.1));
    }
    s.push_str(&format!(
        ",x={},y={},w={},h={}",
        p.source.x, p.source.y, p.source.width, p.source.height
    ));
    if p.cell_offset.0 != 0 {
        s.push_str(&format!(",X={}", p.cell_offset.0));
    }
    if p.cell_offset.1 != 0 {
        s.push_str(&format!(",Y={}", p.cell_offset.1));
    }
    if p.z != 0 {
        s.push_str(&format!(",z={}", p.z));
    }
    s
}

/// Standard base64, no line breaks: what the protocol's payload is.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

/// Largest image a `f=100` transmission may decode to. Storage is bounded
/// too, but the decode buffer exists before storage sees it.
const MAX_DECODED_PNG_BYTES: usize = 64 * 1024 * 1024;

/// The decoder libghostty calls for `f=100`. Without one it rejects PNG
/// transmissions, which is what most senders (OMP included) use.
#[derive(Default)]
struct PngDecoder {
    buf: Vec<u8>,
}

impl gfx::DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc Allocator<'_>,
        data: &[u8],
    ) -> Option<gfx::DecodedImage<'alloc>> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
        // Normalize to 8 bits per channel: expand a palette, expand a low
        // bit depth, strip 16-bit. It does NOT make everything RGBA —
        // `Transformations::ALPHA` only adds alpha to a paletted image, and
        // there is no grayscale-to-RGB rule at all — so a grayscale PNG
        // arrives as `Grayscale` or `GrayscaleAlpha` and the expansion to
        // RGBA happens below. Rejecting those instead (which is what
        // checking for `Rgba` did) silently drops every monochrome plot and
        // optipng-converted asset a child sends.
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().ok()?;
        let size = reader.output_buffer_size()?;
        let (width, height) = reader.info().size();
        let rgba_len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if size == 0 || rgba_len == 0 || rgba_len > MAX_DECODED_PNG_BYTES {
            return None;
        }
        self.buf.clear();
        self.buf.resize(size, 0);
        let info = reader.next_frame(&mut self.buf).ok()?;
        if info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        let src = &self.buf[..info.buffer_size()];
        let mut bytes = Bytes::new_with_alloc(alloc, rgba_len).ok()?;
        match info.color_type {
            png::ColorType::Rgba => bytes.copy_from_slice(src),
            png::ColorType::Rgb => expand(src, 3, &mut bytes, |px, out| {
                out.copy_from_slice(&[px[0], px[1], px[2], 0xff])
            }),
            png::ColorType::GrayscaleAlpha => expand(src, 2, &mut bytes, |px, out| {
                out.copy_from_slice(&[px[0], px[0], px[0], px[1]])
            }),
            png::ColorType::Grayscale => expand(src, 1, &mut bytes, |px, out| {
                out.copy_from_slice(&[px[0], px[0], px[0], 0xff])
            }),
            // `normalize_to_color8` leaves no other 8-bit output type.
            _ => return None,
        }
        Some(gfx::DecodedImage {
            width: info.width,
            height: info.height,
            data: bytes,
        })
    }
}

/// Widen `src`, `stride` bytes per pixel, into 8-bit RGBA.
fn expand(src: &[u8], stride: usize, out: &mut [u8], px: impl Fn(&[u8], &mut [u8])) {
    for (i, chunk) in src.chunks_exact(stride).enumerate() {
        let Some(dst) = out.get_mut(i * 4..i * 4 + 4) else {
            return;
        };
        px(chunk, dst);
    }
}

/// Install the PNG decoder for this thread's terminals. Idempotent; the
/// decoder is thread-local in libghostty, so it must run on the thread that
/// owns the terminal.
pub fn install_png_decoder() -> bool {
    gfx::set_png_decoder(Some(Box::new(PngDecoder::default()))).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64(&[0xfb, 0xf0]), "+/A=");
    }

    #[test]
    fn diacritics_are_an_index_table() {
        assert_eq!(diacritic_index('\u{305}'), Some(0));
        assert_eq!(diacritic_index('\u{30d}'), Some(1));
        assert_eq!(diacritic_index('\u{1d244}'), Some(296));
        assert_eq!(diacritic_index('a'), None);
    }
}
