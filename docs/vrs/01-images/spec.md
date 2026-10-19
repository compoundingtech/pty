# Terminal images — Specification

## Status

Implemented. The session daemon's terminal holds images, `serialize::vt`
replays them, and the embedding handle reads them. This node is lazy — it
stands on [./requirements.md](./requirements.md) alone and requires no parent
`docs/vrs` artifacts. The rationale for the shape below is
[decision 0012](../../decisions/0012-kitty-graphics-replay.md); this
specification states the mechanism and does not restate the argument.

## Scope

This specification defines how a pty session holds, bounds, reports, and
replays the terminal images a child draws with the Kitty graphics protocol:
storage admission and its byte bound, placement and crop geometry resolved
against a requested window, the cell pixel metrics the geometry needs, the
replay wire form, and the storage lifecycle across reset and screen switches.

It does not define rendering — a consumer decides how, where, and whether to
draw what it reads. It does not define the SIXEL or iTerm2 protocols, which
are not read. It does not define Node parity for images: the Node daemon
holds no image state at all, so there is nothing to compare against
(see the Node parity map in [docs/parity.md](../../parity.md) for the
surfaces that do compare).

Requirements `PTY.IMG-R01` through `PTY.IMG-R10` are in
[./requirements.md](./requirements.md) and are cited inline below.

## Module map

| Concern | Source |
| --- | --- |
| Admission, bound, cell metrics, generation | `crates/pty-terminal/src/actor.rs` — `TerminalActor::enable_graphics`, `set_graphics_storage_limit`, `graphics_options`, `set_cell_size`, `graphics_generation`, `reset` |
| Storage bound and options | `crates/pty-terminal/src/graphics.rs` — `MAX_STORAGE_BYTES` (32 MiB), `GraphicsOptions` |
| Window read | `crates/pty-terminal/src/graphics.rs` — `read`, `screen_origin`, `scan_placeholders`, `placeholder_cell`, `PLACEHOLDER` (U+10EEEE), `ROWCOLUMN_DIACRITICS` |
| Owned reported values | `crates/pty-terminal/src/graphics.rs` — `GraphicsState`, `ImageDesc`, `Placement`, `PlacementPosition`, `PlaceholderRect`, `SourceRect`, `image_bytes` |
| Replay emission | `crates/pty-terminal/src/graphics.rs` — `replay`, `transmit`, `place_virtual`, `place_direct`, `clip_top`, `placement_params`; called from `crates/pty-terminal/src/serialize.rs` — `vt` |
| PNG normalization | `crates/pty-terminal/src/graphics.rs` — `PngDecoder`, `expand`, `install_png_decoder`, `MAX_DECODED_PNG_BYTES` (64 MiB) |
| Cell metric wire | `crates/pty-core/src/protocol.rs` — `encode_attach_with_cell`, `encode_resize_with_cell`, `decode_cell` |
| Client and daemon adoption | `crates/pty/src/daemon/clients.rs` — `adopt_cell_size`; `crates/pty/src/daemon/lifecycle.rs` — `terminal_actor` |
| Embedding surface | `crates/pty/src/handle.rs` — `TerminalHandle::frame`, `observe_frames`, `request_frame`, `graphics`, `image_bytes`, `set_cell_size`, `graphics_generation`, `Frame::image_bytes`, `HandleEvent::Graphics` |

## Admission and bound

`TerminalActor::enable_graphics(GraphicsOptions)` sets libghostty's image
storage limit and installs the `f=100` PNG decoder. Until an owner asks — via
`SpawnOptions::graphics`, `AttachOptions::graphics`, or a non-zero
`set_graphics_storage_limit` — the limit stays zero, which is libghostty's own
"protocol disabled" (`PTY.IMG-R01`). Any requested limit is clamped to
`graphics::MAX_STORAGE_BYTES` and the effective value is readable through
`graphics_options()`, so a caller that asked for more can see what it got.
Zeroing the limit turns the protocol back off.

File, temporary-file, and shared-memory transmission media are left disabled,
so a child can only send bytes and never name a path (`PTY.IMG-R08`). PNG
(`f=100`) payloads are expanded to 8-bit RGBA before storage sees them, and
rejected above `MAX_DECODED_PNG_BYTES` (`PTY.IMG-R09`).

The same number bounds the state and the wire: there is no second, smaller
replay cap, so a retained image that `graphics_state` reports for a placement
is carried in full to a late client, whatever its size up to the bound
(`PTY.IMG-R02`). Reachability is scoped to placed images and to formats the
protocol can name — an image transmitted and never placed is not enumerated
(`PTY.IMG-C01`), and the grayscale pixel formats have no kitty `f=` value
(`PTY.IMG-C02`). The limit is a cap, not an allocation — a session whose
child never transmits holds nothing.

## Window read

`TerminalActor::graphics_state(scroll_offset)` (and
`TerminalHandle::graphics(scroll_offset)`) answers for the same window
`snapshot(scroll_offset)` reads (`PTY.IMG-R03`). Independent handle reads can
observe different actor updates: use an observed `TerminalHandle::frame()`
for the live viewport, or `request_frame(scroll_offset)` for history, to keep
cells, modes, placements and owned pixels from one consistent update.
A cursor-positioned placement is
located by its own screen-space rectangle, which answers for any window and
reports a negative origin row when its top has scrolled above the active area.
A virtual placement has no position of its own, so `scan_placeholders` decodes
the `U+10EEEE` placeholder cells of the window — one pass, never the
scrollback — and reports both the visible cell box and the image cell indices
covered; a bare continuation cell inherits row, column, and image-id high byte
from its predecessor.

Source rectangles are resolved on read and on replay: the protocol's `w=`/`h=`
default of "the whole dimension" is expanded and then clamped to the image, so
a crop is a concrete rectangle everywhere it is reported (`PTY.IMG-R04`).

The explicit actor and handle `image_bytes(id)` APIs copy pixels on request,
keyed by `ImageDesc::generation`. An immutable `Frame` captures pixels for
placed images alongside its grid and graphics state. Later frames reuse
the same `Arc<ImageBytes>` when the full image description, including its
generation, is unchanged. Held frames keep their exact pixels alive after
replacement (including same-size replacement) or deletion. A frame's
`image_bytes(id)` never fetches a newer actor generation.

## Replay wire form

`serialize::vt(term, scrollback, cell)` appends a graphics block after the
cursor move. The block is:

```text
first transmission frame   ESC _G a=t,q=2,i=<id>,f=<fmt>[,s=<w>,v=<h>][,o=z][,m=1];<base64 chunk> ESC \
continuation frame         ESC _G q=2,m=<1|0>;<base64 chunk> ESC \
virtual placement          ESC _G a=p,U=1,q=2<params> ESC \
cursor-positioned          ESC 7  ESC [<row>;<col>H  ESC _G a=p,q=2<params> ESC \  ESC 8

<params>                   ,i=<id>[,p=<pid>][,c=<cols>][,r=<rows>],x=,y=,w=,h=[,X=][,Y=][,z=]
```

`q=2` suppresses the terminal's replies on every command, so a replay is
silent. One `a=t` transmission per image, its base64 payload chunked at 4096
bytes: the first frame carries the full parameter list and gains `,m=1` only
when another chunk follows, and each continuation frame carries just `q=2`
and `m=`, ending with `m=0`. A payload that fits one chunk therefore has no
`m=` at all. `s=`/`v=` carry the pixel dimensions of a raw format and `o=z`
marks a zlib-deflated payload.

Placement commands carry no payload, so they have no `;` and no terminating
data: the parameter list runs straight into `ESC \`. `p=`, `c=`, `r=`, `X=`,
`Y=`, and `z=` are emitted only when nonzero, since zero is the protocol's
own default for each. The source rectangle is the exception and is always
emitted in full (`x=`, `y=`, `w=`, `h=`), because `w=`/`h=` default to "the
whole image", so omitting them would replay a cropped placement as the wrong
pixels at the right size (`PTY.IMG-R04`).

A virtual placement is emitted wherever its cells are, history included, since
its command carries no position. A cursor-positioned one needs a cell in the
active area, so it is bracketed by `ESC 7` / `ESC 8` around a `CUP` to that
cell: one whose top has scrolled partly above it is anchored at row 0 with its
crop advanced by the rows that are gone (`clip_top`, which also shrinks `r=`),
and one that has scrolled entirely above it is not re-emitted
(`PTY.IMG-C03`). For a terminal holding no images the block is empty, so a
session that never sent one serializes exactly the bytes it did before
(`PTY.IMG-R06`).

## Cell metric wire form

Cell pixel metrics come from the font on the client's host, which a session
daemon may never see, so they are declared rather than guessed
(`PTY.IMG-R05`). The size payload of ATTACH and RESIZE grows an optional
4-byte suffix:

```text
byte  0..1   rows        u16 big-endian
byte  2..3   cols        u16 big-endian
byte  4..5   cell_width  u16 big-endian   (optional)
byte  6..7   cell_height u16 big-endian   (optional)
```

`encode_attach_with_cell` and `encode_resize_with_cell` write the 8-byte form;
`encode_attach` and `encode_resize` write the plain 4-byte one. Every reader
of a size payload takes rows and cols from the first four bytes and the frame
carries its own length, so a daemon that predates the suffix — the Node one
included — reads the size it always read and ignores the rest. `decode_cell`
returns `None` for a payload shorter than 8 bytes or a degenerate zero.

The daemon adopts a declaration on either message (`adopt_cell_size` →
`TerminalActor::set_cell_size`); the newest declaration wins. Undeclared is
explicit, not silent: geometry falls back to `CellSize::FALLBACK` (8x16) and
`GraphicsState::cell_declared` is false. A declared cell changes only derived
geometry — a placement that named neither `c=` nor `r=` takes its cell extent
from the image's pixel size divided by the cell size — and never moves a
placement that declared its own extent.

## Lifecycle

The session daemon's own terminal has images on
(`lifecycle.rs`, `terminal_actor`, `GraphicsOptions::DEFAULT`): the session is
the durable owner of the child's screen, so it is the durable owner of the
child's images; a daemon without them would serve a replay whose placeholder
cells name images no client can have. When enabling fails the daemon warns and
serves text as before.

`TerminalActor::reset` re-enables images after `RIS` (`PTY.IMG-R07`).
libghostty's reset restores its own defaults, which include no storage and no
cell metrics, and `reset` is what precedes a replay — a terminal that lost its
storage there would reject the very images the replay is about to carry.

Storage is per screen, as in the protocol: the alternate screen holds its own
images, so a full-screen program's are not the primary screen's and neither
set is lost when the child switches. A replay taken from the alternate screen
brackets the normal half so that the normal screen's images land in the normal
screen's storage (`PTY.IMG-R07`).

Content change is signalled once, by a counter: `HandleEvent::Graphics(gen)`
and `graphics_generation()` key a texture cache (`PTY.IMG-R10`). Scrolling and
resizing move placements without bumping the generation, so a dirty frame
re-reads positions but does not re-upload pixels; a new transmission and a
child-sent delete both bump it.

## Validation

Test names are function names in the files given. `graphics.rs` is under
`crates/pty-terminal/tests/`, `handle.rs` under `crates/pty/tests/`,
`protocol.rs` under `crates/pty-core/tests/`.

| Requirement | Owning source | Executable evidence |
| --- | --- | --- |
| `PTY.IMG-R01` | `actor.rs`, `graphics.rs` | `graphics.rs::graphics_are_off_until_the_owner_asks`, `zeroing_the_limit_turns_the_protocol_off`, `the_storage_limit_alone_turns_graphics_fully_on`, `the_storage_limit_is_clamped_to_what_a_replay_can_carry`, `raising_the_storage_limit_keeps_the_cell_size_and_decodes_png` |
| `PTY.IMG-R02` | `graphics.rs` (`replay`, `transmit`), `serialize.rs` | `graphics.rs::a_late_client_reconstructs_the_image_from_the_replay_alone`, `an_image_far_larger_than_three_mib_still_replays`, `a_cursor_positioned_placement_replays_at_its_cell`, `handle.rs::a_late_attach_gets_the_image_the_child_drew_before_it_connected` |
| `PTY.IMG-R03` | `graphics.rs` (`read`, `screen_origin`, `scan_placeholders`) | `graphics.rs::an_omp_write_gives_image_bytes_placement_identity_crop_and_position`, `scrolling_moves_the_placement_and_scrollback_still_finds_it`, `a_resize_keeps_the_image_and_reprojects_it`, `a_virtual_placement_scrolled_into_history_still_replays`, `a_partially_scrolled_direct_placement_replays_clipped`, `a_palette_foreground_names_a_placeholder_image`, `a_bare_continuation_cell_inherits_row_column_and_id_high_byte` |
| `PTY.IMG-R04` | `graphics.rs` (`placement_params`, `clip_top`) | `graphics.rs::a_source_rect_and_offsets_come_back_resolved`, `a_cropped_placement_keeps_its_crop_through_a_replay` |
| `PTY.IMG-R05` | `protocol.rs` (`decode_cell`), `clients.rs`, `graphics.rs` (`CellSize`) | `protocol.rs::attach_can_declare_a_cell_size_without_changing_the_size_it_carries`, `resize_can_declare_a_cell_size`, `a_plain_size_payload_declares_no_cell`, `graphics.rs::an_implicit_placement_takes_its_extent_from_the_declared_cell`, `a_declared_cell_does_not_move_an_explicit_placement`, `handle.rs::a_client_declares_its_cell_size_and_the_session_geometry_follows` |
| `PTY.IMG-R06` | `graphics.rs` (`replay`), `serialize.rs` (`vt`) | `graphics.rs::a_replay_without_graphics_is_unchanged` |
| `PTY.IMG-R07` | `actor.rs` (`reset`), `lifecycle.rs` | `graphics.rs::the_alternate_screen_has_its_own_storage`, `a_replay_from_the_alt_screen_puts_the_normal_screens_images_on_the_normal_screen`, `handle.rs::a_late_attach_gets_the_image_the_child_drew_before_it_connected` (reconnect half) |
| `PTY.IMG-R08` | `actor.rs` (`enable_graphics`), `graphics.rs` (`GraphicsOptions`) | None. Enforced by leaving the file, temporary-file, and shared-memory media disabled; backed by [decision 0012](../../decisions/0012-kitty-graphics-replay.md) only. A regression here would not fail the suite. |
| `PTY.IMG-R09` | `graphics.rs` (`PngDecoder`, `expand`, `MAX_DECODED_PNG_BYTES`) | `graphics.rs::a_grayscale_png_is_stored_as_rgba`, `raising_the_storage_limit_keeps_the_cell_size_and_decodes_png`, `handle.rs::a_late_attach_gets_the_image_the_child_drew_before_it_connected` |
| `PTY.IMG-R10` | `graphics.rs` (`generation`), `handle.rs` (`HandleEvent::Graphics`) | `handle.rs::a_spawned_child_that_draws_an_image_is_queryable_through_the_handle`, `graphics.rs::a_delete_from_the_child_drops_the_placement_and_the_bytes`, `scrolling_moves_the_placement_and_scrollback_still_finds_it`, `a_resize_keeps_the_image_and_reprojects_it` |

`crates/pty/tests/handle_frame.rs` additionally covers immutable embedding
frames: `held_image_generations_survive_replacement_and_deletion_and_reuse_unchanged_pixels`
checks retained and reused pixel generations, including same-size replacement;
`explicit_history_frame_aligns_grid_graphics_modes_and_image_generations`
checks consistent history windows; and
`observed_frame_advances_while_output_is_backlogged_before_the_tail` checks
publication progress while the actor input remains backlogged.

Run the whole map with `cargo test -p pty-terminal -p pty-core` and
`cargo test -p pty --test handle`; the image
cases alone with `cargo test -p pty-terminal --test graphics`. No gated
`_node` / `_rust` conformance pair exists or can: the Node side has no image
state to compare against (`PTY.IMG-C05`).
