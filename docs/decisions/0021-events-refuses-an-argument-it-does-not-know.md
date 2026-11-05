# 0021 — `pty events` refuses an argument it does not know

**Status:** accepted

**Node behavior.** `pty events` (`src/cli.ts`, the `events` case) reads
options only before the session name, and stops at the first token it does
not know. That token becomes the name, and every argument after the name is
ignored. So `pty events NAME --wait T -t 60` follows `NAME` with no wait and
no deadline, and runs until it is killed.

**Rust before.** Since ad48f88, options may also follow the name, but the
first token the parser did not know still ended parsing, and everything after
it was ignored. `--timeout=60`, `-t=60` and `--wait=T` are not spelled the way
the parser reads them, so each one dropped the deadline, and `--wait=T` the
wait as well. The command then ran forever with no error (#218).

Measured with no terminal, against the pinned Node reference on Node
22.23.2, main `8dd3fd42`, and this change. `q` is a running session, and each
run was killed at 4 s:

| command | Node | Rust before | Rust now |
| --- | --- | --- | --- |
| `pty events --wait user.x -t 2 q` | exit 1 at 2 s, timed out | exit 1 at 2 s, timed out | exit 1 at 2 s, timed out |
| `pty events q --wait user.x -t 2` | ran on | exit 1 at 2 s, timed out | exit 1 at 2 s, timed out |
| `pty events q --wait user.x --timeout=2` | ran on | ran on | exit 1 at once: `Unexpected argument: --timeout=2` |
| `pty events q --wait user.x -t=2` | ran on | ran on | exit 1 at once: `Unexpected argument: -t=2` |
| `pty events q --wait=user.x -t 2` | ran on | ran on | exit 1 at once: `Unexpected argument: --wait=user.x` |
| `pty events q --wait user.x --bogus -t 2` | ran on | ran on | exit 1 at once: `Unexpected argument: --bogus` |
| `pty events --wait user.x -t 2 q other` | exit 1 at 2 s, timed out | exit 1 at 2 s, timed out | exit 1 at once: `Unexpected argument: other` |
| `pty events --timeout=2 q` | exit 1: `Session "--timeout=2" not found.` | the same | exit 1 at once: `Unexpected argument: --timeout=2` |

**Rust behavior.** After the name, an argument that is not one of
`pty events`' options, or a second name, is an error: `Unexpected argument:
<token>`, exit 1, before anything is followed. That is `pty send`'s message
for the same mistake. When an unknown dash token comes before the name, the
error names that token. A lone unknown dash token is still taken as the name,
as in Node, so `pty events --bogus` still says `Session "--bogus" not found.`

**Why.** A wait whose deadline was dropped never ends, and #218 reported waits
that ran well past `-t 7200`. A caller cannot tell a dropped option from a
slow event, so the mistake has to surface at once. An error costs a script one fix. A silently ignored option
costs a process that never exits.

**Client effect.**
- A script that passed a stray argument to `pty events` now exits 1 where it
  used to run with the argument ignored, or with every option after it
  ignored.
- Spell the options `-t <sec>` or `--timeout <sec>`, and `--wait <type>`, as
  separate arguments. `--timeout=60`, `-t=60` and `--wait=<type>` are errors.
- At most one session name.

**Test.** `crates/pty/tests/cli_events.rs`,
`an_unexpected_argument_is_refused_rather_than_dropping_the_options_after_it`,
checks seven spellings, each exiting 1 at once with the exact message. No
conformance test passes `pty events` an argument it does not know, so the
divergence ledger needs no entry.

**Related.** The same change (#219) ends a wait or a named follow when the
session's daemon has recorded its pid and then dies without recording an
exit. Before, it waited forever, as Node's does. That extends ad48f88 and #197,
which end a wait when the session's exit is recorded or its entry is removed.

**Migration / negotiation.** None on the wire. Release note: `pty events` now
rejects an argument it does not recognise (exit 1), instead of ignoring it and
every option after it. Pass `-t <sec>`, not `-t=60` or `--timeout=60`, and
`--wait <type>`.
