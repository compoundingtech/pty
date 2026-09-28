//! Kitty graphics state: what a compositor reads, and what survives a
//! replay.
//!
//! Every test drives the actor with the exact bytes OMP writes
//! (`packages/tui/src/terminal-capabilities.ts` `encodeKittyTransmit`,
//! `packages/tui/src/kitty-graphics.ts` `encodeKittyVirtualPlacement` and
//! `encodeKittyPlaceholderGrid`): a `f=100` PNG transmission, a `a=p,U=1`
//! virtual placement, and placeholder cells carrying the image id in their
//! foreground colour and the placement id in their underline colour.

use pty_terminal::graphics::PLACEHOLDER;
use pty_terminal::{
    CellSize, GraphicsOptions, PixelFormat, PlacementPosition, Range, SerializeOpts, TerminalActor,
};

/// A 16x8 RGBA PNG. Generated once, checked in as bytes: the point is that
/// the terminal decodes a real PNG, not that this test can make one.
const PNG_16X8_BASE64: &str = concat!(
    "iVBORw0KGgoAAAANSUhEUgAAABAAAAAICAYAAADwdn+XAAAAqUlEQVR4nBXMwYBFIQAAwIewCCGEEEIIIYQQQggh",
    "hBBCCCFk8Hc6zHW+72u/PwKRRKZQaXQGk8XmcPm+ICAQSWQKlUZnMFlsDje8IAkIRBKZQqXRGUwWm8NNLygCApFE",
    "plBpdAaTxeZwywuagEAkkSlUGp3BZLE53PaCISAQSWQKlUZnMFlsDne8YAkIRBKZQqXRGUwWm8NdLzgCApFEplBp",
    "dAaTxeZw+Qf7mTOQwddl2QAAAABJRU5ErkJggg=="
);

const IMAGE_ID: u32 = 4242;
const PLACEMENT_ID: u32 = 7;

fn actor(rows: u16, cols: u16, scrollback: usize) -> TerminalActor {
    let mut a = TerminalActor::new(rows, cols, scrollback);
    assert!(
        a.enable_graphics(GraphicsOptions {
            cell: CellSize {
                width: 8,
                height: 16
            },
            ..GraphicsOptions::DEFAULT
        }),
        "graphics must turn on"
    );
    a
}

#[test]
fn a_graphics_enabled_session_accepts_file_medium_images() {
    let a = actor(24, 80, 100);
    assert!(a.terminal().is_kitty_image_from_file_allowed().unwrap());
    assert!(!a.terminal().is_kitty_image_from_temp_file_allowed().unwrap());
    assert!(!a.terminal().is_kitty_image_from_shared_mem_allowed().unwrap());
}

/// OMP's `encodeKittyTransmit`: `a=t,f=100,q=2,i=<id>;<base64>`.
fn omp_transmit(id: u32) -> String {
    format!("\x1b_Ga=t,f=100,q=2,i={id};{PNG_16X8_BASE64}\x1b\\")
}

/// OMP's `encodeKittyVirtualPlacement`: `a=p,U=1,q=2,i=<id>,p=<pid>,c=,r=`.
fn omp_virtual_placement(id: u32, pid: u32, cols: u32, rows: u32) -> String {
    format!("\x1b_Ga=p,U=1,q=2,i={id},p={pid},c={cols},r={rows}\x1b\\")
}

/// The row/column diacritics OMP indexes into, first four entries.
const DIACRITICS: [char; 4] = ['\u{305}', '\u{30d}', '\u{30e}', '\u{310}'];

/// OMP's `encodeKittyPlaceholderGrid`: image id in the foreground colour,
/// placement id in the underline colour, every cell naming its own row and
/// column. One string per row, no cursor movement.
fn omp_placeholder_rows(id: u32, pid: u32, cols: usize, rows: usize) -> Vec<String> {
    let fg = format!(
        "\x1b[38;2;{};{};{}m",
        (id >> 16) & 0xff,
        (id >> 8) & 0xff,
        id & 0xff
    );
    let ul = format!(
        "\x1b[58:2::{}:{}:{}m",
        (pid >> 16) & 0xff,
        (pid >> 8) & 0xff,
        pid & 0xff
    );
    (0..rows)
        .map(|r| {
            let mut line = format!("{fg}{ul}");
            for c in 0..cols {
                line.push(PLACEHOLDER);
                line.push(DIACRITICS[r]);
                line.push(DIACRITICS[c]);
            }
            line.push_str("\x1b[39;59m");
            line
        })
        .collect()
}

/// The whole OMP render: transmit, then the placement APC in front of the
/// first placeholder row, rows separated by CR/LF.
fn omp_image(id: u32, pid: u32, cols: usize, rows: usize) -> String {
    let mut out = omp_transmit(id);
    let grid = omp_placeholder_rows(id, pid, cols, rows);
    for (i, row) in grid.iter().enumerate() {
        if i == 0 {
            out.push_str(&omp_virtual_placement(id, pid, cols as u32, rows as u32));
        } else {
            out.push_str("\r\n");
        }
        out.push_str(row);
    }
    out
}

#[test]
fn graphics_are_off_until_the_owner_asks() {
    let mut a = TerminalActor::new(10, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    let state = a.graphics_state(0);
    assert!(!state.enabled, "no storage limit, no protocol");
    assert!(state.images.is_empty());
    assert!(state.placements.is_empty());
    assert_eq!(a.graphics_generation(), 0);
    assert!(a.image_bytes(IMAGE_ID).is_none());
}

#[test]
fn an_omp_write_gives_image_bytes_placement_identity_crop_and_position() {
    let mut a = actor(10, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());

    let state = a.graphics_state(0);
    assert!(state.enabled);
    assert_ne!(state.generation, 0, "a transmit mutates the storage");

    let image = state.image(IMAGE_ID).expect("the image is stored");
    assert_eq!((image.width, image.height), (16, 8));
    // The PNG was decoded: libghostty stores 8-bit RGBA.
    assert_eq!(image.format, PixelFormat::Rgba);
    assert_eq!(image.len, 16 * 8 * 4);
    assert_ne!(image.generation, 0);

    let bytes = a.image_bytes(IMAGE_ID).expect("bytes are readable");
    assert_eq!(bytes.data.len(), 16 * 8 * 4);
    assert_eq!(bytes.desc.generation, image.generation);

    assert_eq!(state.placements.len(), 1);
    let p = &state.placements[0];
    assert_eq!((p.image_id, p.placement_id), (IMAGE_ID, PLACEMENT_ID));
    assert!(p.is_virtual, "U=1 is a virtual placement");
    assert_eq!(p.requested_cells, (2, 2));
    // No source rect was sent, so the crop is the whole image.
    assert_eq!(
        (p.source.x, p.source.y, p.source.width, p.source.height),
        (0, 0, 16, 8)
    );
    match p.position {
        PlacementPosition::Placeholder(r) => {
            assert_eq!((r.row, r.col), (0, 0), "written at the home position");
            assert_eq!((r.rows, r.cols), (2, 2));
            assert_eq!((r.cell_row, r.cell_col), (0, 0));
            assert_eq!((r.origin_row, r.origin_col), (0, 0));
        }
        other => panic!("expected a placeholder position, got {other:?}"),
    }
}

#[test]
fn a_source_rect_and_offsets_come_back_resolved() {
    let mut a = actor(10, 20, 0);
    a.write(omp_transmit(IMAGE_ID).as_bytes());
    // A crop with a zero height: kitty means "to the bottom edge".
    a.write(
        format!("\x1b_Ga=p,U=1,q=2,i={IMAGE_ID},x=4,y=2,w=8,h=0,X=3,Y=5,z=-1,c=2,r=1\x1b\\")
            .as_bytes(),
    );
    for row in omp_placeholder_rows(IMAGE_ID, 0, 2, 1) {
        a.write(row.as_bytes());
    }
    let state = a.graphics_state(0);
    let p = &state.placements[0];
    assert_eq!(p.placement_id, 0, "no p= means no placement id");
    assert_eq!(
        (p.source.x, p.source.y, p.source.width, p.source.height),
        (4, 2, 8, 6),
        "h=0 resolves to the rest of the image"
    );
    assert_eq!(p.cell_offset, (3, 5));
    assert_eq!(p.z, -1);
}

#[test]
fn scrolling_moves_the_placement_and_scrollback_still_finds_it() {
    let mut a = actor(6, 20, 100);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    let before = a.graphics_state(0).generation;

    // Push the image up by three rows.
    a.write(b"\r\n\r\n\r\n\r\n\r\nx");

    let state = a.graphics_state(0);
    assert_eq!(
        state.generation, before,
        "scrolling does not touch the storage"
    );
    let p = &state.placements[0];
    match p.position {
        PlacementPosition::Placeholder(r) => {
            assert_eq!(r.row, 0, "the image's second row is now the top row");
            assert_eq!(r.rows, 1, "its first row has scrolled into history");
            assert_eq!(r.cell_row, 1, "the visible row is image row 1");
            assert_eq!(r.origin_row, -1, "image row 0 is one row above");
        }
        other => panic!("expected a placeholder position, got {other:?}"),
    }

    // The same read one row back into history sees the whole image again.
    let scrolled = a.graphics_state(1);
    match scrolled.placements[0].position {
        PlacementPosition::Placeholder(r) => {
            assert_eq!((r.row, r.rows, r.cell_row), (0, 2, 0));
            assert_eq!(r.origin_row, 0);
        }
        other => panic!("expected a placeholder position, got {other:?}"),
    }
}

#[test]
fn a_resize_keeps_the_image_and_reprojects_it() {
    let mut a = actor(6, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    let generation = a.graphics_state(0).generation;

    a.resize(10, 8);

    let state = a.graphics_state(0);
    assert_eq!(state.generation, generation, "a resize keeps the storage");
    assert_eq!(state.cell, CellSize { width: 8, height: 16 });
    assert!(
        a.image_bytes(IMAGE_ID).is_some(),
        "the pixels survive a resize"
    );
    assert!(
        matches!(
            state.placements[0].position,
            PlacementPosition::Placeholder(_)
        ),
        "the placeholder cells reflowed with the text, and were found again"
    );
}

#[test]
fn the_alternate_screen_has_its_own_storage() {
    let mut a = actor(6, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert_eq!(a.graphics_state(0).placements.len(), 1);

    a.write(b"\x1b[?1049h");
    assert!(
        a.graphics_state(0).placements.is_empty(),
        "the alternate screen starts with no images"
    );
    assert!(
        a.graphics_state(0).enabled,
        "the protocol is on for both screens"
    );

    // An image placed on the alternate screen is the alternate screen's.
    a.write(omp_image(99, 1, 1, 1).as_bytes());
    let alt = a.graphics_state(0);
    assert_eq!(alt.placements.len(), 1);
    assert_eq!(alt.placements[0].image_id, 99);

    a.write(b"\x1b[?1049l");
    let back = a.graphics_state(0);
    assert_eq!(back.placements.len(), 1, "the primary screen kept its own");
    assert_eq!(back.placements[0].image_id, IMAGE_ID);
}

#[test]
fn a_delete_from_the_child_drops_the_placement_and_the_bytes() {
    let mut a = actor(6, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    let before = a.graphics_state(0).generation;

    // OMP's delete-placement, then its delete-image.
    a.write(format!("\x1b_Ga=d,d=i,i={IMAGE_ID},p={PLACEMENT_ID},q=2\x1b\\").as_bytes());
    let after_placement = a.graphics_state(0);
    assert!(after_placement.placements.is_empty());
    assert_ne!(after_placement.generation, before, "a delete is a mutation");
    assert!(
        a.image_bytes(IMAGE_ID).is_some(),
        "deleting a placement keeps the image"
    );

    a.write(format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\").as_bytes());
    assert!(a.image_bytes(IMAGE_ID).is_none(), "now the image is gone");
}

#[test]
fn clear_graphics_drops_everything_and_keeps_the_protocol() {
    let mut a = actor(6, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert!(a.image_bytes(IMAGE_ID).is_some());

    a.clear_graphics();

    let state = a.graphics_state(0);
    assert!(state.enabled, "the protocol stays on");
    assert!(state.images.is_empty());
    assert!(state.placements.is_empty());
    assert!(a.image_bytes(IMAGE_ID).is_none());

    // And the terminal still accepts a new image.
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert_eq!(a.graphics_state(0).placements.len(), 1);
}

#[test]
fn zeroing_the_limit_turns_the_protocol_off() {
    let mut a = actor(6, 20, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());

    a.set_graphics_storage_limit(0);

    assert!(!a.graphics_state(0).enabled);
    assert!(a.graphics_options().is_none());
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert!(
        a.graphics_state(0).placements.is_empty(),
        "a disabled terminal stores nothing"
    );
}

/// The case the Node daemon loses: a client that was not connected when the
/// image was written attaches, gets only the SCREEN payload, and must end up
/// with the same image bytes and the same placement.
#[test]
fn a_late_client_reconstructs_the_image_from_the_replay_alone() {
    let mut source = actor(8, 20, 100);
    source.write(b"before\r\n");
    source.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    source.write(b"\r\nafter");

    let payload = source.serialize(SerializeOpts::ATTACH);
    assert!(
        payload.contains(&format!("i={IMAGE_ID}")),
        "the replay carries the image"
    );
    assert!(
        payload.contains("a=p,U=1"),
        "and the virtual placement that binds the placeholder cells"
    );

    // A fresh terminal that has seen none of the original DATA.
    let mut late = actor(8, 20, 100);
    late.reset();
    late.write(payload.as_bytes());

    let state = late.graphics_state(0);
    let image = state
        .image(IMAGE_ID)
        .expect("the late client has the image");
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(image.format, PixelFormat::Rgba);
    assert_eq!(
        late.image_bytes(IMAGE_ID).map(|b| b.data),
        source.image_bytes(IMAGE_ID).map(|b| b.data),
        "byte for byte the same pixels"
    );

    assert_eq!(state.placements.len(), 1);
    let p = &state.placements[0];
    assert_eq!(p.image_id, IMAGE_ID);
    assert_eq!(p.placement_id, PLACEMENT_ID, "placement identity survives");
    assert!(p.is_virtual);
    assert_eq!(p.requested_cells, (2, 2));

    // And it is in the same place as in the source terminal.
    assert_eq!(
        p.position,
        source.graphics_state(0).placements[0].position,
        "same cells"
    );
    assert_eq!(late.plain(Range::Full), source.plain(Range::Full));
}

#[test]
fn a_cursor_positioned_placement_replays_at_its_cell() {
    let mut source = actor(8, 20, 0);
    source.write(b"\r\n\r\n  ");
    source.write(omp_transmit(IMAGE_ID).as_bytes());
    source.write(format!("\x1b_Ga=p,q=2,i={IMAGE_ID},p=3,c=2,r=1\x1b\\").as_bytes());
    let placed = match source.graphics_state(0).placements[0].position {
        PlacementPosition::Direct { col, row, .. } => (col, row),
        other => panic!("expected a direct placement, got {other:?}"),
    };
    assert_eq!(placed, (2, 2), "at the cursor");
    let cursor = source.cursor();

    let payload = source.serialize(SerializeOpts::ATTACH);
    let mut late = actor(8, 20, 0);
    late.reset();
    late.write(payload.as_bytes());

    let state = late.graphics_state(0);
    assert_eq!(state.placements.len(), 1);
    assert_eq!(state.placements[0].placement_id, 3);
    assert_eq!(
        state.placements[0].position,
        PlacementPosition::Direct {
            col: 2,
            row: 2,
            cols: 2,
            rows: 1
        }
    );
    assert_eq!(
        late.cursor(),
        cursor,
        "the graphics block leaves the cursor where the replay put it"
    );
}

#[test]
fn a_replay_without_graphics_is_unchanged() {
    let mut plain = TerminalActor::new(6, 20, 0);
    plain.write(b"hello\r\nworld");
    let without = plain.serialize(SerializeOpts::ATTACH);

    let mut enabled = actor(6, 20, 0);
    enabled.write(b"hello\r\nworld");
    assert_eq!(
        enabled.serialize(SerializeOpts::ATTACH),
        without,
        "a session that never sent an image serializes exactly as before"
    );
}

// ── bounds ──

/// A raw RGBA transmission, chunked at 4096 base64 bytes the way every real
/// sender does it (kitty's own limit per APC command): `m=1` on every chunk
/// but the last.
fn chunked_rgba_transmit(id: u32, width: u32, height: u32, data: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut payload = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], c.get(1).copied().unwrap_or(0), c.get(2).copied().unwrap_or(0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        payload.push(B64[(n >> 18) as usize & 63] as char);
        payload.push(B64[(n >> 12) as usize & 63] as char);
        payload.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        payload.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    let mut out = String::with_capacity(payload.len() + 4096);
    let mut chunks = payload.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = chunks.peek().is_some();
        let chunk = std::str::from_utf8(chunk).expect("base64 is ascii");
        if first {
            let m = if more { ",m=1" } else { "" };
            out.push_str(&format!(
                "\x1b_Ga=t,q=2,i={id},f=32,s={width},v={height}{m};{chunk}\x1b\\"
            ));
            first = false;
        } else {
            let m = if more { 1 } else { 0 };
            out.push_str(&format!("\x1b_Gq=2,m={m};{chunk}\x1b\\"));
        }
    }
    out
}

/// The bound on a replay is the bound on the state. An image well past the
/// 3 MiB a former replay cap allowed is still stored *and* still replayed:
/// otherwise a supported terminal state would be one a late client can never
/// be given, which is the exact failure this module exists to prevent.
#[test]
fn an_image_far_larger_than_three_mib_still_replays() {
    // 1024 x 1024 RGBA = 4 MiB, over any per-image cap and under
    // MAX_STORAGE_BYTES.
    let (w, h) = (1024u32, 1024u32);
    let data: Vec<u8> = (0..(w * h * 4))
        .map(|i| (i % 251) as u8)
        .collect();
    assert!(data.len() > 3 * 1024 * 1024);

    let mut source = actor(8, 20, 0);
    source.write(chunked_rgba_transmit(IMAGE_ID, w, h, &data).as_bytes());
    source.write(omp_virtual_placement(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    for row in omp_placeholder_rows(IMAGE_ID, PLACEMENT_ID, 2, 2) {
        source.write(row.as_bytes());
    }
    let stored = source
        .image_bytes(IMAGE_ID)
        .expect("the terminal accepted a 4 MiB image");
    assert_eq!(stored.data.len(), data.len());
    assert_eq!(stored.data, data, "chunked transmission arrived intact");

    let payload = source.serialize(SerializeOpts::ATTACH);
    let mut late = actor(8, 20, 0);
    late.reset();
    late.write(payload.as_bytes());

    assert_eq!(
        late.image_bytes(IMAGE_ID).map(|b| b.data),
        Some(data),
        "a late client gets every byte of it"
    );
    let state = late.graphics_state(0);
    assert_eq!(state.placements.len(), 1);
    assert_eq!(state.placements[0].placement_id, PLACEMENT_ID);
}

#[test]
fn the_storage_limit_is_clamped_to_what_a_replay_can_carry() {
    let mut a = TerminalActor::new(8, 20, 0);
    assert!(a.enable_graphics(GraphicsOptions {
        storage_bytes: 8 * 1024 * 1024 * 1024,
        ..GraphicsOptions::DEFAULT
    }));
    assert_eq!(
        a.graphics_options().map(|o| o.storage_bytes),
        Some(pty_terminal::graphics::MAX_STORAGE_BYTES),
        "asking for more than can be replayed gets the supported bound"
    );
    assert_eq!(
        a.graphics_state(0).storage_bytes,
        pty_terminal::graphics::MAX_STORAGE_BYTES
    );

    a.set_graphics_storage_limit(u64::MAX);
    assert_eq!(
        a.graphics_state(0).storage_bytes,
        pty_terminal::graphics::MAX_STORAGE_BYTES
    );
}

// ── cell metrics ──

/// A placement that named neither `c=` nor `r=` gets its cell extent from the
/// image's pixel size and the cell size — so the cell size has to be the
/// client's real one, not a guess about its font.
#[test]
fn an_implicit_placement_takes_its_extent_from_the_declared_cell() {
    let mut a = TerminalActor::new(8, 40, 0);
    assert!(a.enable_graphics(GraphicsOptions {
        cell: CellSize::default(), // undeclared
        ..GraphicsOptions::DEFAULT
    }));
    let state = a.graphics_state(0);
    assert!(!state.cell_declared, "nobody has said how big a cell is");
    assert_eq!(state.cell, CellSize::FALLBACK);

    // A 32x32 image placed with no c=/r=: 4x2 cells at the 8x16 fallback.
    let data = vec![0u8; 32 * 32 * 4];
    a.write(chunked_rgba_transmit(IMAGE_ID, 32, 32, &data).as_bytes());
    a.write(format!("\x1b_Ga=p,q=2,i={IMAGE_ID},p=1\x1b\\").as_bytes());
    assert_eq!(a.graphics_state(0).placements[0].cell_size, (4, 2));

    // The client says its cells are 16x16: the same image is now 2x2 cells.
    a.set_cell_size(CellSize {
        width: 16,
        height: 16,
    });
    let state = a.graphics_state(0);
    assert!(state.cell_declared);
    assert_eq!(state.cell, CellSize { width: 16, height: 16 });
    assert_eq!(
        state.placements[0].cell_size,
        (2, 2),
        "geometry follows the declared cell"
    );
    assert_eq!(
        state.placements[0].requested_cells,
        (0, 0),
        "the child never named a size; only the derived extent moved"
    );

    // And it survives a grid resize, which is a change of grid, not of font.
    a.resize(20, 6);
    let state = a.graphics_state(0);
    assert!(state.cell_declared);
    assert_eq!(state.placements[0].cell_size, (2, 2));
}

#[test]
fn a_declared_cell_does_not_move_an_explicit_placement() {
    let mut a = actor(8, 40, 0);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert_eq!(a.graphics_state(0).placements[0].cell_size, (2, 2));
    a.set_cell_size(CellSize {
        width: 20,
        height: 40,
    });
    assert_eq!(
        a.graphics_state(0).placements[0].cell_size,
        (2, 2),
        "c=2,r=2 is the child's own answer and no cell size changes it"
    );
}

// ── replay fidelity (regressions) ──

/// `w=`/`h=` default to 0 in the protocol, which means "the whole image", so
/// a replay that omits them turns a cropped placement into an uncropped one
/// squeezed into the cropped placement's cell box: the wrong pixels at the
/// right size, on every reattaching client.
#[test]
fn a_cropped_placement_keeps_its_crop_through_a_replay() {
    let mut source = actor(8, 20, 0);
    source.write(omp_transmit(IMAGE_ID).as_bytes());
    source.write(
        format!("\x1b_Ga=p,U=1,q=2,i={IMAGE_ID},p={PLACEMENT_ID},x=4,y=2,w=8,h=6,c=2,r=1\x1b\\")
            .as_bytes(),
    );
    for row in omp_placeholder_rows(IMAGE_ID, PLACEMENT_ID, 2, 1) {
        source.write(row.as_bytes());
    }
    let crop = source.graphics_state(0).placements[0].source;
    assert_eq!((crop.x, crop.y, crop.width, crop.height), (4, 2, 8, 6));

    let payload = source.serialize(SerializeOpts::ATTACH);
    assert!(
        payload.contains("w=8,h=6"),
        "the replay names the crop: {payload:?}"
    );

    let mut late = actor(8, 20, 0);
    late.reset();
    late.write(payload.as_bytes());
    let replayed = late.graphics_state(0).placements[0].source;
    assert_eq!(
        (replayed.x, replayed.y, replayed.width, replayed.height),
        (4, 2, 8, 6),
        "same pixels, not the whole image measured from (4, 2)"
    );
}

/// A virtual placement's command carries no position, and its placeholder
/// cells are serialized wherever they are — including the scrollback. So a
/// replay must carry it even when nothing of it is in the viewport, or a
/// late client holds placeholder cells naming an image it never received.
#[test]
fn a_virtual_placement_scrolled_into_history_still_replays() {
    let mut source = actor(4, 20, 100);
    source.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    // Push it entirely out of the viewport.
    source.write(b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\nbottom");
    assert_eq!(
        source.graphics_state(0).placements[0].position,
        PlacementPosition::Offscreen,
        "nothing of it is in the live viewport any more"
    );

    let payload = source.serialize(SerializeOpts::ATTACH);
    assert!(
        payload.contains("a=p,U=1"),
        "the placement is still replayed: {payload:?}"
    );

    let mut late = actor(4, 20, 100);
    late.reset();
    late.write(payload.as_bytes());
    let state = late.graphics_state(0);
    assert_eq!(state.placements.len(), 1);
    assert_eq!(
        (state.placements[0].image_id, state.placements[0].placement_id),
        (IMAGE_ID, PLACEMENT_ID)
    );
    assert!(late.image_bytes(IMAGE_ID).is_some());
}

/// A cursor-positioned placement whose top has scrolled above the viewport is
/// still partly on screen. libghostty reports a negative row for exactly this
/// case, so dropping every negative row loses an image the source terminal is
/// showing.
#[test]
fn a_partially_scrolled_direct_placement_replays_clipped() {
    let mut source = actor(6, 20, 100);
    source.write(omp_transmit(IMAGE_ID).as_bytes());
    source.write(format!("\x1b_Ga=p,q=2,i={IMAGE_ID},p=3,c=2,r=4\x1b\\").as_bytes());
    // Scroll it up by two rows: its top two cell rows are gone, its bottom
    // two are still on screen.
    source.write(b"\r\n\r\n\r\nx");
    let position = source.graphics_state(0).placements[0].position;
    let PlacementPosition::Direct { row, .. } = position else {
        panic!("expected a direct placement, got {position:?}");
    };
    assert!(row < 0, "its top has scrolled above the viewport: {row}");

    let payload = source.serialize(SerializeOpts::ATTACH);
    assert!(
        payload.contains(&format!("i={IMAGE_ID},p=3")),
        "the visible part is still replayed: {payload:?}"
    );
    let mut late = actor(6, 20, 100);
    late.reset();
    late.write(payload.as_bytes());
    let state = late.graphics_state(0);
    assert_eq!(state.placements.len(), 1, "not dropped");
    assert_eq!(state.placements[0].placement_id, 3);
    assert!(
        state.placements[0].requested_cells.1 < 4,
        "the rows that scrolled off are not re-placed: {:?}",
        state.placements[0].requested_cells
    );
}

/// A grayscale PNG is an ordinary PNG. Rejecting it (which is what checking
/// the decoder's output for RGBA did) drops every monochrome plot a child
/// sends, with no diagnostic.
#[test]
fn a_grayscale_png_is_stored_as_rgba() {
    const GRAY_4X2_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAACCAAAAABawyK/AAAAEklEQVR4nGNgsKnYwsDl1rQPAAwmAvkL8nz8AAAAAElFTkSuQmCC";
    let mut a = actor(8, 20, 0);
    a.write(format!("\x1b_Ga=t,f=100,q=2,i=55;{GRAY_4X2_PNG}\x1b\\").as_bytes());
    a.write("\x1b_Ga=p,U=1,q=2,i=55,p=1,c=1,r=1\x1b\\".as_bytes());
    for row in omp_placeholder_rows(55, 1, 1, 1) {
        a.write(row.as_bytes());
    }
    let bytes = a.image_bytes(55).expect("the grayscale PNG was accepted");
    assert_eq!((bytes.desc.width, bytes.desc.height), (4, 2));
    assert_eq!(bytes.desc.format, PixelFormat::Rgba);
    assert_eq!(bytes.data.len(), 4 * 2 * 4);
    for px in bytes.data.chunks_exact(4) {
        assert_eq!(px[0], px[1], "grey expands to r == g == b");
        assert_eq!(px[1], px[2]);
        assert_eq!(px[3], 0xff, "with full alpha");
    }
}

/// Kitty allows a placeholder cell to name its image with a 256-colour
/// foreground, and its own worked example uses one. A reader that insists on
/// truecolor does not see such a cell at all.
#[test]
fn a_palette_foreground_names_a_placeholder_image() {
    let mut a = actor(8, 20, 0);
    a.write(omp_transmit(42).as_bytes());
    a.write("\x1b_Ga=p,U=1,q=2,i=42,c=1,r=1\x1b\\".as_bytes());
    a.write(format!("\x1b[38;5;42m{PLACEHOLDER}\u{305}\u{305}\x1b[39m").as_bytes());
    let state = a.graphics_state(0);
    assert_eq!(state.placements.len(), 1);
    match state.placements[0].position {
        PlacementPosition::Placeholder(r) => assert_eq!((r.row, r.col), (0, 0)),
        other => panic!("a palette foreground must name the image, got {other:?}"),
    }
}

/// Kitty's compact placeholder form leaves the diacritics off every cell but
/// the first: those cells inherit the row, the column plus one, and the image
/// id's high byte from the cell to their left.
#[test]
fn a_bare_continuation_cell_inherits_row_column_and_id_high_byte() {
    let id: u32 = 0x0100_0009;
    let mut a = actor(8, 20, 0);
    a.write(format!("\x1b_Ga=t,f=100,q=2,i={id};{PNG_16X8_BASE64}\x1b\\").as_bytes());
    a.write(format!("\x1b_Ga=p,U=1,q=2,i={id},c=3,r=1\x1b\\").as_bytes());
    // Row 0, column 0, id high byte 1 — then two bare continuation cells.
    let first = format!("{PLACEHOLDER}\u{305}\u{305}\u{30d}");
    a.write(
        format!("\x1b[38;2;0;0;9m{first}{PLACEHOLDER}{PLACEHOLDER}\x1b[39m").as_bytes(),
    );

    let state = a.graphics_state(0);
    assert_eq!(state.placements.len(), 1);
    assert_eq!(state.images[0].id, id, "the high byte reached the image id");
    match state.placements[0].position {
        PlacementPosition::Placeholder(r) => {
            assert_eq!((r.row, r.col), (0, 0));
            assert_eq!(
                (r.rows, r.cols),
                (1, 3),
                "all three cells belong to the same placement"
            );
        }
        other => panic!("expected a placeholder position, got {other:?}"),
    }
}

/// Raising the limit is the one way graphics get turned on, so it has to turn
/// them on completely: the cell metrics the terminal already had, and a PNG
/// decoder. The old shape rebuilt the options from the defaults, which
/// silently replaced a declared cell size with 8x16.
#[test]
fn raising_the_storage_limit_keeps_the_cell_size_and_decodes_png() {
    let mut a = TerminalActor::new(8, 20, 0);
    assert!(a.enable_graphics(GraphicsOptions {
        cell: CellSize {
            width: 10,
            height: 21
        },
        ..GraphicsOptions::DEFAULT
    }));
    a.set_graphics_storage_limit(0);
    assert!(!a.graphics_state(0).enabled);

    a.set_graphics_storage_limit(4 * 1024 * 1024);
    let state = a.graphics_state(0);
    assert!(state.enabled);
    assert!(state.cell_declared);
    assert_eq!(
        state.cell,
        CellSize {
            width: 10,
            height: 21
        },
        "the declared cell size survived"
    );

    // And PNG still decodes, which it would not with no decoder installed.
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert!(a.image_bytes(IMAGE_ID).is_some());
}

/// An actor that never enabled graphics and then has its limit raised must
/// end up in the same complete state, not one that stores images it cannot
/// decode.
#[test]
fn the_storage_limit_alone_turns_graphics_fully_on() {
    let mut a = TerminalActor::new(8, 20, 0);
    a.set_graphics_storage_limit(4 * 1024 * 1024);
    assert!(a.graphics_state(0).enabled);
    a.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    let bytes = a.image_bytes(IMAGE_ID).expect("a PNG transmission decodes");
    assert_eq!((bytes.desc.width, bytes.desc.height), (16, 8));
}

/// The normal screen's images have to land in the client's normal screen.
/// Kitty storage is per screen and the replay's alt-screen prefix is at byte
/// 0, so the normal half has to say where it belongs.
#[test]
fn a_replay_from_the_alt_screen_puts_the_normal_screens_images_on_the_normal_screen() {
    let mut source = actor(6, 20, 100);
    source.write(omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2).as_bytes());
    assert_eq!(source.graphics_state(0).placements.len(), 1);
    source.write(b"\x1b[?1049h");
    source.write(b"full-screen program");
    assert!(source.graphics_state(0).placements.is_empty());

    let payload = source.serialize(SerializeOpts::ATTACH);
    assert!(
        payload.starts_with("\x1b[?1049h"),
        "the prefix position is the contract"
    );

    let mut late = actor(6, 20, 100);
    late.reset();
    late.write(payload.as_bytes());
    assert!(
        late.graphics_state(0).placements.is_empty(),
        "the alternate screen has no images, as in the source"
    );
    assert_eq!(
        late.plain(Range::Viewport),
        source.plain(Range::Viewport),
        "the alternate screen replays as it was"
    );

    // The program exits: the normal screen comes back, and so do its images.
    late.write(b"\x1b[?1049l");
    let state = late.graphics_state(0);
    assert_eq!(
        state.placements.len(),
        1,
        "the normal screen's image survived the replay"
    );
    assert_eq!(state.placements[0].placement_id, PLACEMENT_ID);
    assert!(late.image_bytes(IMAGE_ID).is_some());
}
