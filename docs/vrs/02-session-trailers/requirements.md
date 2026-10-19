# Session trailers — Requirements

## Context

This node defines what an interactive client says when it stops showing a session. Covered clients are `pty attach` and `pty peek -f`. The messages it defines are the attach banner and the trailer that follows a detach, an exit, or a lost session. The Node pty prints the same bytes. The conformance suite ([docs/conformance.md](../../conformance.md)) holds both implementations to them.

## Assumptions

- **PTY.TRL-A01 Interactive reader:** A trailer is read by a person at a terminal, who has just lost sight of the session and needs to know which session it was and how to get back.
- **PTY.TRL-A02 Parsed CLI texts:** Relays and scripts parse the dead-session prompt of `pty attach` (`Session "<id>" exited with code N.`, `Command was: …`) and the `pty kill` report. Those texts are not trailers.
- **PTY.TRL-A03 Machine consumers:** An `--attach-stream-fd-v1` consumer reads framed packets on its descriptor and treats stderr as diagnostics.

## Constraints

- **PTY.TRL-C01 Remote rows:** A remote host's session row carries id, display name, cwd, command, and tags. It carries no creation time and no pid.
- **PTY.TRL-C02 Reap at exit:** By default a session's registry entry is removed when the session exits, so an exited session is usually unknown to `pty attach` afterwards.

## Acceptable Tradeoffs

- **PTY.TRL-T01 One extra remote request:** A remote attach or `peek -f` asks the peer for its session list once per dial to learn the session's row.
- **PTY.TRL-T02 Longer detach output:** A detach prints up to three lines instead of one.

## Requirements

### Must say which session and what happened

- **PTY.TRL-R01 Named event:** Every end of an interactive attach or `peek -f` must print a header that names the event and the session id. Events: detach, exit, daemon close without an exit, remote route refused, remote reconnect exhausted.
- **PTY.TRL-R02 Session line:** When the client knows the session's info, the header must be followed by the session's `pty list` line. When the client does not know it, the line is omitted, never guessed.
- **PTY.TRL-R03 Named attach:** The attach banner must name the session by its display label (`<display name> (<id>)`, or `<id>` alone).

### Must show a way back that works

- **PTY.TRL-R04 Working hint:** A hint must name a command that works for that session at that moment. Detach and remote connection loss always get one. An exit gets a restart hint only when the session's registry entry outlives the exit. A remote hint names the peer.

### Must show current information

- **PTY.TRL-R05 Fresh local info:** For a local session, the trailer must reflect metadata changes made during the attach, such as a `pty rename`. When the exit has already removed the entry, the trailer uses what was known at the start.

### Must not disturb other consumers

- **PTY.TRL-R06 Terminal first:** The terminal must be restored (cooked mode, terminal reset sequence) before a trailer is written.
- **PTY.TRL-R07 Machine mode unchanged:** In `--attach-stream-fd-v1` mode a detach or exit adds no output. Remote status lines stay header-only on stderr.
- **PTY.TRL-R08 Client drops are not session ends:** A connection that the client itself drops, because it received a malformed frame, must not be announced as a session end.
- **PTY.TRL-R09 Parsed texts unchanged:** The texts named in `PTY.TRL-A02` are outside this node and keep their shape.

### Must be one rendering

- **PTY.TRL-R10 Shared line:** `pty list` and the trailers must render a session's line with the same code, and `pty list` output must stay byte-identical.
- **PTY.TRL-R11 Cross-runtime parity:** The Node pty and pty-rust must emit byte-identical banners, successful detach trailers, and received-EXIT trailers. Transport failures are excluded: following PTY.TRL-R08, an attachment loss is not proof that the session ended. Decision 2026-10-08: narrow parity to preserve truthful transport-failure reporting.
