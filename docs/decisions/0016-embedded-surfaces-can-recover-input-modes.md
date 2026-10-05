# 0016 — Embedded surfaces can recover input modes

**Status:** accepted

**Node behavior.** Unknown frame tags are ignored. A client can detach or send
child input, but cannot reset the daemon's terminal modes without the child
printing the escape sequences itself. The screen-size query CSI 19 t and
modifyOtherKeys state query CSI ? 4 m have no daemon-owned reply.

**Rust behavior.** Empty frame tag 11 (`ResetInputModes`) is accepted only from
a writable attached client. The actor cancels unfinished control strings,
returns to the normal screen, clears both kitty keyboard stacks and restores
mouse, focus, paste, keypad, cursor, synchronized-output and resize-reporting
modes. Normal text and scrollback survive. The daemon broadcasts the reset as
output; it never writes it to the child's PTY. Readonly and command connections,
and nonempty reset frames, cannot change state. Older daemons ignore tag 11.
`SessionConnection::reset_input_modes` exposes the operation to embedders.

The actor answers CSI 19 t with the virtual surface's cell dimensions and
CSI ? 4 m with its current modifyOtherKeys level. Both queries are stripped
from client DATA, just like the existing size and keyboard queries. Kitty
pop counts now remove the requested number of tracked stack entries.

Mouse button and wheel events in all-motion tracking use the same codes as
normal tracking; only motion events carry the motion bit. This corrects the
underlying encoder's treatment of all events as motion in mode 1003.

**Why.** A real embedded terminal probe reproduced lost input after programs
left mouse and enhanced keyboard modes enabled. Client-only resets were undone
by the daemon's next SCREEN replay. Child input cannot express the operation:
its escape bytes would be read as shell input. A small authorized output-side
operation fixes the durable owner of the modes and every attached surface.

**Client effect.** An embedded surface can provide a recovery chord while
retaining the session's history. A client talking to an older daemon can reset
its local parser, but cannot promise recovery after reconnect. Screen and
keyboard state queries receive one answer from the daemon rather than being
forwarded to an outer terminal with different geometry or modes.

**Tests.** `crates/pty/tests/daemon_input_recovery.rs` uses real child and client
PTYs to check readonly refusal, the output-only reset, retained history and
late-attach state. `crates/pty-terminal/tests/queries.rs` checks recovery of
both keyboard stacks and the two query replies; `tests/input.rs` checks press,
wheel and hover bytes under mode 1003. The new frame is a deliberate extension,
not a changed Node operation, so existing conformance assertions stay intact.
