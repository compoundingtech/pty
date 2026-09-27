# Session trailers — Specification

This document specifies the banner and trailers that `pty attach` and `pty peek -f` print, and how the client obtains the session info they show. It builds on [requirements.md](./requirements.md).

## Status

Active. Implemented in pty-rust and in the Node pty; the bytes below are the contract both emit.

## Scope

Defined here: the banner, the trailer for each end event, the summary line, the hint rules, where the session info comes from, and machine-mode behavior.

Not defined here: the terminal reset bytes (`TERMINAL_SANITIZE`, `CURSOR_TO_BOTTOM` in `crates/pty-client/src/sanitize.rs`), the detach key and its double-tap window, the reconnect backoff, and the texts excluded by `PTY.TRL-A02`.

## Shape

```
SAN = TERMINAL_SANITIZE + CURSOR_TO_BOTTOM

SAN "\r\n" <header> "\r\n"
    [ "  " <summary line> "\r\n" ]      PTY.TRL-R02
    [ "  " <hint> "\r\n" ]              PTY.TRL-R04
```

Example:

```text
[detached from web-3f2a]
  My Web Server (web-3f2a) #role=web — ~/proj — node app.js
  reattach: pty attach web-3f2a
```

## Events

`<id>` is the session's stable id. `<attach>` is `pty attach <id>` for a local session and `pty attach --remote <peer> <id>` for a remote one.

| Event | Client | Header (`PTY.TRL-R01`) | Hint (`PTY.TRL-R04`) |
| --- | --- | --- | --- |
| Ctrl+\ | attach | `[detached from <id>]` | `reattach: <attach>` |
| Ctrl+\ | peek -f | `[detached from <id>]` | `reattach: pty peek -f [--remote <peer>] <id>` |
| EXIT packet | attach, peek -f | `[<id> exited with code <N> after <age>]` | `restart: pty attach <id>` — local only, and only when the entry outlives the exit |
| Clean close without EXIT | attach, peek -f | `[<id> session ended]` | none |
| Remote route refused | attach | `[<id> session ended]` | none |
| Remote reconnect budget exhausted | attach | `[connection lost to <id>]` | `reconnect: pty attach --remote <peer> <id>` |

- `<age>` is `format_duration(now − createdAt)` (`2h14m`, `5s`). When `createdAt` is unknown or does not parse, ` after <age>` is omitted.
- "Outlives the exit" is `!should_reap_at_exit(tags, ephemeral, reap_on_exit_default())`: a `keep` tag, `strategy=permanent`, or `PTY_REAP_ON_EXIT` turned off (`PTY.TRL-C02`).
- `peek -f --plain` strips ANSI from every trailer line. It omits SAN on exit and on clean close, and keeps SAN on detach.
- A remote route refused after a malformed frame still reports `session ended`: the refusal says the session is gone.

## Summary line

The summary line is the `pty list` line with no status part (`PTY.TRL-R10`):

```
<prefix><label><marker><tags><status> — <short cwd> — \x1b[2m<command>\x1b[0m

label   \x1b[1m<display name>\x1b[0m \x1b[2m(<id>)\x1b[0m   |   \x1b[1m<id>\x1b[0m
marker  " \x1b[31m[flapping]\x1b[0m" | " \x1b[33m[permanent]\x1b[0m" | ""
tags    " #k=v …" for keys that are not reserved
```

A trailer uses prefix `"  "`, bold `\x1b[1m`, and an empty status. `pty list` uses other prefixes, colors, and status parts (`(pid: N)`, `(exited with code N, 5s ago)`, `(vanished, started 1m ago)`, `● `/`○ ` for remote hosts). An empty cwd leaves its slot empty (`—  —`).

## Banner

Written to stderr when an interactive attach starts, local or remote, unless the client is in machine mode:

```
[attached to <display name> (<id>) — press Ctrl+\ to detach]
[attached to <id> — press Ctrl+\ to detach]
```

## Session info

```
local   attach start ──► read_metadata(id) ──► snapshot
        trailer      ──► read_metadata(id) ──┬─► found:   fresh summary      PTY.TRL-R05
                                             └─► missing: snapshot
remote  dial ──► list over the control path ──► row (or none) ──► route ──► client loop
```

- A `SummaryProvider` (`FnMut() -> Option<SessionSummary>`) is called when the trailer is written. `local_summary_provider` implements the local flow. `fixed_summary_provider` holds a remote row.
- The remote list request runs before the route (`dial_route_and_describe`), so no request waits while the routed session socket is live. A failed list yields no row, and the trailer shows no summary line (`PTY.TRL-R02`).

## Ordering and suppression

1. The client stops reading input and restores the tty before it writes a trailer (`PTY.TRL-R06`). In pty-rust `trailer()` calls `clean_exit()` first.
2. After the client itself drops a connection for a malformed frame, it prints `pty client: dropping connection — …` on stderr and no `session ended` trailer (`PTY.TRL-R08`). A successful reconnect clears that state.
3. Machine mode (`PTY.TRL-R07`): detach and EXIT print nothing extra. The remote status lines print only their header plus `\n` on stderr (`[<id> session ended]`, `[connection lost to <id>]`). A close without EXIT still reports `machine stream truncated before EXIT: connection closed`.

## Module map

| Concern | Source |
| --- | --- |
| Summary, line, trailer, banner | `crates/pty-client/src/summary.rs` — `SessionSummary`, `LineStyle`, `SessionEnd`, `TrailerTarget`, `render_trailer`, `trailer_header`, `attach_banner`, `local_summary_provider`, `fixed_summary_provider` |
| Attach events | `crates/pty-client/src/attach.rs` — `Attach::trailer`, `finish_detach`, `handle_packets`, `on_disconnect`, `try_reconnect` |
| peek -f events | `crates/pty-client/src/peek.rs` — `follow`, `trailer` |
| Remote row | `crates/pty-client/src/remote.rs` — `dial_route_and_describe` |
| CLI wiring | `crates/pty/src/cli/attach.rs` (`do_attach`, `attach_remote`), `crates/pty/src/cli/peek.rs` |
| `pty list` | `crates/pty/src/cli/list.rs` — `cmd_list`, `line_style` |
| Node | `src/client.ts` (`trailer`, `exitHeader`), `src/session-presentation.ts` |

## Traceability

| Requirement | Tests |
| --- | --- |
| `PTY.TRL-R01` | `client_attach.rs::single_detach_key_detaches_and_prints_the_detached_line`, `close_without_exit_says_the_session_ended_and_exits_0`, `reconnect_refusal_prints_session_ended_and_exits_0`; `client_attach_stream.rs::reconnect_gives_up_after_the_attempt_cap`; `client_peek.rs::follow_detaches_on_ctrl_backslash`; `cli_e2e.rs::attach_is_interactive_and_detaches` |
| `PTY.TRL-R02` | `summary.rs::detach_trailer_names_the_session_and_how_to_return`, `remote_hints_name_the_peer_and_survive_a_missing_row` |
| `PTY.TRL-R03` | `summary.rs::banner_uses_the_plain_label` |
| `PTY.TRL-R04` | `summary.rs::restart_hint_only_when_the_entry_outlives_the_exit`, `remote_hints_name_the_peer_and_survive_a_missing_row`; conformance `sanitize.rs::attach_emits_sanitize_then_exit_trailer` |
| `PTY.TRL-R05` | `client_attach.rs::detach_trailer_shows_the_session_summary_at_detach_time` |
| `PTY.TRL-R06` | conformance `sanitize.rs::attach_emits_sanitize_then_exit_trailer`, `detach_emits_sanitize_then_detached_trailer` |
| `PTY.TRL-R07` | `client_attach_stream.rs` (the whole file pins the machine-mode stderr) |
| `PTY.TRL-R08` | `client_attach.rs::oversize_packet_prints_the_dropping_line` |
| `PTY.TRL-R10` | `cli_list.rs::text_layout_is_byte_exact` |
| `PTY.TRL-R11` | conformance `sanitize.rs`, `attach_no_restart.rs`, `remote_reconnect.rs`, and `attach_stream.rs`, run against both binaries |
