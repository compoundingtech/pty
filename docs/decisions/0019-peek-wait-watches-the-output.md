# 0019 — `peek --wait` watches the output, not only the screen

**Status:** accepted

**Node behavior.** `cmdPeekWait` (`src/cli.ts`) reads the plain visible screen
every 200 ms and stops when a pattern is a substring of it. A pattern that is
printed and then scrolled off the screen between two reads is never seen. The
wait runs on to its timeout, or, with no timeout, until the session exits and
the saved `lastLines` happen to include the pattern. A timeout reads the same
whether the pattern was missed or never printed.

**Rust behavior.** The 200 ms screen read stays. Alongside it, `peek --wait`
holds one read-only peek connection for the whole wait. The daemon sends that
connection the screen and then every byte of output after it. The wait
matches the pattern against that screen, and against the output's visible
text as it arrives:
- Escape sequences (CSI, OSC, DCS and the like) are dropped, also when a chunk
  boundary splits one.
- A pattern split across two chunks still matches, and so does one split by a
  style change.
- Text either side of anything that can move the cursor is not joined: every
  CSI but SGR, and the single-byte escapes such as `ESC 8`. `END \e[2;1H004`
  never formed "END 004" on the terminal, so it does not match. Text drawn
  that way is left to the screen read.
- After its look is due, the watch reads at most one more chunk. A session
  that prints without pause cannot hold the wait past its screen read or its
  timeout.

On a timeout the wait reads the full scrollback once. If the pattern is there,
it was printed before the wait began, and the error says so on a second line
after Node's text:

```
Timed out after 1s waiting for "END 004".
It is in the scrollback from before the wait began, no longer on screen.
```

**Why.** A marker printed while someone is waiting for it should end the
wait. Losing it to the sampling interval made a finite wait end only at its
bound, or at the session's exit, which reads as a hung command. Telling a
missed marker from an absent one tells the caller whether to look earlier in
the output or to keep waiting.

**Client effect.**
- A wait returns as soon as the pattern is printed, even when the same write
  scrolls it away. What it prints is the screen at that moment, which may no
  longer show the pattern.
- While the wait runs, the session counts one more read-only client, as it
  does during `peek -f`.
- A wait that matches what is already on screen opens no more connections
  than before: the watch's own screen is the first look.
- Exit codes and the first line of every error are Node's.

**Test.**
- `crates/pty/tests/cli_peek_wait.rs` runs against a real session: a marker
  printed and scrolled away by one write while the wait runs, and a marker
  from before the wait next to a pattern that never appeared.
- `crates/pty-client/tests/client_peek.rs` covers the same two cases against a
  fake daemon, with the marker split across output chunks and styles. It also
  checks that a 1 s wait on output that never pauses still times out in about
  1 s.
- `crates/pty-client/src/peek.rs` holds the unit tests:
  - the visible-text scanner, including that text either side of a cursor
    move is not joined;
  - that a watch past its deadline reads one chunk of a full socket and
    returns.

All four behavior tests fail on the screen-only wait. There is no gated Node
half: the Node pty is not installed where these ran, and it is deprecated.

**Migration / negotiation.** None.
