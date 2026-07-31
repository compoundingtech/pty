# CLI and package requirements

> **Role.** Define the executable and distribution boundary. These requirements
> refine the parent surface contract.

## Requirements

- **PTY.SURF.CLI-R01 — Single CLI process.** `bin/pty` validates compiled output
  and imports `dist/cli.js` in-process without spawning a forwarding child.
  _refines: PTY.SURF-R04, PTY.SURF-R07._
- **PTY.SURF.CLI-R02 — Inherited descriptor fidelity.** Every descriptor
  inherited by the package entrypoint remains available to CLI features; the
  launcher does not assume only stdin/stdout/stderr. _refines: PTY.SURF-R04._
- **PTY.SURF.CLI-R03 — Direct signal ownership.** The OS-visible CLI process is
  the process running command handlers, so termination signals do not depend on
  a wrapper's forwarding or orphan a handler child. _refines: PTY.SURF-R04._
- **PTY.SURF.CLI-R04 — Machine attach separation.** In machine mode, stdin and
  stdout remain the controlling terminal, terminal events use only the selected
  descriptor, diagnostics use stderr, and exactly one `EXIT` or `DETACH` outcome
  is flushed before clean CLI completion. _refines: PTY.SURF-R03,
  PTY.SURF-R04._
- **PTY.SURF.CLI-R05 — Fail-closed v1 admission.** The descriptor is a valid
  writable inherited fd greater than or equal to 3. Any emitted terminal
  baseline begins with geometry then screen; a local detach may instead emit
  its terminal outcome before the baseline. Unsupported server-event order or
  EOF without a terminal outcome exits non-zero without silently degrading to
  raw output. _refines: PTY.SURF-R03._
- **PTY.SURF.CLI-R06 — Completion parity.** Required values such as the machine
  stream fd are emitted as value-taking options in bash, fish, and zsh rather
  than boolean switches. _refines: PTY.SURF-R05._
