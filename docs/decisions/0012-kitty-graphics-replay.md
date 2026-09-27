# 0012 — kitty graphics are terminal state, and a replay carries them

**Status:** accepted

**Node behavior.** The Node daemon has no image state. `xterm-headless` has no
kitty graphics handler, so a `ESC _G ... ESC \` transmission is parsed and
discarded; the placeholder cells of a virtual placement survive only because
they are ordinary text with a foreground colour. A client that was connected
when the child wrote the image saw the raw bytes in `DATA` and its own terminal
kept them; a client that attaches afterwards gets a `SCREEN`
(`src/server.ts:962`, `@xterm/addon-serialize`) with the placeholder cells and
no image and no placement. Nothing in the Node daemon can answer "which images
does this session hold, and where are they".

**Rust behavior.** Graphics are terminal state, off by default and bounded when
on:

- `TerminalActor::enable_graphics` (`SpawnOptions::graphics` /
  `AttachOptions::graphics`) sets a byte limit on libghostty's image storage
  and installs a PNG decoder for `f=100`. Without it the storage limit stays
  zero, which is libghostty's own "protocol disabled": a child cannot make a
  terminal hold images its owner never asked for. The file, temporary-file,
  and shared-memory transmission media stay disabled, so a child cannot name a
  path the owner did not authorize; inline transmission only.
- One number bounds the state and the wire: `graphics::MAX_STORAGE_BYTES`
  (32 MiB), which `enable_graphics` and `set_graphics_storage_limit` clamp to
  and which a replay carries in full. A smaller replay cap would have made a
  supported state — an image the terminal accepted and reports — one that a
  late client can never be given, which is the exact failure this record is
  about. A caller asking for more gets the bound and can see it in
  `graphics_options()`.
- Cell pixel metrics come from whoever draws the cells and travel on the wire.
  A placement that named neither `c=` nor `r=` gets its cell extent from the
  image's pixel size divided by the cell size, so that size has to be the
  client's real one: it comes from a font on the client's host, which a
  session daemon may never see. `AttachOptions::graphics`'s cell size is
  appended to ATTACH, `TerminalHandle::set_cell_size` sends it on RESIZE
  (`encode_attach_with_cell` / `encode_resize_with_cell`: four optional bytes
  after the existing rows/cols, so every older reader — the Node daemon
  included — takes the size it always took and ignores the rest), and the
  daemon adopts it (`clients.rs`, `adopt_cell_size`). Undeclared is explicit,
  not silent: geometry uses `CellSize::FALLBACK` (8x16) and
  `GraphicsState::cell_declared` is false.
- `TerminalActor::graphics_state(scroll_offset)` /
  `TerminalHandle::graphics(scroll_offset)` answer with owned values
  (`GraphicsState`, `ImageDesc`, `Placement`, `SourceRect`,
  `PlacementPosition`) for the same window `snapshot(scroll_offset)` reads, so
  a grid and a graphics state taken with the same offset line up cell for cell.
  `image_bytes(id)` copies the pixels once, on request, keyed by
  `ImageDesc::generation`.
- `serialize::vt` appends a graphics block after the cursor move: one
  `a=t` transmission per image (chunked at 4096 base64 bytes, `s=`/`v=` for raw
  formats, `o=z` for a compressed one), then `a=p,U=1` per virtual placement
  and `DECSC` + `CUP` + `a=p` + `DECRC` per cursor-positioned one. The source
  rectangle is always emitted in full (`x=`,`y=`,`w=`,`h=`): `w=`/`h=` default
  to "the whole image", so omitting them replays a cropped placement as the
  wrong pixels at the right size. A virtual placement is emitted wherever its
  cells are, history included, because its command carries no position at
  all; a cursor-positioned one needs a cell in the active area, and one whose
  top has scrolled above it is anchored at row 0 with its crop advanced by
  the rows that are gone. It is empty
  for a terminal with no graphics, so a session that never sent an image
  serializes exactly as it did before
  (`crates/pty-terminal/tests/graphics.rs::a_replay_without_graphics_is_unchanged`).
- The daemon's own terminal has graphics on
  (`crates/pty/src/daemon/lifecycle.rs`, `terminal_actor`). The session is the
  durable owner of the child's screen, so it is the durable owner of the
  child's images; a daemon without them would serve a `SCREEN` whose
  placeholder cells name images no client can have. The limit is a cap, not an
  allocation: a session whose child never transmits holds nothing.

**Why.** Two other shapes were available and are worse. Passing the child's
graphics bytes through to an outer terminal cannot work for an embedder that
draws a sub-rectangle: the child's coordinates are its own, and the embedder
clips, pans, and draws chrome around it. Keeping the state in each client
cannot work either: a client that attaches later never sees the `DATA` that
carried the image, so the state has to be reconstructible from the replay the
daemon already sends. Re-emitting the storage in the protocol the child used is
the smallest thing that makes a late client and a live client hold the same
images.

Two deviations from libghostty's own API are deliberate:

- Positions are resolved against the window that was asked for, not the live
  viewport. A cursor-positioned placement's own rectangle
  (`PlacementIteration::rect` + `point_from_grid_ref(PointSpace::Screen)`)
  answers for any window and reports a negative row for a placement whose top
  has scrolled above it; `viewport_pos` answers only for the live viewport and
  reports nothing at all for a placement above it, which would make a
  scrolled-back reader lose exactly the images it wants.
- libghostty reports no viewport position for a virtual placement — correctly,
  because a virtual placement has none: it is wherever its placeholder cells
  are. Those cells each name their own image row and column, so
  `graphics::scan_placeholders` decodes them from the grid (one pass over the
  window, never the scrollback) and reports both the visible cell box and the
  image cell indices it covers. That is what makes a partially scrolled image
  answerable: a two-row image scrolled by one row reports one visible row,
  `cell_row = 1`, and `origin_row = -1`.
- `TerminalActor::reset` re-enables graphics after `RIS`. libghostty's reset
  restores its defaults, which include no storage and no cell metrics; since
  `reset` is what precedes a `SCREEN` replay, a terminal that lost its storage
  there would reject the very images the replay carries.

**Client effect.** A consumer that opts in can ask, at any time and from any
client, for the image bytes, the placement identity (`image_id` +
`placement_id`), the resolved source crop, the rendered pixel and cell size,
and the placement's position in a chosen window; `HandleEvent::Graphics(gen)`
says when the storage content changed, and `graphics_generation()` keys a
texture cache. Scrolling and resizing move placements without changing the
generation, so a dirty frame still re-reads positions. The alternate screen has
its own storage, as in the protocol: a full-screen program's images are not the
primary screen's, and neither set is lost when the child switches.

Residual differences a consumer can observe:

1. `GraphicsState::images` lists the images that have at least one placement.
   libghostty exposes lookup by id, not enumeration, so an image transmitted
   and never placed is not listed (it is also not drawable).
2. Grayscale images (`Gray`, `GrayAlpha`) have no kitty `f=` value and are not
   re-emitted into a replay. Every PNG this terminal accepts is expanded to
   8-bit RGBA before libghostty stores it (a grayscale PNG decodes as
   `Grayscale`/`GrayscaleAlpha`, which `PngDecoder` widens itself — rejecting
   it instead would drop every monochrome plot a child sends), so the
   grayscale variants are unreachable in practice and kept as a typed dead end
   rather than a silent conversion.
3. Cell metrics are the newest declaration, not a negotiation. Two clients
   whose fonts differ cannot both be right about a placement that left its
   size implicit, and unlike rows and cols there is nothing to reconcile: the
   metrics change no bytes and no client's screen, only the derived geometry
   this session reports. A client that draws pixels should declare its own and
   read `GraphicsState::cell_declared` rather than trust the fallback.
4. A cursor-positioned placement that has scrolled *entirely* above the
   active area is not re-emitted: there is no cell left to put the cursor
   on. One that is partly on screen is re-emitted clipped, so what a late
   client gets is what the source terminal is showing. Virtual placements
   have no such limit at all, which is one more reason they are the preferred
   form.
5. A replay of the alternate screen brackets the normal half in
   `ESC[?1049l` / `ESC[?1049h` + `ESC[H`. Node's payload writes its normal
   half after `ESC[?1049h`, so a Node client's normal buffer takes it while
   already switched; that is invisible for text but would put the normal
   screen's images in the client's alternate storage, since kitty storage is
   per screen.
6. `SIXEL` and the iTerm2 protocol are not read at all. Neither offers a
   placement contract an embedder can reproject into a sub-rectangle.

**Test.** `crates/pty-terminal/tests/graphics.rs` — twenty-six cases driven with
the exact bytes OMP writes (`packages/tui/src/terminal-capabilities.ts`
`encodeKittyTransmit`, `packages/tui/src/kitty-graphics.ts`
`encodeKittyVirtualPlacement` / `encodeKittyPlaceholderGrid`): opt-in, live
write, source crop, scroll, scrollback window, resize, alternate screen,
child-sent delete of a placement and of an image, `clear_graphics`, zeroed
limit, the replay cases
(`a_late_client_reconstructs_the_image_from_the_replay_alone`,
`a_cursor_positioned_placement_replays_at_its_cell`), the handle path
(`a_spawned_child_that_draws_an_image_is_queryable_through_the_handle`, which
also pins `HandleEvent::Graphics`), the bound
(`an_image_far_larger_than_three_mib_still_replays` — a 4 MiB image
transmitted in 4096-byte chunks, stored and then recovered byte for byte from
the replay alone — and
`the_storage_limit_is_clamped_to_what_a_replay_can_carry`), and the cell
metrics (`an_implicit_placement_takes_its_extent_from_the_declared_cell`,
`a_declared_cell_does_not_move_an_explicit_placement`).

Replay fidelity has a case per failure it can have, each of which fails on the
shape that preceded it: `a_cropped_placement_keeps_its_crop_through_a_replay`
(the crop survives, not the whole image measured from its origin),
`a_virtual_placement_scrolled_into_history_still_replays`,
`a_partially_scrolled_direct_placement_replays_clipped`,
`a_grayscale_png_is_stored_as_rgba`,
`a_palette_foreground_names_a_placeholder_image`,
`a_bare_continuation_cell_inherits_row_column_and_id_high_byte`,
`raising_the_storage_limit_keeps_the_cell_size_and_decodes_png`,
`the_storage_limit_alone_turns_graphics_fully_on`, and
`a_replay_from_the_alt_screen_puts_the_normal_screens_images_on_the_normal_screen`
(the normal screen's image is there after the full-screen program exits, and
the alternate screen replays character for character). Unit tests for the
base64 encoder and the diacritic table live in `graphics.rs`; the wire form is
pinned in `crates/pty-core/tests/protocol.rs`
(`attach_can_declare_a_cell_size_without_changing_the_size_it_carries`,
`resize_can_declare_a_cell_size`, `a_plain_size_payload_declares_no_cell`).

End to end, against a real session daemon:
`crates/pty/tests/handle.rs::a_late_attach_gets_the_image_the_child_drew_before_it_connected`
— a child transmits a PNG, places it virtually, and writes a placeholder cell;
a `TerminalHandle` attaches only afterwards, so it never sees that `DATA`, and
still reads the decoded pixels (`[255, 0, 0, 255]`), the placement identity
(image 4242, placement 7), and its cell from the `SCREEN` replay alone. The
same test then reconnects and asserts the image and its cell come back, which
is what the graphics-preserving `reset` is for. Its sibling
`a_client_declares_its_cell_size_and_the_session_geometry_follows` attaches
with a declared 16x16 cell to a session whose child placed a 16x16 image with
no `c=`/`r=`, asserts the extent is 1x1 cell, then declares 8x16 and asserts
it becomes 2x1.

No gated `_node` / `_rust` conformance pair exists, and none can: the Node side
has no graphics state to compare against. The difference is not "Rust renders
this differently" but "Rust has state Node does not", so the record stands on
the Rust tests plus the Node source above.

**Migration / negotiation.** None. A client's graphics are off unless it asks
(`SpawnOptions::graphics` / `AttachOptions::graphics`); the daemon's are on,
and a session whose child never transmits an image serves the same `SCREEN`
bytes it did before. A client that does not opt in is unaffected by a session
that holds images: the extra `ESC _G ... ESC \` in its replay is an APC string,
which a terminal that does not implement the protocol ignores. A Node daemon
feeding a Rust client still works — the client then has only what the Node
`SCREEN` carries (placeholder cells, no images), which is exactly what a Node
client would have. Full replay fidelity needs the daemon side to be the Rust
one.
