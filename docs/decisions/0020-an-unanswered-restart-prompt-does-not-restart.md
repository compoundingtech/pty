# 0020 — An unanswered restart prompt does not restart

**Status:** accepted

**Node behavior.** `handleDeadSession` (`src/cli.ts`) asks `Restart? [Y/n] `
through `readline/promises`, with no terminal check. Any line but a lone `n`
restarts the session, an empty line included. When stdin reaches EOF first,
the question never resolves: the process exits 0 after the prompt, and
nothing restarts.

Measured with no terminal, against the pinned Node reference on Node 22.23.2
and 24.18.0, and against Rust before this change:

| stdin | Node | Rust before |
| --- | --- | --- |
| `</dev/null` | exit 0, not restarted | restarted |
| an empty line | restarted | restarted |
| `future-input` | restarted | restarted |
| `y` or `yes` | restarted | restarted |
| `n` | exit 0, not restarted | exit 0, not restarted |
| `-r`, `</dev/null` | restarted | restarted |
| `--force`, `</dev/null` | exit 0, not restarted | restarted |

**Rust behavior.** The dead-session prompt in `attach` counts only an answer
that someone gave (`ask::ask_consent`):
- At a terminal, as in Node, any line but a lone `n` restarts, an empty line
  included.
- Anywhere else, only `y` or `yes`, in any case, restarts.
- EOF is never an answer, at a terminal or not.
- With no answer, attach ends the prompt line and prints two lines to standard
  error. It exits 1 and keeps the exited record:

  ```
  pty attach: Session "<name>" was not restarted: the restart prompt got no answer.
    Answer y, or pass -r to restart it without asking.
  ```
- A lone `n` still exits 0. `-r` still restarts without asking. `--force`
  still only allows attaching from inside another session, and it answers
  nothing.

**Why.** Agents and scripts call `attach` without a terminal (#214). A prompt
that nobody answered is not consent to run a stored command again, and a
rerun repeats whatever the command does. Off a terminal, an empty line or
relayed input is no more a person's answer than EOF is, so only `y` counts
there. Node already restarts nothing at EOF, but it exits 0, the same as a
declined prompt. Exit 1, with the flag that restarts, tells the caller what
happened and what to do.

**Client effect.**
- A script or seat that relied on `attach` restarting a dead session without a
  terminal must pass `-r` now, or pipe `y`.
- At EOF, `attach` exits 1 where Node exits 0. Neither restarts.
- Off a terminal, any line but `y`, `yes` or `n` exits 1 where Node restarts.
- At a terminal nothing changes, except that EOF (Ctrl+D) exits 1.

**Not changed.** `pty restart` asks `Kill and restart? [Y/n]` through the old
`ask`, so at EOF it still stops a running session and starts it again. Node
exits 0 there and leaves the session running. That is a parity defect outside
this decision and is left for a separate change.

**Test.** `crates/pty-conformance/tests/attach_no_restart.rs`:
- `a_restart_prompt_at_eof_declines`,
  `a_restart_prompt_off_a_terminal_needs_a_yes` and
  `force_is_not_an_answer_to_the_restart_prompt` fail on Node.
  `crates/pty-conformance/divergences.toml` records all three as intended.
- `a_piped_yes_restarts` and `auto_restart_needs_no_terminal` pass on both.
- The terminal cases before them are unchanged and pass on both.

The unit test `consent_needs_an_answer_and_off_a_terminal_a_yes` in
`crates/pty/src/cli/ask.rs` covers each answer at a terminal and off one.

**Migration / negotiation.** Pass `-r` to restart a dead session without a
terminal, or `--no-restart` never to restart one. Release note: `pty attach`
no longer restarts a dead session when the restart prompt gets no answer.
That covers stdin at EOF, and stdin that is not a terminal unless it gives `y`
or `yes`. It exits 1 instead, without running the stored command. Any
script or seat that relied on `attach` restarting a dead session
non-interactively must pass `-r` now.
